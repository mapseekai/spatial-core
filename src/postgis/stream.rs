use super::{PostGisError, Result, WriteLimits, connection::SridDefinition};
use crate::control::Cancellation;
use bytes::Bytes;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use tokio::{
    sync::{OwnedSemaphorePermit, Semaphore, mpsc, oneshot},
    task::JoinHandle,
};

pub(crate) struct CopyRow {
    pub bytes: Bytes,
    _permit: OwnedSemaphorePermit,
}

pub(crate) struct RowSender {
    sender: mpsc::Sender<CopyRow>,
    budget: Arc<Semaphore>,
    cancel: Cancellation,
    limits: WriteLimits,
    count: u64,
    srid: Option<oneshot::Receiver<SridDefinition>>,
    validated: Option<oneshot::Sender<()>>,
}

impl RowSender {
    pub fn validate_srid(&mut self) -> Result<()> {
        self.srid
            .take()
            .expect("SRID checked once")
            .blocking_recv()
            .map_err(|_| PostGisError::Cancelled)?
            .validate()?;
        self.validated
            .take()
            .expect("SRID checked once")
            .send(())
            .map_err(|_| PostGisError::Cancelled)
    }

    pub fn send(&mut self, row: Vec<u8>) -> Result<()> {
        if self.cancel.is_cancelled() {
            return Err(PostGisError::Cancelled);
        }
        if row.len() > self.limits.max_record_bytes || self.count >= self.limits.max_records {
            return Err(PostGisError::ResourceLimit(
                "record size or count exceeds write limits".into(),
            ));
        }
        let permit = futures::executor::block_on(async {
            tokio::select! {
                biased;
                _ = self.cancel.cancelled() => Err(PostGisError::Cancelled),
                permit = self.budget.clone().acquire_many_owned(row.len().max(1) as u32) => {
                    permit.map_err(|_| PostGisError::Cancelled)
                }
            }
        })?;
        self.sender
            .blocking_send(CopyRow {
                bytes: row.into(),
                _permit: permit,
            })
            .map_err(|_| PostGisError::Cancelled)?;
        self.count += 1;
        Ok(())
    }
    pub fn cancellation(&self) -> &Cancellation {
        &self.cancel
    }
}

/// One worker opens, plans and reads its dataset; channel EOF is never success.
pub(crate) struct SourceStream<M> {
    metadata: Option<oneshot::Receiver<M>>,
    pub rows: mpsc::Receiver<CopyRow>,
    worker: Option<JoinHandle<Result<u64>>>,
    cancel: Cancellation,
    srid: Option<oneshot::Sender<SridDefinition>>,
    validated: Option<oneshot::Receiver<()>>,
    pending: Arc<AtomicBool>,
}

impl<M: Send + 'static> SourceStream<M> {
    pub fn start(
        limits: WriteLimits,
        read: impl FnOnce(oneshot::Sender<M>, &mut RowSender) -> Result<()> + Send + 'static,
    ) -> Self {
        let (tx, rows) = mpsc::channel(32);
        let (metadata, rx) = oneshot::channel();
        let (srid, definition) = oneshot::channel();
        let (validated, checked) = oneshot::channel();
        let cancel = Cancellation::default();
        let pending = Arc::new(AtomicBool::new(true));
        let worker_pending = pending.clone();
        let mut sender = RowSender {
            sender: tx,
            budget: Arc::new(Semaphore::new(limits.max_queued_bytes)),
            cancel: cancel.clone(),
            limits,
            count: 0,
            srid: Some(definition),
            validated: Some(validated),
        };
        let worker = tokio::task::spawn_blocking(move || {
            // Only the worker or cancellation can claim startup; a cancelled queued reader never opens input.
            if !worker_pending.swap(false, Ordering::AcqRel) || sender.cancel.is_cancelled() {
                return Err(PostGisError::Cancelled);
            }
            read(metadata, &mut sender)?;
            Ok(sender.count)
        });
        Self {
            metadata: Some(rx),
            rows,
            worker: Some(worker),
            cancel,
            srid: Some(srid),
            validated: Some(checked),
            pending,
        }
    }

    pub async fn validate_srid(&mut self, definition: SridDefinition) -> Result<()> {
        // The existing worker checks CRS before producing rows; no second blocking task can starve.
        let sent = self
            .srid
            .take()
            .expect("SRID checked once")
            .send(definition);
        if sent.is_err()
            || self
                .validated
                .take()
                .expect("SRID checked once")
                .await
                .is_err()
        {
            self.finish().await?;
            return Err(PostGisError::InvalidInput(
                "source did not validate its SRID".into(),
            ));
        }
        Ok(())
    }

    pub async fn metadata(&mut self) -> Result<M> {
        match self.metadata.take().expect("metadata read once").await {
            Ok(metadata) => Ok(metadata),
            Err(_) => {
                self.finish().await?;
                Err(PostGisError::InvalidInput(
                    "source produced no schema".into(),
                ))
            }
        }
    }

    pub async fn finish(&mut self) -> Result<u64> {
        let result = self.worker.as_mut().expect("worker joined once").await;
        self.worker.take();
        result.map_err(PostGisError::Worker)?
    }

    pub async fn stop(&mut self) -> Result<()> {
        self.cancel.cancel();
        self.srid.take();
        self.rows.close();
        while self.rows.try_recv().is_ok() {}
        if let Some(worker) = self.worker.take() {
            let pending = self.pending.swap(false, Ordering::AcqRel);
            worker.abort();
            if pending {
                // Tokio cannot join an aborted queued blocking task until a thread becomes available.
                return Ok(());
            }
            match worker.await {
                Ok(Ok(_) | Err(PostGisError::Cancelled)) => (),
                Ok(Err(error)) => return Err(error),
                Err(error) if error.is_cancelled() => (),
                Err(error) => return Err(PostGisError::Worker(error)),
            }
        }
        Ok(())
    }
}

impl<M> Drop for SourceStream<M> {
    fn drop(&mut self) {
        self.cancel.cancel();
        self.pending.store(false, Ordering::Release);
        if let Some(worker) = &self.worker {
            worker.abort();
        }
        self.srid.take();
        self.rows.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queued_sources_cancel_without_waiting_for_unrelated_blocking_work() -> anyhow::Result<()> {
        tokio::runtime::Builder::new_current_thread()
            .max_blocking_threads(1)
            .enable_all()
            .build()?
            .block_on(async {
                for stop in [true, false] {
                    let (started, ready) = oneshot::channel();
                    let (release, blocked) = std::sync::mpsc::channel();
                    let blocker = tokio::task::spawn_blocking(move || {
                        let _ = started.send(());
                        blocked.recv().unwrap();
                    });
                    ready.await?;
                    let ran = Arc::new(AtomicBool::new(false));
                    let reader_ran = ran.clone();
                    let mut stream =
                        SourceStream::start(WriteLimits::default(), move |metadata, _| {
                            reader_ran.store(true, Ordering::Release);
                            metadata.send(()).map_err(|_| PostGisError::Cancelled)
                        });
                    let result = if stop {
                        tokio::time::timeout(std::time::Duration::from_secs(1), stream.stop()).await
                    } else {
                        drop(stream);
                        Ok(Ok(()))
                    };
                    // Always free the test's thread before asserting, including on a regression failure.
                    release.send(())?;
                    blocker.await?;
                    tokio::task::spawn_blocking(|| ()).await?;
                    result??;
                    assert!(!ran.load(Ordering::Acquire));
                }
                Ok(())
            })
    }

    #[tokio::test]
    async fn stopping_a_source_releases_a_worker_waiting_for_srid_validation() -> Result<()> {
        let mut stream = SourceStream::start(WriteLimits::default(), |metadata, sender| {
            metadata.send(()).map_err(|_| PostGisError::Cancelled)?;
            sender.validate_srid()
        });
        stream.metadata().await?;
        tokio::time::timeout(std::time::Duration::from_secs(2), stream.stop())
            .await
            .expect("preflight worker exits")?;
        Ok(())
    }

    #[tokio::test]
    async fn dropping_a_stream_wakes_a_worker_waiting_on_bytes() -> Result<()> {
        let (finished, done) = oneshot::channel();
        let mut stream =
            SourceStream::start(WriteLimits::new(8, 8, 10)?, move |metadata, sender| {
                metadata.send(()).map_err(|_| PostGisError::Cancelled)?;
                let result = (|| {
                    sender.send(vec![1; 8])?;
                    sender.send(vec![2; 8])
                })();
                let _ = finished.send(());
                result
            });
        stream.metadata().await?;
        let row = stream.rows.recv().await.expect("first row");
        // Keep its byte permit: cancellation, not released capacity, must unblock the worker.
        drop(stream);
        tokio::time::timeout(std::time::Duration::from_secs(2), done)
            .await
            .expect("worker exits")
            .expect("worker signal");
        drop(row);
        Ok(())
    }

    #[tokio::test]
    async fn worker_error_and_panic_are_not_successful_eof() -> Result<()> {
        let mut stream = SourceStream::start(WriteLimits::default(), |metadata, sender| {
            metadata.send(()).map_err(|_| PostGisError::Cancelled)?;
            sender.send(vec![1])?;
            Err(PostGisError::InvalidInput(
                "reader failed after one row".into(),
            ))
        });
        stream.metadata().await?;
        assert!(stream.rows.recv().await.is_some());
        assert!(stream.rows.recv().await.is_none());
        assert!(matches!(
            stream.finish().await,
            Err(PostGisError::InvalidInput(_))
        ));
        let mut stream =
            SourceStream::<()>::start(WriteLimits::default(), |_, _| panic!("reader panic"));
        assert!(matches!(
            stream.metadata().await,
            Err(PostGisError::Worker(_))
        ));
        Ok(())
    }

    #[tokio::test]
    async fn record_count_limit_fails_the_entire_stream() -> Result<()> {
        let mut stream = SourceStream::start(WriteLimits::new(1, 8, 2)?, |metadata, sender| {
            metadata.send(()).map_err(|_| PostGisError::Cancelled)?;
            for _ in 0..3 {
                sender.send(vec![1])?;
            }
            Ok(())
        });
        stream.metadata().await?;
        let mut count = 0;
        while stream.rows.recv().await.is_some() {
            count += 1;
        }
        assert_eq!(count, 2);
        assert!(matches!(
            stream.finish().await,
            Err(PostGisError::ResourceLimit(_))
        ));
        Ok(())
    }
}
