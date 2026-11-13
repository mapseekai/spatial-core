//! Low-level GIS building blocks extracted from MapSeek.
//!
//! The library provides GDAL raster/vector conversion, cancellable PostGIS
//! ingestion, database metadata and GDAL configuration helpers. It does not
//! implement service endpoints, authorization, business job state, operator
//! registration or DAG orchestration. Callers own those responsibilities.
//!
//! GDAL datasets and synchronous conversion functions belong on a blocking
//! thread. Async ingestion functions manage their own blocking readers and
//! accept a [`postgis::WriteControl`]. See the repository README for native
//! dependencies, explicit configuration, and transaction contracts.

pub mod config;
pub mod control;
pub mod dataset;
pub mod operation;
pub mod postgis;
pub mod raster;
pub mod vector;

pub(crate) mod gdal_ext;
