// Portions of the original path/null-pointer helpers correspond to georust/gdal.
// Copyright (c) 2014 Alex Morega. Upstream MIT notice is preserved in
// third_party/georust-gdal/LICENSE.txt; scope and local changes are documented
// in docs/third-party-notices.md. This is not a whole-project license change.

use gdal::errors::GdalError;
use std::ffi::{CStr, CString};
use std::path::Path;

/// Check the thread-local CPL error channel even when a GDAL wrapper returns a
/// value. OGR's feature iterator maps both native read failures and EOF to None.
/// Reset before each operation so unrelated prior errors are not misclassified.
pub(crate) fn checked_gdal_call<T>(operation: impl FnOnce() -> T) -> gdal::errors::Result<T> {
    // SAFETY: CPL error state is local to this OS thread and owns its message.
    unsafe { gdal_sys::CPLErrorReset() };
    let output = operation();
    // SAFETY: these functions read the current thread's GDAL-owned error state.
    let error = unsafe {
        let class = gdal_sys::CPLGetLastErrorType();
        (class >= gdal_sys::CPLErr::CE_Failure).then(|| GdalError::CplError {
            class,
            number: gdal_sys::CPLGetLastErrorNo(),
            // Copy before resetting the thread-local GDAL-owned message.
            msg: CStr::from_ptr(gdal_sys::CPLGetLastErrorMsg())
                .to_string_lossy()
                .into_owned(),
        })
    };
    if let Some(error) = error {
        // SAFETY: the error and its message above are now owned Rust values.
        unsafe { gdal_sys::CPLErrorReset() };
        return Err(error);
    }
    Ok(output)
}

pub(crate) fn last_null_pointer_err(method_name: &'static str) -> GdalError {
    // SAFETY: GDAL returns a non-null, NUL-terminated thread-local message, copied before reset.
    let last_err_msg = unsafe {
        CStr::from_ptr(gdal_sys::CPLGetLastErrorMsg())
            .to_string_lossy()
            .into_owned()
    };
    // SAFETY: resetting GDAL's thread-local error state requires no caller-owned pointers.
    unsafe { gdal_sys::CPLErrorReset() };
    GdalError::NullPointer {
        method_name,
        msg: last_err_msg,
    }
}

pub(crate) fn path_to_c_string(path: &Path) -> gdal::errors::Result<CString> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        CString::new(path.as_os_str().as_bytes()).map_err(Into::into)
    }
    #[cfg(not(unix))]
    {
        let path = path.to_str().ok_or_else(|| {
            GdalError::BadArgument("dataset path cannot be represented as UTF-8".into())
        })?;
        CString::new(path.as_bytes()).map_err(Into::into)
    }
}

#[cfg(test)]
mod tests {
    use super::{checked_gdal_call, last_null_pointer_err};
    use gdal::errors::GdalError;

    #[cfg(unix)]
    #[test]
    fn unix_paths_preserve_raw_bytes_and_still_reject_nul() {
        use std::{ffi::OsStr, os::unix::ffi::OsStrExt, path::Path};

        let raw = b"/tmp/raster_\xff.tif";
        let path = Path::new(OsStr::from_bytes(raw));
        assert_eq!(super::path_to_c_string(path).unwrap().as_bytes(), raw);
        let nul = Path::new(OsStr::from_bytes(b"/tmp/raster\0.tif"));
        assert!(super::path_to_c_string(nul).is_err());
    }

    #[test]
    fn copies_error_message_before_resetting_gdal_state() {
        // SAFETY: the static C string is NUL-terminated and contains no printf placeholders.
        unsafe { gdal_sys::CPLError(gdal_sys::CPLErr::CE_Failure, 1, c"test GDAL error".as_ptr()) };
        let error = last_null_pointer_err("test operation");
        assert!(
            matches!(error, GdalError::NullPointer { method_name: "test operation", msg } if msg == "test GDAL error")
        );
        let reset = last_null_pointer_err("after reset");
        assert!(matches!(reset, GdalError::NullPointer { msg, .. } if msg.is_empty()));
    }

    #[test]
    fn checked_calls_ignore_stale_errors_and_capture_new_failures() {
        // SAFETY: static NUL-terminated messages contain no printf placeholders.
        unsafe { gdal_sys::CPLError(gdal_sys::CPLErr::CE_Failure, 1, c"stale failure".as_ptr()) };
        assert_eq!(checked_gdal_call(|| 42).unwrap(), 42);
        let error = checked_gdal_call(|| {
            // SAFETY: the static message is NUL-terminated with no format arguments.
            unsafe {
                gdal_sys::CPLError(gdal_sys::CPLErr::CE_Failure, 23, c"read failure".as_ptr())
            };
            None::<usize>
        })
        .unwrap_err();
        assert!(
            matches!(error, GdalError::CplError { class, number: 23, msg } if class == gdal_sys::CPLErr::CE_Failure && msg == "read failure")
        );
        // SAFETY: reads only this thread's GDAL error state.
        let error_class = unsafe { gdal_sys::CPLGetLastErrorType() };
        assert_eq!(error_class, gdal_sys::CPLErr::CE_None);
    }

    #[test]
    fn checked_calls_preserve_warning_only_results() {
        let value = checked_gdal_call(|| {
            // SAFETY: the static message is NUL-terminated with no format arguments.
            unsafe {
                gdal_sys::CPLError(gdal_sys::CPLErr::CE_Warning, 1, c"read warning".as_ptr())
            };
            7
        })
        .unwrap();
        assert_eq!(value, 7);
        // SAFETY: resetting thread-local error state has no external effect.
        unsafe { gdal_sys::CPLErrorReset() };
    }
}
