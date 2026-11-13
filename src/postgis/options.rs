use super::{PostGisError, Result};
use std::time::Duration;

/// Effect on the target table. Defaults to refusing an existing target.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum WriteMode {
    /// Create a new table; fail if it exists.
    #[default]
    Create,
    /// Drop/recreate in one transaction; do not preserve OID or grants.
    ReplaceTable,
    /// Keep the existing schema and constraints; truncate only this table.
    ReplaceRows,
}

/// Whether a successfully read, empty vector source may be committed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum EmptyInputPolicy {
    /// Fail without modifying committed data.
    #[default]
    Reject,
    /// Allow an empty source with a complete schema.
    Allow,
}

/// Bounds for encoded records and the producer queue, not process RSS/GDAL cache.
#[derive(Debug, Clone, Copy)]
pub struct WriteLimits {
    pub(crate) max_record_bytes: usize,
    pub(crate) max_queued_bytes: usize,
    pub(crate) max_records: u64,
}

impl Default for WriteLimits {
    fn default() -> Self {
        Self {
            max_record_bytes: 64 * 1024 * 1024,
            max_queued_bytes: 128 * 1024 * 1024,
            max_records: 100_000_000,
        }
    }
}

impl WriteLimits {
    /// Construct nonzero budgets. A record must fit in the queue budget.
    ///
    /// # Errors
    /// Rejects zero limits, records larger than the queue, or queues above u32::MAX.
    pub fn new(max_record_bytes: usize, max_queued_bytes: usize, max_records: u64) -> Result<Self> {
        if max_record_bytes == 0
            || max_record_bytes > max_queued_bytes
            || max_queued_bytes > u32::MAX as usize
            || max_queued_bytes > tokio::sync::Semaphore::MAX_PERMITS
            || max_records == 0
        {
            return Err(PostGisError::InvalidInput("invalid write budgets".into()));
        }
        Ok(Self {
            max_record_bytes,
            max_queued_bytes,
            max_records,
        })
    }

    /// Maximum encoded record bytes (also raster peak encoding budget).
    pub fn max_record_bytes(self) -> usize {
        self.max_record_bytes
    }
    /// Maximum encoded bytes held in the producer queue.
    pub fn max_queued_bytes(self) -> usize {
        self.max_queued_bytes
    }
    /// Maximum number of records in one write.
    pub fn max_records(self) -> u64 {
        self.max_records
    }
}

#[derive(Debug, Clone)]
pub(crate) struct TargetOptions {
    pub mode: WriteMode,
    pub create_schema: bool,
    pub lock_timeout: Duration,
}

impl Default for TargetOptions {
    fn default() -> Self {
        Self {
            mode: WriteMode::Create,
            create_schema: false,
            lock_timeout: Duration::from_secs(30),
        }
    }
}
