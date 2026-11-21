use super::RasterStorage;
use crate::raster::tiling::TileWindow;
use anyhow::{Context, Result, anyhow};
use gdal::{Dataset, raster::GdalDataType};

const WKB_NDR_LITTLE_ENDIAN: u8 = 1;
const WKB_RASTER_HEADER_BYTES: usize = 61;
const BAND_FLAG_OFFLINE: u8 = 0x80;
const BAND_FLAG_HAS_NODATA: u8 = 0x40;

const PT_8BUI: u8 = 4;
const PT_16BSI: u8 = 5;
const PT_16BUI: u8 = 6;
const PT_32BSI: u8 = 7;
const PT_32BUI: u8 = 8;
const PT_32BF: u8 = 10;
const PT_64BF: u8 = 11;

const HEX: &[u8; 16] = b"0123456789abcdef";

#[inline]
fn push_hex_byte(out: &mut Vec<u8>, byte: u8) {
    out.push(HEX[(byte >> 4) as usize]);
    out.push(HEX[(byte & 0x0f) as usize]);
}

#[inline]
fn push_hex_slice(out: &mut Vec<u8>, bytes: &[u8]) {
    for &byte in bytes {
        push_hex_byte(out, byte);
    }
}

pub(super) fn validate_band_nodata(t: GdalDataType, nodata: Option<f64>) -> Result<()> {
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
            return Err(anyhow!("NoData value {value} does not fit band type {t:?}"));
        }
    }
    pixtype_code(t)?;
    Ok(())
}

fn pixtype_code(t: GdalDataType) -> Result<u8> {
    match t {
        GdalDataType::UInt8 => Ok(PT_8BUI),
        GdalDataType::Int16 => Ok(PT_16BSI),
        GdalDataType::UInt16 => Ok(PT_16BUI),
        GdalDataType::Int32 => Ok(PT_32BSI),
        GdalDataType::UInt32 => Ok(PT_32BUI),
        GdalDataType::Float32 => Ok(PT_32BF),
        GdalDataType::Float64 => Ok(PT_64BF),
        other => Err(anyhow!("unsupported GDAL band type: {other:?}")),
    }
}

fn pixel_bytes(t: GdalDataType) -> Result<usize> {
    match t {
        GdalDataType::UInt8 => Ok(1),
        GdalDataType::Int16 | GdalDataType::UInt16 => Ok(2),
        GdalDataType::Int32 | GdalDataType::UInt32 | GdalDataType::Float32 => Ok(4),
        GdalDataType::Float64 => Ok(8),
        other => Err(anyhow!("unsupported GDAL band type: {other:?}")),
    }
}

fn push_nodata_hex(out: &mut Vec<u8>, t: GdalDataType, nodata: Option<f64>) -> Result<()> {
    match t {
        GdalDataType::UInt8 => push_hex_byte(out, nodata.map_or(0u8, |v| v as u8)),
        GdalDataType::Int16 => {
            push_hex_slice(out, &nodata.map_or(0i16, |v| v as i16).to_le_bytes())
        }
        GdalDataType::UInt16 => {
            push_hex_slice(out, &nodata.map_or(0u16, |v| v as u16).to_le_bytes())
        }
        GdalDataType::Int32 => {
            push_hex_slice(out, &nodata.map_or(0i32, |v| v as i32).to_le_bytes())
        }
        GdalDataType::UInt32 => {
            push_hex_slice(out, &nodata.map_or(0u32, |v| v as u32).to_le_bytes())
        }
        GdalDataType::Float32 => {
            push_hex_slice(out, &nodata.map_or(0f32, |v| v as f32).to_le_bytes())
        }
        GdalDataType::Float64 => push_hex_slice(out, &nodata.unwrap_or(0f64).to_le_bytes()),
        other => return Err(anyhow!("unsupported GDAL band type: {other:?}")),
    }
    Ok(())
}

fn write_indb_pixels_hex(
    out: &mut Vec<u8>,
    band: &gdal::raster::RasterBand<'_>,
    band_type: GdalDataType,
    window: TileWindow,
) -> Result<()> {
    let origin = (window.x_off as isize, window.y_off as isize);
    let size = (window.width, window.height);
    match band_type {
        GdalDataType::UInt8 => {
            let buffer = band
                .read_as::<u8>(origin, size, size, None)
                .context("failed to read raster window")?;
            push_hex_slice(out, buffer.data());
        }
        GdalDataType::Int16 => {
            let buffer = band
                .read_as::<i16>(origin, size, size, None)
                .context("failed to read raster window")?;
            for &pixel in buffer.data() {
                push_hex_slice(out, &pixel.to_le_bytes());
            }
        }
        GdalDataType::UInt16 => {
            let buffer = band
                .read_as::<u16>(origin, size, size, None)
                .context("failed to read raster window")?;
            for &pixel in buffer.data() {
                push_hex_slice(out, &pixel.to_le_bytes());
            }
        }
        GdalDataType::Int32 => {
            let buffer = band
                .read_as::<i32>(origin, size, size, None)
                .context("failed to read raster window")?;
            for &pixel in buffer.data() {
                push_hex_slice(out, &pixel.to_le_bytes());
            }
        }
        GdalDataType::UInt32 => {
            let buffer = band
                .read_as::<u32>(origin, size, size, None)
                .context("failed to read raster window")?;
            for &pixel in buffer.data() {
                push_hex_slice(out, &pixel.to_le_bytes());
            }
        }
        GdalDataType::Float32 => {
            let buffer = band
                .read_as::<f32>(origin, size, size, None)
                .context("failed to read raster window")?;
            for &pixel in buffer.data() {
                push_hex_slice(out, &pixel.to_le_bytes());
            }
        }
        GdalDataType::Float64 => {
            let buffer = band
                .read_as::<f64>(origin, size, size, None)
                .context("failed to read raster window")?;
            for &pixel in buffer.data() {
                push_hex_slice(out, &pixel.to_le_bytes());
            }
        }
        other => return Err(anyhow!("unsupported GDAL band type: {other:?}")),
    }
    Ok(())
}

/// Encode one raster tile directly into a single-allocation PostGIS `COPY ... FORMAT text` row.
///
/// PostGIS's `raster` type defines `raster_in` (hex WKB) without a PostgreSQL binary `typreceive`
/// function. Encoding RFC2 WKB directly into a pre-sized hex buffer avoids intermediate `Vec<Vec>`
/// allocations and writes full-width zero placeholders when a band has no NoData value.
pub(super) fn encode_window_copy_row(
    dataset: &Dataset,
    storage: &RasterStorage,
    srid: i32,
    geo_transform: [f64; 6],
    window: TileWindow,
) -> Result<Vec<u8>> {
    let width_u16 = u16::try_from(window.width)
        .map_err(|_| anyhow!("window width exceeds u16: {}", window.width))?;
    let height_u16 = u16::try_from(window.height)
        .map_err(|_| anyhow!("window height exceeds u16: {}", window.height))?;
    let band_count = dataset.raster_count();
    if band_count == 0 {
        return Err(anyhow!("dataset has no raster bands"));
    }
    let band_count_u16 =
        u16::try_from(band_count).map_err(|_| anyhow!("band count exceeds u16: {band_count}"))?;
    let pixel_count = window
        .width
        .checked_mul(window.height)
        .ok_or_else(|| anyhow!("window pixel count overflow"))?;

    let mut wkb_len = WKB_RASTER_HEADER_BYTES;
    for idx in 1..=band_count {
        let band = dataset
            .rasterband(idx)
            .with_context(|| format!("failed to open raster band {idx}"))?;
        let bytes = pixel_bytes(band.band_type())?;
        let payload = match storage {
            RasterStorage::InDb => pixel_count
                .checked_mul(bytes)
                .ok_or_else(|| anyhow!("band payload size overflow"))?,
            RasterStorage::OutDb { server_path } => server_path
                .len()
                .checked_add(2)
                .ok_or_else(|| anyhow!("OutDB path size overflow"))?,
        };
        wkb_len = wkb_len
            .checked_add(1 + bytes)
            .and_then(|n| n.checked_add(payload))
            .ok_or_else(|| anyhow!("raster WKB size overflow"))?;
    }
    let encoded_len = wkb_len
        .checked_mul(2)
        .and_then(|n| n.checked_add(1))
        .ok_or_else(|| anyhow!("encoded raster row size overflow"))?;

    let ip_x = geo_transform[0]
        + geo_transform[1] * (window.x_off as f64)
        + geo_transform[2] * (window.y_off as f64);
    let ip_y = geo_transform[3]
        + geo_transform[4] * (window.x_off as f64)
        + geo_transform[5] * (window.y_off as f64);

    let mut row = Vec::with_capacity(encoded_len);
    // PostGIS RFC2 Raster WKB header (61 bytes, little-endian).
    push_hex_byte(&mut row, WKB_NDR_LITTLE_ENDIAN);
    push_hex_slice(&mut row, &0u16.to_le_bytes()); // version
    push_hex_slice(&mut row, &band_count_u16.to_le_bytes());
    push_hex_slice(&mut row, &geo_transform[1].to_le_bytes()); // scaleX
    push_hex_slice(&mut row, &geo_transform[5].to_le_bytes()); // scaleY
    push_hex_slice(&mut row, &ip_x.to_le_bytes());
    push_hex_slice(&mut row, &ip_y.to_le_bytes());
    push_hex_slice(&mut row, &geo_transform[2].to_le_bytes()); // skewX
    push_hex_slice(&mut row, &geo_transform[4].to_le_bytes()); // skewY
    push_hex_slice(&mut row, &srid.to_le_bytes());
    push_hex_slice(&mut row, &width_u16.to_le_bytes());
    push_hex_slice(&mut row, &height_u16.to_le_bytes());

    let offline_flag = if matches!(storage, RasterStorage::OutDb { .. }) {
        BAND_FLAG_OFFLINE
    } else {
        0
    };
    for idx in 1..=band_count {
        let band = dataset
            .rasterband(idx)
            .with_context(|| format!("failed to open raster band {idx}"))?;
        let band_type = band.band_type();
        let nodata = band.no_data_value();
        validate_band_nodata(band_type, nodata)?;
        let flags = pixtype_code(band_type)?
            | offline_flag
            | if nodata.is_some() {
                BAND_FLAG_HAS_NODATA
            } else {
                0
            };
        push_hex_byte(&mut row, flags);
        push_nodata_hex(&mut row, band_type, nodata)?;
        match storage {
            RasterStorage::InDb => write_indb_pixels_hex(&mut row, &band, band_type, window)?,
            RasterStorage::OutDb { server_path } => {
                let band_index = u8::try_from(idx - 1)
                    .map_err(|_| anyhow!("OutDB band index exceeds u8: {}", idx - 1))?;
                push_hex_byte(&mut row, band_index);
                push_hex_slice(&mut row, server_path.as_bytes());
                push_hex_byte(&mut row, 0);
            }
        }
    }
    row.push(b'\n');

    anyhow::ensure!(row.len() == encoded_len, "encoded raster WKB size mismatch");
    Ok(row)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode_hex(hex: &[u8]) -> Vec<u8> {
        hex.as_chunks::<2>()
            .0
            .iter()
            .map(|&pair| {
                let s = std::str::from_utf8(&pair).expect("valid ascii hex");
                u8::from_str_radix(s, 16).expect("valid hex byte")
            })
            .collect()
    }

    #[test]
    fn nodata_must_fit_the_exact_band_representation() {
        assert!(validate_band_nodata(GdalDataType::UInt8, Some(256.)).is_err());
        assert!(validate_band_nodata(GdalDataType::Int16, Some(0.5)).is_err());
        assert!(validate_band_nodata(GdalDataType::Float32, Some(0.1)).is_err());
        assert!(validate_band_nodata(GdalDataType::Float32, Some(f64::NAN)).is_ok());
        assert!(validate_band_nodata(GdalDataType::Float64, Some(0.1)).is_ok());
    }

    #[test]
    fn raster_wkb_writes_full_width_placeholder_when_nodata_is_absent() -> Result<()> {
        let driver = gdal::DriverManager::get_driver_by_name("MEM")?;
        let dataset = driver.create_with_band_type::<u16, _>("", 2, 1, 2)?;
        dataset.rasterband(1)?.write(
            (0, 0),
            (2, 1),
            &mut gdal::raster::Buffer::new((2, 1), vec![0x1122u16, 0x3344u16]),
        )?;
        dataset.rasterband(2)?.write(
            (0, 0),
            (2, 1),
            &mut gdal::raster::Buffer::new((2, 1), vec![1u16, 2u16]),
        )?;
        dataset.rasterband(2)?.set_no_data_value(Some(65535.))?;

        let window = TileWindow {
            x_off: 0,
            y_off: 0,
            width: 2,
            height: 1,
        };
        let row = encode_window_copy_row(
            &dataset,
            &RasterStorage::InDb,
            4326,
            [10., 0.5, 0., 20., 0., -0.5],
            window,
        )?;
        assert_eq!(row.last(), Some(&b'\n'));
        let wkb = decode_hex(&row[..row.len() - 1]);
        // 61 (header) + 2 * (1 flag + 2 nodata + 4 pixels)
        assert_eq!(wkb.len(), 75);
        assert_eq!(wkb[0], WKB_NDR_LITTLE_ENDIAN);
        assert_eq!(u16::from_le_bytes(wkb[3..5].try_into().unwrap()), 2);
        // Band 1: no nodata -> full 2-byte zero placeholder before pixels.
        assert_eq!(wkb[61], PT_16BUI);
        assert_eq!(&wkb[62..64], &[0, 0]);
        assert_eq!(u16::from_le_bytes(wkb[64..66].try_into().unwrap()), 0x1122);
        assert_eq!(u16::from_le_bytes(wkb[66..68].try_into().unwrap()), 0x3344);
        // Band 2: has nodata 65535 -> flag includes BAND_FLAG_HAS_NODATA.
        assert_eq!(wkb[68], PT_16BUI | BAND_FLAG_HAS_NODATA);
        assert_eq!(u16::from_le_bytes(wkb[69..71].try_into().unwrap()), 65535);
        assert_eq!(u16::from_le_bytes(wkb[71..73].try_into().unwrap()), 1);
        assert_eq!(u16::from_le_bytes(wkb[73..75].try_into().unwrap()), 2);
        Ok(())
    }
}
