use super::super::destination::DatasetDestination;
use super::super::ffi::last_null_pointer_err;
use super::progress::{GDAL_PROGRESS_TRAMPOLINE, ProgressCallbackState};
use crate::raster::convert::GdalOperationProgress;
use gdal::{
    Dataset,
    errors::{GdalError, Result},
};
use gdal_sys::{
    GDALWarp, GDALWarpAppOptions, GDALWarpAppOptionsFree, GDALWarpAppOptionsNew,
    GDALWarpAppOptionsSetProgress,
};
use libc::c_char;
use std::ffi::{CString, c_void};
use std::ptr::{null, null_mut};

pub struct WarpOptions {
    c_options: *mut GDALWarpAppOptions,
}

impl WarpOptions {
    pub fn new<S: Into<Vec<u8>>, I: IntoIterator<Item = S>>(args: I) -> Result<Self> {
        let cstr_args = args
            .into_iter()
            .map(CString::new)
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let mut c_args = cstr_args
            .iter()
            .map(|x| x.as_ptr() as *mut c_char)
            .chain(std::iter::once(null_mut()))
            .collect::<Vec<_>>();

        // SAFETY: the null-terminated argv and its CString buffers outlive synchronous option parsing.
        let c_options = unsafe { GDALWarpAppOptionsNew(c_args.as_mut_ptr(), null_mut()) };
        if c_options.is_null() {
            return Err(last_null_pointer_err("GDALWarpAppOptionsNew"));
        }

        Ok(Self { c_options })
    }

    /// # Safety
    /// The callback and user_data must remain valid until every GDAL call using these options returns.
    pub(crate) unsafe fn set_progress(
        &mut self,
        callback: gdal_sys::GDALProgressFunc,
        user_data: *mut c_void,
    ) {
        // SAFETY: options owns a valid handle; the caller guarantees the callback's state lifetime.
        unsafe {
            GDALWarpAppOptionsSetProgress(self.c_options, callback, user_data);
        }
    }
}

impl Drop for WarpOptions {
    fn drop(&mut self) {
        // SAFETY: new rejects null handles and this wrapper exclusively owns the options.
        unsafe {
            GDALWarpAppOptionsFree(self.c_options);
        }
    }
}

impl TryFrom<Vec<&str>> for WarpOptions {
    type Error = GdalError;

    fn try_from(value: Vec<&str>) -> Result<Self> {
        WarpOptions::new(value)
    }
}

pub(crate) fn warp_with_progress<F>(
    src: &Dataset,
    dest: DatasetDestination,
    mut options: Option<WarpOptions>,
    mut progress: F,
) -> Result<Dataset>
where
    F: FnMut(GdalOperationProgress) -> bool,
{
    let mut fallback_options = WarpOptions::new(std::iter::empty::<&str>())?;
    let options = options.as_mut().unwrap_or(&mut fallback_options);

    let mut progress_state = ProgressCallbackState::new(&mut progress);
    // SAFETY: the callback and its exclusive state live through the synchronous _warp call.
    unsafe {
        options.set_progress(GDAL_PROGRESS_TRAMPOLINE, progress_state.user_data());
    }

    _warp(src, dest, Some(options))
}

fn _warp(
    src: &Dataset,
    mut dest: DatasetDestination,
    options: Option<&WarpOptions>,
) -> Result<Dataset> {
    let (psz_dest_option, h_dst_ds) = match &dest {
        DatasetDestination::Path(c_path) => (Some(c_path), null_mut()),
        DatasetDestination::Dataset { dataset, .. } => (None, dataset.c_dataset()),
    };
    let psz_dest = psz_dest_option.map(|x| x.as_ptr()).unwrap_or_else(null);
    let c_options = options
        .map(|x| x.c_options as *const GDALWarpAppOptions)
        .unwrap_or(null());

    // SAFETY: the live source array matches its count; destination and options outlive this call.
    let dataset_out = unsafe {
        let mut sources = vec![src.c_dataset()];
        GDALWarp(
            psz_dest,
            h_dst_ds,
            sources.len() as i32,
            sources.as_mut_ptr(),
            c_options,
            null_mut(),
        )
    };

    if dataset_out.is_null() {
        return Err(last_null_pointer_err("GDALWarp"));
    }

    // SAFETY: the successful call returned the destination handle, transferring its ownership.
    unsafe {
        dest.relinquish_dataset_ownership();
    }

    // SAFETY: the result is non-null and the moved-in destination will no longer free it.
    let result = unsafe { Dataset::from_c_dataset(dataset_out) };
    Ok(result)
}
