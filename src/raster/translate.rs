use super::{
    DataType, Resampling,
    execute::Job,
    options::{self, common, debug_builder, numbers, pair},
};
use crate::{
    dataset::{Kind, Source},
    gdal_ext::raster::utility,
    operation::{Control, Error, Report, Result},
};
use std::path::Path;

/// Convert a raster into a new local dataset (GTiff by default).
///
/// ```no_run
/// # fn example() -> spatial_core::operation::Result<()> {
/// let _ = spatial_core::raster::Translate::new("image.tif", "rgb.tif")
///     .bands([1, 2, 3]).creation_option("COMPRESS", "DEFLATE").run()?;
/// # Ok(()) }
/// ```
#[derive(Clone)]
#[must_use]
pub struct Translate {
    job: Job,
    bands: Option<Vec<usize>>,
    data_type: Option<DataType>,
    size: Option<(u32, u32)>,
    window: Option<[u32; 4]>,
    resampling: Resampling,
    nodata: Option<f64>,
    srs: Option<String>,
    scale: Option<[f64; 4]>,
}
debug_builder!(Translate);
impl Translate {
    /// Describe a conversion without I/O. The default output format is GTiff.
    pub fn new(source: impl Into<Source>, output: impl AsRef<Path>) -> Self {
        Self {
            job: Job::new(source, output, "GTiff"),
            bands: None,
            data_type: None,
            size: None,
            window: None,
            resampling: Resampling::default(),
            nodata: None,
            srs: None,
            scale: None,
        }
    }
    common!();
    /// Select output bands in order, using one-based source band indices.
    pub fn bands(mut self, bands: impl IntoIterator<Item = usize>) -> Self {
        self.bands = Some(bands.into_iter().collect());
        self
    }
    /// Convert pixel values to a GDAL data type.
    pub fn data_type(mut self, data_type: DataType) -> Self {
        self.data_type = Some(data_type);
        self
    }
    /// Resize to this exact pixel width and height.
    pub fn size(mut self, width: u32, height: u32) -> Self {
        self.size = Some((width, height));
        self
    }
    /// Extract a source pixel window: x offset, y offset, width, height.
    pub fn source_window(mut self, x: u32, y: u32, width: u32, height: u32) -> Self {
        self.window = Some([x, y, width, height]);
        self
    }
    /// Set the resize interpolation. Defaults to nearest neighbor.
    pub fn resampling(mut self, method: Resampling) -> Self {
        self.resampling = method;
        self
    }
    /// Assign the output NoData value; NaN is allowed, infinity is rejected.
    pub fn nodata(mut self, value: f64) -> Self {
        self.nodata = Some(value);
        self
    }
    /// Assign a CRS label without changing coordinates; use Warp for reprojection.
    pub fn assign_srs(mut self, definition: impl Into<String>) -> Self {
        self.srs = Some(definition.into());
        self
    }
    /// Scale input range to output range. Values outside the range are not clamped.
    pub fn scale(
        mut self,
        source_min: f64,
        source_max: f64,
        target_min: f64,
        target_max: f64,
    ) -> Self {
        self.scale = Some([source_min, source_max, target_min, target_max]);
        self
    }
    /// Run and explicitly close all datasets before returning.
    pub fn run(self) -> Result<Report> {
        self.run_with(&mut Control::default())
    }
    /// Run with borrowed progress and cancellation control.
    pub fn run_with(self, control: &mut Control<'_>) -> Result<Report> {
        self.resampling.validate_translate()?;
        let mut args = vec!["-of".into(), self.job.driver.clone()];
        pair(&mut args, "-r", self.resampling.name());
        if let Some(bands) = &self.bands {
            if bands.is_empty() {
                return Err(Error::invalid("band selection must not be empty"));
            }
            for index in bands {
                options::band(*index)?;
                pair(&mut args, "-b", index);
            }
        }
        if let Some((width, height)) = self.size {
            options::size(width, height)?;
            args.extend(["-outsize".into(), width.to_string(), height.to_string()]);
        }
        if let Some([x, y, w, h]) = self.window {
            options::size(w, h)?;
            args.push("-srcwin".into());
            args.extend([x, y, w, h].map(|v| v.to_string()));
        }
        if let Some(value) = self.nodata {
            if value.is_infinite() {
                return Err(Error::invalid("NoData must not be infinite"));
            }
            pair(&mut args, "-a_nodata", value);
        }
        if let Some(data_type) = self.data_type {
            pair(&mut args, "-ot", data_type.name());
        }
        if let Some(srs) = &self.srs {
            crate::dataset::validate_text(srs, "spatial reference")?;
            pair(&mut args, "-a_srs", srs);
        }
        if let Some(values) = self.scale {
            for value in values {
                options::finite(value, "scale range")?;
            }
            if values[0] >= values[1] {
                return Err(Error::invalid("source scale range must be increasing"));
            }
            numbers(&mut args, "-scale", values);
        }
        self.job.creation.append_args("-co", &mut args);
        self.job
            .run(Kind::Raster, false, control, move |prepared, progress| {
                if let Some(srs) = &self.srs {
                    options::srs(srs)?;
                }
                if let Some(bands) = self.bands {
                    for index in bands {
                        options::source_band(&prepared.inputs[0], index)?;
                    }
                }
                if let Some([x, y, w, h]) = self.window {
                    let (width, height) = prepared.inputs[0].raster_size();
                    if u64::from(x) + u64::from(w) > width as u64
                        || u64::from(y) + u64::from(h) > height as u64
                    {
                        return Err(Error::invalid("source window lies outside the raster"));
                    }
                }
                prepared.utility(utility::Kind::Translate, &args, None, progress)
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::raster::test_support::{fixture, pixels};
    #[test]
    fn converts_window_type_and_checks_closed_output() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let src = dir.path().join("source.tif");
        let out = dir.path().join("out.tif");
        fixture(&src, 3, 2, &[1u8, 2, 3, 4, 5, 6])?;
        let _ = Translate::new(&src, &out)
            .source_window(1, 0, 2, 2)
            .data_type(DataType::Float32)
            .nodata(-9999.)
            .creation_option("COMPRESS", "DEFLATE")
            .run()?;
        assert_eq!(pixels(&out)?, [2., 3., 5., 6.]);
        let ds = gdal::Dataset::open(&out)?;
        assert_eq!(ds.geo_transform()?, [1., 1., 0., 2., 0., -1.]);
        assert_eq!(ds.spatial_ref()?.auth_code()?, 4326);
        assert_eq!(ds.rasterband(1)?.band_type(), DataType::Float32);
        assert_eq!(ds.rasterband(1)?.no_data_value(), Some(-9999.));
        ds.close()?;
        assert_eq!(
            Translate::new(&src, &out).run().unwrap_err().kind(),
            crate::operation::ErrorKind::OutputExists
        );
        assert!(
            Translate::new(&src, dir.path().join("bad.tif"))
                .source_window(2, 1, 2, 2)
                .run()
                .is_err()
        );
        assert!(!dir.path().join("bad.tif").exists());
        Ok(())
    }
    #[test]
    fn preserves_source_on_invalid_options_cancellation_and_panic() -> anyhow::Result<()> {
        use crate::{
            control::Cancellation,
            operation::{ErrorKind, OutputState},
        };
        let dir = tempfile::tempdir()?;
        let src = dir.path().join("source.tif");
        fixture(&src, 256, 256, &vec![7u8; 256 * 256])?;
        let before = std::fs::read(&src)?;
        assert!(Translate::new(&src, &src).run().is_err());
        let token = Cancellation::default();
        token.cancel();
        let out = dir.path().join("pre.tif");
        let error = Translate::new(&src, &out)
            .run_with(&mut Control::default().cancellation(&token))
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Cancelled);
        assert_eq!(error.output_state(), OutputState::NotModified);
        assert!(!out.exists());
        for panics in [false, true] {
            let mut callback = |_: &crate::operation::Progress| {
                assert!(!panics, "callback panic");
                std::ops::ControlFlow::Break(())
            };
            let error = Translate::new(&src, dir.path().join(format!("cancel-{panics}.tif")))
                .run_with(&mut Control::default().on_progress(&mut callback))
                .unwrap_err();
            assert_eq!(
                error.kind(),
                if panics {
                    ErrorKind::CallbackPanicked
                } else {
                    ErrorKind::Cancelled
                }
            );
        }
        assert_eq!(before, std::fs::read(src)?);
        Ok(())
    }
}
