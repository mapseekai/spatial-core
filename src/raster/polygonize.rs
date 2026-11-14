use super::{
    Connectivity, Mask,
    execute::{Job, check_status, processing_error, spatial_ref, string_list},
    options::{self, common, debug_builder},
};
use crate::{
    dataset::{Kind, Options, Source, validate_text},
    operation::{Control, Error, Report, Result},
};
use gdal::vector::{LayerAccess, LayerOptions, OGRFieldType, OGRwkbGeometryType};
use std::{path::Path, ptr};

/// Convert connected pixel categories into polygons in a new vector dataset (GPKG).
#[derive(Clone)]
#[must_use]
pub struct Polygonize {
    job: Job,
    band: usize,
    connectivity: Connectivity,
    mask: Mask,
    floating: bool,
    layer: String,
    field: String,
    layer_options: Options,
}
debug_builder!(Polygonize);
impl Polygonize {
    /// Default: band 1, integer categories, validity mask, four neighbors.
    pub fn new(source: impl Into<Source>, output: impl AsRef<Path>) -> Self {
        Self {
            job: Job::new(source, output, "GPKG"),
            band: 1,
            connectivity: Connectivity::default(),
            mask: Mask::default(),
            floating: false,
            layer: "polygons".into(),
            field: "value".into(),
            layer_options: Options::default(),
        }
    }
    common!();
    /// Select a one-based category band.
    pub fn band(mut self, value: usize) -> Self {
        self.band = value;
        self
    }
    /// Select four or eight connected neighbors.
    pub fn connectivity(mut self, value: Connectivity) -> Self {
        self.connectivity = value;
        self
    }
    /// Select which pixels are valid.
    pub fn mask(mut self, value: Mask) -> Self {
        self.mask = value;
        self
    }
    /// Use GDALFPolygonize's Float32 category buffer instead of the integer path.
    pub fn floating_values(mut self, enabled: bool) -> Self {
        self.floating = enabled;
        self
    }
    /// Name the output layer.
    pub fn layer_name(mut self, value: impl Into<String>) -> Self {
        self.layer = value.into();
        self
    }
    /// Name the category attribute.
    pub fn value_field(mut self, value: impl Into<String>) -> Self {
        self.field = value.into();
        self
    }
    /// Set a layer creation option, such as SPATIAL_INDEX.
    pub fn layer_option(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.layer_options.set(key.into(), value.into());
        self
    }
    /// Execute and close the output before returning.
    pub fn run(self) -> Result<Report> {
        self.run_with(&mut Control::default())
    }
    /// Execute with borrowed progress and cancellation.
    pub fn run_with(self, control: &mut Control<'_>) -> Result<Report> {
        options::band(self.band)?;
        validate_layer(&self.layer, &self.field, &self.layer_options)?;
        let layer_options = self.layer_options.strings();
        let refs = layer_options.iter().map(String::as_str).collect::<Vec<_>>();
        self.job
            .run(Kind::Raster, true, control, move |prepared, progress| {
                let source = &prepared.inputs[0];
                let band = options::source_band(source, self.band)?;
                if !self.floating
                    && (!band.band_type().is_integer()
                        || band.band_type() == gdal::raster::GdalDataType::UInt64)
                {
                    return Err(Error::invalid(
                        "integer polygonize requires a signed-compatible integer band",
                    ));
                }
                let srs = spatial_ref(source)?;
                // Validate mask selection before creating the output.
                let _ = options::mask(source, &band, self.mask)?;
                prepared.create_vector()?;
                let band = options::source_band(&prepared.inputs[0], self.band)?;
                let mask = options::mask(&prepared.inputs[0], &band, self.mask)?;
                let layer = prepared
                    .output
                    .as_mut()
                    .expect("created output")
                    .create_layer(LayerOptions {
                        name: &self.layer,
                        srs: srs.as_ref(),
                        ty: OGRwkbGeometryType::wkbPolygon,
                        options: Some(&refs),
                    })
                    .map_err(processing_error)?;
                let field = layer.defn().fields().count();
                layer
                    .create_defn_fields(&[(
                        &self.field,
                        if self.floating {
                            OGRFieldType::OFTReal
                        } else {
                            OGRFieldType::OFTInteger64
                        },
                    )])
                    .map_err(processing_error)?;
                // Drivers can launder field names (for example DBF's ten-character limit).
                if layer.defn().fields().count() != field + 1 {
                    return Err(Error::unsupported("driver did not add the category field"));
                }
                let field = i32::try_from(field)
                    .map_err(|_| Error::unsupported("too many output fields"))?;
                let mut options = Options::default();
                if self.connectivity == Connectivity::Eight {
                    options.set("8CONNECTED".into(), "8".into());
                }
                let options = string_list(&options)?;
                // SAFETY: borrowed band, optional mask, layer, argv and callback all
                // remain live until the synchronous algorithm has stopped.
                let status = unsafe {
                    let mask = mask.as_ref().map_or(ptr::null_mut(), |m| m.c_rasterband());
                    if self.floating {
                        gdal_sys::GDALFPolygonize(
                            band.c_rasterband(),
                            mask,
                            layer.c_layer(),
                            field,
                            options.as_ptr(),
                            progress.native_callback(),
                            progress.user_data(),
                        )
                    } else {
                        gdal_sys::GDALPolygonize(
                            band.c_rasterband(),
                            mask,
                            layer.c_layer(),
                            field,
                            options.as_ptr(),
                            progress.native_callback(),
                            progress.user_data(),
                        )
                    }
                };
                check_status(status)
            })
    }
}
pub(super) fn validate_layer(layer: &str, field: &str, options: &Options) -> Result<()> {
    validate_text(layer, "layer name")?;
    validate_text(field, "field name")?;
    options.validate()?;
    if !layer
        .chars()
        .all(|c| c.is_alphanumeric() || c == '_' || c == '-')
    {
        return Err(Error::invalid(
            "algorithm layer names must contain only letters, digits, '_' or '-'",
        ));
    }
    if options.get("OVERWRITE").is_some() {
        return Err(Error::invalid("algorithm output does not permit OVERWRITE"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::raster::test_support::{fixture, pixels};
    #[test]
    fn integer_categories_use_masks_and_connectivity() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let src = dir.path().join("classes.tif");
        fixture(&src, 2, 2, &[7u8, 0, 0, 7])?;
        let ds = gdal::Dataset::open_ex(
            &src,
            gdal::DatasetOptions {
                open_flags: gdal::GdalOpenFlags::GDAL_OF_UPDATE,
                ..Default::default()
            },
        )?;
        ds.rasterband(1)?.set_no_data_value(Some(0.))?;
        ds.close()?;
        for (connectivity, count) in [(Connectivity::Four, 2), (Connectivity::Eight, 1)] {
            let out = dir.path().join(format!("{count}.gpkg"));
            let _ = Polygonize::new(&src, &out)
                .connectivity(connectivity)
                .run()?;
            let ds = gdal::Dataset::open(&out)?;
            let mut layer = ds.layer(0)?;
            assert_eq!(layer.feature_count(), count);
            assert_eq!(layer.spatial_ref().unwrap().auth_code()?, 4326);
            for f in layer.features() {
                assert_eq!(f.field_as_integer64(0)?, Some(7));
            }
            ds.close()?;
        }
        assert_eq!(pixels(&src)?, [7., 0., 0., 7.]);
        Ok(())
    }
    #[test]
    fn floating_categories_are_explicit() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let src = dir.path().join("values.tif");
        fixture(&src, 2, 1, &[1.25f32, 2.5])?;
        assert!(
            Polygonize::new(&src, dir.path().join("integer.gpkg"))
                .run()
                .is_err()
        );
        let out = dir.path().join("float.gpkg");
        let _ = Polygonize::new(&src, &out).floating_values(true).run()?;
        let ds = gdal::Dataset::open(out)?;
        let mut layer = ds.layer(0)?;
        let values = layer
            .features()
            .map(|f| f.field_as_double(0).unwrap().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(values, [1.25, 2.5]);
        Ok(())
    }

    #[test]
    fn laundered_shapefile_field_receives_category_values() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let src = dir.path().join("classes.tif");
        fixture(&src, 2, 1, &[7u8, 9])?;
        let out = dir.path().join("classes.shp");
        let _ = Polygonize::new(&src, &out)
            .driver("ESRI Shapefile")
            .value_field("classification_value")
            .run()?;
        let ds = gdal::Dataset::open(out)?;
        let mut layer = ds.layer(0)?;
        assert_ne!(
            layer.defn().fields().next().unwrap().name(),
            "classification_value"
        );
        let values = layer
            .features()
            .map(|f| f.field_as_integer64(0).unwrap().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(values, [7, 9]);
        ds.close()?;
        Ok(())
    }
}
