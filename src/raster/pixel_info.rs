use super::{PixelValue, options, value};
use crate::{
    config::gdal::GdalOptions,
    dataset::{Kind, Source},
    gdal_ext::query::{self, inspect_error},
    operation::{Control, Error, Outcome, Result},
};
use gdal::{
    GeoTransformEx,
    spatial_ref::{AxisMappingStrategy, CoordTransform, SpatialRef},
};

/// An unambiguous pixel or map-coordinate location.
#[derive(Debug, Clone)]
pub enum SampleLocation {
    /// Zero-based base-resolution pixel indices.
    Pixel { column: usize, row: usize },
    /// Coordinates in the specified CRS, or the raster CRS when None.
    Coordinate {
        x: f64,
        y: f64,
        spatial_ref: Option<String>,
    },
}
/// One band's sample in original stored units.
#[derive(Debug, Clone)]
pub struct BandSample {
    /// One-based band index.
    pub band: usize,
    /// Stored numeric value; integers remain exact.
    pub value: PixelValue,
    /// Whether the GDAL validity mask accepts this sample.
    pub valid: bool,
    /// Native NoData representation.
    pub nodata: Option<PixelValue>,
    /// Scale metadata, not implicitly applied to value.
    pub scale: Option<f64>,
    /// Offset metadata, not implicitly applied to value.
    pub offset: Option<f64>,
}
/// Owned point-query result.
#[derive(Debug)]
pub struct PixelSample {
    /// Zero-based sampled column.
    pub column: usize,
    /// Zero-based sampled row.
    pub row: usize,
    /// Requested bands in order.
    pub bands: Vec<BandSample>,
}
/// Read a pixel without loading the raster or changing its metadata.
#[derive(Debug, Clone)]
#[must_use]
pub struct PixelInfo {
    source: Source,
    location: SampleLocation,
    bands: Option<Vec<usize>>,
    config: GdalOptions,
}
impl PixelInfo {
    /// Describe a point query; no I/O occurs yet.
    pub fn new(source: impl Into<Source>, location: SampleLocation) -> Self {
        Self {
            source: source.into(),
            location,
            bands: None,
            config: GdalOptions::default(),
        }
    }
    /// Select one-based bands; default is all bands.
    pub fn bands(mut self, values: impl IntoIterator<Item = usize>) -> Self {
        self.bands = Some(values.into_iter().collect());
        self
    }
    /// Apply native configuration until the input has closed.
    pub fn gdal_options(mut self, config: GdalOptions) -> Self {
        self.config = config;
        self
    }
    /// Execute and return an owned sample.
    pub fn run(self) -> Result<Outcome<PixelSample>> {
        self.run_with(&mut Control::default())
    }
    /// Execute with borrowed control.
    pub fn run_with(self, control: &mut Control<'_>) -> Result<Outcome<PixelSample>> {
        if let Some(bands) = &self.bands {
            for &band in bands {
                options::band(band)?;
            }
        }
        query::run(
            self.source,
            Kind::Raster,
            self.config,
            control,
            |input, progress| {
                let (column, row) = match self.location {
                    SampleLocation::Pixel { column, row } => (column, row),
                    SampleLocation::Coordinate { x, y, spatial_ref } => {
                        if !x.is_finite() || !y.is_finite() {
                            return Err(Error::invalid("sample coordinates must be finite"));
                        }
                        let mut x = [x];
                        let mut y = [y];
                        if let Some(definition) = spatial_ref {
                            let mut from =
                                SpatialRef::from_definition(&definition).map_err(inspect_error)?;
                            let mut to = input.spatial_ref().map_err(inspect_error)?;
                            from.set_axis_mapping_strategy(
                                AxisMappingStrategy::TraditionalGisOrder,
                            );
                            to.set_axis_mapping_strategy(AxisMappingStrategy::TraditionalGisOrder);
                            CoordTransform::new(&from, &to)
                                .map_err(inspect_error)?
                                .transform_coords(&mut x, &mut y, &mut [])
                                .map_err(inspect_error)?;
                        }
                        let transform = input.geo_transform().map_err(|e| {
                            Error::unsupported("coordinate queries require an affine geotransform")
                                .with_source(e)
                        })?;
                        let inverse = transform.invert().map_err(inspect_error)?;
                        let (column, row) = inverse.apply(x[0], y[0]);
                        if !column.is_finite()
                            || !row.is_finite()
                            || column < 0.0
                            || row < 0.0
                            || column >= input.raster_size().0 as f64
                            || row >= input.raster_size().1 as f64
                        {
                            return Err(Error::invalid("sample coordinate is outside the raster"));
                        }
                        (column.floor() as usize, row.floor() as usize)
                    }
                };
                let (width, height) = input.raster_size();
                if column >= width || row >= height {
                    return Err(Error::invalid("sample pixel is outside the raster"));
                }
                let indices = self
                    .bands
                    .unwrap_or_else(|| (1..=input.raster_count()).collect());
                let mut bands = Vec::with_capacity(indices.len());
                for index in indices {
                    query::pulse(progress, 0.0, c"reading pixel")?;
                    let band = options::source_band(input, index)?;
                    let position = (column as isize, row as isize);
                    let value = value::sample(&band, position)?;
                    let mask = band.open_mask_band().map_err(inspect_error)?;
                    let valid = mask
                        .read_as::<u8>(position, (1, 1), (1, 1), None)
                        .map_err(inspect_error)?
                        .data()[0]
                        != 0;
                    bands.push(BandSample {
                        band: index,
                        value,
                        valid,
                        nodata: value::nodata(&band),
                        scale: band.scale(),
                        offset: band.offset(),
                    });
                }
                query::pulse(progress, 1.0, c"pixel query complete")?;
                Ok(PixelSample { column, row, bands })
            },
        )
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rotated_grid_and_uint64_samples_keep_exact_values() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("exact.tif");
        let mut ds = gdal::DriverManager::get_driver_by_name("GTiff")?
            .create_with_band_type::<u64, _>(&path, 2, 2, 1)?;
        let transform = [10., 2., 0.5, 20., 0.25, -2.];
        ds.set_geo_transform(&transform)?;
        let exact = 9_007_199_254_740_993;
        ds.rasterband(1)?.write(
            (0, 0),
            (2, 2),
            &mut gdal::raster::Buffer::new((2, 2), vec![0, 1, 2, exact]),
        )?;
        ds.rasterband(1)?.set_no_data_value_u64(Some(exact))?;
        ds.close()?;
        let (x, y) = transform.apply(1.5, 1.5);
        let result = PixelInfo::new(
            &path,
            SampleLocation::Coordinate {
                x,
                y,
                spatial_ref: None,
            },
        )
        .run()?;
        assert_eq!(result.value().bands[0].value, PixelValue::UInt(exact));
        assert_eq!(
            result.value().bands[0].nodata,
            Some(PixelValue::UInt(exact))
        );
        assert!(!result.value().bands[0].valid);
        assert!(
            PixelInfo::new(&path, SampleLocation::Pixel { column: 2, row: 0 })
                .run()
                .is_err()
        );
        Ok(())
    }
}
