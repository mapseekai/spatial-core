//! Synchronous GDAL DEM utility with the same cancellation-safe progress bridge
//! as Warp/Translate. No shell command or external process is involved.
use super::super::ffi::{last_null_pointer_err, path_to_c_string};
use crate::gdal_ext::progress::{GDAL_PROGRESS_TRAMPOLINE, ProgressCallbackState};
use crate::raster::convert::GdalOperationProgress;
use gdal::{Dataset, errors::Result};
use std::{ffi::CString, path::Path, ptr::null_mut};

use super::utility::DemOptions;

pub use crate::raster::dem::DemMode;

pub fn dem_to_path_with_progress<F>(
    source: &Dataset,
    target: &Path,
    mode: DemMode,
    args: &[String],
    mut progress: F,
) -> Result<Dataset>
where
    F: FnMut(GdalOperationProgress) -> bool + Send,
{
    let options = DemOptions::new(args.iter().map(String::as_str))?;
    let path = path_to_c_string(target)?;
    let mode = CString::new(mode.name())?;
    let mut callback = ProgressCallbackState::new(&mut progress);
    // SAFETY: callback, source, options and both C strings remain alive until the
    // synchronous call returns; the shared trampoline catches Rust panics.
    let output = unsafe {
        gdal_sys::GDALDEMProcessingOptionsSetProgress(
            options.c_options,
            GDAL_PROGRESS_TRAMPOLINE,
            callback.user_data(),
        );
        gdal_sys::GDALDEMProcessing(
            path.as_ptr(),
            source.c_dataset(),
            mode.as_ptr(),
            std::ptr::null(),
            options.c_options,
            null_mut(),
        )
    };
    let result = if output.is_null() {
        Err(last_null_pointer_err("GDALDEMProcessing"))
    } else {
        // SAFETY: a non-null utility result is an owned handle, also closed if the callback aborted.
        Ok(unsafe { Dataset::from_c_dataset(output) })
    };
    callback.check()?;
    result
}
