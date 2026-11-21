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

impl WriteProgress {
    /// Advisory completion fraction normalized to `[0.0, 1.0]` when a positive total is known.
    pub fn fraction(&self) -> Option<f64> {
        let total = self.total.filter(|&total| total > 0)?;
        Some((self.records as f64 / total as f64).clamp(0.0, 1.0))
    }
}

/// Cancellation and an optional synchronous, lightweight progress callback.
#[must_use]
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
    /// Observe a shared cooperative cancellation token.
    pub fn cancellation(mut self, cancel: &Cancellation) -> Self {
        self.cancel = cancel.clone();
        self
    }
    /// Install a callback invoked on the async coordinator, never in GDAL FFI.
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

impl From<Cancellation> for WriteControl<'_> {
    fn from(cancel: Cancellation) -> Self {
        Self::new(cancel)
    }
}

impl From<&Cancellation> for WriteControl<'_> {
    fn from(cancel: &Cancellation) -> Self {
        Self::new(cancel.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_control_shares_cancellation_and_normalizes_progress_fraction() {
        let token = Cancellation::default();
        let control = WriteControl::default().cancellation(&token);
        let converted = WriteControl::from(&token);
        assert!(!control.cancel.is_cancelled());
        token.cancel();
        assert!(control.cancel.is_cancelled());
        assert!(converted.cancel.is_cancelled());

        let unknown = WriteProgress {
            stage: WriteStage::Copying,
            records: 5,
            total: None,
        };
        assert_eq!(unknown.fraction(), None);

        let zero_total = WriteProgress {
            stage: WriteStage::Copying,
            records: 0,
            total: Some(0),
        };
        assert_eq!(zero_total.fraction(), None);

        let half = WriteProgress {
            stage: WriteStage::Copying,
            records: 5,
            total: Some(10),
        };
        assert_eq!(half.fraction(), Some(0.5));

        let capped = WriteProgress {
            stage: WriteStage::Copying,
            records: 12,
            total: Some(10),
        };
        assert_eq!(capped.fraction(), Some(1.0));
    }
}
