//! Optional, synchronous vector analysis with DuckDB Spatial.
//!
//! Enable the `duckdb` Cargo feature and provision matching signed `spatial` and
//! `httpfs` extensions before opening a session. Patched Spatial is provisioned
//! from a local repository and signature-checked against the caller's key.
//! Run sessions on a blocking thread; SQL is a trusted caller API, not a sandbox.
//! See `docs/DUCKDB.md` for deployment and data contracts.

mod error;
mod output;
mod query;
mod session;
mod source;

pub use ::duckdb::{arrow, types::Value};
pub use error::{Error, Result};
pub use output::FileReport;
pub use query::{Control, Query, QueryReport};
pub use session::{Capabilities, Driver, HttpSecret, S3Secret, Session, SessionConfig};
pub use source::{GeometryColumn, Reader, VectorMetadata, VectorSource};

fn literal(value: &str) -> Result<String> {
    if value.contains('\0') {
        return Err(Error::InvalidInput("SQL values cannot contain NUL"));
    }
    Ok(format!("'{}'", value.replace('\'', "''")))
}

fn identifier(value: &str) -> Result<String> {
    if value.is_empty() || value.contains('\0') {
        return Err(Error::InvalidInput("empty identifier or embedded NUL"));
    }
    Ok(format!("\"{}\"", value.replace('"', "\"\"")))
}

fn path_text(path: &std::path::Path) -> Result<&str> {
    path.to_str().ok_or(Error::InvalidInput(
        "DuckDB requires a UTF-8 path; lossy conversion is not allowed",
    ))
}

#[cfg(test)]
fn test_session(directory: &std::path::Path) -> Session {
    let extensions = std::env::var("SPATIAL_CORE_DUCKDB_EXTENSION_DIR")
        .expect("provision extensions and set SPATIAL_CORE_DUCKDB_EXTENSION_DIR");
    let mut config = SessionConfig::new(extensions, directory);
    config.spatial_public_key = Some(
        std::fs::read_to_string(
            std::env::var("SPATIAL_CORE_DUCKDB_SPATIAL_PUBLIC_KEY").expect(
                "set SPATIAL_CORE_DUCKDB_SPATIAL_PUBLIC_KEY to the trusted public key file",
            ),
        )
        .expect("read public key"),
    );
    Session::open(config)
        .unwrap_or_else(|error| panic!("{error}: {:?}", error.sensitive_diagnostic()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sql_quoting_preserves_values_and_rejects_nul() {
        assert_eq!(
            literal("a'; DROP TABLE x;--").unwrap(),
            "'a''; DROP TABLE x;--'"
        );
        assert_eq!(identifier("中文\"列").unwrap(), "\"中文\"\"列\"");
        assert!(literal("a\0b").is_err());
        assert!(identifier("").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_paths_are_rejected_without_lossy_conversion() {
        use std::os::unix::ffi::OsStringExt;
        let path =
            std::path::PathBuf::from(std::ffi::OsString::from_vec(b"file-\xff.parquet".to_vec()));
        assert!(matches!(path_text(&path), Err(Error::InvalidInput(_))));
    }
}
