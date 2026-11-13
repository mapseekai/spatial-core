use super::WriteStage;

/// A failure of input validation, reading, writing, or transaction completion.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum PostGisError {
    /// Invalid names, options, source CRS, or values.
    #[error("invalid input: {0}")]
    InvalidInput(String),
    /// An input or target outside the supported contract.
    #[error("unsupported input or target: {0}")]
    Unsupported(String),
    /// The existing table does not match the requested schema.
    #[error("target schema mismatch: {0}")]
    SchemaMismatch(String),
    /// The reader failed. The source chain retains the native error.
    #[error("source failed at record {record}")]
    Source {
        /// One-based record, or zero while opening/planning.
        record: u64,
        /// Original reader error; may contain sensitive source information.
        #[source]
        source: anyhow::Error,
    },
    /// PostgreSQL failure, including its original SQLSTATE.
    #[error("database failed during {stage:?}")]
    Database {
        /// Operation stage.
        stage: WriteStage,
        /// Original database error.
        #[source]
        source: tokio_postgres::Error,
    },
    /// Pool acquisition failed.
    #[error("database pool acquisition failed: {0}")]
    Pool(#[source] deadpool_postgres::PoolError),
    /// A record, queue, or record count exceeds the caller's budget.
    #[error("resource limit exceeded: {0}")]
    ResourceLimit(String),
    /// Cooperative cancellation before COMMIT was sent.
    #[error("write cancelled")]
    Cancelled,
    /// A blocking reader panicked or was otherwise lost.
    #[error("source worker failed: {0}")]
    Worker(#[source] tokio::task::JoinError),
    /// COMMIT was sent, but the server's result could not be confirmed.
    #[error("commit outcome unknown: {0}")]
    CommitOutcomeUnknown(#[source] tokio_postgres::Error),
    /// Cancellation delivery or rollback exceeded the cleanup deadline; the connection is discarded.
    #[error("database cleanup timed out")]
    CleanupTimeout,
    /// Cleanup failed after the original failure. The connection was discarded.
    #[error("{primary}; cleanup also failed: {cleanup}")]
    Cleanup {
        /// Original operation failure.
        #[source]
        primary: Box<PostGisError>,
        /// Cancellation/rollback failure.
        cleanup: Box<PostGisError>,
    },
}

/// Result returned by PostGIS operations.
pub type Result<T> = std::result::Result<T, PostGisError>;

impl PostGisError {
    /// The original error, before any cleanup failures were attached.
    pub fn primary(&self) -> &Self {
        match self {
            Self::Cleanup { primary, .. } => primary.primary(),
            other => other,
        }
    }

    pub(crate) fn database(stage: WriteStage, source: tokio_postgres::Error) -> Self {
        Self::Database { stage, source }
    }

    pub(crate) fn source(record: u64, source: impl Into<anyhow::Error>) -> Self {
        let source = source.into();
        if source.is::<Self>() {
            source.downcast::<Self>().expect("error type was checked")
        } else if source.is::<crate::control::Cancelled>() {
            Self::Cancelled
        } else if let Some(error) = source.downcast_ref::<crate::control::InvalidInput>() {
            Self::InvalidInput(error.0.clone())
        } else {
            Self::Source { record, source }
        }
    }

    pub(crate) fn with_cleanup(self, cleanup: Self) -> Self {
        Self::Cleanup {
            primary: Box::new(self),
            cleanup: Box::new(cleanup),
        }
    }
}
