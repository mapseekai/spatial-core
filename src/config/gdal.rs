//! Explicit GDAL configuration. Import workers restore thread-local overrides.
use anyhow::{Context, Result};
use gdal::config::set_config_option;
use std::{
    ffi::{CStr, CString},
    marker::PhantomData,
    path::Path,
    rc::Rc,
};

/// Explicit input options, including credentials. Debug never prints values.
#[derive(Clone, Default)]
pub struct GdalOptions(Vec<(CString, CString)>);

impl std::fmt::Debug for GdalOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GdalOptions")
            .field(
                "keys",
                &self.0.iter().map(|(key, _)| key).collect::<Vec<_>>(),
            )
            .finish()
    }
}

impl GdalOptions {
    /// Add an option for the input worker's lifetime.
    /// # Errors
    /// Rejects empty keys and NUL bytes in keys or values.
    pub fn set(mut self, key: &str, value: &str) -> Result<Self> {
        anyhow::ensure!(!key.is_empty(), "GDAL option key is empty");
        self.0.push((CString::new(key)?, CString::new(value)?));
        Ok(self)
    }

    pub(crate) fn enter(&self) -> ThreadOptions {
        let mut guard = ThreadOptions {
            original: Vec::new(),
            _thread: PhantomData,
        };
        for (key, value) in &self.0 {
            guard.set(key, value);
        }
        for key in [c"GDAL_HTTP_TIMEOUT", c"GDAL_HTTP_CONNECTTIMEOUT"] {
            // SAFETY: static key; NULL asks whether any thread/global/env value exists.
            if unsafe { gdal_sys::CPLGetConfigOption(key.as_ptr(), std::ptr::null()) }.is_null() {
                guard.set(key, c"30");
            }
        }
        guard
    }
}

pub(crate) struct ThreadOptions {
    original: Vec<(CString, Option<CString>)>,
    // Restoration must happen on the same native thread, including unwinding.
    _thread: PhantomData<Rc<()>>,
}

impl ThreadOptions {
    fn set(&mut self, key: &CStr, value: &CStr) {
        // SAFETY: strings outlive calls; GDAL owns the old value and copies the new one.
        let old = unsafe {
            let ptr = gdal_sys::CPLGetThreadLocalConfigOption(key.as_ptr(), std::ptr::null());
            let old = (!ptr.is_null()).then(|| CStr::from_ptr(ptr).to_owned());
            gdal_sys::CPLSetThreadLocalConfigOption(key.as_ptr(), value.as_ptr());
            old
        };
        self.original.push((key.to_owned(), old));
    }
}

impl Drop for ThreadOptions {
    fn drop(&mut self) {
        for (key, value) in self.original.iter().rev() {
            // SAFETY: the saved strings are alive; NULL removes the local override.
            unsafe {
                gdal_sys::CPLSetThreadLocalConfigOption(
                    key.as_ptr(),
                    value.as_ref().map_or(std::ptr::null(), |v| v.as_ptr()),
                );
            }
        }
    }
}

/// Apply process-wide GDAL options during application initialization.
///
/// # Errors
/// Returns an error for embedded NUL bytes. Does not read CWD configuration.
pub fn apply_gdal_options(options: &[(&str, &str)]) -> Result<()> {
    for (key, value) in options {
        set_config_option(key, value)?;
    }
    Ok(())
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

    tracing::info!(
        path = %path.display(),
        override_env,
        "GDAL config file loaded (process-global)"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn worker_options_restore_after_unwind_and_do_not_expose_values() -> Result<()> {
        let key = "SPATIAL_CORE_TEST_PRIVATE";
        gdal::config::set_thread_local_config_option(key, "original")?;
        let options = GdalOptions::default().set(key, "secret")?;
        assert!(!format!("{options:?}").contains("secret"));
        let _ = std::panic::catch_unwind(|| {
            let _scope = options.enter();
            assert_eq!(
                gdal::config::get_thread_local_config_option(key, "").unwrap(),
                "secret"
            );
            panic!("exercise restoration");
        });
        assert_eq!(
            gdal::config::get_thread_local_config_option(key, "")?,
            "original"
        );
        gdal::config::clear_thread_local_config_option(key)?;
        {
            let _scope = options.enter();
        }
        assert_eq!(
            gdal::config::get_thread_local_config_option(key, "missing")?,
            "missing"
        );
        Ok(())
    }
}
