//! Raster processing and database-independent window planning.
pub mod convert;
pub mod tiling;
pub use convert::{
    GdalOperationProgress, translate_to_path_with_progress, warp_to_path_with_progress,
};

mod calculate;
mod contour;
pub(crate) mod dem;
mod execute;
mod fill_nodata;
mod native;
mod options;
mod overviews;
mod polygonize;
mod rasterize;
mod sieve;
mod translate;
mod value;
mod vrt;
mod warp;
pub use overviews::{BuildOverviews, ClearOverviews, OverviewStorage, RefreshOverviews};
mod reclassify;
pub use calculate::{Calculate, ExpressionDialect, NoDataPolicy};
pub use reclassify::{Reclassify, Unmatched};
pub use value::PixelValue;
mod info;
mod pixel_info;
pub use contour::Contour;
pub use dem::{Dem, DemAlgorithm, DemMode};
pub use fill_nodata::FillNodata;
pub use info::{
    BandInfo, BandStatistics, Histogram, HistogramOptions, Info, RasterInfo, StatisticsMode,
    StatisticsOrigin,
};
pub use native::{NativeBuildVrt, NativeDem, NativeRasterize, NativeTranslate, NativeWarp};
pub use options::{Connectivity, DataType, Mask, Resampling};
pub use pixel_info::{BandSample, PixelInfo, PixelSample, SampleLocation};
pub use polygonize::Polygonize;
pub use rasterize::Rasterize;
pub use sieve::Sieve;
pub use translate::Translate;
pub use vrt::BuildVrt;
pub use warp::Warp;

#[cfg(test)]
mod test_support;
