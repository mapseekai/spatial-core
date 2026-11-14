//! Raster processing and database-independent window planning.
pub mod convert;
pub mod tiling;
pub use convert::{
    GdalOperationProgress, translate_to_path_with_progress, warp_to_path_with_progress,
};

mod contour;
pub(crate) mod dem;
mod execute;
mod fill_nodata;
mod native;
mod options;
mod polygonize;
mod rasterize;
mod sieve;
mod translate;
mod vrt;
mod warp;
pub use contour::Contour;
pub use dem::{Dem, DemAlgorithm, DemMode};
pub use fill_nodata::FillNodata;
pub use native::{NativeBuildVrt, NativeDem, NativeRasterize, NativeTranslate, NativeWarp};
pub use options::{Connectivity, DataType, Mask, Resampling};
pub use polygonize::Polygonize;
pub use rasterize::Rasterize;
pub use sieve::Sieve;
pub use translate::Translate;
pub use vrt::BuildVrt;
pub use warp::Warp;

#[cfg(test)]
mod test_support;
