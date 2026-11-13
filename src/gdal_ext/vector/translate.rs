// Portions of the options/translation wrappers correspond to georust/gdal.
// Copyright (c) 2014 Alex Morega. Upstream MIT notice is preserved in
// third_party/georust-gdal/LICENSE.txt; scope and local changes are documented
// in docs/third-party-notices.md. This is not a whole-project license change.

use crate::gdal_ext::ffi::last_null_pointer_err;
use crate::gdal_ext::progress::{GDAL_PROGRESS_TRAMPOLINE, ProgressCallbackState};
use gdal::{
    Dataset,
    errors::{GdalError, Result},
};
use std::{
    ffi::CString,
    ptr::{self, NonNull},
};

pub(crate) struct Options(NonNull<gdal_sys::GDALVectorTranslateOptions>);

impl Options {
    pub(crate) fn new(args: &[String]) -> Result<Self> {
        let strings = args
            .iter()
            .map(|arg| CString::new(arg.as_bytes()))
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let mut pointers = strings
            .iter()
            .map(|s| s.as_ptr().cast_mut())
            .chain(std::iter::once(ptr::null_mut()))
            .collect::<Vec<_>>();
        // SAFETY: options parsing copies the null-terminated arguments synchronously.
        let raw = unsafe {
            gdal_sys::GDALVectorTranslateOptionsNew(pointers.as_mut_ptr(), ptr::null_mut())
        };
        NonNull::new(raw)
            .map(Self)
            .ok_or_else(|| last_null_pointer_err("GDALVectorTranslateOptionsNew"))
    }
}

impl Drop for Options {
    fn drop(&mut self) {
        // SAFETY: this is the sole owner of the allocated native options.
        unsafe { gdal_sys::GDALVectorTranslateOptionsFree(self.0.as_ptr()) };
    }
}

pub(crate) struct Outcome {
    pub created: Option<Dataset>,
    pub error: Option<GdalError>,
    pub usage_error: bool,
}

pub(crate) fn translate(
    source: &mut Dataset,
    name: &CString,
    output: Option<&mut Dataset>,
    options: &mut Options,
    progress: Option<&mut ProgressCallbackState<'_>>,
) -> Outcome {
    if let Some(progress) = progress {
        // SAFETY: state lives through the synchronous call and all its native callbacks.
        unsafe {
            gdal_sys::GDALVectorTranslateOptionsSetProgress(
                options.0.as_ptr(),
                GDAL_PROGRESS_TRAMPOLINE,
                progress.user_data(),
            )
        };
    }
    let destination = output.as_ref().map_or(ptr::null_mut(), |ds| ds.c_dataset());
    let mut source_handle = source.c_dataset();
    let mut usage_error = 0;
    let destination_name = if destination.is_null() {
        name.as_ptr()
    } else {
        ptr::null()
    };
    // SAFETY: source/output/options are exclusively borrowed and stay alive. GDAL
    // borrows existing handles; it transfers ownership only for a newly opened output.
    let handle = unsafe {
        gdal_sys::GDALVectorTranslate(
            destination_name,
            destination,
            1,
            &mut source_handle,
            options.0.as_ptr(),
            &mut usage_error,
        )
    };
    let error = handle
        .is_null()
        .then(|| last_null_pointer_err("GDALVectorTranslate"));
    let created = if destination.is_null() && !handle.is_null() {
        // SAFETY: newly returned non-null handle has no other Rust owner.
        Some(unsafe { Dataset::from_c_dataset(handle) })
    } else {
        None
    };
    Outcome {
        created,
        error,
        usage_error: usage_error != 0,
    }
}
