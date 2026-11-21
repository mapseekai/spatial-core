//! Cooperative cancellation and unified error classification across domains.

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

/// Domain-independent failure category for upper-layer orchestration and retry policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ErrorCategory {
    /// Invalid arguments, names, coordinates, or input values.
    InvalidInput,
    /// Unsupported driver, version, data type, or target capability.
    Unsupported,
    /// Existing output, layer, or schema contract conflict.
    Conflict,
    /// Required source or target layer/object was not found.
    NotFound,
    /// Caller-configured record, row, or byte budget was exceeded.
    ResourceLimit,
    /// Cooperative cancellation was observed.
    Cancelled,
    /// Final commit or completion state could not be confirmed; reconcile before retrying.
    UnknownOutcome,
    /// Resource close, rollback, or temporary cleanup failed or timed out.
    Cleanup,
    /// Native engine, I/O, pool, callback, or database execution failure.
    Execution,
}

/// Unified classification interface implemented by domain error types across this crate.
pub trait SpatialError: std::error::Error + Send + Sync {
    /// Coarse machine-readable failure category derived from the primary error.
    fn category(&self) -> ErrorCategory;

    /// Whether the primary failure was cooperative cancellation.
    fn is_cancelled(&self) -> bool {
        self.category() == ErrorCategory::Cancelled
    }

    /// Whether the final commit/completion outcome is unknown and requires reconciliation.
    fn is_outcome_unknown(&self) -> bool {
        self.category() == ErrorCategory::UnknownOutcome
    }

    /// Whether a close, rollback, or temporary-file cleanup failure was also observed.
    fn has_cleanup_failure(&self) -> bool {
        false
    }
}

#[derive(Debug, Clone, Default)]
pub struct Cancellation(Arc<CancellationState>);

#[derive(Debug, Default)]
struct CancellationState {
    cancelled: AtomicBool,
    changed: tokio::sync::Notify,
}

#[derive(Debug, thiserror::Error)]
#[error("processing cancelled")]
pub struct Cancelled;

impl SpatialError for Cancelled {
    fn category(&self) -> ErrorCategory {
        ErrorCategory::Cancelled
    }
}

#[derive(Debug, thiserror::Error)]
#[error("invalid processing input: {0}")]
pub struct InvalidInput(pub String);

impl SpatialError for InvalidInput {
    fn category(&self) -> ErrorCategory {
        ErrorCategory::InvalidInput
    }
}

impl Cancellation {
    pub fn cancel(&self) {
        self.0.cancelled.store(true, Ordering::Release);
        self.0.changed.notify_waiters();
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.cancelled.load(Ordering::Acquire)
    }

    pub async fn cancelled(&self) {
        let notified = self.0.changed.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if !self.is_cancelled() {
            notified.await;
        }
    }

    pub fn check(&self) -> Result<(), Cancelled> {
        if self.is_cancelled() {
            return Err(Cancelled);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancellation_is_shared_and_returns_a_typed_error() {
        let cancel = Cancellation::default();
        assert!(cancel.check().is_ok());
        cancel.clone().cancel();
        let error = cancel.check().unwrap_err();
        assert_eq!(error.category(), ErrorCategory::Cancelled);
        assert!(error.is_cancelled());
        assert!(!error.is_outcome_unknown());
        assert!(!error.has_cleanup_failure());
    }

    #[tokio::test]
    async fn cancellation_wakes_waiters_and_is_sticky() {
        let cancel = Cancellation::default();
        let waiter = cancel.clone();
        let task = tokio::spawn(async move { waiter.cancelled().await });
        tokio::task::yield_now().await;
        cancel.cancel();
        task.await.unwrap();
        cancel.cancelled().await;
    }
}
