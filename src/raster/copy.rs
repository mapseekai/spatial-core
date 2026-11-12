use super::convert::warp_to_path_with_progress;
use crate::control::{Cancellation, InvalidInput};
use crate::pg::{PgCopy, RasterCopyRow, acquire_connection};
use anyhow::{Context, Result, anyhow};
use deadpool_postgres::Pool;
use gdal::Dataset;
use gdal::raster::GdalDataType;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use tokio::sync::mpsc;
use tokio::task;
use wkb_raster::{
    Endian, InMemoryRasterData, OfflineRasterData, PixType, Raster, RasterBand, RasterDataSource,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StorageMode {
    InDb,
    OutDb,
}

impl StorageMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::InDb => "indb",
            Self::OutDb => "outdb",
        }
    }
}

impl FromStr for StorageMode {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        match s.trim().to_lowercase().as_str() {
            "indb" => Ok(Self::InDb),
            "outdb" => Ok(Self::OutDb),
            other => Err(anyhow!("unsupported storage mode: {}", other)),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IngestMode {
    Whole,
    Tiled,
}

impl IngestMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Whole => "whole",
            Self::Tiled => "tiled",
        }
    }
}

impl FromStr for IngestMode {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        match s.trim().to_lowercase().as_str() {
            "whole" => Ok(Self::Whole),
            "tiled" => Ok(Self::Tiled),
            other => Err(anyhow!("unsupported ingest mode: {}", other)),
        }
    }
}

#[derive(Debug, Clone)]
pub struct RasterCopyOptions {
    pub storage_mode: StorageMode,
    pub ingest_mode: IngestMode,
    pub source_reference: String,
    pub tile_size: u16,
    pub tile_overlap: u16,
    pub batch_size: usize,
    pub target_srid: Option<u32>,
    /// Peak encoding budget per window; the row queue uses the same bound.
    pub max_window_bytes: usize,
    pub max_windows: usize,
}

impl Default for RasterCopyOptions {
    fn default() -> Self {
        Self {
            storage_mode: StorageMode::InDb,
            ingest_mode: IngestMode::Whole,
            source_reference: String::new(),
            tile_size: 256,
            tile_overlap: 0,
            batch_size: 256,
            target_srid: None,
            max_window_bytes: 64 * 1024 * 1024,
            max_windows: 1_000_000,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct RasterCopySummary {
    pub srid: i32,
    pub width: usize,
    pub height: usize,
    pub tile_count: usize,
}

pub async fn copy_to_postgis<P: AsRef<Path>>(
    pool: &Pool,
    path: P,
    schema: &str,
    table: &str,
    options: RasterCopyOptions,
    cancel: &Cancellation,
) -> Result<RasterCopySummary> {
    cancel.check()?;
    let plan_path = path.as_ref().to_path_buf();
    let plan_options = options.clone();
    let plan_cancel = cancel.clone();
    let plan =
        task::spawn_blocking(move || build_copy_plan(&plan_path, &plan_options, &plan_cancel))
            .await
            .context("raster plan build task failed")??;
    cancel.check()?;
    // Acquire the connection before starting a reader that can block on sending.
    let mut client = acquire_connection(pool, cancel).await?;
    let batch_size = options.batch_size;
    let capacity = (options.max_window_bytes / plan.window_bytes)
        .max(1)
        .min(batch_size);
    let (row_tx, row_rx) = mpsc::channel(capacity);
    let summary = RasterCopySummary {
        srid: plan.srid,
        width: plan.width,
        height: plan.height,
        tile_count: 0,
    };
    let reader_cancel = cancel.clone();
    let read_handle =
        task::spawn_blocking(move || stream_copy_rows(&plan, &options, row_tx, &reader_cancel));
    let stats = PgCopy::copy_raster_rows_from_receiver(
        &mut client,
        schema,
        table,
        row_rx,
        batch_size,
        read_handle,
        cancel,
    )
    .await?;
    Ok(RasterCopySummary {
        tile_count: stats.committed_rows,
        ..summary
    })
}

struct CopyPlan {
    path: PathBuf,
    // Keeps the lazy warped VRT alive until the blocking reader exits.
    _warped: Option<tempfile::NamedTempFile>,
    srid: i32,
    width: usize,
    height: usize,
    geo_transform: [f64; 6],
    windows: RasterWindows,
    window_bytes: usize,
}

fn invalid(message: impl Into<String>) -> anyhow::Error {
    InvalidInput(message.into()).into()
}

fn build_copy_plan(
    path: &Path,
    options: &RasterCopyOptions,
    cancel: &Cancellation,
) -> Result<CopyPlan> {
    cancel.check()?;
    if options.batch_size == 0
        || options.batch_size > 4096
        || options.max_window_bytes == 0
        || options.max_windows == 0
    {
        return Err(invalid(
            "raster COPY batch_size must be 1..=4096 and resource budgets must be positive",
        ));
    }
    if options.storage_mode == StorageMode::OutDb
        && (options.source_reference.is_empty() || options.source_reference.contains('\0'))
    {
        return Err(invalid(
            "outdb raster requires a non-empty source reference without NUL bytes",
        ));
    }
    if let Some(target) = options.target_srid
        && !matches!(target, 3857 | 4326)
    {
        return Err(invalid(format!("unsupported target SRID: {target}")));
    }
    let source = Dataset::open(path)
        .with_context(|| format!("failed to open raster dataset: {}", path.display()))?;
    let source_srs = source
        .spatial_ref()
        .map_err(|_| invalid("source raster has no CRS"))?;
    let source_srid = if source_srs.auth_name().as_deref() == Some("EPSG") {
        source_srs
            .auth_code()
            .ok()
            .filter(|code| *code > 0)
            .map(|code| code as u32)
    } else {
        None
    };
    if options.target_srid.is_none() && source_srid.is_none() {
        return Err(invalid(
            "preserving source raster CRS requires a positive EPSG authority code",
        ));
    }
    let mut warped = None;
    let mut read_path = path.to_path_buf();
    let dataset = if options
        .target_srid
        .is_some_and(|target| Some(target) != source_srid)
    {
        if options.storage_mode == StorageMode::OutDb {
            return Err(invalid(
                "outdb reprojection requires a persistent source in the requested target CRS",
            ));
        }
        let temp = tempfile::Builder::new().suffix(".vrt").tempfile()?;
        let target = format!("EPSG:{}", options.target_srid.expect("target exists"));
        let result = warp_to_path_with_progress(
            &source,
            temp.path(),
            vec!["-of", "VRT", "-t_srs", &target, "-wm", "64", "-overwrite"],
            |_| !cancel.is_cancelled(),
        );
        cancel.check()?;
        let dataset = result.context("failed to reproject raster")?;
        read_path = temp.path().to_path_buf();
        warped = Some(temp);
        dataset
    } else {
        source
    };
    let (width, height) = dataset.raster_size();
    let geo_transform = dataset
        .geo_transform()
        .map_err(|_| invalid("raster dataset has no valid geotransform"))?;
    if geo_transform.iter().any(|value| !value.is_finite()) {
        return Err(invalid("raster geotransform contains non-finite values"));
    }
    let srid = dataset.spatial_ref()?.auth_code()?;
    let windows = RasterWindows::new(width, height, options)?;
    let band_count = dataset.raster_count();
    if band_count == 0
        || band_count > usize::from(u16::MAX)
        || (options.storage_mode == StorageMode::OutDb && band_count > 128)
    {
        return Err(invalid("raster band count is outside WKB limits"));
    }
    let first = windows.clone().next().expect("nonzero raster dimensions");
    let mut bytes_per_pixel = 0usize;
    for idx in 1..=band_count {
        bytes_per_pixel += match dataset.rasterband(idx)?.band_type() {
            GdalDataType::UInt8 => 1,
            GdalDataType::UInt16 | GdalDataType::Int16 => 2,
            GdalDataType::UInt32 | GdalDataType::Int32 | GdalDataType::Float32 => 4,
            GdalDataType::Float64 => 8,
            other => return Err(invalid(format!("unsupported GDAL band type: {other:?}"))),
        };
    }
    // wkb-raster builds per-band hex buffers and then grows the aggregate
    // buffer. Include their overlapping allocations, raw data and row headers.
    let window_bytes = if options.storage_mode == StorageMode::InDb {
        first
            .width
            .checked_mul(first.height)
            .and_then(|n| n.checked_mul(bytes_per_pixel))
            .and_then(|n| n.checked_mul(12))
            .and_then(|n| {
                first
                    .height
                    .checked_mul(band_count)?
                    .checked_mul(24)?
                    .checked_add(n)
            })
            .and_then(|n| {
                band_count
                    .checked_mul(std::mem::size_of::<RasterBand>() + 64)?
                    .checked_add(n)
            })
            .and_then(|n| n.checked_add(2048))
    } else {
        options
            .source_reference
            .len()
            .checked_add(64)
            .and_then(|n| n.checked_mul(band_count))
            .and_then(|n| n.checked_mul(12))
            .and_then(|n| n.checked_add(2048))
    }
    .ok_or_else(|| invalid("raster window byte estimate overflow"))?;
    if window_bytes > options.max_window_bytes {
        return Err(invalid(format!(
            "raster window requires approximately {window_bytes} bytes, exceeding {} byte budget; use smaller tiles",
            options.max_window_bytes
        )));
    }
    cancel.check()?;
    Ok(CopyPlan {
        path: read_path,
        _warped: warped,
        srid,
        width,
        height,
        geo_transform,
        windows,
        window_bytes,
    })
}

fn stream_copy_rows(
    plan: &CopyPlan,
    options: &RasterCopyOptions,
    row_tx: mpsc::Sender<Result<RasterCopyRow>>,
    cancel: &Cancellation,
) -> Result<()> {
    cancel.check()?;
    let dataset = Dataset::open(&plan.path)
        .with_context(|| format!("failed to open raster dataset: {}", plan.path.display()))?;
    for window in plan.windows.clone() {
        cancel.check()?;
        let rast_hex = encode_window(&dataset, options, plan.srid, plan.geo_transform, window)?;
        cancel.check()?;
        row_tx
            .blocking_send(Ok(RasterCopyRow { rast_hex }))
            .map_err(|_| anyhow!("raster COPY receiver closed"))?;
    }
    Ok(())
}

#[derive(Debug, Clone, Copy)]
struct RasterWindow {
    x_off: usize,
    y_off: usize,
    width: usize,
    height: usize,
}

/// O(1) window planning regardless of source dimensions.
#[derive(Clone)]
struct RasterWindows {
    width: usize,
    height: usize,
    tile_width: usize,
    tile_height: usize,
    stride: usize,
    x: usize,
    y: usize,
}

impl RasterWindows {
    fn new(width: usize, height: usize, options: &RasterCopyOptions) -> Result<Self> {
        if width == 0 || height == 0 || width > isize::MAX as usize || height > isize::MAX as usize
        {
            return Err(invalid(
                "raster dimensions must be positive and fit GDAL offsets",
            ));
        }
        let whole = options.ingest_mode == IngestMode::Whole;
        if whole && (width > usize::from(u16::MAX) || height > usize::from(u16::MAX)) {
            return Err(invalid(
                "whole raster exceeds WKB dimension limits; use tiled ingest",
            ));
        }
        if options.tile_size == 0 || options.tile_overlap >= options.tile_size {
            return Err(invalid(
                "tile_size must be positive and tile_overlap must be smaller than tile_size",
            ));
        }
        let stride = if whole {
            width.max(height)
        } else {
            usize::from(options.tile_size - options.tile_overlap)
        };
        let count = width
            .div_ceil(stride)
            .checked_mul(height.div_ceil(stride))
            .ok_or_else(|| invalid("raster tile count overflow"))?;
        if count > options.max_windows {
            return Err(invalid(format!(
                "raster requires {count} windows, exceeding {} window budget",
                options.max_windows
            )));
        }
        Ok(Self {
            width,
            height,
            tile_width: if whole {
                width
            } else {
                usize::from(options.tile_size)
            },
            tile_height: if whole {
                height
            } else {
                usize::from(options.tile_size)
            },
            stride,
            x: 0,
            y: 0,
        })
    }
}

impl Iterator for RasterWindows {
    type Item = RasterWindow;
    fn next(&mut self) -> Option<Self::Item> {
        if self.y >= self.height {
            return None;
        }
        let window = RasterWindow {
            x_off: self.x,
            y_off: self.y,
            width: (self.width - self.x).min(self.tile_width),
            height: (self.height - self.y).min(self.tile_height),
        };
        self.x = self.x.saturating_add(self.stride);
        if self.x >= self.width {
            self.x = 0;
            self.y = self.y.saturating_add(self.stride);
        }
        Some(window)
    }
}

fn band_type_to_pixtype(t: GdalDataType, nodata: Option<f64>) -> Result<PixType> {
    if let Some(value) = nodata {
        let valid = match t {
            GdalDataType::UInt8 => {
                value.is_finite() && value.fract() == 0.0 && (0.0..=u8::MAX as f64).contains(&value)
            }
            GdalDataType::UInt16 => {
                value.is_finite()
                    && value.fract() == 0.0
                    && (0.0..=u16::MAX as f64).contains(&value)
            }
            GdalDataType::UInt32 => {
                value.is_finite()
                    && value.fract() == 0.0
                    && (0.0..=u32::MAX as f64).contains(&value)
            }
            GdalDataType::Int16 => {
                value.is_finite()
                    && value.fract() == 0.0
                    && (i16::MIN as f64..=i16::MAX as f64).contains(&value)
            }
            GdalDataType::Int32 => {
                value.is_finite()
                    && value.fract() == 0.0
                    && (i32::MIN as f64..=i32::MAX as f64).contains(&value)
            }
            GdalDataType::Float32 => !value.is_finite() || (value as f32).is_finite(),
            GdalDataType::Float64 => true,
            _ => false,
        };
        if !valid {
            return Err(invalid(format!(
                "NoData value {value} does not fit band type {t:?}"
            )));
        }
    }
    match t {
        GdalDataType::UInt8 => Ok(PixType::UInt8(nodata.map(|value| value as u8))),
        GdalDataType::UInt16 => Ok(PixType::UInt16(nodata.map(|value| value as u16))),
        GdalDataType::Int16 => Ok(PixType::Int16(nodata.map(|value| value as i16))),
        GdalDataType::Int32 => Ok(PixType::Int32(nodata.map(|value| value as i32))),
        GdalDataType::UInt32 => Ok(PixType::UInt32(nodata.map(|value| value as u32))),
        GdalDataType::Float32 => Ok(PixType::Float32(nodata.map(|value| value as f32))),
        GdalDataType::Float64 => Ok(PixType::Float64(nodata)),
        other => Err(anyhow!("unsupported GDAL band type: {:?}", other)),
    }
}

fn build_outdb_bands(dataset: &Dataset, source_ref: &str) -> Result<Vec<RasterBand>> {
    let band_count = dataset.raster_count();
    if band_count == 0 {
        return Err(anyhow!("dataset has no raster bands"));
    }
    let mut bands = Vec::with_capacity(band_count);
    for idx in 1..=band_count {
        let rb = dataset
            .rasterband(idx)
            .with_context(|| format!("failed to open raster band {}", idx))?;
        let pixtype = band_type_to_pixtype(rb.band_type(), rb.no_data_value())?;
        bands.push(RasterBand {
            is_nodata_value: false,
            data: RasterDataSource::Offline(OfflineRasterData {
                band: (idx - 1) as i8,
                path: PathBuf::from(source_ref),
                pixtype,
            }),
        });
    }
    Ok(bands)
}

fn read_indb_band(dataset: &Dataset, band_idx: usize, window: RasterWindow) -> Result<RasterBand> {
    let rb = dataset
        .rasterband(band_idx)
        .with_context(|| format!("failed to open raster band {}", band_idx))?;
    let band_type = rb.band_type();
    let nodata = rb.no_data_value();
    band_type_to_pixtype(band_type, nodata)?;
    let data = match band_type {
        GdalDataType::UInt8 => {
            let buffer = rb
                .read_as::<u8>(
                    (window.x_off as isize, window.y_off as isize),
                    (window.width, window.height),
                    (window.width, window.height),
                    None,
                )
                .context("failed to read UInt8 raster data")?;
            let rows: Vec<Vec<u8>> = buffer
                .data()
                .chunks(window.width)
                .map(|chunk| chunk.to_vec())
                .collect();
            InMemoryRasterData::UInt8 {
                data: rows,
                nodata: nodata.map(|value| value as u8),
            }
        }
        GdalDataType::UInt16 => {
            let buffer = rb
                .read_as::<u16>(
                    (window.x_off as isize, window.y_off as isize),
                    (window.width, window.height),
                    (window.width, window.height),
                    None,
                )
                .context("failed to read UInt16 raster data")?;
            let rows: Vec<Vec<u16>> = buffer
                .data()
                .chunks(window.width)
                .map(|chunk| chunk.to_vec())
                .collect();
            InMemoryRasterData::UInt16 {
                data: rows,
                nodata: nodata.map(|value| value as u16),
            }
        }
        GdalDataType::Int16 => {
            let buffer = rb
                .read_as::<i16>(
                    (window.x_off as isize, window.y_off as isize),
                    (window.width, window.height),
                    (window.width, window.height),
                    None,
                )
                .context("failed to read Int16 raster data")?;
            let rows: Vec<Vec<i16>> = buffer
                .data()
                .chunks(window.width)
                .map(|chunk| chunk.to_vec())
                .collect();
            InMemoryRasterData::Int16 {
                data: rows,
                nodata: nodata.map(|value| value as i16),
            }
        }
        GdalDataType::Int32 => {
            let buffer = rb
                .read_as::<i32>(
                    (window.x_off as isize, window.y_off as isize),
                    (window.width, window.height),
                    (window.width, window.height),
                    None,
                )
                .context("failed to read Int32 raster data")?;
            let rows: Vec<Vec<i32>> = buffer
                .data()
                .chunks(window.width)
                .map(|chunk| chunk.to_vec())
                .collect();
            InMemoryRasterData::Int32 {
                data: rows,
                nodata: nodata.map(|value| value as i32),
            }
        }
        GdalDataType::UInt32 => {
            let buffer = rb
                .read_as::<u32>(
                    (window.x_off as isize, window.y_off as isize),
                    (window.width, window.height),
                    (window.width, window.height),
                    None,
                )
                .context("failed to read UInt32 raster data")?;
            let rows: Vec<Vec<u32>> = buffer
                .data()
                .chunks(window.width)
                .map(|chunk| chunk.to_vec())
                .collect();
            InMemoryRasterData::UInt32 {
                data: rows,
                nodata: nodata.map(|value| value as u32),
            }
        }
        GdalDataType::Float32 => {
            let buffer = rb
                .read_as::<f32>(
                    (window.x_off as isize, window.y_off as isize),
                    (window.width, window.height),
                    (window.width, window.height),
                    None,
                )
                .context("failed to read Float32 raster data")?;
            let rows: Vec<Vec<f32>> = buffer
                .data()
                .chunks(window.width)
                .map(|chunk| chunk.to_vec())
                .collect();
            InMemoryRasterData::Float32 {
                data: rows,
                nodata: nodata.map(|value| value as f32),
            }
        }
        GdalDataType::Float64 => {
            let buffer = rb
                .read_as::<f64>(
                    (window.x_off as isize, window.y_off as isize),
                    (window.width, window.height),
                    (window.width, window.height),
                    None,
                )
                .context("failed to read Float64 raster data")?;
            let rows: Vec<Vec<f64>> = buffer
                .data()
                .chunks(window.width)
                .map(|chunk| chunk.to_vec())
                .collect();
            InMemoryRasterData::Float64 { data: rows, nodata }
        }
        other => return Err(anyhow!("unsupported GDAL band type: {:?}", other)),
    };
    Ok(RasterBand {
        is_nodata_value: false,
        data: RasterDataSource::InMemory(data),
    })
}

fn read_indb_bands(dataset: &Dataset, window: RasterWindow) -> Result<Vec<RasterBand>> {
    let band_count = dataset.raster_count();
    if band_count == 0 {
        return Err(anyhow!("dataset has no raster bands"));
    }
    let mut bands = Vec::with_capacity(band_count);
    for idx in 1..=band_count {
        bands.push(read_indb_band(dataset, idx, window)?);
    }
    Ok(bands)
}

fn encode_window(
    dataset: &Dataset,
    options: &RasterCopyOptions,
    srid: i32,
    geo_transform: [f64; 6],
    window: RasterWindow,
) -> Result<String> {
    let width_u16 = u16::try_from(window.width)
        .map_err(|_| anyhow!("window width exceeds u16: {}", window.width))?;
    let height_u16 = u16::try_from(window.height)
        .map_err(|_| anyhow!("window height exceeds u16: {}", window.height))?;

    let ip_x = geo_transform[0]
        + geo_transform[1] * (window.x_off as f64)
        + geo_transform[2] * (window.y_off as f64);
    let ip_y = geo_transform[3]
        + geo_transform[4] * (window.x_off as f64)
        + geo_transform[5] * (window.y_off as f64);

    let band_count = dataset.raster_count();
    if band_count == 0 {
        return Err(anyhow!("dataset has no raster bands"));
    }

    let bands = if options.storage_mode == StorageMode::OutDb {
        build_outdb_bands(dataset, &options.source_reference)?
    } else {
        read_indb_bands(dataset, window)?
    };

    let raster = Raster {
        endian: Endian::Big,
        version: 0,
        scale_x: geo_transform[1],
        scale_y: geo_transform[5],
        ip_x,
        ip_y,
        skew_x: geo_transform[2],
        skew_y: geo_transform[4],
        srid,
        width: width_u16,
        height: height_u16,
        bands,
    };
    Ok(raster.to_wkb_string())
}

#[cfg(test)]
mod tests {
    use super::{
        IngestMode, RasterCopyOptions, RasterWindows, StorageMode, build_outdb_bands,
        read_indb_band,
    };
    use anyhow::Result;
    use gdal::{DriverManager, spatial_ref::SpatialRef};
    use tempfile::NamedTempFile;
    use wkb_raster::RasterDataSource;

    fn create_test_geotiff(bands: usize) -> Result<NamedTempFile> {
        let file = NamedTempFile::new()?;
        let path = file
            .path()
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("temp file path is not valid UTF-8"))?;
        let driver = DriverManager::get_driver_by_name("GTiff")?;
        let mut dataset = driver.create_with_band_type::<u8, _>(path, 4, 4, bands)?;
        dataset.set_geo_transform(&[0.0, 1.0, 0.0, 0.0, 0.0, -1.0])?;
        let srs = SpatialRef::from_epsg(4326)?;
        dataset.set_spatial_ref(&srs)?;
        dataset.flush_cache()?;
        drop(dataset);
        Ok(file)
    }

    #[test]
    fn build_windows_returns_single_window_for_whole_mode() {
        let windows: Vec<_> = RasterWindows::new(1024, 512, &RasterCopyOptions::default())
            .unwrap()
            .collect();
        assert_eq!(windows.len(), 1);
        assert_eq!(windows[0].x_off, 0);
        assert_eq!(windows[0].y_off, 0);
    }

    #[test]
    fn build_windows_tiles_with_overlap_stride() {
        let windows: Vec<_> = RasterWindows::new(
            512,
            512,
            &RasterCopyOptions {
                ingest_mode: IngestMode::Tiled,
                tile_overlap: 16,
                ..Default::default()
            },
        )
        .unwrap()
        .collect();
        assert!(!windows.is_empty());
        assert!(windows.iter().all(|w| w.width > 0 && w.height > 0));
    }

    #[test]
    fn storage_mode_is_enum_based() {
        assert!(matches!(StorageMode::InDb, StorageMode::InDb));
        assert!(matches!(StorageMode::OutDb, StorageMode::OutDb));
    }

    #[test]
    fn build_outdb_bands_reads_all_bands() -> Result<()> {
        let file = create_test_geotiff(3)?;
        let dataset = gdal::Dataset::open(file.path())?;
        let bands = build_outdb_bands(&dataset, "s3://bucket/path.tif")?;
        assert_eq!(bands.len(), 3);
        for (i, band) in bands.iter().enumerate() {
            match &band.data {
                RasterDataSource::Offline(offline) => {
                    assert_eq!(offline.band as usize, i);
                    assert_eq!(offline.path.to_string_lossy(), "s3://bucket/path.tif");
                }
                _ => panic!("expected offline raster data source"),
            }
        }
        Ok(())
    }

    #[test]
    fn read_indb_band_reads_uint8_band() -> Result<()> {
        let file = create_test_geotiff(1)?;
        let dataset = gdal::Dataset::open(file.path())?;
        let window = super::RasterWindow {
            x_off: 0,
            y_off: 0,
            width: 4,
            height: 4,
        };
        let band = read_indb_band(&dataset, 1usize, window)?;
        match band.data {
            RasterDataSource::InMemory(wkb_raster::InMemoryRasterData::UInt8 { data, .. }) => {
                assert_eq!(data.len(), 4);
                assert_eq!(data[0].len(), 4);
            }
            _ => panic!("expected InMemory UInt8 data source"),
        }
        Ok(())
    }
    #[test]
    fn lazy_windows_accept_large_tiled_rasters_and_reject_excessive_work() {
        let options = RasterCopyOptions {
            ingest_mode: IngestMode::Tiled,
            ..Default::default()
        };
        let mut windows = RasterWindows::new(100_000, 100_000, &options).unwrap();
        assert_eq!(windows.next().unwrap().width, 256);
        assert!(std::mem::size_of_val(&windows) < 100);
        let overlapping = RasterCopyOptions {
            tile_overlap: 255,
            ..options.clone()
        };
        assert!(RasterWindows::new(10_000, 10_000, &overlapping).is_err());
        let invalid_overlap = RasterCopyOptions {
            tile_overlap: 256,
            ..options.clone()
        };
        assert!(RasterWindows::new(512, 512, &invalid_overlap).is_err());
        assert!(RasterWindows::new(100_000, 4, &RasterCopyOptions::default()).is_err());
    }

    #[test]
    fn plan_enforces_encoding_budget_and_target_crs() -> Result<()> {
        let source = create_test_geotiff(1)?;
        let cancel = crate::control::Cancellation::default();
        let budget = RasterCopyOptions {
            max_window_bytes: 8,
            ..Default::default()
        };
        let err = super::build_copy_plan(source.path(), &budget, &cancel)
            .err()
            .unwrap();
        assert!(err.downcast_ref::<crate::control::InvalidInput>().is_some());
        let target = RasterCopyOptions {
            target_srid: Some(3857),
            ..Default::default()
        };
        let plan = super::build_copy_plan(source.path(), &target, &cancel)?;
        assert_eq!(plan.srid, 3857);
        assert!(plan.geo_transform[1] > 100_000.0);
        assert!(plan._warped.is_some());
        let outdb = RasterCopyOptions {
            storage_mode: StorageMode::OutDb,
            source_reference: source.path().to_string_lossy().into_owned(),
            ..target
        };
        assert!(super::build_copy_plan(source.path(), &outdb, &cancel).is_err());
        cancel.cancel();
        assert!(
            super::build_copy_plan(source.path(), &RasterCopyOptions::default(), &cancel).is_err()
        );
        Ok(())
    }

    #[test]
    fn nodata_is_retained_for_indb_and_outdb() -> Result<()> {
        let driver = DriverManager::get_driver_by_name("MEM")?;
        let dataset = driver.create_with_band_type::<u16, _>("", 4, 4, 1)?;
        dataset.rasterband(1)?.set_no_data_value(Some(65535.0))?;
        let window = super::RasterWindow {
            x_off: 0,
            y_off: 0,
            width: 4,
            height: 4,
        };
        let band = read_indb_band(&dataset, 1, window)?;
        assert!(matches!(
            band.data,
            RasterDataSource::InMemory(wkb_raster::InMemoryRasterData::UInt16 {
                nodata: Some(65535),
                ..
            })
        ));
        let bands = build_outdb_bands(&dataset, "/path/source.tif")?;
        assert!(
            matches!(&bands[0].data, RasterDataSource::Offline(offline) if matches!(&offline.pixtype, wkb_raster::PixType::UInt16(Some(65535))))
        );
        assert!(
            super::band_type_to_pixtype(gdal::raster::GdalDataType::UInt8, Some(-9999.0)).is_err()
        );
        Ok(())
    }
}
