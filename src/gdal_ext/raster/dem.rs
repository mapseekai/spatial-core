//! Synchronous GDAL DEM utility with the same cancellation-safe progress bridge
//! as Warp/Translate. No shell command or external process is involved.
use super::super::ffi::{last_null_pointer_err, path_to_c_string};
use crate::gdal_ext::progress::{GDAL_PROGRESS_TRAMPOLINE, ProgressCallbackState};
use crate::raster::convert::GdalOperationProgress;
use gdal::{Dataset, errors::Result};
use std::{ffi::CString, path::Path, ptr::null_mut};

struct Options(*mut gdal_sys::GDALDEMProcessingOptions);
impl Drop for Options {
    fn drop(&mut self) {
        // SAFETY: constructed by GDALDEMProcessingOptionsNew and freed exactly once.
        unsafe { gdal_sys::GDALDEMProcessingOptionsFree(self.0) };
    }
}

#[derive(Debug, Clone, Copy)]
pub enum DemMode {
    Hillshade,
    Slope,
    Aspect,
    Roughness,
    Tpi,
    Tri,
}
impl DemMode {
    fn name(self) -> &'static str {
        match self {
            Self::Hillshade => "hillshade",
            Self::Slope => "slope",
            Self::Aspect => "aspect",
            Self::Roughness => "roughness",
            Self::Tpi => "TPI",
            Self::Tri => "TRI",
        }
    }
}

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
    let strings = args
        .iter()
        .map(|s| CString::new(s.as_str()))
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let mut argv = strings
        .iter()
        .map(|s| s.as_ptr().cast_mut())
        .chain(std::iter::once(null_mut()))
        .collect::<Vec<_>>();
    // SAFETY: argv is null-terminated and strings outlive the synchronous options parser.
    let raw = unsafe { gdal_sys::GDALDEMProcessingOptionsNew(argv.as_mut_ptr(), null_mut()) };
    if raw.is_null() {
        return Err(last_null_pointer_err("GDALDEMProcessingOptionsNew"));
    }
    let options = Options(raw);
    let path = path_to_c_string(target)?;
    let mode = CString::new(mode.name())?;
    let mut callback = ProgressCallbackState::new(&mut progress);
    // SAFETY: callback, source, options and both C strings remain alive until the
    // synchronous call returns; the shared trampoline catches Rust panics.
    let output = unsafe {
        gdal_sys::GDALDEMProcessingOptionsSetProgress(
            options.0,
            GDAL_PROGRESS_TRAMPOLINE,
            callback.user_data(),
        );
        gdal_sys::GDALDEMProcessing(
            path.as_ptr(),
            source.c_dataset(),
            mode.as_ptr(),
            std::ptr::null(),
            options.0,
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
