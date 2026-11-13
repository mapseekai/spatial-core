use crate::postgis::{ColumnName, PostGisError, Result};

/// Whether pixels live in PostgreSQL or in a persistent server-readable file.
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub enum RasterStorage {
    /// Copy pixel values into the database.
    #[default]
    InDb,
    /// Store a persistent reference interpreted by the database server.
    OutDb {
        /// Stable file/virtual filesystem path visible to PostgreSQL.
        server_path: String,
    },
}

/// Existing supported GDAL/PostGIS pixel types.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelType {
    /// Unsigned byte.
    UInt8,
    /// Unsigned 16-bit integer.
    UInt16,
    /// Signed 16-bit integer.
    Int16,
    /// Unsigned 32-bit integer.
    UInt32,
    /// Signed 32-bit integer.
    Int32,
    /// IEEE float32.
    Float32,
    /// IEEE float64.
    Float64,
}

impl PixelType {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::UInt8 => "8BUI",
            Self::UInt16 => "16BUI",
            Self::Int16 => "16BSI",
            Self::UInt32 => "32BUI",
            Self::Int32 => "32BSI",
            Self::Float32 => "32BF",
            Self::Float64 => "64BF",
        }
    }
    pub(crate) fn from_gdal(value: gdal::raster::GdalDataType) -> Result<Self> {
        use gdal::raster::GdalDataType as G;
        Ok(match value {
            G::UInt8 => Self::UInt8,
            G::UInt16 => Self::UInt16,
            G::Int16 => Self::Int16,
            G::UInt32 => Self::UInt32,
            G::Int32 => Self::Int32,
            G::Float32 => Self::Float32,
            G::Float64 => Self::Float64,
            _ => {
                return Err(PostGisError::Unsupported(format!(
                    "raster pixel type {value:?}"
                )));
            }
        })
    }
    pub(crate) fn bytes(self) -> usize {
        match self {
            Self::UInt8 => 1,
            Self::UInt16 | Self::Int16 => 2,
            Self::UInt32 | Self::Int32 | Self::Float32 => 4,
            Self::Float64 => 8,
        }
    }
}

/// Band type and optional NoData marker; band order is significant.
#[derive(Debug, Clone)]
pub struct RasterBandSchema {
    /// Pixel representation.
    pub pixel_type: PixelType,
    /// NoData value, including NaN for floating bands.
    pub nodata: Option<f64>,
}

/// Raster column contract inferred from the source/output grid.
#[derive(Debug, Clone)]
pub struct RasterSchema {
    /// Identity column.
    pub identity: ColumnName,
    /// Raster column.
    pub raster: ColumnName,
    /// Confirmed output EPSG/database SRID.
    pub srid: i32,
    /// Planned GDAL affine transform [origin x, scale x, skew x, origin y, skew y, scale y].
    /// Scale/skew are checked on every stored tile; the origin describes the import plan,
    /// not a reconstruction of tile locations after caller-defined triggers.
    pub geo_transform: [f64; 6],
    /// Ordered bands.
    pub bands: Vec<RasterBandSchema>,
}
