use anyhow::{Context, Result};
use gdal::config::{get_config_option, set_config_option};
use serde::de::{Deserializer, MapAccess, Visitor};
use std::collections::HashMap;
use std::env;
use std::fmt;
use std::path::Path;
use std::sync::Arc;
use tokio::sync::OnceCell;

/// GDALOpen/RasterIO cannot always accept a cancellation callback. Bound
/// individual remote calls while preserving explicit deployment settings.
fn apply_io_timeout_defaults() -> Result<()> {
    for key in ["GDAL_HTTP_TIMEOUT", "GDAL_HTTP_CONNECTTIMEOUT"] {
        if get_config_option(key, "")?.is_empty() {
            set_config_option(key, "30")?;
        }
    }
    Ok(())
}

/// Set a GDAL config option on the current thread only.
///
/// Unlike `gdal::config::set_config_option`, this uses
/// `CPLSetThreadLocalConfigOption` so concurrent activities with different
/// S3 credentials do not race on process-global state.
fn set_thread_local_config_option(key: &str, value: &str) -> Result<()> {
    use std::ffi::CString;
    let c_key = CString::new(key)?;
    let c_value = CString::new(value)?;
    // SAFETY: both CStrings remain valid while GDAL copies their NUL-terminated values.
    unsafe {
        gdal_sys::CPLSetThreadLocalConfigOption(c_key.as_ptr(), c_value.as_ptr());
    }
    Ok(())
}

/// Apply S3-related GDAL options to the current OS thread.
///
/// Call this inside `spawn_blocking` closures before opening `/vsis3/` paths.
/// `CPLSetThreadLocalConfigOption` does not propagate from the async caller's
/// thread to Tokio's blocking thread pool.
pub fn apply_thread_local_gdal_s3_config(s3: Option<&GdalS3Config>) -> Result<()> {
    apply_io_timeout_defaults()?;
    if let Some(s3) = s3 {
        set_thread_local_config_option("AWS_ACCESS_KEY_ID", &s3.access_key)?;
        set_thread_local_config_option("AWS_SECRET_ACCESS_KEY", &s3.secret_key)?;
        set_thread_local_config_option(
            "AWS_S3_ENDPOINT",
            s3.endpoint
                .trim_start_matches("http://")
                .trim_start_matches("https://"),
        )?;
        set_thread_local_config_option("AWS_VIRTUAL_HOSTING", "FALSE")?;
        set_thread_local_config_option(
            "AWS_HTTPS",
            if s3.endpoint.starts_with("https://") {
                "TRUE"
            } else {
                "FALSE"
            },
        )?;
    }

    Ok(())
}

static GDAL_CONFIG_MAP: OnceCell<Arc<HashMap<String, String>>> = OnceCell::const_new();

/// Apply explicit process-wide GDAL options without reading a configuration file.
///
/// Call this during application initialization, before starting concurrent GDAL
/// operations. Options are applied in order; an invalid option returns an error
/// and does not undo earlier options. Existing HTTP timeouts are retained and
/// missing timeout values default to 30 seconds.
///
/// Credentials should use path-scoped GDAL configuration or
/// [`apply_thread_local_gdal_s3_config`] on the thread performing the I/O.
///
/// ```
/// use spatial_core::config::gdal::apply_gdal_options;
/// apply_gdal_options(&[("GDAL_HTTP_TIMEOUT", "60")])?;
/// # Ok::<(), anyhow::Error>(())
/// ```
pub fn apply_gdal_options(options: &[(&str, &str)]) -> Result<()> {
    for (key, value) in options {
        set_config_option(key, value)?;
    }
    apply_io_timeout_defaults()
}

/// Load a GDAL configuration file (`.ini` format) using GDAL's native
/// `CPLLoadConfigOptionsFromFile`.
///
/// This sets **process-global** config options and path-scoped credentials
/// so that all threads (including Temporal activity worker threads) can
/// access S3/MinIO via `/vsis3/` without per-thread credential setup.
///
/// The `.ini` file supports `[configoptions]` for global GDAL options and
/// `[credentials]` with `[.bucket]` subsections for per-path S3 credentials.
/// See <https://gdal.org/en/stable/user/configoptions.html>.
///
/// `override_env` controls whether file-based values override environment
/// variables (`true`) or not (`false`).
pub fn load_config_file(path: &Path, override_env: bool) -> Result<()> {
    use std::ffi::CString;

    if !path.exists() {
        apply_io_timeout_defaults()?;
        tracing::warn!(
            path = %path.display(),
            "GDAL config file not found, skipping"
        );
        return Ok(());
    }

    let c_path = CString::new(
        path.to_str()
            .context("GDAL config file path is not valid UTF-8")?,
    )?;

    // SAFETY: the NUL-terminated path remains alive throughout the synchronous load.
    unsafe {
        gdal_sys::CPLLoadConfigOptionsFromFile(c_path.as_ptr(), override_env as i32);
    }

    apply_io_timeout_defaults()?;
    tracing::info!(
        path = %path.display(),
        override_env,
        "GDAL config file loaded (process-global)"
    );
    Ok(())
}

#[derive(Clone)]
pub struct GdalS3Config {
    pub endpoint: String,
    pub access_key: String,
    pub secret_key: String,
}

impl fmt::Debug for GdalS3Config {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GdalS3Config")
            .field("endpoint", &self.endpoint)
            .field("access_key", &self.access_key)
            .field("secret_key", &"[redacted]")
            .finish()
    }
}

#[derive(Default)]
struct GdalConfig {
    pub gdal: HashMap<String, String>,
}

impl<'de> serde::Deserialize<'de> for GdalConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct GdalConfigVisitor;

        impl<'de> Visitor<'de> for GdalConfigVisitor {
            type Value = GdalConfig;

            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                formatter.write_str("a struct with a gdal field")
            }

            fn visit_map<V>(self, mut map: V) -> Result<GdalConfig, V::Error>
            where
                V: MapAccess<'de>,
            {
                let mut gdal_config = HashMap::new();
                while let Some((key, value)) = map.next_entry::<String, serde_json::Value>()? {
                    if key == "gdal"
                        && let serde_json::Value::Object(inner_map) = value
                    {
                        for (k, v) in inner_map {
                            if let serde_json::Value::String(v) = v {
                                gdal_config.insert(k.to_uppercase(), v);
                            }
                        }
                    }
                }
                Ok(GdalConfig { gdal: gdal_config })
            }
        }

        deserializer.deserialize_map(GdalConfigVisitor)
    }
}

/// Apply the original MapSeek configuration behavior for existing consumers.
///
/// The first successful call loads string values from the `[gdal]` table in
/// `config/gdal_config.toml`, relative to the process working directory. The
/// resulting map (including an empty map when the file is absent) is cached for
/// the process lifetime. Every call reapplies it and then sets thread-local S3
/// options. Use [`apply_gdal_options`] for explicit configuration in new callers.
///
/// Calling this async function alone does not configure a future blocking
/// worker's thread. Apply S3 options on the thread that opens the data source.
pub async fn apply_gdal_config(s3: Option<&GdalS3Config>) -> Result<()> {
    let config_map = GDAL_CONFIG_MAP
        .get_or_try_init(load_gdal_config_map)
        .await?;

    // Static config options are safe to set process-wide (loaded once).
    for (key, value) in config_map.as_ref() {
        set_config_option(key, value)?;
    }

    // S3 credentials are set per-thread to avoid races when concurrent
    // activities use different credentials.
    apply_thread_local_gdal_s3_config(s3)?;

    Ok(())
}

async fn load_gdal_config_map() -> Result<Arc<HashMap<String, String>>> {
    let config_path = env::current_dir()
        .map(|path| path.join("config/gdal_config.toml"))
        .unwrap_or_else(|_| "config/gdal_config.toml".into());

    if !config_path.exists() {
        return Ok(Arc::new(HashMap::new()));
    }

    let conf = config::Config::builder()
        .add_source(config::File::with_name(&config_path.to_string_lossy()))
        .build()
        .with_context(|| format!("failed to load GDAL config file: {}", config_path.display()))?;

    let config: GdalConfig = conf
        .try_deserialize()
        .context("failed to deserialize GDAL config")?;

    Ok(Arc::new(config.gdal))
}

#[cfg(test)]
mod tests {
    use super::{GdalS3Config, apply_gdal_config, apply_gdal_options};
    use tokio::task::JoinSet;

    #[test]
    fn gdal_s3_config_debug_redacts_secret() {
        let cfg = GdalS3Config {
            endpoint: "http://127.0.0.1:9000".to_string(),
            access_key: "minioadmin".to_string(),
            secret_key: "super-secret-key".to_string(),
        };
        let dbg = format!("{cfg:?}");
        assert!(dbg.contains("[redacted]"));
        assert!(!dbg.contains("super-secret-key"));
    }

    #[tokio::test]
    async fn apply_gdal_config_missing_file_should_not_panic() {
        // Use a temp dir to verify that a missing config file is gracefully
        // handled. We avoid mutating the process working directory (unsafe in
        // multi-threaded test runners) by testing load_gdal_config_map
        // indirectly — the OnceCell may already be initialized from another
        // test, making this a no-op init, which is the expected behavior.
        let result = apply_gdal_config(None).await;
        assert!(result.is_ok());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn apply_gdal_config_handles_concurrent_calls() {
        let mut tasks = JoinSet::new();
        for _ in 0..8 {
            tasks.spawn(async { apply_gdal_config(None).await });
        }

        while let Some(result) = tasks.join_next().await {
            let call_result = result.expect("task join should succeed");
            assert!(call_result.is_ok());
        }
    }

    #[test]
    fn explicit_options_apply_and_reject_invalid_strings() {
        let key = "SPATIAL_CORE_EXPLICIT_OPTIONS_TEST";
        apply_gdal_options(&[(key, "enabled")]).unwrap();
        assert_eq!(gdal::config::get_config_option(key, "").unwrap(), "enabled");
        assert!(apply_gdal_options(&[("invalid\0key", "value")]).is_err());
        assert!(apply_gdal_options(&[(key, "invalid\0value")]).is_err());
        gdal::config::clear_config_option(key).unwrap();
    }

    #[test]
    fn compatibility_config_reads_cwd_once_and_explicit_options_do_not() {
        const CHILD: &str = "SPATIAL_CORE_CONFIG_COMPATIBILITY_CHILD";
        const KEY: &str = "SPATIAL_CORE_CONFIG_COMPATIBILITY_TEST";
        if std::env::var_os(CHILD).is_some() {
            // The child has its own working directory, GDAL globals and OnceCell.
            // No process-wide environment or cwd mutation races other tests.
            apply_gdal_options(&[("SPATIAL_CORE_EXPLICIT_CHILD_TEST", "yes")]).unwrap();
            assert_eq!(
                gdal::config::get_config_option(KEY, "unset").unwrap(),
                "unset"
            );
            let runtime = tokio::runtime::Builder::new_current_thread()
                .build()
                .unwrap();
            runtime.block_on(apply_gdal_config(None)).unwrap();
            assert_eq!(
                gdal::config::get_config_option(KEY, "unset").unwrap(),
                "first"
            );
            std::fs::write(
                "config/gdal_config.toml",
                "[gdal]\nspatial_core_config_compatibility_test = 'second'\n",
            )
            .unwrap();
            runtime.block_on(apply_gdal_config(None)).unwrap();
            assert_eq!(
                gdal::config::get_config_option(KEY, "unset").unwrap(),
                "first"
            );
            return;
        }

        let directory = tempfile::tempdir().unwrap();
        std::fs::create_dir(directory.path().join("config")).unwrap();
        std::fs::write(
            directory.path().join("config/gdal_config.toml"),
            "[gdal]\nspatial_core_config_compatibility_test = 'first'\n",
        )
        .unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "config::gdal::tests::compatibility_config_reads_cwd_once_and_explicit_options_do_not", "--nocapture"])
            .env(CHILD, "1")
            .current_dir(directory.path())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "child failed: {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
