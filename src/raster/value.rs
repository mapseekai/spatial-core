use super::DataType;
use crate::{gdal_ext::query::inspect_error, operation::Result};
use gdal::raster::RasterBand;

/// An exact integer or floating-point pixel/NoData value.
#[derive(Debug, Clone, Copy, PartialEq)]
#[non_exhaustive]
pub enum PixelValue {
    /// Signed integer, without a floating-point round trip.
    Int(i64),
    /// Unsigned integer, without a floating-point round trip.
    UInt(u64),
    /// Floating-point value, which may be NaN.
    Float(f64),
}
impl std::fmt::Display for PixelValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Int(value) => value.fmt(f),
            Self::UInt(value) => value.fmt(f),
            Self::Float(value) => value.fmt(f),
        }
    }
}
macro_rules! from_value {
    ($variant:ident, $storage:ty, $($ty:ty),+) => {$(
        impl From<$ty> for PixelValue {
            fn from(value: $ty) -> Self { Self::$variant(value as $storage) }
        }
    )+};
}
from_value!(Int, i64, i8, i16, i32, i64);
from_value!(UInt, u64, u8, u16, u32, u64);
from_value!(Float, f64, f32, f64);

pub(super) fn nodata(band: &RasterBand<'_>) -> Option<PixelValue> {
    match band.band_type() {
        DataType::Int64 => band.no_data_value_i64().map(PixelValue::Int),
        DataType::UInt64 => band.no_data_value_u64().map(PixelValue::UInt),
        _ => band.no_data_value().map(PixelValue::Float),
    }
}
pub(super) fn sample(band: &RasterBand<'_>, position: (isize, isize)) -> Result<PixelValue> {
    match band.band_type() {
        DataType::Int8 | DataType::Int16 | DataType::Int32 | DataType::Int64 => {
            let data = band
                .read_as::<i64>(position, (1, 1), (1, 1), None)
                .map_err(inspect_error)?;
            Ok(PixelValue::Int(data.data()[0]))
        }
        DataType::UInt8 | DataType::UInt16 | DataType::UInt32 | DataType::UInt64 => {
            let data = band
                .read_as::<u64>(position, (1, 1), (1, 1), None)
                .map_err(inspect_error)?;
            Ok(PixelValue::UInt(data.data()[0]))
        }
        DataType::Float32 | DataType::Float64 => {
            let data = band
                .read_as::<f64>(position, (1, 1), (1, 1), None)
                .map_err(inspect_error)?;
            Ok(PixelValue::Float(data.data()[0]))
        }
        _ => Err(crate::operation::Error::unsupported(
            "pixel query requires a real numeric band",
        )),
    }
}
