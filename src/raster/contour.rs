use super::{
    execute::{Job, check_status, processing_error, spatial_ref, string_list},
    options::{self, common, debug_builder},
    polygonize::validate_layer,
};
use crate::{
    dataset::{Kind, Options, Source},
    operation::{Control, Error, Report, Result},
};
use gdal::vector::{LayerAccess, LayerOptions, OGRFieldType, OGRwkbGeometryType};
use std::path::Path;

/// Generate elevation contour lines in a new vector dataset (GPKG).
#[derive(Clone)]
#[must_use]
pub struct Contour {
    job: Job,
    band: usize,
    interval: Option<f64>,
    levels: Option<Vec<f64>>,
    base: f64,
    nodata: Option<f64>,
    ignore_nodata: bool,
    ignore_mask: bool,
    three_d: bool,
    layer: String,
    field: String,
    layer_options: Options,
}
debug_builder!(Contour);
impl Contour {
    /// Describe contours; choose interval or levels before running.
    pub fn new(source: impl Into<Source>, output: impl AsRef<Path>) -> Self {
        Self {
            job: Job::new(source, output, "GPKG"),
            band: 1,
            interval: None,
            levels: None,
            base: 0.,
            nodata: None,
            ignore_nodata: false,
            ignore_mask: false,
            three_d: false,
            layer: "contours".into(),
            field: "elevation".into(),
            layer_options: Options::default(),
        }
    }
    common!();
    /// Select a one-based elevation band.
    pub fn band(mut self, value: usize) -> Self {
        self.band = value;
        self
    }
    /// Select regularly spaced levels; conflicts with levels.
    pub fn interval(mut self, value: f64) -> Self {
        self.interval = Some(value);
        self
    }
    /// Select exact contour elevations; conflicts with interval.
    pub fn levels(mut self, values: impl IntoIterator<Item = f64>) -> Self {
        self.levels = Some(values.into_iter().collect());
        self
    }
    /// Shift the regular level sequence. Requires interval.
    pub fn base(mut self, value: f64) -> Self {
        self.base = value;
        self
    }
    /// Override the input band's NoData value.
    pub fn nodata(mut self, value: f64) -> Self {
        self.nodata = Some(value);
        self
    }
    /// Explicitly process source NoData values as regular elevations.
    pub fn ignore_nodata(mut self, enabled: bool) -> Self {
        self.ignore_nodata = enabled;
        self
    }
    /// Accept ignoring an independent/alpha validity mask; GDAL contour cannot use it.
    pub fn ignore_mask(mut self, enabled: bool) -> Self {
        self.ignore_mask = enabled;
        self
    }
    /// Write elevation into geometry Z coordinates as well as the attribute.
    pub fn three_dimensional(mut self, enabled: bool) -> Self {
        self.three_d = enabled;
        self
    }
    /// Name the output layer.
    pub fn layer_name(mut self, value: impl Into<String>) -> Self {
        self.layer = value.into();
        self
    }
    /// Name the elevation attribute.
    pub fn elevation_field(mut self, value: impl Into<String>) -> Self {
        self.field = value.into();
        self
    }
    /// Set an output layer creation option.
    pub fn layer_option(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.layer_options.set(key.into(), value.into());
        self
    }
    /// Execute and check explicit closure.
    pub fn run(self) -> Result<Report> {
        self.run_with(&mut Control::default())
    }
    /// Execute with borrowed progress and cancellation.
    pub fn run_with(self, control: &mut Control<'_>) -> Result<Report> {
        options::band(self.band)?;
        validate_layer(&self.layer, &self.field, &self.layer_options)?;
        if self.interval.is_some() == self.levels.is_some() {
            return Err(Error::invalid(
                "contour requires exactly one of interval or levels",
            ));
        }
        if self.ignore_nodata && self.nodata.is_some() {
            return Err(Error::invalid("nodata and ignore_nodata conflict"));
        }
        if self.base != 0. && self.interval.is_none() {
            return Err(Error::invalid("base requires interval"));
        }
        options::finite(self.base, "contour base")?;
        let mut args = Options::default();
        if let Some(interval) = self.interval {
            options::positive(interval, "contour interval")?;
            args.set("LEVEL_INTERVAL".into(), interval.to_string());
            args.set("LEVEL_BASE".into(), self.base.to_string());
        }
        if let Some(levels) = self.levels {
            if levels.is_empty()
                || levels.iter().any(|v| !v.is_finite())
                || levels.windows(2).any(|v| v[0] >= v[1])
            {
                return Err(Error::invalid(
                    "contour levels must be nonempty, finite, and strictly increasing",
                ));
            }
            args.set(
                "FIXED_LEVELS".into(),
                levels
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(","),
            );
        }
        if self.nodata.is_some_and(f64::is_infinite) {
            return Err(Error::invalid("NoData must not be infinite"));
        }
        let layer_options = self.layer_options.strings();
        let refs = layer_options.iter().map(String::as_str).collect::<Vec<_>>();
        self.job
            .run(Kind::Raster, true, control, move |prepared, progress| {
                let source = &prepared.inputs[0];
                let band = options::source_band(source, self.band)?;
                // SAFETY: the band handle is valid for this metadata query.
                let flags = unsafe { gdal_sys::GDALGetMaskFlags(band.c_rasterband()) };
                if !self.ignore_mask && !matches!(flags, 1 | 8) {
                    return Err(Error::unsupported(
                        "normalize the independent mask to NoData or explicitly ignore_mask",
                    ));
                }
                if !self.ignore_nodata
                    && let Some(value) = self.nodata.or_else(|| band.no_data_value())
                {
                    args.set("NODATA".into(), value.to_string());
                }
                let srs = spatial_ref(source)?;
                prepared.create_vector()?;
                let layer = prepared
                    .output
                    .as_mut()
                    .expect("created output")
                    .create_layer(LayerOptions {
                        name: &self.layer,
                        srs: srs.as_ref(),
                        ty: if self.three_d {
                            OGRwkbGeometryType::wkbLineString25D
                        } else {
                            OGRwkbGeometryType::wkbLineString
                        },
                        options: Some(&refs),
                    })
                    .map_err(processing_error)?;
                let index = layer.defn().fields().count();
                layer
                    .create_defn_fields(&[(&self.field, OGRFieldType::OFTReal)])
                    .map_err(processing_error)?;
                // Use the added field's position even when the driver launders its name.
                if layer.defn().fields().count() != index + 1 {
                    return Err(Error::unsupported("driver did not add the elevation field"));
                }
                args.set("ELEV_FIELD".into(), index.to_string());
                let args = string_list(&args)?;
                let band = options::source_band(&prepared.inputs[0], self.band)?;
                // SAFETY: input band, output layer, option strings and callback remain
                // live throughout the synchronous call; Rust panics cannot cross it.
                let status = unsafe {
                    gdal_sys::GDALContourGenerateEx(
                        band.c_rasterband(),
                        layer.c_layer(),
                        args.as_ptr(),
                        progress.native_callback(),
                        progress.user_data(),
                    )
                };
                check_status(status)
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::raster::test_support::{fixture, pixels};
    #[test]
    fn fixed_levels_preserve_elevation_and_three_dimensional_geometry() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let src = dir.path().join("elevation.tif");
        fixture(&src, 2, 2, &[0f64, 20., 0., 20.])?;
        let out = dir.path().join("lines.gpkg");
        let _ = Contour::new(&src, &out)
            .levels([5., 15.])
            .three_dimensional(true)
            .run()?;
        let ds = gdal::Dataset::open(&out)?;
        let mut layer = ds.layer(0)?;
        assert_eq!(layer.feature_count(), 2);
        for f in layer.features() {
            let elevation = f.field_as_double(0)?.unwrap();
            assert!([5., 15.].contains(&elevation));
            assert_eq!(f.geometry().unwrap().get_point(0).2, elevation);
        }
        ds.close()?;
        assert!(
            Contour::new(&src, dir.path().join("bad.gpkg"))
                .interval(10.)
                .levels([5.])
                .run()
                .is_err()
        );
        assert_eq!(pixels(&src)?, [0., 20., 0., 20.]);
        Ok(())
    }

    #[test]
    fn laundered_shapefile_field_receives_elevation() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let src = dir.path().join("elevation.tif");
        fixture(&src, 2, 2, &[0f32, 20., 0., 20.])?;
        let out = dir.path().join("lines.shp");
        let _ = Contour::new(&src, &out)
            .driver("ESRI Shapefile")
            .elevation_field("contour_elevation")
            .levels([5., 15.])
            .run()?;
        let ds = gdal::Dataset::open(out)?;
        let mut layer = ds.layer(0)?;
        let mut values = layer
            .features()
            .map(|f| f.field_as_double(0).unwrap().unwrap())
            .collect::<Vec<_>>();
        values.sort_by(f64::total_cmp);
        assert_eq!(values, [5., 15.]);
        ds.close()?;
        Ok(())
    }

    #[test]
    fn tuple_nodata_mask_requires_explicit_normalization_or_opt_out() -> anyhow::Result<()> {
        use gdal::{Metadata, raster::Buffer};
        let dir = tempfile::tempdir()?;
        let src = dir.path().join("tuple.tif");
        let mut ds = gdal::DriverManager::get_driver_by_name("GTiff")?
            .create_with_band_type::<u8, _>(&src, 2, 2, 2)?;
        ds.rasterband(1)?
            .write((0, 0), (2, 2), &mut Buffer::new((2, 2), vec![0, 20, 0, 20]))?;
        ds.rasterband(2)?.fill(0., None)?;
        ds.set_metadata_item("NODATA_VALUES", "0 0", "")?;
        ds.close()?;
        let out = dir.path().join("unsafe.gpkg");
        let error = Contour::new(&src, &out)
            .levels([5., 15.])
            .run()
            .unwrap_err();
        assert_eq!(
            error.output_state(),
            crate::operation::OutputState::NotModified
        );
        assert!(!out.exists());
        let out = dir.path().join("explicit.gpkg");
        let _ = Contour::new(&src, &out)
            .levels([5., 15.])
            .ignore_mask(true)
            .nodata(0.)
            .run()?;
        assert_eq!(gdal::Dataset::open(out)?.layer(0)?.feature_count(), 0);
        Ok(())
    }
}
