use super::{
    Resampling,
    execute::Job,
    options::{self, debug_builder, numbers, pair},
};
use crate::{
    config::gdal::GdalOptions,
    dataset::{Kind, Source},
    gdal_ext::raster::utility,
    operation::{Control, Error, Report, Result},
};
use std::path::Path;

/// Build a virtual mosaic or band stack. Source data must remain accessible.
/// Incompatible sources fail instead of being silently skipped.
/// Source open options are unsupported: first persist them with [`super::Translate`]
/// to a VRT, then use that VRT as the source.
#[derive(Clone)]
#[must_use]
pub struct BuildVrt {
    job: Job,
    separate: bool,
    bands: Option<Vec<usize>>,
    resolution: Option<(f64, f64)>,
    bounds: Option<[f64; 4]>,
    resampling: Resampling,
    nodata: Option<f64>,
}
debug_builder!(BuildVrt);
impl BuildVrt {
    /// Describe ordered input rasters and an absent local .vrt output.
    pub fn new<I, S>(sources: I, output: impl AsRef<Path>) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<Source>,
    {
        Self {
            job: Job::multiple(sources, output, "VRT"),
            separate: false,
            bands: None,
            resolution: None,
            bounds: None,
            resampling: Resampling::default(),
            nodata: None,
        }
    }
    /// Stack bands instead of mosaicking. Native band defaults depend on GDAL version.
    pub fn separate(mut self, enabled: bool) -> Self {
        self.separate = enabled;
        self
    }
    /// Explicitly select one-based source band indices.
    pub fn bands(mut self, values: impl IntoIterator<Item = usize>) -> Self {
        self.bands = Some(values.into_iter().collect());
        self
    }
    /// Set positive pixel spacing in the shared source CRS.
    pub fn resolution(mut self, x: f64, y: f64) -> Self {
        self.resolution = Some((x, y));
        self
    }
    /// Set xmin, ymin, xmax, ymax in the shared source CRS.
    pub fn bounds(mut self, bounds: [f64; 4]) -> Self {
        self.bounds = Some(bounds);
        self
    }
    /// Set interpolation used when reading the VRT.
    pub fn resampling(mut self, value: Resampling) -> Self {
        self.resampling = value;
        self
    }
    /// Set the VRT's output NoData value.
    pub fn nodata(mut self, value: f64) -> Self {
        self.nodata = Some(value);
        self
    }
    /// Apply configuration during construction; reopen requires equivalent source access.
    pub fn gdal_options(mut self, value: GdalOptions) -> Self {
        self.job.config = value;
        self
    }
    /// Execute and close the VRT before closing its inputs.
    pub fn run(self) -> Result<Report> {
        self.run_with(&mut Control::default())
    }
    /// Execute with borrowed progress and cancellation.
    pub fn run_with(self, control: &mut Control<'_>) -> Result<Report> {
        self.resampling.validate_translate()?;
        let mut args = vec!["-strict".into()];
        pair(&mut args, "-r", self.resampling.name());
        if self.separate {
            args.push("-separate".into());
        }
        if let Some(bands) = &self.bands {
            if bands.is_empty() {
                return Err(Error::invalid("band selection must not be empty"));
            }
            for band in bands {
                options::band(*band)?;
                pair(&mut args, "-b", band);
            }
        }
        if let Some((x, y)) = self.resolution {
            options::positive(x, "x resolution")?;
            options::positive(y, "y resolution")?;
            pair(&mut args, "-resolution", "user");
            numbers(&mut args, "-tr", [x, y]);
        }
        if let Some(bounds) = self.bounds {
            options::bounds(bounds)?;
            numbers(&mut args, "-te", bounds);
        }
        if let Some(value) = self.nodata {
            if value.is_infinite() {
                return Err(Error::invalid("NoData must not be infinite"));
            }
            pair(&mut args, "-vrtnodata", value);
        }
        self.job
            .run(Kind::Raster, false, control, move |prepared, progress| {
                if let Some(bands) = self.bands {
                    for input in &prepared.inputs {
                        for band in &bands {
                            options::source_band(input, *band)?;
                        }
                    }
                }
                prepared.utility(utility::Kind::BuildVrt, &args, None, progress)
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::raster::test_support::{fixture, pixels};
    #[test]
    fn source_open_options_cannot_silently_change_persisted_pixels() -> anyhow::Result<()> {
        use crate::{
            operation::{ErrorKind, OutputState, Stage},
            raster::{NativeBuildVrt, Translate},
        };
        let dir = tempfile::tempdir()?;
        let src = dir.path().join("overview.tif");
        fixture(&src, 4, 4, &(1u8..=16).collect::<Vec<_>>())?;
        let mut ds = gdal::Dataset::open_ex(
            &src,
            gdal::DatasetOptions {
                open_flags: gdal::GdalOpenFlags::GDAL_OF_UPDATE,
                ..Default::default()
            },
        )?;
        ds.build_overviews("NEAREST", &[2], &[])?;
        ds.close()?;
        let source = Source::new(&src).open_option("OVERVIEW_LEVEL", "0");
        let direct = source.open_kind(Kind::Raster)?;
        assert_eq!(
            direct.rasterband(1)?.read_band_as::<u8>()?.data(),
            &[1, 3, 9, 11]
        );
        direct.close()?;
        for native in [false, true] {
            // The second input must also be checked; the first has no open options.
            let sources = [Source::new(&src), source.clone()];
            let out = dir.path().join(format!("rejected-{native}.vrt"));
            if native {
                std::fs::write(&out, "existing output")?;
            }
            let error = if native {
                NativeBuildVrt::new(sources, &out, ["-strict"]).run()
            } else {
                BuildVrt::new(sources, &out).run()
            }
            .unwrap_err();
            assert_eq!(error.kind(), ErrorKind::Unsupported);
            assert_eq!(error.stage(), Stage::Prepare);
            assert_eq!(error.output_state(), OutputState::NotModified);
            if native {
                assert_eq!(std::fs::read_to_string(out)?, "existing output");
            } else {
                assert!(!out.exists());
            }
        }
        // Translate persists per-source options, so the supported two-step path reads correctly.
        let proxy = dir.path().join("source.vrt");
        let _ = Translate::new(source, &proxy).driver("VRT").run()?;
        assert_eq!(pixels(&proxy)?, [1., 3., 9., 11.]);
        for native in [false, true] {
            let out = dir.path().join(format!("mosaic-{native}.vrt"));
            let _ = if native {
                NativeBuildVrt::new([&proxy], &out, ["-strict"]).run()?
            } else {
                BuildVrt::new([&proxy], &out).run()?
            };
            assert_eq!(pixels(&out)?, [1., 3., 9., 11.]);
        }
        Ok(())
    }
    #[test]
    fn closes_inputs_and_leaves_a_readable_virtual_mosaic() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let a = dir.path().join("a.tif");
        let b = dir.path().join("b.tif");
        fixture(&a, 2, 2, &[2u8; 4])?;
        fixture(&b, 2, 2, &[8u8; 4])?;
        let out = dir.path().join("mosaic.vrt");
        let _ = BuildVrt::new([&a, &b], &out).run()?;
        assert_eq!(pixels(&out)?, [8.; 4]);
        let stack = dir.path().join("stack.vrt");
        let _ = BuildVrt::new([&a, &b], &stack)
            .separate(true)
            .bands([1])
            .run()?;
        let ds = gdal::Dataset::open(stack)?;
        assert_eq!(ds.raster_count(), 2);
        assert_eq!(ds.rasterband(2)?.read_band_as::<u8>()?.data(), &[8; 4]);
        ds.close()?;
        let _ = pixels(&a)?;
        Ok(())
    }
}
