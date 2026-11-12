//! Cooperative cancellation shared by async orchestration and GDAL workers.

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

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

#[derive(Debug, thiserror::Error)]
#[error("invalid processing input: {0}")]
pub struct InvalidInput(pub String);

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

    pub fn check(&self) -> anyhow::Result<()> {
        if self.is_cancelled() {
            return Err(Cancelled.into());
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
        assert!(cancel.check().unwrap_err().is::<Cancelled>());
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
