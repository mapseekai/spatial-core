use std::{fmt, io};

/// Errors from the optional DuckDB backend.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    #[error("invalid DuckDB input: {0}")]
    InvalidInput(&'static str),
    #[error("unsupported DuckDB data: {0}")]
    Unsupported(&'static str),
    #[error("DuckDB {stage} failed")]
    Engine {
        stage: &'static str,
        /// Intentionally redacted from Display and Debug.
        diagnostic: Diagnostic,
    },
    #[error("DuckDB operation cancelled")]
    Cancelled,
    #[error("DuckDB output exceeds {0}")]
    LimitExceeded(&'static str),
    #[error("DuckDB batch consumer failed")]
    Consumer(#[source] Box<dyn std::error::Error + Send + Sync>),
    #[error("DuckDB filesystem operation failed: {0}")]
    Io(#[from] io::Error),
    #[error("DuckDB output was published, but temporary-file cleanup failed: {0}")]
    PublishedCleanup(io::Error),
    #[error("DuckDB temporary output cleanup failed: {cleanup}")]
    Cleanup {
        /// The operation error, if cleanup followed a failed operation.
        primary: Box<Error>,
        cleanup: io::Error,
    },
    #[error("DuckDB source registration rollback failed; session state is unconfirmed")]
    Rollback {
        primary: Box<Error>,
        rollback: Box<Error>,
    },
}

/// Native diagnostics may include SQL, data, paths and signed URLs.
/// Only expose them to a trusted operator; normal error formatting redacts them.
pub struct Diagnostic(String);

impl fmt::Debug for Diagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[redacted]")
    }
}

impl Error {
    pub(crate) fn engine(stage: &'static str, error: ::duckdb::Error) -> Self {
        Self::Engine {
            stage,
            diagnostic: Diagnostic(error.to_string()),
        }
    }

    /// Unredacted engine diagnostics, which may contain sensitive input.
    /// Credential setup errors never retain the native diagnostic.
    pub fn sensitive_diagnostic(&self) -> Option<&str> {
        match self {
            Self::Engine { diagnostic, .. } => Some(&diagnostic.0),
            Self::Cleanup { primary, .. } => primary.sensitive_diagnostic(),
            Self::Rollback { primary, .. } => primary.sensitive_diagnostic(),
            _ => None,
        }
    }

    pub(crate) fn secret() -> Self {
        Self::InvalidInput("secret configuration was rejected")
    }
}

pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn engine_errors_do_not_log_native_sql() {
        let error = Error::engine(
            "query",
            ::duckdb::Error::InvalidParameterName("private-token".into()),
        );
        assert!(!format!("{error:?} {error}").contains("private-token"));
        assert!(
            error
                .sensitive_diagnostic()
                .unwrap()
                .contains("private-token")
        );
        assert!(Error::secret().sensitive_diagnostic().is_none());
    }
}
