//! Raster ingestion with explicit InDB/OutDB storage and tile layout.
//!
//! ```no_run
//! use spatial_core::postgis::{PostGisClient, TableName, WriteControl};
//! use spatial_core::postgis::raster::{PostGisRasterWriter, RasterSource, TileLayout};
//!
//! # async fn import(db: &PostGisClient) -> anyhow::Result<()> {
//! let report = PostGisRasterWriter::new(db, TableName::new("gis", "elevation")?)
//!     .tile_layout(TileLayout::tiled(256, 256)?)
//!     .write(RasterSource::new("elevation.tif"), &WriteControl::default())
//!     .await?;
//! println!("committed {} tiles", report.tile_count);
//! # Ok(())
//! # }
//! ```
mod metadata;
mod schema;
mod source;
mod table;
mod wkb;
mod writer;

pub use crate::raster::tiling::TileLayout;
pub use metadata::{RasterMetadata, read_metadata};
pub use schema::{PixelType, RasterBandSchema, RasterSchema, RasterStorage};
pub use source::RasterSource;
pub use writer::{PostGisRasterWriter, RasterWriteOptions, RasterWriteReport, Resampling};
