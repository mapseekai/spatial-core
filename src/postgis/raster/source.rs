use super::{PixelType, RasterBandSchema, RasterSchema, RasterStorage, RasterWriteOptions, wkb};
use crate::{
    config::gdal::GdalOptions,
    postgis::{PostGisError, stream::SourceStream},
    raster::{tiling::TileWindows, warp_to_path_with_progress},
};
use gdal::{
    Dataset,
    spatial_ref::{AxisMappingStrategy, SpatialRef},
};
use std::path::PathBuf;

/// Input raster path, independent of any OutDB server reference.
#[derive(Debug, Clone)]
pub struct RasterSource {
    path: PathBuf,
    options: GdalOptions,
}

impl RasterSource {
    /// Input path, URI or explicit GDAL subdataset name.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            options: GdalOptions::default(),
        }
    }
    /// Thread-local options restored when the worker exits.
    #[must_use]
    pub fn gdal_options(mut self, options: GdalOptions) -> Self {
        self.options = options;
        self
    }
}

pub(crate) struct SourceInfo {
    pub schema: RasterSchema,
    pub source_size: (usize, usize),
    pub output_size: (usize, usize),
    pub count: u64,
    pub fixed_tiles: bool,
    pub tile_size: (usize, usize),
}

fn invalid(message: &str) -> PostGisError {
    PostGisError::InvalidInput(message.into())
}

pub(crate) fn start(source: RasterSource, options: RasterWriteOptions) -> SourceStream<SourceInfo> {
    SourceStream::start(options.limits, move |metadata, sender| {
        let _scope = source.options.enter();
        if let RasterStorage::OutDb { server_path } = &options.storage
            && (server_path.is_empty() || server_path.contains('\0'))
        {
            return Err(invalid(
                "OutDB requires a stable nonempty server path without NUL",
            ));
        }
        let input = Dataset::open(&source.path).map_err(|e| PostGisError::source(0, e))?;
        input.geo_transform().map_err(|_| {
            PostGisError::Unsupported(
                "input needs an affine geotransform; convert GCP/RPC geolocation explicitly".into(),
            )
        })?;
        for index in 1..=input.raster_count() {
            let band = input
                .rasterband(index)
                .map_err(|e| PostGisError::source(0, e))?;
            let mask = band.mask_flags().map_err(|e| PostGisError::source(0, e))?;
            if (!mask.is_all_valid() && !mask.is_nodata())
                || band.scale().is_some_and(|v| v != 1.0)
                || band.offset().is_some_and(|v| v != 0.0)
            {
                return Err(PostGisError::Unsupported(
                    "independent masks and band scale/offset require explicit conversion".into(),
                ));
            }
        }
        let source_size = input.raster_size();
        let mut source_crs = input
            .spatial_ref()
            .map_err(|e| PostGisError::source(0, e))?;
        if source_crs.auth_name().is_none()
            && matches!(
                source_crs.name().as_deref(),
                Some("Undefined geographic SRS" | "Undefined Cartesian SRS")
            )
        {
            return Err(invalid("source raster has no usable CRS"));
        }
        let source_srid = (source_crs.auth_name().as_deref() == Some("EPSG"))
            .then(|| source_crs.auth_code().ok())
            .flatten();
        let srid = options.target_srid.or(source_srid).ok_or_else(|| {
            invalid("preserving raster CRS requires an EPSG identifier; specify a target")
        })?;
        if !(1..=998999).contains(&srid) {
            return Err(invalid("invalid target SRID"));
        }
        let mut target_crs =
            SpatialRef::from_epsg(srid as u32).map_err(|e| PostGisError::source(0, e))?;
        source_crs.set_axis_mapping_strategy(AxisMappingStrategy::TraditionalGisOrder);
        target_crs.set_axis_mapping_strategy(AxisMappingStrategy::TraditionalGisOrder);
        if options.target_srid.is_none() && source_crs != target_crs {
            return Err(invalid(
                "source CRS differs from its EPSG authority; specify an explicit target",
            ));
        }
        let mut warped = None;
        let dataset = if source_crs != target_crs {
            if matches!(options.storage, RasterStorage::OutDb { .. }) {
                return Err(invalid(
                    "OutDB reprojection requires a persistent file already in the target CRS",
                ));
            }
            let temp = tempfile::Builder::new()
                .suffix(".vrt")
                .tempfile()
                .map_err(|e| PostGisError::source(0, e))?;
            let target = format!("EPSG:{srid}");
            let dataset = warp_to_path_with_progress(
                &input,
                temp.path(),
                vec![
                    "-of",
                    "VRT",
                    "-t_srs",
                    &target,
                    "-r",
                    options.resampling.name(),
                    "-wm",
                    "64",
                    "-overwrite",
                ],
                |_| !sender.cancellation().is_cancelled(),
            )
            .map_err(|e| PostGisError::source(0, e))?;
            warped = Some(temp);
            dataset
        } else {
            input
        };
        // Dataset and temporary VRT remain on this thread until all windows are read.
        let output_size = dataset.raster_size();
        let transform = dataset
            .geo_transform()
            .map_err(|e| PostGisError::source(0, e))?;
        if transform.iter().any(|v| !v.is_finite())
            || transform[1] * transform[5] - transform[2] * transform[4] == 0.0
        {
            return Err(invalid("raster has an invalid affine transform"));
        }
        let windows = TileWindows::new(output_size.0, output_size.1, options.layout)
            .map_err(|e| invalid(&e.0))?;
        if windows.tile_count() > options.limits.max_records {
            return Err(PostGisError::ResourceLimit(
                "raster tile count exceeds limit".into(),
            ));
        }
        let first = windows
            .clone()
            .next()
            .ok_or_else(|| invalid("raster has no windows"))?;
        if first.width > u16::MAX as usize || first.height > u16::MAX as usize {
            return Err(invalid(
                "raster WKB dimensions exceed u16; select smaller tiles",
            ));
        }
        let band_count = dataset.raster_count();
        if band_count == 0
            || band_count > u16::MAX as usize
            || (matches!(options.storage, RasterStorage::OutDb { .. }) && band_count > 128)
        {
            return Err(invalid("band count exceeds raster WKB limits"));
        }
        let mut bands = Vec::with_capacity(band_count);
        for index in 1..=band_count {
            let band = dataset
                .rasterband(index)
                .map_err(|e| PostGisError::source(0, e))?;
            let pixel_type = PixelType::from_gdal(band.band_type())?;
            wkb::band_type_to_pixtype(band.band_type(), band.no_data_value())
                .map_err(|e| PostGisError::InvalidInput(e.to_string()))?;
            bands.push(RasterBandSchema {
                pixel_type,
                nodata: band.no_data_value(),
            });
        }
        let bytes_per_pixel = bands.iter().map(|b| b.pixel_type.bytes()).sum::<usize>();
        let estimate = match &options.storage {
            RasterStorage::InDb => first
                .width
                .checked_mul(first.height)
                .and_then(|n| n.checked_mul(bytes_per_pixel))
                .and_then(|n| n.checked_mul(12))
                .and_then(|n| {
                    n.checked_add(first.height.checked_mul(band_count)?.checked_mul(24)?)
                }),
            RasterStorage::OutDb { server_path } => server_path
                .len()
                .checked_add(64)
                .and_then(|n| n.checked_mul(band_count))
                .and_then(|n| n.checked_mul(12)),
        }
        .and_then(|n| {
            n.checked_add(
                band_count.checked_mul(std::mem::size_of::<wkb_raster::RasterBand>() + 64)?,
            )
        })
        .and_then(|n| n.checked_add(2048));
        if estimate.is_none_or(|n| n > options.limits.max_record_bytes) {
            return Err(PostGisError::ResourceLimit(
                "raster peak encoding exceeds record budget; select smaller tiles".into(),
            ));
        }
        let fixed_tiles = match options.layout {
            super::TileLayout::Whole => true,
            super::TileLayout::Tiled {
                width,
                height,
                overlap: 0,
            } => output_size.0 % width as usize == 0 && output_size.1 % height as usize == 0,
            _ => false,
        };
        let schema = RasterSchema {
            identity: options.identity.clone(),
            raster: options.column.clone(),
            srid,
            geo_transform: transform,
            bands,
        };
        metadata
            .send(SourceInfo {
                schema,
                source_size,
                output_size,
                count: windows.tile_count(),
                fixed_tiles,
                tile_size: (first.width, first.height),
            })
            .map_err(|_| PostGisError::Cancelled)?;
        sender.validate_srid()?;
        for (index, window) in windows.enumerate() {
            if sender.cancellation().is_cancelled() {
                return Err(PostGisError::Cancelled);
            }
            let mut encoded =
                wkb::encode_window(&dataset, &options.storage, srid, transform, window)
                    .map_err(|e| PostGisError::source(index as u64 + 1, e))?
                    .into_bytes();
            encoded.push(b'\n');
            sender.send(encoded)?;
        }
        drop(dataset);
        drop(warped);
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_huge_raster_is_rejected_before_reading_or_materializing_windows()
    -> anyhow::Result<()> {
        let file = tempfile::NamedTempFile::with_suffix(".vrt")?;
        std::fs::write(
            file.path(),
            r#"<VRTDataset rasterXSize="1000000" rasterYSize="1000000"><SRS>EPSG:4326</SRS><GeoTransform>0,1,0,0,0,-1</GeoTransform><VRTRasterBand dataType="Byte" band="1"/></VRTDataset>"#,
        )?;
        let mut stream = start(
            RasterSource::new(file.path()),
            RasterWriteOptions::default(),
        );
        assert!(matches!(
            stream.metadata().await,
            Err(PostGisError::ResourceLimit(_))
        ));
        Ok(())
    }

    #[tokio::test]
    async fn band_scale_and_independent_masks_are_explicitly_rejected() -> anyhow::Result<()> {
        for mask in [false, true] {
            let file = tempfile::NamedTempFile::with_suffix(".tif")?;
            let mut dataset = gdal::DriverManager::get_driver_by_name("GTiff")?
                .create_with_band_type::<u8, _>(file.path(), 2, 2, 1)?;
            dataset.set_spatial_ref(&SpatialRef::from_epsg(4326)?)?;
            dataset.set_geo_transform(&[0., 1., 0., 0., 0., -1.])?;
            if mask {
                dataset.rasterband(1)?.create_mask_band(true)?;
            } else {
                dataset.rasterband(1)?.set_scale(2.)?;
            }
            dataset.flush_cache()?;
            drop(dataset);
            let mut stream = start(
                RasterSource::new(file.path()),
                RasterWriteOptions::default(),
            );
            assert!(matches!(
                stream.metadata().await,
                Err(PostGisError::Unsupported(_))
            ));
        }
        Ok(())
    }
}
