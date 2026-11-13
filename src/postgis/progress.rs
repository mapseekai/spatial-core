use crate::control::Cancellation;

/// Stage of one write. Only Completed means the commit was confirmed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum WriteStage {
    /// Reading input metadata and validating the target.
    Preparing,
    /// Transferring features or tiles.
    Copying,
    /// Checking constraints, indexes, and metadata.
    Finalizing,
    /// Awaiting COMMIT without a cancellation race.
    Committing,
    /// COMMIT acknowledged by the database.
    Completed,
}

/// Progress is advisory; the final report supplies committed counts.
#[derive(Debug, Clone, Copy)]
pub struct WriteProgress {
    /// Current stage.
    pub stage: WriteStage,
    /// Features or tiles sent so far.
    pub records: u64,
    /// Cheap input count when available; never an EOF/completion check.
    pub total: Option<u64>,
}

/// Cancellation and an optional synchronous, lightweight progress callback.
pub struct WriteControl<'a> {
    pub(crate) cancel: Cancellation,
    callback: Option<&'a (dyn Fn(WriteProgress) + Send + Sync)>,
}

impl<'a> WriteControl<'a> {
    /// Create control for one write. Cancel and continue awaiting to join cleanup.
    pub fn new(cancel: Cancellation) -> Self {
        Self {
            cancel,
            callback: None,
        }
    }
    /// Install a callback invoked on the async coordinator, never in GDAL FFI.
    #[must_use]
    pub fn on_progress(mut self, callback: &'a (dyn Fn(WriteProgress) + Send + Sync)) -> Self {
        self.callback = Some(callback);
        self
    }
    pub(crate) fn emit(&self, stage: WriteStage, records: u64, total: Option<u64>) {
        if let Some(callback) = self.callback {
            callback(WriteProgress {
                stage,
                records,
                total,
            });
        }
    }
}

impl Default for WriteControl<'_> {
    fn default() -> Self {
        Self::new(Cancellation::default())
    }
}
