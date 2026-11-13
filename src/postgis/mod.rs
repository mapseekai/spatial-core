//! Transactional PostGIS ingestion. Callers own the pool and Tokio runtime.
//!
//! Writers replace one ordinary table or its rows in one transaction. A returned
//! report confirms COMMIT; [`PostGisError::CommitOutcomeUnknown`] requires the
//! caller to reconcile the target before retrying.
//!
//! ```no_run
//! use spatial_core::postgis::{PostGisClient, TableName, WriteControl};
//! use spatial_core::postgis::vector::{PostGisVectorWriter, VectorSource};
//!
//! # async fn import(pool: deadpool_postgres::Pool) -> spatial_core::postgis::Result<()> {
//! let db = PostGisClient::from_pool(pool, tokio_postgres::NoTls);
//! let report = PostGisVectorWriter::new(&db, TableName::new("gis", "roads")?)
//!     .target_srid(3857)
//!     .write(VectorSource::new("roads.gpkg").layer_name("roads"), &WriteControl::default())
//!     .await?;
//! assert_eq!(report.schema.geometry.srid, 3857);
//! # Ok(())
//! # }
//! ```

mod connection;
mod error;
mod options;
mod progress;
mod sql;
mod stream;
#[cfg(test)]
mod test_support;
mod transaction;

pub mod raster;
pub mod vector;

pub use connection::PostGisClient;
pub use error::{PostGisError, Result};
pub use options::{EmptyInputPolicy, WriteLimits, WriteMode};
pub use progress::{WriteControl, WriteProgress, WriteStage};
pub use sql::{ColumnName, TableName};
