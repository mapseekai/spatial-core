use super::{
    CoverageCheck, CoverageLimits, LayerSource,
    coverage::{self, Action, Settings},
    execute::Output,
};
use crate::{
    config::gdal::GdalOptions,
    operation::{Control, Outcome, Result},
};
use std::path::Path;

/// Check a complete selected polygon coverage with GDAL 3.12 / GEOS 3.12+.
#[derive(Debug, Clone)]
#[must_use]
pub struct CheckCoverage {
    settings: Settings,
    gap: f64,
    limit: usize,
    issues: Option<Output>,
}
impl CheckCoverage {
    /// Describe a coverage check; construction does no I/O.
    pub fn new(source: impl Into<LayerSource>) -> Self {
        Self {
            settings: Settings::new(source),
            gap: 0.,
            limit: 100,
            issues: None,
        }
    }
    /// Flag gaps no wider than this source-CRS distance; default zero.
    pub fn maximum_gap_width(mut self, value: f64) -> Self {
        self.gap = value;
        self
    }
    /// Bound retained samples; the scan still checks all selected features.
    pub fn sample_limit(mut self, value: usize) -> Self {
        self.limit = value;
        self
    }
    /// Write all issues to a new GPKG.
    pub fn issues_output(mut self, path: impl AsRef<Path>) -> Self {
        self.issues = Some(Output::new(path, "coverage_issues"));
        self
    }
    coverage::settings_methods!();
    /// Check synchronously.
    pub fn run(self) -> Result<Outcome<CoverageCheck>> {
        self.run_with(&mut Control::default())
    }
    /// Check with borrowed progress/cancellation.
    pub fn run_with(self, control: &mut Control<'_>) -> Result<Outcome<CoverageCheck>> {
        coverage::run(
            self.settings,
            Action::Check {
                gap: self.gap,
                limit: self.limit,
                issues: self.issues,
            },
            None,
            control,
        )
    }
}
