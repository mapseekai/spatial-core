use super::super::destination::DatasetDestination;
use super::super::ffi::last_null_pointer_err;
use crate::gdal_ext::progress::{GDAL_PROGRESS_TRAMPOLINE, ProgressCallbackState};
use crate::raster::convert::GdalOperationProgress;
use gdal::{
    Dataset,
    errors::{GdalError, Result},
};
use gdal_sys::{
    GDALTranslate, GDALTranslateOptions, GDALTranslateOptionsFree, GDALTranslateOptionsSetProgress,
};
use libc::c_char;
use std::ffi::CString;
use std::ffi::c_void;
use std::ptr::{null, null_mut};

#[allow(dead_code)]
pub struct TranslateOptions {
    c_options: *mut GDALTranslateOptions,
}

#[allow(dead_code)]
impl TranslateOptions {
    /// See [GDALTranslateOptionsNew].
    ///
    /// [GDALTranslateOptionsNew]: https://gdal.org/api/gdal_utils.html#_CPPv429GDALTranslateOptionsNewPPcP35GDALTranslateOptionsForBinary
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
        let c_options =
            unsafe { gdal_sys::GDALTranslateOptionsNew(c_args.as_mut_ptr(), null_mut()) };
        if c_options.is_null() {
            return Err(last_null_pointer_err("GDALTranslateOptionsNew"));
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
            GDALTranslateOptionsSetProgress(self.c_options, callback, user_data);
        }
    }
}

impl Drop for TranslateOptions {
    fn drop(&mut self) {
        // SAFETY: new rejects null handles and this wrapper exclusively owns the options.
        unsafe {
            GDALTranslateOptionsFree(self.c_options);
        }
    }
}

impl TryFrom<Vec<&str>> for TranslateOptions {
    type Error = GdalError;

    fn try_from(value: Vec<&str>) -> Result<Self> {
        TranslateOptions::new(value)
    }
}

/// Converts simple features data between file formats.
///
/// Wraps [GDALTranslate].
/// See the [program docs] for more details.
///
/// [GDALTranslate]: https://gdal.org/api/gdal_utils.html#_CPPv419GDALTranslatePKc12GDALDatasetHiP12GDALDatasetHPK26GDALTranslateOptionsPi
/// [program docs]: https://gdal.org/programs/ogr2ogr.html
///
///
#[allow(dead_code)]
pub fn translate(
    src: &Dataset,
    dest: DatasetDestination,
    options: Option<TranslateOptions>,
) -> Result<Dataset> {
    _translate(src, dest, options.as_ref())
}

#[allow(dead_code)]
pub(crate) fn translate_with_progress<F>(
    src: &Dataset,
    dest: DatasetDestination,
    mut options: Option<TranslateOptions>,
    mut progress: F,
) -> Result<Dataset>
where
    F: FnMut(GdalOperationProgress) -> bool + Send,
{
    let mut fallback_options = TranslateOptions::new(std::iter::empty::<&str>())?;
    let options = options.as_mut().unwrap_or(&mut fallback_options);

    let mut progress_state = ProgressCallbackState::new(&mut progress);
    // SAFETY: the callback and its exclusive state live through the synchronous _translate call.
    unsafe {
        options.set_progress(GDAL_PROGRESS_TRAMPOLINE, progress_state.user_data());
    }

    let result = _translate(src, dest, Some(options));
    progress_state.check()?;
    result
}

fn _translate(
    datasets: &Dataset,
    mut dest: DatasetDestination,
    options: Option<&TranslateOptions>,
) -> Result<Dataset> {
    let (psz_dest_option, _h_dst_ds) = match &dest {
        DatasetDestination::Path(c_path) => (Some(c_path), null_mut()),
        DatasetDestination::Dataset { dataset, .. } => (None, dataset.c_dataset()),
    };

    let psz_dest = psz_dest_option.map(|x| x.as_ptr()).unwrap_or_else(null);

    let c_options = options
        .map(|x| x.c_options as *const GDALTranslateOptions)
        .unwrap_or(null());

    // SAFETY: source, path and options handles remain valid throughout this synchronous call.
    let dataset_out =
        unsafe { GDALTranslate(psz_dest, datasets.c_dataset(), c_options, null_mut()) };

    if dataset_out.is_null() {
        return Err(last_null_pointer_err("GDALTranslate"));
    }

    // SAFETY: a successful result transfers dataset ownership to the returned Dataset wrapper.
    unsafe {
        dest.relinquish_dataset_ownership();
    }

    // SAFETY: the result is non-null and no other Rust wrapper retains ownership.
    let result = unsafe { Dataset::from_c_dataset(dataset_out) };

    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::{translate, translate_with_progress};
    use gdal::{Dataset, DriverManager};
    use tempfile::tempdir;

    fn create_test_geotiff(path: &std::path::Path) {
        let driver =
            DriverManager::get_driver_by_name("GTiff").expect("GTiff driver should be available");
        let mut dataset = driver
            .create_with_band_type::<u8, _>(path, 4, 4, 1)
            .expect("test dataset should be created");
        dataset.flush_cache().expect("flush_cache should succeed");
    }

    #[test]
    fn test_translate() {
        let tempdir = tempdir().expect("tempdir should be created");
        let src = tempdir.path().join("translate-source.tif");
        let out = tempdir.path().join("translate-output.tif");
        create_test_geotiff(&src);

        let dataset = &Dataset::open(&src).expect("fixture should be readable");
        translate(dataset, out.try_into().expect("destination path"), None)
            .expect("translate should succeed");
    }

    #[test]
    fn test_translate_with_progress_reports_completion() {
        let tempdir = tempdir().expect("tempdir should be created");
        let src = tempdir.path().join("translate-progress-source.tif");
        let out = tempdir.path().join("translate-progress-output.tif");
        create_test_geotiff(&src);

        let dataset = &Dataset::open(&src).expect("fixture should be readable");
        let mut fractions = Vec::new();

        translate_with_progress(
            dataset,
            out.try_into().expect("destination path"),
            None,
            |progress| {
                fractions.push(progress.fraction);
                true
            },
        )
        .expect("translate_with_progress should succeed");

        assert!(
            !fractions.is_empty(),
            "expected progress callback to be invoked"
        );
        assert!(
            fractions.iter().any(|fraction| *fraction >= 0.99),
            "expected progress to reach completion, got {fractions:?}"
        );
    }
}
