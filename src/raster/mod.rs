//! ```compile_fail
//! use spatial_core::raster::MinioSettings;
//! ```

pub mod convert;
pub mod copy;

pub use convert::{
    GdalOperationProgress, translate_to_path_with_progress, warp_to_path_with_progress,
};
pub use copy::{IngestMode, RasterCopyOptions, RasterCopySummary, StorageMode, copy_to_postgis};
