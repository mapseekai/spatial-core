use super::{
    CoverageLimits, LayerSource, MergeStrategy,
    coverage::{self, Action, Settings},
    execute::Output,
};
use crate::{
    config::gdal::GdalOptions,
    operation::{Control, Report, Result},
};
use std::path::Path;

/// Clean shared edges with GDAL 3.12 / GEOS 3.14+; input is never updated.
#[derive(Debug, Clone)]
#[must_use]
pub struct CleanCoverage {
    settings: Settings,
    output: Output,
    snap: f64,
    gap: f64,
    merge: MergeStrategy,
}
impl CleanCoverage {
    /// Describe a new result; default tolerances are zero, never guessed.
    pub fn new(source: impl Into<LayerSource>, output: impl AsRef<Path>) -> Self {
        Self {
            settings: Settings::new(source),
            output: Output::new(output, "cleaned"),
            snap: 0.,
            gap: 0.,
            merge: MergeStrategy::default(),
        }
    }
    /// Set source-CRS snapping distance.
    pub fn snapping_distance(mut self, value: f64) -> Self {
        self.snap = value;
        self
    }
    /// Set the maximum gap width to close in source units.
    pub fn maximum_gap_width(mut self, value: f64) -> Self {
        self.gap = value;
        self
    }
    /// Select which polygon owns overlapping area.
    pub fn merge_strategy(mut self, value: MergeStrategy) -> Self {
        self.merge = value;
        self
    }
    coverage::settings_methods!();
    coverage::output_methods!();
    /// Process synchronously.
    pub fn run(self) -> Result<Report> {
        self.run_with(&mut Control::default())
    }
    /// Process with borrowed progress/cancellation.
    pub fn run_with(self, control: &mut Control<'_>) -> Result<Report> {
        coverage::run(
            self.settings,
            Action::Clean {
                snap: self.snap,
                gap: self.gap,
                merge: self.merge,
            },
            Some(self.output),
            control,
        )
        .map(|result| result.into_parts().1)
    }
}
