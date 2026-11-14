use super::super::destination::DatasetDestination;
use super::super::ffi::last_null_pointer_err;
use crate::gdal_ext::progress::{GDAL_PROGRESS_TRAMPOLINE, ProgressCallbackState};
use crate::raster::convert::GdalOperationProgress;
use gdal::{
    Dataset,
    errors::{GdalError, Result},
};
use gdal_sys::{GDALTranslate, GDALTranslateOptions};
use std::ptr::{null, null_mut};

pub(crate) use super::utility::TranslateOptions;

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
    dest: DatasetDestination,
    options: Option<&TranslateOptions>,
) -> Result<Dataset> {
    let DatasetDestination::Path(path) = dest;

    let c_options = options
        .map(|x| x.c_options as *const GDALTranslateOptions)
        .unwrap_or(null());

    // SAFETY: source, path and options handles remain valid throughout this synchronous call.
    let dataset_out =
        unsafe { GDALTranslate(path.as_ptr(), datasets.c_dataset(), c_options, null_mut()) };

    if dataset_out.is_null() {
        return Err(last_null_pointer_err("GDALTranslate"));
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
