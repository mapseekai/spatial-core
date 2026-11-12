// Portions of the options/translation wrappers correspond to georust/gdal.
// Copyright (c) 2014 Alex Morega. Upstream MIT notice is preserved in
// third_party/georust-gdal/LICENSE.txt; scope and local changes are documented
// in docs/third-party-notices.md. This is not a whole-project license change.

use std::ffi::CString;
use std::ptr::{null, null_mut};

use super::super::destination::DatasetDestination;
use super::super::ffi::last_null_pointer_err;
use gdal::{
    Dataset,
    errors::{GdalError, Result},
};
use gdal_sys::{GDALVectorTranslate, GDALVectorTranslateOptions, GDALVectorTranslateOptionsFree};
use libc::c_char;

/// Wraps a [GDALVectorTranslateOptions] object.
///
/// [GDALVectorTranslateOptions]: https://gdal.org/api/gdal_utils.html#_CPPv426GDALVectorTranslateOptions
pub struct VectorTranslateOptions {
    c_options: *mut GDALVectorTranslateOptions,
}

impl VectorTranslateOptions {
    /// See [GDALVectorTranslateOptionsNew].
    ///
    /// [GDALVectorTranslateOptionsNew]: https://gdal.org/api/gdal_utils.html#_CPPv429GDALVectorTranslateOptionsNewPPcP35GDALVectorTranslateOptionsForBinary
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

        // SAFETY: the null-terminated argument array and its CString values
        // remain alive until GDAL has parsed and copied the option values.
        let c_options =
            unsafe { gdal_sys::GDALVectorTranslateOptionsNew(c_args.as_mut_ptr(), null_mut()) };
        if c_options.is_null() {
            return Err(last_null_pointer_err("GDALVectorTranslateOptionsNew"));
        }

        Ok(Self { c_options })
    }
}

impl Drop for VectorTranslateOptions {
    fn drop(&mut self) {
        // SAFETY: this wrapper exclusively owns the non-null options handle.
        unsafe {
            GDALVectorTranslateOptionsFree(self.c_options);
        }
    }
}

impl TryFrom<Vec<&str>> for VectorTranslateOptions {
    type Error = GdalError;

    fn try_from(value: Vec<&str>) -> Result<Self> {
        VectorTranslateOptions::new(value)
    }
}

/// Converts simple features data between file formats.
///
/// Wraps [GDALVectorTranslate].
/// See the [program docs] for more details.
///
/// [GDALVectorTranslate]: https://gdal.org/api/gdal_utils.html#_CPPv419GDALVectorTranslatePKc12GDALDatasetHiP12GDALDatasetHPK26GDALVectorTranslateOptionsPi
/// [program docs]: https://gdal.org/programs/ogr2ogr.html
///
pub fn vector_translate(
    src: &Dataset,
    mut dest: DatasetDestination,
    options: Option<VectorTranslateOptions>,
) -> Result<Dataset> {
    let (psz_dest_option, h_dst_ds) = match &dest {
        DatasetDestination::Path(c_path) => (Some(c_path), null_mut()),
        DatasetDestination::Dataset { dataset, .. } => (None, dataset.c_dataset()),
    };

    let psz_dest = psz_dest_option.map(|x| x.as_ptr()).unwrap_or_else(null);

    let c_options = options
        .as_ref()
        .map(|x| x.c_options as *const GDALVectorTranslateOptions)
        .unwrap_or(null());

    let mut datasets_raw = [src.c_dataset()];
    // SAFETY: GDALVectorTranslate supports exactly one input. `src` keeps
    // this valid handle alive, and the one-element array matches the count.
    // Destination and option pointers remain valid for this synchronous call.
    let dataset_out = unsafe {
        GDALVectorTranslate(
            psz_dest,
            h_dst_ds,
            // only 1 supported currently
            1,
            datasets_raw.as_mut_ptr(),
            c_options,
            null_mut(),
        )
    };

    if dataset_out.is_null() {
        return Err(last_null_pointer_err("GDALVectorTranslate"));
    }

    // SAFETY: a successful call returned the destination handle; prevent
    // the moved-in destination from freeing that same handle twice.
    unsafe {
        dest.relinquish_dataset_ownership();
    }

    // SAFETY: the non-null result is now owned exclusively by this Dataset.
    let result = unsafe { Dataset::from_c_dataset(dataset_out) };

    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::{VectorTranslateOptions, vector_translate};
    use gdal::{
        DriverManager,
        spatial_ref::SpatialRef,
        vector::{Geometry, LayerAccess, LayerOptions},
    };

    #[test]
    fn translates_exactly_one_owned_source() -> anyhow::Result<()> {
        let temp = tempfile::tempdir()?;
        let source_path = temp.path().join("source.gpkg");
        let output_path = temp.path().join("output.geojson");
        let mut dataset =
            DriverManager::get_driver_by_name("GPKG")?.create_vector_only(&source_path)?;
        let srs = SpatialRef::from_epsg(4326)?;
        {
            let mut layer = dataset.create_layer(LayerOptions {
                name: "features",
                srs: Some(&srs),
                ty: gdal_sys::OGRwkbGeometryType::wkbPoint,
                ..Default::default()
            })?;
            layer.create_feature(Geometry::from_wkt("POINT (1 2)")?)?;
        }
        // Closing the writer commits the GPKG creation transaction before a
        // fresh reader is passed to the GDAL utility.
        drop(dataset);
        let dataset = gdal::Dataset::open(&source_path)?;
        assert_eq!(dataset.layer(0)?.feature_count(), 1);
        let output = vector_translate(
            &dataset,
            output_path.as_path().try_into()?,
            Some(VectorTranslateOptions::new(["-f", "GeoJSON"])?),
        )?;
        drop(output);
        let output = gdal::Dataset::open(&output_path)?;
        assert_eq!(output.layer(0)?.feature_count(), 1);
        Ok(())
    }
}
