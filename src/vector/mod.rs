//! Vector file conversion. See `postgis::vector` for database operations.
#[cfg(test)]
mod analysis_test_support;
mod check_coverage;
mod clean_coverage;
mod coverage;
mod execute;
mod native;
mod options;
mod simplify_coverage;
mod source;
mod target;
mod translate;
pub use check_coverage::CheckCoverage;
pub use clean_coverage::CleanCoverage;
pub use coverage::{CoverageCheck, CoverageLimits, MergeStrategy};
pub use simplify_coverage::SimplifyCoverage;
mod overlay;
pub use overlay::{Overlay, OverlayOperation};
mod check_geometry;
pub use check_geometry::{
    CheckGeometry, CheckScope, GeometryCheck, GeometryIssue, GeometryIssueKind, GeometryPolicy,
};
pub use source::LayerSource;
mod info;
pub use info::{FieldInfo, GeometryFieldInfo, Info, LayerInfo, ScanMode, VectorInfo};

pub use native::NativeTranslate;
pub use options::{Clip, Dimensions, ErrorPolicy, GeometryType, SqlDialect, WriteMode};
pub use translate::Translate;
