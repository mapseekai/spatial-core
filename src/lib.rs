//! Low-level GIS building blocks extracted from MapSeek.
//!
//! The library provides GDAL raster/vector conversion, cancellable PostGIS
//! ingestion, database metadata and GDAL configuration helpers. It does not
//! implement service endpoints, authorization, business job state, operator
//! registration or DAG orchestration. Callers own those responsibilities.
//!
//! GDAL datasets and synchronous conversion functions belong on a blocking
//! thread. Async ingestion functions manage their own blocking readers and
//! accept a [`control::Cancellation`] token. See the repository README for
//! native dependencies and the retained configuration/ingestion contracts.

pub mod config;
pub mod control;
pub mod pg;
pub mod raster;
pub mod vector;

pub(crate) mod gdal_ext;
