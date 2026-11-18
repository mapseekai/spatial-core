use super::{
    CoverageLimits, LayerSource,
    coverage::{self, Action, Settings},
    execute::Output,
};
use crate::{
    config::gdal::GdalOptions,
    operation::{Control, Error, Report, Result},
};
use std::path::Path;

/// Simplify shared edges after validating the complete selected coverage.
#[derive(Debug, Clone)]
#[must_use]
pub struct SimplifyCoverage {
    settings: Settings,
    output: Output,
    tolerance: Option<f64>,
    preserve: bool,
}
impl SimplifyCoverage {
    /// Describe a new result; tolerance must be explicitly selected.
    pub fn new(source: impl Into<LayerSource>, output: impl AsRef<Path>) -> Self {
        Self {
            settings: Settings::new(source),
            output: Output::new(output, "simplified"),
            tolerance: None,
            preserve: false,
        }
    }
    /// Set a finite nonnegative source-CRS tolerance.
    pub fn tolerance(mut self, value: f64) -> Self {
        self.tolerance = Some(value);
        self
    }
    /// Keep the exterior boundary unchanged.
    pub fn preserve_boundary(mut self, value: bool) -> Self {
        self.preserve = value;
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
        let tolerance = self.tolerance.ok_or_else(|| {
            Error::invalid("coverage simplification requires an explicit tolerance")
        })?;
        coverage::run(
            self.settings,
            Action::Simplify {
                tolerance,
                preserve: self.preserve,
            },
            Some(self.output),
            control,
        )
        .map(|result| result.into_parts().1)
    }
}
