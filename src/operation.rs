//! Control, diagnostics and completion contracts for synchronous GDAL operations.

use crate::control::Cancellation;
use std::{fmt, ops::ControlFlow};

/// Native progress is advisory; only a successful operation return confirms closure.
#[derive(Debug, Clone, PartialEq)]
pub struct Progress {
    /// Native completion fraction, normalized to `[0, 1]`.
    pub fraction: f64,
    /// Native progress message. It may be empty.
    pub message: String,
}

pub(crate) type ProgressCallback<'a> = dyn FnMut(&Progress) -> ControlFlow<()> + Send + 'a;

/// Borrowed control for one synchronous operation.
///
/// Callbacks may run on GDAL worker threads, but are invoked serially. They can
/// borrow stack values and must not depend on the identity of the calling thread.
#[derive(Default)]
#[must_use]
pub struct Control<'a> {
    pub(crate) cancellation: Option<&'a Cancellation>,
    pub(crate) callback: Option<&'a mut ProgressCallback<'a>>,
}

impl fmt::Debug for Control<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Control")
            .field("cancellation", &self.cancellation.is_some())
            .field("callback", &self.callback.is_some())
            .finish()
    }
}

impl<'a> Control<'a> {
    /// Observe a shared cooperative cancellation token.
    pub fn cancellation(mut self, cancellation: &'a Cancellation) -> Self {
        self.cancellation = Some(cancellation);
        self
    }

    /// Observe progress or request cancellation with `ControlFlow::Break(())`.
    pub fn on_progress(mut self, callback: &'a mut ProgressCallback<'a>) -> Self {
        self.callback = Some(callback);
        self
    }

    pub(crate) fn check(&self) -> Result<()> {
        if self.cancellation.is_some_and(Cancellation::is_cancelled) {
            Err(Error::new(
                ErrorKind::Cancelled,
                Stage::Prepare,
                "operation cancelled",
            ))
        } else {
            Ok(())
        }
    }

    pub(crate) fn is_active(&self) -> bool {
        self.callback.is_some() || self.cancellation.is_some()
    }
}

/// The origin of a native diagnostic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Severity {
    /// Driver warning; the operation may still succeed.
    Warning,
    /// Native failure; its effect depends on the selected error policy.
    Failure,
    /// Native fatal error.
    Fatal,
}

/// A bounded copy of a native diagnostic. Messages may contain input data or paths.
#[derive(Clone, PartialEq, Eq)]
pub struct Diagnostic {
    /// Native severity.
    pub severity: Severity,
    /// CPL error number.
    pub number: i32,
    /// Native text; intentionally omitted from Debug output.
    pub message: String,
    /// Whether the native message exceeded the retained size.
    pub truncated: bool,
}

impl fmt::Debug for Diagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Diagnostic")
            .field("severity", &self.severity)
            .field("number", &self.number)
            .field("truncated", &self.truncated)
            .finish_non_exhaustive()
    }
}

/// Successful completion of the selected policy, including explicit output close.
///
/// This is not a promise of lossless conversion or an exact feature count. In
/// skip-failures/native mode inspect diagnostics before publishing the output.
#[derive(Debug, Default)]
#[must_use]
pub struct Report {
    pub(crate) diagnostics: Vec<Diagnostic>,
    pub(crate) diagnostics_truncated: bool,
}

impl Report {
    /// Retained warnings and failures, in observed order.
    pub fn diagnostics(&self) -> &[Diagnostic] {
        &self.diagnostics
    }

    /// Whether diagnostic entries or message bytes were discarded.
    pub fn diagnostics_truncated(&self) -> bool {
        self.diagnostics_truncated
    }
}

/// A recoverable category, independent of GDAL's diagnostic wording.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ErrorKind {
    /// Invalid or conflicting arguments.
    InvalidArgument,
    /// Unsupported driver, version, or combination.
    Unsupported,
    /// The output dataset or a companion file already exists.
    OutputExists,
    /// A target layer already exists.
    LayerExists,
    /// A required source or target layer is absent.
    LayerNotFound,
    /// Dataset open failed.
    Open,
    /// Native conversion failed.
    Translate,
    /// Native raster processing failed.
    Processing,
    /// A dataset could not be closed successfully.
    Close,
    /// Cancellation was observed by the execution path.
    Cancelled,
    /// A user progress callback panicked.
    CallbackPanicked,
}

/// Stage associated with a failure or cleanup error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Stage {
    /// Argument validation, driver/CRS checks and native option parsing.
    Prepare,
    /// Opening the source.
    OpenSource,
    /// Inspecting input and target metadata.
    Inspect,
    /// Opening an existing target for update; this may modify it.
    OpenOutput,
    /// Calling the native utility.
    Execute,
    /// Closing the output.
    CloseOutput,
    /// Closing the input.
    CloseSource,
}

/// Conservative output state; never infer rollback from an error.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum OutputState {
    /// No potentially writing target operation was entered.
    #[default]
    NotModified,
    /// An operation which may have changed the output was entered.
    MayHaveChanged,
}

/// A separate cleanup failure which did not replace the primary failure.
#[derive(thiserror::Error)]
#[error("dataset cleanup failed during {stage:?}")]
pub struct CleanupError {
    /// Which dataset was being closed.
    pub stage: Stage,
    #[source]
    pub(crate) source: gdal::errors::GdalError,
}

impl fmt::Debug for CleanupError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CleanupError")
            .field("stage", &self.stage)
            .finish_non_exhaustive()
    }
}

/// Operation failure with diagnostics and separately retained cleanup errors.
#[derive(thiserror::Error)]
#[error("{kind:?} during {stage:?}: {message}")]
pub struct Error {
    kind: ErrorKind,
    stage: Stage,
    message: String,
    #[source]
    source: Option<Box<dyn std::error::Error + Send + Sync>>,
    pub(crate) report: Report,
    pub(crate) cleanup: Vec<CleanupError>,
    pub(crate) output_state: OutputState,
}

impl fmt::Debug for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Error")
            .field("kind", &self.kind)
            .field("stage", &self.stage)
            .field("output_state", &self.output_state)
            .field("diagnostics", &self.report.diagnostics.len())
            .field("cleanup_errors", &self.cleanup.len())
            .finish_non_exhaustive()
    }
}

impl Error {
    /// Machine-readable category.
    pub fn kind(&self) -> ErrorKind {
        self.kind
    }
    /// Stage of the primary failure.
    pub fn stage(&self) -> Stage {
        self.stage
    }
    /// Whether the output may already have changed.
    pub fn output_state(&self) -> OutputState {
        self.output_state
    }
    /// Retained native diagnostics, including cleanup diagnostics.
    pub fn diagnostics(&self) -> &[Diagnostic] {
        self.report.diagnostics()
    }
    /// Whether any native diagnostic text or entries were truncated.
    pub fn diagnostics_truncated(&self) -> bool {
        self.report.diagnostics_truncated()
    }
    /// Output and input close errors, with their origins.
    pub fn cleanup_errors(&self) -> &[CleanupError] {
        &self.cleanup
    }

    pub(crate) fn new(kind: ErrorKind, stage: Stage, message: impl Into<String>) -> Self {
        Self {
            kind,
            stage,
            message: message.into(),
            source: None,
            report: Report::default(),
            cleanup: Vec::new(),
            output_state: OutputState::NotModified,
        }
    }

    pub(crate) fn invalid(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::InvalidArgument, Stage::Prepare, message)
    }

    pub(crate) fn unsupported(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Unsupported, Stage::Prepare, message)
    }

    pub(crate) fn with_source(
        mut self,
        source: impl std::error::Error + Send + Sync + 'static,
    ) -> Self {
        self.source = Some(Box::new(source));
        self
    }

    pub(crate) fn native(stage: Stage, source: gdal::errors::GdalError) -> Self {
        let kind = match stage {
            Stage::OpenSource | Stage::OpenOutput => ErrorKind::Open,
            Stage::CloseSource | Stage::CloseOutput => ErrorKind::Close,
            Stage::Prepare | Stage::Inspect => ErrorKind::InvalidArgument,
            Stage::Execute => ErrorKind::Translate,
        };
        Self::new(
            kind,
            stage,
            "GDAL operation failed; inspect source and diagnostics",
        )
        .with_source(source)
    }
}

/// Result of a synchronous GDAL operation.
pub type Result<T> = std::result::Result<T, Error>;
