//! Owned dataset descriptions. Native handles are opened only while executing.

use crate::gdal_ext::ffi::{last_null_pointer_err, path_to_c_string};
use crate::operation::{Error, ErrorKind, Result, Stage};
use gdal::{Dataset, DriverManager};
use std::{
    ffi::CString,
    fmt,
    path::{Path, PathBuf},
    ptr,
};

/// A local dataset or an explicitly named GDAL connection/VSI source.
#[derive(Clone)]
#[must_use]
pub struct Source {
    location: Location,
    drivers: Vec<String>,
    options: Options,
}

#[derive(Clone)]
enum Location {
    Path(PathBuf),
    Name(String),
}

impl fmt::Debug for Source {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Source")
            .field("local", &matches!(self.location, Location::Path(_)))
            .field("drivers", &self.drivers)
            .field("options", &self.options)
            .finish_non_exhaustive()
    }
}

impl Source {
    /// Describe a local file or directory. Does not perform I/O.
    pub fn new(path: impl AsRef<Path>) -> Self {
        Self {
            location: Location::Path(path.as_ref().to_owned()),
            drivers: Vec::new(),
            options: Options::default(),
        }
    }

    /// Describe a GDAL VSI path, URL, or connection string without rewriting it.
    pub fn gdal_name(name: impl Into<String>) -> Self {
        Self {
            location: Location::Name(name.into()),
            drivers: Vec::new(),
            options: Options::default(),
        }
    }

    /// Replace the input driver restriction. An empty list means any driver.
    pub fn allowed_drivers<I, S>(mut self, names: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.drivers = names.into_iter().map(Into::into).collect();
        self
    }

    /// Set a driver-specific input open option, replacing the same key.
    pub fn open_option(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.options.set(key.into(), value.into());
        self
    }

    pub(crate) fn local_path(&self) -> Option<&Path> {
        match &self.location {
            Location::Path(path) => Some(path),
            Location::Name(_) => None,
        }
    }

    pub(crate) fn validate(&self) -> Result<()> {
        self.options.validate()?;
        for name in &self.drivers {
            validate_text(name, "input driver")?;
        }
        match &self.location {
            Location::Path(path) => {
                local_name(path)?;
            }
            Location::Name(name) => validate_text(name, "GDAL source name")?,
        }
        Ok(())
    }

    pub(crate) fn name(&self) -> Result<CString> {
        match &self.location {
            Location::Path(path) => {
                let path = path.canonicalize().map_err(|error| {
                    Error::new(
                        ErrorKind::Open,
                        Stage::OpenSource,
                        "cannot resolve local input",
                    )
                    .with_source(error)
                })?;
                local_name(&path)
            }
            Location::Name(name) => CString::new(name.as_bytes())
                .map_err(|_| Error::invalid("GDAL source name contains NUL")),
        }
    }

    pub(crate) fn open(&self) -> Result<Dataset> {
        open(&self.name()?, &self.drivers, &self.options, false)
            .map_err(|e| Error::native(Stage::OpenSource, e))
    }
}

impl From<&str> for Source {
    fn from(path: &str) -> Self {
        Self::new(path)
    }
}
impl From<String> for Source {
    fn from(path: String) -> Self {
        Self::new(path)
    }
}
impl From<&String> for Source {
    fn from(path: &String) -> Self {
        Self::new(path)
    }
}
impl From<PathBuf> for Source {
    fn from(path: PathBuf) -> Self {
        Self::new(path)
    }
}
impl From<&PathBuf> for Source {
    fn from(path: &PathBuf) -> Self {
        Self::new(path)
    }
}
impl From<&Path> for Source {
    fn from(path: &Path) -> Self {
        Self::new(path)
    }
}

/// Values stay separated until the GDAL boundary; Debug only reveals keys.
#[derive(Clone, Default)]
pub(crate) struct Options(Vec<(String, String)>);

impl fmt::Debug for Options {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list()
            .entries(self.0.iter().map(|(key, _)| key))
            .finish()
    }
}

impl Options {
    pub(crate) fn set(&mut self, key: String, value: String) {
        if let Some(pair) = self
            .0
            .iter_mut()
            .find(|(name, _)| name.eq_ignore_ascii_case(&key))
        {
            *pair = (key, value);
        } else {
            self.0.push((key, value));
        }
    }
    pub(crate) fn get(&self, key: &str) -> Option<&str> {
        self.0
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(key))
            .map(|(_, value)| value.as_str())
    }
    pub(crate) fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
    pub(crate) fn validate(&self) -> Result<()> {
        for (key, value) in &self.0 {
            validate_text(key, "option key")?;
            if key.contains(['=', ':'])
                || key.chars().any(char::is_whitespace)
                || value.contains('\0')
            {
                return Err(Error::invalid(
                    "option keys cannot contain separators or whitespace; values cannot contain NUL",
                ));
            }
        }
        Ok(())
    }
    pub(crate) fn strings(&self) -> Vec<String> {
        self.0
            .iter()
            .map(|(key, value)| format!("{key}={value}"))
            .collect()
    }
    pub(crate) fn append_args(&self, flag: &str, args: &mut Vec<String>) {
        for value in self.strings() {
            args.extend([flag.to_owned(), value]);
        }
    }
}

pub(crate) fn validate_text(value: &str, label: &str) -> Result<()> {
    if value.is_empty() || value.contains('\0') {
        return Err(Error::invalid(format!(
            "{label} must be nonempty and contain no NUL"
        )));
    }
    Ok(())
}

pub(crate) fn local_name(path: &Path) -> Result<CString> {
    if path.as_os_str().is_empty() {
        return Err(Error::invalid("dataset path is empty"));
    }
    #[cfg(not(unix))]
    if path.to_str().is_none() {
        return Err(Error::invalid(
            "dataset path cannot be represented as UTF-8",
        ));
    }
    path_to_c_string(path).map_err(|e| Error::invalid("dataset path contains NUL").with_source(e))
}

pub(crate) fn open(
    name: &CString,
    drivers: &[String],
    options: &Options,
    update: bool,
) -> gdal::errors::Result<Dataset> {
    DriverManager::count(); // Uses gdal's once-only driver registration.
    let drivers = drivers
        .iter()
        .map(|v| CString::new(v.as_bytes()))
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let options = options
        .strings()
        .into_iter()
        .map(CString::new)
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let driver_ptrs = pointers(&drivers);
    let option_ptrs = pointers(&options);
    let flags = gdal::GdalOpenFlags::GDAL_OF_VECTOR | gdal::GdalOpenFlags::GDAL_OF_VERBOSE_ERROR;
    // SAFETY: strings and null-terminated pointer lists live through this synchronous open.
    let dataset = unsafe {
        gdal_sys::GDALOpenEx(
            name.as_ptr(),
            flags.bits() | u32::from(update),
            if drivers.is_empty() {
                ptr::null()
            } else {
                driver_ptrs.as_ptr()
            },
            option_ptrs.as_ptr(),
            ptr::null(),
        )
    };
    if dataset.is_null() {
        Err(last_null_pointer_err("GDALOpenEx"))
    } else {
        // SAFETY: non-null handle was newly opened and has one owner.
        Ok(unsafe { Dataset::from_c_dataset(dataset) })
    }
}

fn pointers(strings: &[CString]) -> Vec<*const std::ffi::c_char> {
    strings
        .iter()
        .map(|v| v.as_ptr())
        .chain(std::iter::once(ptr::null()))
        .collect()
}
