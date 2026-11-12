//! Synchronous GDAL DEM utility with the same cancellation-safe progress bridge
//! as Warp/Translate. No shell command or external process is involved.
use super::super::ffi::last_null_pointer_err;
use super::progress::{GDAL_PROGRESS_TRAMPOLINE, ProgressCallbackState};
use crate::raster::convert::GdalOperationProgress;
use gdal::{Dataset, errors::Result};
use std::{cell::Cell, ffi::CString, path::Path, ptr::null_mut};

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
    F: FnMut(GdalOperationProgress) -> bool,
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
    let path = CString::new(target.to_string_lossy().as_bytes())?;
    let mode = CString::new(mode.name())?;
    // Some GDAL drivers return a partially created dataset after a progress abort.
    // Track rejection independently of the returned pointer, including callback panics.
    let aborted = Cell::new(false);
    let mut forward = |value| {
        if aborted.get() {
            return false;
        }
        aborted.set(true);
        let keep_going = progress(value);
        aborted.set(!keep_going);
        keep_going
    };
    let mut callback = ProgressCallbackState::new(&mut forward);
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
    if aborted.get() {
        if !output.is_null() {
            // SAFETY: a non-null utility result is an owned handle even on progress abort.
            drop(unsafe { Dataset::from_c_dataset(output) });
        }
        return Err(gdal::errors::GdalError::CplError {
            class: gdal_sys::CPLErr::CE_Failure,
            number: 9, // CPLE_UserInterrupt
            msg: "GDAL DEM processing cancelled by progress callback".into(),
        });
    }
    if output.is_null() {
        return Err(last_null_pointer_err("GDALDEMProcessing"));
    }
    // SAFETY: non-null successful GDAL result transfers one owned dataset handle.
    Ok(unsafe { Dataset::from_c_dataset(output) })
}
