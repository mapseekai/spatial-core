use super::super::destination::DatasetDestination;
use super::super::ffi::last_null_pointer_err;
use crate::gdal_ext::progress::{GDAL_PROGRESS_TRAMPOLINE, ProgressCallbackState};
use crate::raster::convert::GdalOperationProgress;
use gdal::{
    Dataset,
    errors::{GdalError, Result},
};
use gdal_sys::{GDALWarp, GDALWarpAppOptions};
use std::ptr::{null, null_mut};

pub(crate) use super::utility::WarpOptions;

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
    F: FnMut(GdalOperationProgress) -> bool + Send,
{
    let mut fallback_options = WarpOptions::new(std::iter::empty::<&str>())?;
    let options = options.as_mut().unwrap_or(&mut fallback_options);

    let mut progress_state = ProgressCallbackState::new(&mut progress);
    // SAFETY: the callback and its exclusive state live through the synchronous _warp call.
    unsafe {
        options.set_progress(GDAL_PROGRESS_TRAMPOLINE, progress_state.user_data());
    }

    let result = _warp(src, dest, Some(options));
    progress_state.check()?;
    result
}

fn _warp(
    src: &Dataset,
    dest: DatasetDestination,
    options: Option<&WarpOptions>,
) -> Result<Dataset> {
    let DatasetDestination::Path(path) = dest;
    let c_options = options
        .map(|x| x.c_options as *const GDALWarpAppOptions)
        .unwrap_or(null());

    // SAFETY: the live source array matches its count; destination and options outlive this call.
    let dataset_out = unsafe {
        let mut sources = vec![src.c_dataset()];
        GDALWarp(
            path.as_ptr(),
            null_mut(),
            sources.len() as i32,
            sources.as_mut_ptr(),
            c_options,
            null_mut(),
        )
    };

    if dataset_out.is_null() {
        return Err(last_null_pointer_err("GDALWarp"));
    }

    // SAFETY: the non-null result transfers a newly owned output handle.
    let result = unsafe { Dataset::from_c_dataset(dataset_out) };
    Ok(result)
}
