use super::{
    DataType, Resampling,
    execute::Job,
    options::{self, common, debug_builder, numbers, pair},
};
use crate::{
    dataset::{Kind, Options, Source},
    gdal_ext::raster::utility,
    operation::{Control, Error, Report, Result},
};
use std::path::{Path, PathBuf};

/// Reproject, resample or mosaic ordered inputs into a new local raster (GTiff).
#[derive(Clone)]
#[must_use]
pub struct Warp {
    job: Job,
    target_srs: Option<String>,
    source_srs: Option<String>,
    resampling: Resampling,
    resolution: Option<(f64, f64)>,
    size: Option<(u32, u32)>,
    bounds: Option<[f64; 4]>,
    source_nodata: Option<Vec<f64>>,
    target_nodata: Option<Vec<f64>>,
    data_type: Option<DataType>,
    cutline: Option<PathBuf>,
    crop: bool,
    aligned: bool,
    multi: bool,
    warp_options: Options,
    transformer_options: Options,
}
debug_builder!(Warp);
impl Warp {
    /// Inputs are processed in order. Constructing this builder performs no I/O.
    pub fn new<I, S>(sources: I, output: impl AsRef<Path>) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<Source>,
    {
        Self {
            job: Job::multiple(sources, output, "GTiff"),
            target_srs: None,
            source_srs: None,
            resampling: Resampling::default(),
            resolution: None,
            size: None,
            bounds: None,
            source_nodata: None,
            target_nodata: None,
            data_type: None,
            cutline: None,
            crop: false,
            aligned: false,
            multi: false,
            warp_options: Options::default(),
            transformer_options: Options::default(),
        }
    }
    common!();
    /// Transform coordinates into this target CRS.
    pub fn target_srs(mut self, definition: impl Into<String>) -> Self {
        self.target_srs = Some(definition.into());
        self
    }
    /// Explicitly override the source CRS for every input.
    pub fn source_srs(mut self, definition: impl Into<String>) -> Self {
        self.source_srs = Some(definition.into());
        self
    }
    /// Set interpolation or aggregation. Default is nearest neighbor.
    pub fn resampling(mut self, value: Resampling) -> Self {
        self.resampling = value;
        self
    }
    /// Set positive pixel spacing in output CRS units; conflicts with size.
    pub fn resolution(mut self, x: f64, y: f64) -> Self {
        self.resolution = Some((x, y));
        self
    }
    /// Set output pixel dimensions; conflicts with resolution.
    pub fn size(mut self, width: u32, height: u32) -> Self {
        self.size = Some((width, height));
        self
    }
    /// Set xmin, ymin, xmax, ymax in output CRS units.
    pub fn bounds(mut self, bounds: [f64; 4]) -> Self {
        self.bounds = Some(bounds);
        self
    }
    /// Override source NoData per band (GDAL repeats the last value if needed).
    pub fn source_nodata(mut self, values: impl IntoIterator<Item = f64>) -> Self {
        self.source_nodata = Some(values.into_iter().collect());
        self
    }
    /// Set output NoData per band. NaN is allowed.
    pub fn nodata(mut self, values: impl IntoIterator<Item = f64>) -> Self {
        self.target_nodata = Some(values.into_iter().collect());
        self
    }
    /// Set the output pixel type.
    pub fn data_type(mut self, value: DataType) -> Self {
        self.data_type = Some(value);
        self
    }
    /// Clip with a local vector dataset. GDAL opens this auxiliary input itself.
    pub fn cutline(mut self, path: impl AsRef<Path>) -> Self {
        self.cutline = Some(path.as_ref().to_owned());
        self
    }
    /// Crop the extent to the cutline. Requires cutline.
    pub fn crop_to_cutline(mut self, enabled: bool) -> Self {
        self.crop = enabled;
        self
    }
    /// Align extent to a multiple of resolution. Requires resolution.
    pub fn target_aligned_pixels(mut self, enabled: bool) -> Self {
        self.aligned = enabled;
        self
    }
    /// Enable native I/O/processing overlap. Callbacks remain serialized.
    pub fn multithreaded(mut self, enabled: bool) -> Self {
        self.multi = enabled;
        self
    }
    /// Set a native warp option, such as NUM_THREADS.
    pub fn warp_option(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.warp_options.set(key.into(), value.into());
        self
    }
    /// Set a native transformer option, such as RPC_HEIGHT.
    pub fn transformer_option(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.transformer_options.set(key.into(), value.into());
        self
    }
    /// Execute synchronously and check dataset closure.
    pub fn run(self) -> Result<Report> {
        self.run_with(&mut Control::default())
    }
    /// Execute with borrowed progress and cancellation.
    pub fn run_with(self, control: &mut Control<'_>) -> Result<Report> {
        if self.resolution.is_some() && self.size.is_some() {
            return Err(Error::invalid("resolution and size are mutually exclusive"));
        }
        if self.crop && self.cutline.is_none() {
            return Err(Error::invalid("crop_to_cutline requires a cutline"));
        }
        if self.aligned && self.resolution.is_none() {
            return Err(Error::invalid("target_aligned_pixels requires resolution"));
        }
        let mut args = vec!["-of".into(), self.job.driver.clone()];
        pair(&mut args, "-r", self.resampling.name());
        for (flag, srs) in [("-s_srs", &self.source_srs), ("-t_srs", &self.target_srs)] {
            if let Some(srs) = srs {
                crate::dataset::validate_text(srs, "spatial reference")?;
                pair(&mut args, flag, srs);
            }
        }
        if let Some((x, y)) = self.resolution {
            options::positive(x, "x resolution")?;
            options::positive(y, "y resolution")?;
            numbers(&mut args, "-tr", [x, y]);
        }
        if let Some((w, h)) = self.size {
            options::size(w, h)?;
            args.extend(["-ts".into(), w.to_string(), h.to_string()]);
        }
        if let Some(bounds) = self.bounds {
            options::bounds(bounds)?;
            numbers(&mut args, "-te", bounds);
        }
        for (flag, values) in [
            ("-srcnodata", self.source_nodata),
            ("-dstnodata", self.target_nodata),
        ] {
            if let Some(values) = values {
                if values.is_empty() || values.iter().any(|v| v.is_infinite()) {
                    return Err(Error::invalid(
                        "NoData list must be nonempty and contain no infinity",
                    ));
                }
                pair(
                    &mut args,
                    flag,
                    values
                        .iter()
                        .map(ToString::to_string)
                        .collect::<Vec<_>>()
                        .join(" "),
                );
            }
        }
        if let Some(data_type) = self.data_type {
            pair(&mut args, "-ot", data_type.name());
        }
        if self.crop {
            args.push("-crop_to_cutline".into());
        }
        if self.aligned {
            args.push("-tap".into());
        }
        if self.multi {
            args.push("-multi".into());
        }
        self.warp_options.validate()?;
        self.warp_options.append_args("-wo", &mut args);
        self.transformer_options.validate()?;
        self.transformer_options.append_args("-to", &mut args);
        self.job.creation.append_args("-co", &mut args);
        self.job
            .run(Kind::Raster, false, control, move |prepared, progress| {
                for srs in [&self.source_srs, &self.target_srs].into_iter().flatten() {
                    options::srs(srs)?;
                }
                if let Some(path) = self.cutline {
                    let source = Source::new(path);
                    let name = source.name()?;
                    let name = name.to_str().map_err(|e| {
                        Error::unsupported("cutline argv requires a UTF-8 path").with_source(e)
                    })?;
                    pair(&mut args, "-cutline", name);
                }
                prepared.utility(utility::Kind::Warp, &args, None, progress)
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::raster::test_support::{fixture, pixels};
    #[test]
    fn mosaics_ordered_sources_and_reprojects() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let a = dir.path().join("a.tif");
        let b = dir.path().join("b.tif");
        fixture(&a, 2, 2, &[1u8; 4])?;
        fixture(&b, 2, 2, &[9u8; 4])?;
        let out = dir.path().join("mosaic.tif");
        let _ = Warp::new([&a, &b], &out)
            .target_srs("EPSG:3857")
            .size(2, 2)
            .multithreaded(true)
            .warp_option("NUM_THREADS", "2")
            .run()?;
        assert_eq!(pixels(&out)?, [9.; 4]);
        let ds = gdal::Dataset::open(&out)?;
        assert_eq!(ds.spatial_ref()?.auth_code()?, 3857);
        assert!(ds.geo_transform()?[1] > 100_000.);
        ds.close()?;
        assert!(
            Warp::new([&a], dir.path().join("conflict.tif"))
                .size(1, 1)
                .resolution(1., 1.)
                .run()
                .is_err()
        );
        assert!(
            Warp::new(Vec::<Source>::new(), dir.path().join("empty.tif"))
                .run()
                .is_err()
        );
        Ok(())
    }
}
