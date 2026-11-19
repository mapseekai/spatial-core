use std::path::PathBuf;

use ::duckdb::{Config, Connection};

use super::{Error, Result, literal, path_text};

unsafe extern "C" {
    fn spatial_core_duckdb_debug_allocator() -> bool;
}

/// Explicit native resources and extension locations for one isolated engine.
#[derive(Debug, Clone)]
pub struct SessionConfig {
    pub extension_directory: PathBuf,
    /// Trust this RSA-2048 public key for the patched Spatial repository under
    /// `extension_directory`. None loads the official signed extension on
    /// Release DuckDB hosts; Debug hosts require the patched allocator contract.
    pub spatial_public_key: Option<String>,
    pub temp_directory: PathBuf,
    /// DuckDB buffer-manager limit, not a hard process RSS limit.
    pub memory_limit_bytes: u64,
    pub temp_limit_bytes: u64,
    pub threads: u32,
    pub http_timeout_seconds: u32,
    pub http_retries: u32,
}

impl SessionConfig {
    #[must_use]
    pub fn new(
        extension_directory: impl Into<PathBuf>,
        temp_directory: impl Into<PathBuf>,
    ) -> Self {
        Self {
            extension_directory: extension_directory.into(),
            spatial_public_key: None,
            temp_directory: temp_directory.into(),
            memory_limit_bytes: 512 * 1024 * 1024,
            temp_limit_bytes: 1024 * 1024 * 1024,
            threads: 2,
            http_timeout_seconds: 30,
            http_retries: 2,
        }
    }
}

/// Explicit S3 credentials. No environment or user credential chain is consulted.
/// Scopes select credentials; they do not restrict which resources SQL can read.
pub struct S3Secret {
    pub scope: String,
    pub key_id: String,
    pub secret: String,
    /// Currently rejected: the pinned httpfs forwards this header on redirects.
    pub session_token: Option<String>,
    pub region: String,
    /// Host and optional port, without a URL scheme.
    pub endpoint: Option<String>,
    pub path_style: bool,
    /// Disable only for an explicitly trusted HTTP endpoint (e.g. a local fixture).
    pub use_ssl: bool,
}

/// One Authorization header for an HTTP(S) URL prefix. Custom headers are rejected
/// because the pinned httpfs forwards them on cross-origin redirects.
pub struct HttpSecret {
    pub scope: String,
    pub headers: Vec<(String, String)>,
}

#[derive(Debug, Clone)]
pub struct Driver {
    pub name: String,
    pub can_read: bool,
    pub can_create: bool,
    pub can_copy: bool,
}

#[derive(Debug, Clone)]
pub struct Capabilities {
    pub engine_version: String,
    pub spatial_version: String,
    pub httpfs_version: String,
    pub gdal_read_supported: bool,
    pub drivers: Vec<Driver>,
}

/// One in-memory DuckDB instance and connection. Move to a blocking worker;
/// do not share a session concurrently. Results borrow it exclusively.
pub struct Session {
    // Close the engine before deleting its spill and secret directories.
    pub(crate) connection: Connection,
    pub(crate) gdal_read_supported: bool,
    scratch: tempfile::TempDir,
    next_secret: u64,
}

impl Session {
    pub fn open(config: SessionConfig) -> Result<Self> {
        if config.threads == 0 || config.memory_limit_bytes == 0 || config.http_timeout_seconds == 0
        {
            return Err(Error::InvalidInput(
                "threads, memory and HTTP timeout must be positive",
            ));
        }
        let extensions = config.extension_directory.canonicalize()?;
        let scratch = tempfile::Builder::new()
            .prefix("spatial-duckdb-")
            .tempdir_in(&config.temp_directory)?;
        let opened = (|| -> Result<(Connection, bool)> {
            let secrets = scratch.path().join("secrets");
            let spill = scratch.path().join("spill");
            let repositories = scratch.path().join("repositories");
            let secrets = path_text(&secrets)?;
            let spill = path_text(&spill)?;
            let installed = path_text(&extensions)?;
            let repositories = path_text(&repositories)?;
            let settings = (|| -> ::duckdb::Result<Config> {
                Config::default()
                    .enable_autoload_extension(false)?
                    .threads(i64::from(config.threads))?
                    .max_memory(&format!("{}B", config.memory_limit_bytes))?
                    .with(
                        "max_temp_directory_size",
                        format!("{}B", config.temp_limit_bytes),
                    )?
                    .with("allow_persistent_secrets", "false")?
                    .with("extension_directory", installed)?
                    .with("extension_repository_directory", repositories)?
                    .with(
                        "allow_extension_repositories",
                        if config.spatial_public_key.is_some() {
                            "allowed"
                        } else {
                            "forbidden"
                        },
                    )?
                    .with("secret_directory", secrets)?
                    .with("temp_directory", spill)
            })()
            .map_err(|e| Error::engine("configuration", e))?;
            let connection = Connection::open_in_memory_with_flags(settings)
                .map_err(|e| Error::engine("open", e))?;
            for extension in ["spatial", "httpfs"] {
                if extension == "spatial"
                    && let Some(public_key) = &config.spatial_public_key
                {
                    // A stable installed path lets dlopen reuse the library across sessions.
                    connection.execute_batch(&format!(
                        "CREATE EXTENSION REPOSITORY spatial_core WITH PREFIX {} USING PUBLIC KEY {}; LOAD spatial FROM spatial_core;",
                        literal(path_text(&extensions)?)?, literal(public_key)?,
                    )).map_err(|e| Error::engine("signed local Spatial load (run duckdb_install first)", e))?;
                    continue;
                }
                let file = extensions.join(format!("{extension}.duckdb_extension"));
                if !file.is_file() {
                    return Err(Error::InvalidInput(
                        "preinstalled spatial/httpfs extension is missing",
                    ));
                }
                connection
                    .execute_batch(&format!("LOAD {}", literal(path_text(&file)?)?))
                    .map_err(|e| {
                        Error::engine("extension load (check engine version and platform)", e)
                    })?;
            }
            check_allocator_contract(&connection)?;
            connection.execute_batch(&format!(
            "SET geometry_always_xy = true; SET http_timeout = {}; SET http_retries = {}; SET enable_server_cert_verification = true;",
            config.http_timeout_seconds, config.http_retries,
        )).map_err(|e| Error::engine("session settings", e))?;
            let has_contract: bool = connection.query_row(
                "SELECT count(*) > 0 FROM duckdb_functions() WHERE function_name = 'spatial_core_gdal_read_contract'",
                [], |row| row.get(0),
            ).map_err(|e| Error::engine("GDAL read contract discovery", e))?;
            let gdal_read_supported = has_contract
                && connection
                    .query_row("SELECT spatial_core_gdal_read_contract() = 1", [], |row| {
                        row.get::<_, bool>(0)
                    })
                    .map_err(|e| Error::engine("GDAL read contract", e))?;
            if config.spatial_public_key.is_some() && !gdal_read_supported {
                return Err(Error::Unsupported(
                    "patched Spatial GDAL read contract is missing",
                ));
            }
            Ok((connection, gdal_read_supported))
        })();
        match opened {
            Ok((connection, gdal_read_supported)) => Ok(Self {
                connection,
                gdal_read_supported,
                scratch,
                next_secret: 0,
            }),
            Err(error) => cleanup(Err(error), scratch),
        }
    }

    pub fn capabilities(&mut self) -> Result<Capabilities> {
        let engine_version = self
            .connection
            .version()
            .map_err(|e| Error::engine("version", e))?;
        let version = |name| {
            self.connection.query_row(
            "SELECT extension_version FROM duckdb_extensions() WHERE extension_name = ? AND loaded",
            [name], |row| row.get::<_, String>(0),
        ).map_err(|e| Error::engine("extension version", e))
        };
        let spatial_version = version("spatial")?;
        let httpfs_version = version("httpfs")?;
        let mut statement = self
            .connection
            .prepare("SELECT short_name, can_open, can_create, can_copy FROM ST_Drivers()")
            .map_err(|e| Error::engine("driver discovery", e))?;
        let drivers = statement
            .query_map([], |row| {
                Ok(Driver {
                    name: row.get(0)?,
                    can_read: row.get(1)?,
                    can_create: row.get(2)?,
                    can_copy: row.get(3)?,
                })
            })
            .map_err(|e| Error::engine("driver discovery", e))?
            .collect::<::duckdb::Result<Vec<_>>>()
            .map_err(|e| Error::engine("driver discovery", e))?;
        Ok(Capabilities {
            engine_version,
            spatial_version,
            httpfs_version,
            gdal_read_supported: self.gdal_read_supported,
            drivers,
        })
    }

    /// Add a temporary, connection-scoped secret. Native errors are discarded
    /// because CREATE SECRET diagnostics can echo the credential SQL.
    pub fn add_s3_secret(&mut self, secret: S3Secret) -> Result<()> {
        if secret.session_token.is_some() {
            return Err(Error::Unsupported(
                "S3 session tokens require a redirect policy unavailable in this httpfs version",
            ));
        }
        if !secret.scope.starts_with("s3://")
            || secret.scope.len() <= 5
            || secret.key_id.is_empty()
            || secret.secret.is_empty()
        {
            return Err(Error::InvalidInput(
                "S3 credentials require a nonempty s3:// scope and keys",
            ));
        }
        let mut fields = vec![
            "TYPE s3".into(),
            format!("SCOPE {}", literal(&credential_scope(&secret.scope)?)?),
            format!("KEY_ID {}", literal(&secret.key_id)?),
            format!("SECRET {}", literal(&secret.secret)?),
            format!("REGION {}", literal(&secret.region)?),
            format!("USE_SSL {}", secret.use_ssl),
            format!(
                "URL_STYLE {}",
                if secret.path_style {
                    "'path'"
                } else {
                    "'vhost'"
                }
            ),
        ];
        if let Some(endpoint) = secret.endpoint {
            fields.push(format!("ENDPOINT {}", literal(&endpoint)?));
        }
        self.create_secret(fields.join(", "))
    }

    pub fn add_http_secret(&mut self, secret: HttpSecret) -> Result<()> {
        if !(secret.scope.starts_with("https://") || secret.scope.starts_with("http://"))
            || secret.headers.is_empty()
        {
            return Err(Error::InvalidInput(
                "HTTP credentials require a URL scope and headers",
            ));
        }
        let [(key, value)] = secret.headers.as_slice() else {
            return Err(Error::Unsupported(
                "only one Authorization header is supported",
            ));
        };
        if !key.eq_ignore_ascii_case("Authorization") {
            return Err(Error::Unsupported(
                "custom HTTP headers require a redirect policy unavailable in this httpfs version",
            ));
        }
        if value.is_empty() || value.contains(['\r', '\n']) {
            return Err(Error::InvalidInput("invalid HTTP header"));
        }
        self.create_secret(format!(
            "TYPE http, SCOPE {}, EXTRA_HTTP_HEADERS MAP(['Authorization'], [{}])",
            literal(&credential_scope(&secret.scope)?)?,
            literal(value)?
        ))
    }

    fn create_secret(&mut self, fields: String) -> Result<()> {
        let sql = format!(
            "CREATE SECRET spatial_core_{} IN connection ({fields})",
            self.next_secret
        );
        self.connection
            .execute_batch(&sql)
            .map_err(|_| Error::secret())?;
        self.next_secret += 1;
        Ok(())
    }

    /// Close explicitly to observe temporary-directory cleanup failures.
    /// Dropping a session also cleans up, but cannot report filesystem errors.
    pub fn close(self) -> Result<()> {
        let Self {
            connection,
            scratch,
            ..
        } = self;
        let result = connection.close().map_err(|(connection, e)| {
            drop(connection);
            Error::engine("close", e)
        });
        cleanup(result, scratch)
    }
}

fn check_allocator_contract(connection: &Connection) -> Result<()> {
    // SAFETY: The open connection initialized the allocator; the probe uses the linked core's headers.
    if !unsafe { spatial_core_duckdb_debug_allocator() } {
        return Ok(());
    }
    let has_contract: bool = connection
        .query_row(
            "SELECT count(*) > 0 FROM duckdb_functions() WHERE function_name = 'spatial_core_allocator_contract'",
            [],
            |row| row.get(0),
        )
        .map_err(|e| Error::engine("allocator contract discovery", e))?;
    if !has_contract
        || !connection
            .query_row("SELECT spatial_core_allocator_contract() = 1", [], |row| {
                row.get::<_, bool>(0)
            })
            .map_err(|e| Error::engine("allocator contract", e))?
    {
        return Err(Error::Unsupported(
            "Debug DuckDB requires Spatial with allocator contract 1; rebuild and provision the patched extension",
        ));
    }
    Ok(())
}

fn credential_scope(scope: &str) -> Result<String> {
    let (_, tail) = scope
        .split_once("://")
        .ok_or(Error::InvalidInput("invalid credential scope"))?;
    let authority = tail.split('/').next().unwrap_or_default();
    if authority.is_empty()
        || authority.starts_with(':')
        || !authority
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b".-_:[]".contains(&c))
    {
        return Err(Error::InvalidInput(
            "credential scopes require an explicit host or bucket without userinfo",
        ));
    }
    // DuckDB matches raw prefixes: terminate the authority before selecting credentials.
    Ok(if tail.contains('/') {
        scope.to_owned()
    } else {
        format!("{scope}/")
    })
}

pub(crate) fn cleanup<T>(result: Result<T>, directory: tempfile::TempDir) -> Result<T> {
    match (result, directory.close()) {
        (result, Ok(())) => result,
        (Ok(_), Err(e)) => Err(Error::Io(e)),
        (Err(primary), Err(cleanup)) => Err(Error::Cleanup {
            primary: Box::new(primary),
            cleanup,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credential_scope_preserves_authority_boundaries() {
        for (scope, expected) in [
            ("http://127.0.0.1:5147", "http://127.0.0.1:5147/"),
            ("https://example.test", "https://example.test/"),
            ("s3://bucket", "s3://bucket/"),
            ("https://[::1]:123/prefix", "https://[::1]:123/prefix"),
            ("s3://bucket/prefix", "s3://bucket/prefix"),
        ] {
            assert_eq!(credential_scope(scope).unwrap(), expected);
        }
        for scope in [
            "http://",
            "http:///path",
            "http://:123/",
            "http://user@example.test/",
            "http://example.test?query",
            "http://example.test#fragment",
            "http://example.test\\path",
            "http://bad host/",
            "s3:///path",
        ] {
            assert!(credential_scope(scope).is_err(), "{scope}");
        }
    }

    #[test]
    #[ignore = "requires version-matched Spatial/httpfs extensions"]
    fn credentials_do_not_match_another_authority_prefix() {
        let directory = tempfile::tempdir().unwrap();
        let mut session = super::super::test_session(directory.path());
        session
            .add_http_secret(HttpSecret {
                scope: "http://127.0.0.1:5147".into(),
                headers: vec![("Authorization".into(), "Bearer fixture".into())],
            })
            .unwrap();
        session
            .add_s3_secret(S3Secret {
                scope: "s3://bucket".into(),
                key_id: "fixture".into(),
                secret: "fixture".into(),
                session_token: None,
                region: "us-east-1".into(),
                endpoint: None,
                path_style: false,
                use_ssl: true,
            })
            .unwrap();
        for (uri, kind, expected) in [
            ("http://127.0.0.1:5147/file", "http", 1),
            ("http://127.0.0.1:51478/file", "http", 0),
            ("s3://bucket/file", "s3", 1),
            ("s3://bucket-extra/file", "s3", 0),
        ] {
            let found: u64 = session
                .connection
                .query_row(
                    "SELECT count(*) FROM which_secret(?, ?)",
                    [uri, kind],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(found, expected, "{uri}");
        }
        session.close().unwrap();
    }

    #[test]
    fn invalid_resources_fail_before_loading_extensions() {
        let directory = tempfile::tempdir().unwrap();
        let mut config = SessionConfig::new(directory.path(), directory.path());
        config.threads = 0;
        assert!(matches!(Session::open(config), Err(Error::InvalidInput(_))));
        let config = SessionConfig::new(directory.path(), directory.path());
        assert!(matches!(Session::open(config), Err(Error::InvalidInput(_))));
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
    }

    #[test]
    fn debug_allocator_rejects_missing_or_incompatible_spatial_contract() {
        let connection = Connection::open_in_memory().unwrap();
        // SAFETY: This connection initialized the linked core's allocator.
        let debug_allocator = unsafe { spatial_core_duckdb_debug_allocator() };
        if std::env::var("SPATIAL_CORE_DUCKDB_EXPECT_DEBUG").as_deref() == Ok("1") {
            assert!(debug_allocator, "CI must exercise the native Debug allocator");
        }
        for contract in [None, Some(0), Some(2), Some(1)] {
            if let Some(version) = contract {
                connection
                    .execute_batch(&format!(
                        "CREATE OR REPLACE MACRO spatial_core_allocator_contract() AS {version}"
                    ))
                    .unwrap();
            }
            let result = check_allocator_contract(&connection);
            if debug_allocator && contract != Some(1) {
                assert!(matches!(result, Err(Error::Unsupported(_))));
            } else {
                result.unwrap();
            }
        }
    }

    #[test]
    #[ignore = "requires version-matched Spatial/httpfs extensions"]
    fn session_is_explicit_and_cleans_its_own_files() {
        let directory = tempfile::tempdir().unwrap();
        let mut session = super::super::test_session(directory.path());
        let capabilities = session.capabilities().unwrap();
        assert_eq!(capabilities.engine_version, "v2.0.0");
        assert!(capabilities.gdal_read_supported);
        assert!(
            capabilities
                .drivers
                .iter()
                .any(|driver| driver.name == "FlatGeobuf" && driver.can_read)
        );
        for (setting, expected) in [
            ("autoload_known_extensions", "false"),
            ("autoinstall_known_extensions", "false"),
            ("allow_persistent_secrets", "false"),
            ("allow_unsigned_extensions", "false"),
            ("geometry_always_xy", "true"),
            ("enable_server_cert_verification", "true"),
        ] {
            let value: String = session
                .connection
                .query_row(
                    "SELECT value FROM duckdb_settings() WHERE name = ?",
                    [setting],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(value.to_ascii_lowercase(), expected);
        }
        let scratch = session.scratch.path().to_path_buf();
        assert!(scratch.exists());
        session.close().unwrap();
        assert!(!scratch.exists());
    }

    #[test]
    #[ignore = "requires the signed patched Spatial extension"]
    fn tampered_spatial_signature_is_rejected_and_scratch_is_cleaned() {
        let directory = tempfile::tempdir().unwrap();
        let repository = tempfile::tempdir().unwrap();
        let session = super::super::test_session(directory.path());
        let platform: String = session
            .connection
            .query_row("PRAGMA platform", [], |row| row.get(0))
            .unwrap();
        session.close().unwrap();
        let relative = PathBuf::from("v2.0.0")
            .join(platform)
            .join("repositories/spatial_core")
            .join("spatial.duckdb_extension");
        let extensions = PathBuf::from(std::env::var("SPATIAL_CORE_DUCKDB_EXTENSION_DIR").unwrap());
        let mut bytes = std::fs::read(extensions.join(&relative)).unwrap();
        *bytes.last_mut().unwrap() ^= 1;
        let target = repository.path().join(relative);
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::write(&target, bytes).unwrap();
        std::fs::copy(
            extensions
                .join(target.strip_prefix(repository.path()).unwrap())
                .with_extension("duckdb_extension.info"),
            target.with_extension("duckdb_extension.info"),
        )
        .unwrap();
        let mut config = SessionConfig::new(repository.path(), directory.path());
        config.spatial_public_key = Some(
            std::fs::read_to_string(
                std::env::var("SPATIAL_CORE_DUCKDB_SPATIAL_PUBLIC_KEY").unwrap(),
            )
            .unwrap(),
        );
        let error = Session::open(config)
            .err()
            .expect("tampered extension was loaded");
        assert!(
            error.sensitive_diagnostic().unwrap().contains("signature"),
            "{:?}",
            error.sensitive_diagnostic()
        );
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
    }

    #[test]
    #[ignore = "requires version-matched Spatial/httpfs extensions"]
    fn unsafe_credential_headers_are_rejected_before_registration() {
        let directory = tempfile::tempdir().unwrap();
        let mut session = super::super::test_session(directory.path());
        for headers in [
            vec![("X-Token".into(), "fixture".into())],
            vec![
                ("Authorization".into(), "fixture".into()),
                ("Authorization".into(), "second".into()),
            ],
            vec![("Authorization".into(), "fixture\r\nX-Token: second".into())],
        ] {
            assert!(
                session
                    .add_http_secret(HttpSecret {
                        scope: "https://example.test/".into(),
                        headers
                    })
                    .is_err()
            );
        }
        for token in ["", "fixture-token"] {
            assert!(matches!(
                session.add_s3_secret(S3Secret {
                    scope: "s3://bucket/".into(),
                    key_id: "fixture".into(),
                    secret: "fixture".into(),
                    session_token: Some(token.into()),
                    region: "us-east-1".into(),
                    endpoint: None,
                    path_style: false,
                    use_ssl: true,
                }),
                Err(Error::Unsupported(_))
            ));
        }
        assert_eq!(session.next_secret, 0);
        session.close().unwrap();
    }
}
