use super::RasterStorage;
use crate::raster::tiling::TileWindow;
use anyhow::{Context, Result, anyhow};
use gdal::{Dataset, raster::GdalDataType};
use std::path::PathBuf;
use wkb_raster::{
    Endian, InMemoryRasterData, OfflineRasterData, PixType, Raster, RasterBand, RasterDataSource,
};

pub(super) fn band_type_to_pixtype(t: GdalDataType, nodata: Option<f64>) -> Result<PixType> {
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
            GdalDataType::Float32 => !value.is_finite() || f64::from(value as f32) == value,
            GdalDataType::Float64 => true,
            _ => false,
        };
        if !valid {
            return Err(anyhow!(format!(
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

fn read_pixels<T: gdal::raster::GdalType + Copy>(
    band: &gdal::raster::RasterBand<'_>,
    window: TileWindow,
) -> Result<Vec<Vec<T>>> {
    let buffer = band
        .read_as::<T>(
            (window.x_off as isize, window.y_off as isize),
            (window.width, window.height),
            (window.width, window.height),
            None,
        )
        .context("failed to read raster window")?;
    Ok(buffer
        .data()
        .chunks(window.width)
        .map(<[T]>::to_vec)
        .collect())
}

fn read_indb_band(dataset: &Dataset, band_idx: usize, window: TileWindow) -> Result<RasterBand> {
    let band = dataset
        .rasterband(band_idx)
        .with_context(|| format!("failed to open raster band {band_idx}"))?;
    let nodata = band.no_data_value();
    band_type_to_pixtype(band.band_type(), nodata)?;
    let data = match band.band_type() {
        GdalDataType::UInt8 => InMemoryRasterData::UInt8 {
            data: read_pixels(&band, window)?,
            nodata: nodata.map(|v| v as u8),
        },
        GdalDataType::UInt16 => InMemoryRasterData::UInt16 {
            data: read_pixels(&band, window)?,
            nodata: nodata.map(|v| v as u16),
        },
        GdalDataType::Int16 => InMemoryRasterData::Int16 {
            data: read_pixels(&band, window)?,
            nodata: nodata.map(|v| v as i16),
        },
        GdalDataType::UInt32 => InMemoryRasterData::UInt32 {
            data: read_pixels(&band, window)?,
            nodata: nodata.map(|v| v as u32),
        },
        GdalDataType::Int32 => InMemoryRasterData::Int32 {
            data: read_pixels(&band, window)?,
            nodata: nodata.map(|v| v as i32),
        },
        GdalDataType::Float32 => InMemoryRasterData::Float32 {
            data: read_pixels(&band, window)?,
            nodata: nodata.map(|v| v as f32),
        },
        GdalDataType::Float64 => InMemoryRasterData::Float64 {
            data: read_pixels(&band, window)?,
            nodata,
        },
        other => return Err(anyhow!("unsupported GDAL band type: {other:?}")),
    };
    Ok(RasterBand {
        is_nodata_value: false,
        data: RasterDataSource::InMemory(data),
    })
}

fn read_indb_bands(dataset: &Dataset, window: TileWindow) -> Result<Vec<RasterBand>> {
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

pub(super) fn encode_window(
    dataset: &Dataset,
    storage: &RasterStorage,
    srid: i32,
    geo_transform: [f64; 6],
    window: TileWindow,
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

    let bands = match storage {
        RasterStorage::OutDb { server_path } => build_outdb_bands(dataset, server_path)?,
        RasterStorage::InDb => read_indb_bands(dataset, window)?,
    };

    let band_layout = (1..=band_count)
        .map(|index| {
            let band = dataset.rasterband(index)?;
            let bytes = super::PixelType::from_gdal(band.band_type())?.bytes();
            let payload = match storage {
                RasterStorage::InDb => window.width * window.height * bytes,
                RasterStorage::OutDb { server_path } => server_path.len() + 2,
            };
            Ok((bytes, band.no_data_value().is_some(), payload))
        })
        .collect::<Result<Vec<_>>>()?;
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
    pad_absent_nodata(raster.to_wkb_string(), &band_layout)
}

fn pad_absent_nodata(encoded: String, bands: &[(usize, bool, usize)]) -> Result<String> {
    let complete_size = 122
        + bands
            .iter()
            .map(|(bytes, _, payload)| 2 + (bytes + payload) * 2)
            .sum::<usize>();
    if encoded.len() == complete_size {
        return Ok(encoded);
    }
    // wkb-raster 0.2.1 writes only one placeholder byte, even for wider pixels.
    let mut offset = 61 * 2;
    let mut output = encoded
        .get(..offset)
        .context("truncated raster header")?
        .to_owned();
    for &(bytes, has_nodata, payload) in bands {
        let flag_end = offset + 2;
        let end = flag_end + (payload + if has_nodata { bytes } else { 1 }) * 2;
        output.push_str(
            encoded
                .get(offset..flag_end)
                .context("truncated raster flag")?,
        );
        if !has_nodata {
            output.extend(std::iter::repeat_n('0', (bytes - 1) * 2));
        }
        output.push_str(
            encoded
                .get(flag_end..end)
                .context("truncated raster band")?,
        );
        offset = end;
    }
    anyhow::ensure!(offset == encoded.len(), "raster band size mismatch");
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn nodata_must_fit_the_exact_band_representation() {
        assert!(band_type_to_pixtype(GdalDataType::UInt8, Some(256.)).is_err());
        assert!(band_type_to_pixtype(GdalDataType::Int16, Some(0.5)).is_err());
        assert!(band_type_to_pixtype(GdalDataType::Float32, Some(0.1)).is_err());
        assert!(band_type_to_pixtype(GdalDataType::Float32, Some(f64::NAN)).is_ok());
        assert!(band_type_to_pixtype(GdalDataType::Float64, Some(0.1)).is_ok());
    }

    #[test]
    fn absent_nodata_padding_does_not_shift_following_bands() -> Result<()> {
        let header = "00".repeat(61);
        let raw = format!("{header}0600AABB46FFFF0001");
        assert_eq!(
            pad_absent_nodata(raw, &[(2, false, 2), (2, true, 2)])?,
            format!("{header}060000AABB46FFFF0001")
        );
        let complete = format!("{header}060000AABB46FFFF0001");
        assert_eq!(
            pad_absent_nodata(complete.clone(), &[(2, false, 2), (2, true, 2)])?,
            complete
        );
        Ok(())
    }
}
