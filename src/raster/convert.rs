//! Progress callbacks returning false or panicking make the operation fail.
//! Failed operations close owned datasets but may leave partial output files;
//! in-place changes are not rolled back. Callers manage these outputs.

pub use crate::gdal_ext::raster::dem::{DemMode, dem_to_path_with_progress};
use crate::gdal_ext::{
    destination::DatasetDestination,
    raster::{
        translate::{TranslateOptions, translate_with_progress},
        warp::{WarpOptions, warp_with_progress},
    },
};
use anyhow::Result;
use gdal::Dataset;
use std::path::Path;

#[derive(Debug, Clone, PartialEq)]
pub struct GdalOperationProgress {
    pub fraction: f64,
    pub message: String,
}

pub fn warp_to_path_with_progress<F>(
    source: &Dataset,
    target: &Path,
    args: Vec<&str>,
    progress: F,
) -> Result<Dataset>
where
    F: FnMut(GdalOperationProgress) -> bool,
{
    let options = Some(WarpOptions::new(args)?);
    let dest = DatasetDestination::path(target)?;
    Ok(warp_with_progress(source, dest, options, progress)?)
}

pub fn translate_to_path_with_progress<F>(
    source: &Dataset,
    target: &Path,
    args: Vec<&str>,
    progress: F,
) -> Result<Dataset>
where
    F: FnMut(GdalOperationProgress) -> bool,
{
    let options = Some(TranslateOptions::new(args)?);
    let dest = DatasetDestination::path(target)?;
    Ok(translate_with_progress(source, dest, options, progress)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gdal::{DriverManager, raster::Buffer, spatial_ref::SpatialRef};
    use tempfile::TempDir;

    fn create_epsg4326_source(path: &Path) {
        const SIZE: usize = 256;

        let driver = DriverManager::get_driver_by_name("GTiff").expect("GTiff driver");
        let mut ds = driver
            .create_with_band_type::<u8, _>(path, SIZE, SIZE, 1)
            .expect("create source");
        ds.set_geo_transform(&[-10.0, 0.01, 0.0, 10.0, 0.0, -0.01])
            .expect("geo transform");
        ds.set_spatial_ref(&SpatialRef::from_epsg(4326).expect("EPSG:4326"))
            .expect("spatial ref");

        let data: Vec<u8> = (0..(SIZE * SIZE)).map(|i| (i % 251) as u8).collect();
        let mut buffer = Buffer::new((SIZE, SIZE), data);
        ds.rasterband(1)
            .expect("band 1")
            .write((0, 0), (SIZE, SIZE), &mut buffer)
            .expect("write source data");
        ds.flush_cache().expect("flush");
    }

    #[test]
    fn warp_to_path_reports_progress() {
        let dir = TempDir::new().expect("temp dir");
        let src = dir.path().join("source.tif");
        let dst = dir.path().join("warped.tif");
        create_epsg4326_source(&src);
        let src_ds = Dataset::open(&src).expect("open source");

        let mut calls = Vec::new();
        let _warped = warp_to_path_with_progress(
            &src_ds,
            &dst,
            vec![
                "-t_srs",
                "EPSG:3857",
                "-r",
                "bilinear",
                "-of",
                "GTiff",
                "-co",
                "TILED=YES",
                "-overwrite",
            ],
            |progress| {
                calls.push(progress.fraction);
                true
            },
        )
        .expect("warp");

        assert!(!calls.is_empty(), "warp progress should be invoked");
        assert!(calls.iter().any(|fraction| *fraction >= 0.99));
    }

    #[test]
    fn translate_to_path_reports_progress() {
        let dir = TempDir::new().expect("temp dir");
        let src = dir.path().join("source.tif");
        let dst = dir.path().join("output.tif");
        create_epsg4326_source(&src);
        let src_ds = Dataset::open(&src).expect("open source");

        let mut calls = Vec::new();
        let _translated =
            translate_to_path_with_progress(&src_ds, &dst, vec!["-of", "GTiff"], |progress| {
                calls.push(progress.fraction);
                true
            })
            .expect("translate");

        assert!(!calls.is_empty(), "translate progress should be invoked");
        assert!(calls.iter().any(|fraction| *fraction >= 0.99));
    }

    #[test]
    fn native_progress_rejection_and_panic_return_errors() -> Result<()> {
        let dir = TempDir::new()?;
        let path = dir.path().join("source.tif");
        create_epsg4326_source(&path);
        let source = Dataset::open(&path)?;
        for (format, warp) in [
            ("VRT", false),
            ("VRT", true),
            ("GTiff", false),
            ("GTiff", true),
        ] {
            for panics in [false, true] {
                for threshold in [0.0, 1.0] {
                    let target = dir
                        .path()
                        .join(format!("{format}-{warp}-{panics}-{threshold}"));
                    let mut rejected = false;
                    let callback = |progress: GdalOperationProgress| {
                        if progress.fraction < threshold {
                            return true;
                        }
                        rejected = true;
                        assert!(!panics, "callback failed");
                        false
                    };
                    let result = if warp {
                        warp_to_path_with_progress(
                            &source,
                            &target,
                            vec!["-of", format, "-t_srs", "EPSG:3857"],
                            callback,
                        )
                    } else {
                        translate_to_path_with_progress(
                            &source,
                            &target,
                            vec!["-of", format],
                            callback,
                        )
                    };
                    // GDAL 3.8 can create VRTs without progress; GTiff must exercise cancellation.
                    if format == "GTiff" {
                        assert!(rejected, "the native operation must invoke the callback");
                    }
                    assert_eq!(
                        result.is_err(),
                        rejected,
                        "format={format}, warp={warp}, panics={panics}, at={threshold}"
                    );
                }
            }
        }
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn unix_output_paths_preserve_bytes_without_overwriting_lossy_aliases() -> Result<()> {
        use std::{ffi::OsString, os::unix::ffi::OsStringExt};

        let dir = TempDir::new()?;
        let source_path = dir.path().join("source.tif");
        create_epsg4326_source(&source_path);
        let source = Dataset::open(&source_path)?;
        for operation in ["translate", "warp", "dem"] {
            let mut name = operation.as_bytes().to_vec();
            name.extend_from_slice(b"_\xff.tif");
            let target = dir.path().join(OsString::from_vec(name));
            let lossy = std::path::PathBuf::from(target.to_string_lossy().as_ref());
            std::fs::write(&lossy, b"existing file")?;
            // Some Unix filesystems reject invalid UTF-8 instead of preserving raw bytes.
            let supported = match std::fs::File::create(&target) {
                Ok(file) => {
                    drop(file);
                    std::fs::remove_file(&target)?;
                    true
                }
                Err(error) if matches!(error.raw_os_error(), Some(libc::EILSEQ | libc::EINVAL)) => {
                    false
                }
                Err(error) => return Err(error.into()),
            };
            let result = match operation {
                "translate" => {
                    translate_to_path_with_progress(&source, &target, vec!["-of", "GTiff"], |_| {
                        true
                    })
                }
                "warp" => {
                    warp_to_path_with_progress(&source, &target, vec!["-of", "GTiff"], |_| true)
                }
                _ => dem_to_path_with_progress(&source, &target, DemMode::Hillshade, &[], |_| true)
                    .map_err(Into::into),
            };
            if supported {
                let output = result?;
                assert_eq!(output.raster_size(), source.raster_size());
                drop(output);
                assert!(target.is_file(), "{operation} changed the output path");
            } else {
                assert!(result.is_err(), "{operation} accepted an unsupported path");
            }
            assert_eq!(std::fs::read(&lossy)?, b"existing file");
        }
        Ok(())
    }
}

/// Polygonize classified pixels using the validity mask and a cancellable native callback.
/// Integer GDALPolygonize preserves Int64 category values without float32 rounding.
pub fn polygonize_to_path_with_progress<F>(
    source: &Dataset,
    band_index: usize,
    target: &Path,
    eight_connected: bool,
    field: &str,
    mut progress: F,
) -> Result<Dataset>
where
    F: FnMut(GdalOperationProgress) -> bool,
{
    use crate::gdal_ext::raster::progress::{GDAL_PROGRESS_TRAMPOLINE, ProgressCallbackState};
    use gdal::{
        DriverManager,
        cpl::CslStringList,
        vector::{LayerAccess, LayerOptions, OGRFieldType},
    };
    let band = source.rasterband(band_index)?;
    let mask = band.open_mask_band()?;
    let mut output = DriverManager::get_driver_by_name("GPKG")?.create_vector_only(target)?;
    let srs = source.spatial_ref()?;
    let layer = output.create_layer(LayerOptions {
        name: "polygons",
        srs: Some(&srs),
        ..Default::default()
    })?;
    layer.create_defn_fields(&[(field, OGRFieldType::OFTInteger64)])?;
    let mut options = CslStringList::new();
    if eight_connected {
        options.set_name_value("8CONNECTED", "8")?;
    }
    let mut callback = ProgressCallbackState::new(&mut progress);
    // SAFETY: all GDAL handles and the callback remain alive for this synchronous
    // operation; options are null terminated and callback trampoline catches panics.
    let result = unsafe {
        gdal_sys::GDALPolygonize(
            band.c_rasterband(),
            mask.c_rasterband(),
            layer.c_layer(),
            0,
            options.as_ptr(),
            GDAL_PROGRESS_TRAMPOLINE,
            callback.user_data(),
        )
    };
    callback.check()?;
    if result != gdal_sys::CPLErr::CE_None {
        anyhow::bail!("GDAL polygonize failed or was cancelled");
    }
    output.flush_cache()?;
    Ok(output)
}

/// Burn one geometry with cancellation during GDAL's raster chunks.
pub fn rasterize_geometry_with_progress<F>(
    target: &mut Dataset,
    geometry: &gdal::vector::Geometry,
    value: f64,
    all_touched: bool,
    mut progress: F,
) -> Result<()>
where
    F: FnMut(GdalOperationProgress) -> bool,
{
    use crate::gdal_ext::raster::progress::{GDAL_PROGRESS_TRAMPOLINE, ProgressCallbackState};
    use gdal::cpl::CslStringList;
    if target.raster_count() != 1 || !value.is_finite() {
        anyhow::bail!("rasterize requires one target band and a finite value");
    }
    let mut options = CslStringList::new();
    options.set_name_value("ALL_TOUCHED", if all_touched { "TRUE" } else { "FALSE" })?;
    options.set_name_value("CHUNKYSIZE", "256")?;
    let mut callback = ProgressCallbackState::new(&mut progress);
    let band = 1;
    // SAFETY: the geometry, dataset, options, callback and scalar arrays outlive
    // the synchronous call. GDAL borrows geometry handles and does not own them.
    let result = unsafe {
        let geometry = geometry.c_geometry();
        gdal_sys::GDALRasterizeGeometries(
            target.c_dataset(),
            1,
            &band,
            1,
            &geometry,
            None,
            std::ptr::null_mut(),
            &value,
            options.as_ptr(),
            GDAL_PROGRESS_TRAMPOLINE,
            callback.user_data(),
        )
    };
    callback.check()?;
    if result != gdal_sys::CPLErr::CE_None {
        anyhow::bail!("GDAL rasterize failed or was cancelled");
    }
    Ok(())
}

#[cfg(test)]
mod analysis_tests {
    use super::*;
    use gdal::{DriverManager, raster::Buffer, spatial_ref::SpatialRef, vector::LayerAccess};
    #[test]
    fn polygonize_masks_nodata_and_respects_connectivity() -> Result<()> {
        let dir = tempfile::TempDir::new()?;
        let mut source = DriverManager::get_driver_by_name("MEM")?
            .create_with_band_type::<f64, _>("", 2, 2, 1)?;
        source.set_spatial_ref(&SpatialRef::from_epsg(3857)?)?;
        source.set_geo_transform(&[0., 1., 0., 2., 0., -1.])?;
        source.rasterband(1)?.set_no_data_value(Some(-9999.))?;
        source.rasterband(1)?.write(
            (0, 0),
            (2, 2),
            &mut Buffer::new((2, 2), vec![7., -9999., -9999., 7.]),
        )?;
        let four = polygonize_to_path_with_progress(
            &source,
            1,
            &dir.path().join("four.gpkg"),
            false,
            "value",
            |_| true,
        )?;
        assert_eq!(four.layer(0)?.feature_count(), 2);
        let eight = polygonize_to_path_with_progress(
            &source,
            1,
            &dir.path().join("eight.gpkg"),
            true,
            "value",
            |_| true,
        )?;
        assert_eq!(eight.layer(0)?.feature_count(), 1);
        assert!(
            polygonize_to_path_with_progress(
                &source,
                1,
                &dir.path().join("cancelled.gpkg"),
                false,
                "value",
                |_| false
            )
            .is_err()
        );
        Ok(())
    }
    #[test]
    fn rasterize_burns_values_and_checks_cancellation() -> Result<()> {
        let mut target = DriverManager::get_driver_by_name("MEM")?
            .create_with_band_type::<f64, _>("", 2, 2, 1)?;
        target.set_geo_transform(&[0., 1., 0., 2., 0., -1.])?;
        let geometry = gdal::vector::Geometry::from_wkt("POLYGON((0 0,2 0,2 2,0 2,0 0))")?;
        rasterize_geometry_with_progress(&mut target, &geometry, 9., false, |_| true)?;
        assert_eq!(
            target
                .rasterband(1)?
                .read_as::<f64>((0, 0), (2, 2), (2, 2), None)?
                .data(),
            &[9., 9., 9., 9.]
        );
        assert!(
            rasterize_geometry_with_progress(&mut target, &geometry, 9., false, |_| false).is_err()
        );
        Ok(())
    }
}

/// Fill masked pixels of a single-band dataset in place, with native cancellation.
/// The caller controls validity through the band's mask and preserves original values.
pub fn fill_nodata_with_progress<F>(
    target: &mut Dataset,
    max_distance: f64,
    smoothing_iterations: u32,
    mut progress: F,
) -> Result<()>
where
    F: FnMut(GdalOperationProgress) -> bool,
{
    use crate::gdal_ext::raster::progress::{GDAL_PROGRESS_TRAMPOLINE, ProgressCallbackState};
    if target.raster_count() != 1
        || !max_distance.is_finite()
        || max_distance <= 0.
        || smoothing_iterations > 100
    {
        anyhow::bail!(
            "fill nodata requires one band, a positive finite distance and 0..100 smoothing iterations"
        );
    }
    let band = target.rasterband(1)?;
    let mask = band.open_mask_band()?;
    let mut callback = ProgressCallbackState::new(&mut progress);
    // SAFETY: borrowed band/mask handles and callback remain alive throughout
    // this synchronous GDAL operation; the callback trampoline catches panics.
    let result = unsafe {
        gdal_sys::GDALFillNodata(
            band.c_rasterband(),
            mask.c_rasterband(),
            max_distance,
            0,
            smoothing_iterations as i32,
            std::ptr::null_mut(),
            GDAL_PROGRESS_TRAMPOLINE,
            callback.user_data(),
        )
    };
    callback.check()?;
    if result != gdal_sys::CPLErr::CE_None {
        anyhow::bail!("GDAL fill nodata failed or was cancelled");
    }
    Ok(())
}

/// Sieve integer class codes of a single-band dataset in place with its validity mask.
/// Callers should encode categories into small integer codes before calling GDAL.
pub fn sieve_with_progress<F>(
    target: &mut Dataset,
    threshold: i32,
    eight_connected: bool,
    mut progress: F,
) -> Result<()>
where
    F: FnMut(GdalOperationProgress) -> bool,
{
    use crate::gdal_ext::raster::progress::{GDAL_PROGRESS_TRAMPOLINE, ProgressCallbackState};
    if target.raster_count() != 1 || threshold < 1 {
        anyhow::bail!("sieve requires one band and a positive threshold");
    }
    let band = target.rasterband(1)?;
    let mask = band.open_mask_band()?;
    let mut callback = ProgressCallbackState::new(&mut progress);
    // SAFETY: GDAL explicitly supports an in-place destination. All handles and
    // the callback outlive the synchronous call; no pointer is retained by GDAL.
    let result = unsafe {
        gdal_sys::GDALSieveFilter(
            band.c_rasterband(),
            mask.c_rasterband(),
            band.c_rasterband(),
            threshold,
            if eight_connected { 8 } else { 4 },
            std::ptr::null_mut(),
            GDAL_PROGRESS_TRAMPOLINE,
            callback.user_data(),
        )
    };
    callback.check()?;
    if result != gdal_sys::CPLErr::CE_None {
        anyhow::bail!("GDAL sieve failed or was cancelled");
    }
    Ok(())
}

/// Generate line contours in a CRS-preserving GeoPackage with a real elevation field.
/// Input invalid pixels must already be normalized to the band's explicit NoData.
pub fn contour_to_path_with_progress<F>(
    source: &Dataset,
    target: &Path,
    interval: f64,
    base: f64,
    field: &str,
    mut progress: F,
) -> Result<Dataset>
where
    F: FnMut(GdalOperationProgress) -> bool,
{
    use crate::gdal_ext::raster::progress::{GDAL_PROGRESS_TRAMPOLINE, ProgressCallbackState};
    use gdal::{
        DriverManager,
        cpl::CslStringList,
        vector::{LayerAccess, LayerOptions, OGRFieldType, OGRwkbGeometryType},
    };
    if source.raster_count() != 1 || !interval.is_finite() || interval <= 0. || !base.is_finite() {
        anyhow::bail!("contour requires one band, a positive finite interval and a finite base");
    }
    let band = source.rasterband(1)?;
    let mut output = DriverManager::get_driver_by_name("GPKG")?.create_vector_only(target)?;
    let srs = source.spatial_ref()?;
    let layer = output.create_layer(LayerOptions {
        name: "contours",
        srs: Some(&srs),
        ty: OGRwkbGeometryType::wkbLineString,
        ..Default::default()
    })?;
    layer.create_defn_fields(&[(field, OGRFieldType::OFTReal)])?;
    let mut options = CslStringList::new();
    options.set_name_value("LEVEL_INTERVAL", &interval.to_string())?;
    options.set_name_value("LEVEL_BASE", &base.to_string())?;
    options.set_name_value("ELEV_FIELD", "0")?;
    if let Some(nodata) = band.no_data_value() {
        options.set_name_value("NODATA", &nodata.to_string())?;
    }
    let mut callback = ProgressCallbackState::new(&mut progress);
    // SAFETY: dataset/layer handles, C string options and progress state remain
    // alive for the synchronous call. The callback trampoline catches panics.
    let result = unsafe {
        gdal_sys::GDALContourGenerateEx(
            band.c_rasterband(),
            layer.c_layer(),
            options.as_ptr(),
            GDAL_PROGRESS_TRAMPOLINE,
            callback.user_data(),
        )
    };
    callback.check()?;
    if result != gdal_sys::CPLErr::CE_None {
        anyhow::bail!("GDAL contour failed or was cancelled");
    }
    output.flush_cache()?;
    Ok(output)
}

#[cfg(test)]
mod cleanup_tests {
    use super::*;
    use gdal::{DriverManager, raster::Buffer, spatial_ref::SpatialRef, vector::LayerAccess};

    fn source(w: usize, h: usize, pixels: Vec<f64>) -> Result<Dataset> {
        let mut ds = DriverManager::get_driver_by_name("MEM")?
            .create_with_band_type::<f64, _>("", w, h, 1)?;
        ds.set_spatial_ref(&SpatialRef::from_epsg(3857)?)?;
        ds.set_geo_transform(&[100., 2., 0., 200., 0., -2.])?;
        ds.rasterband(1)?.set_no_data_value(Some(f64::NAN))?;
        ds.rasterband(1)?
            .write((0, 0), (w, h), &mut Buffer::new((w, h), pixels))?;
        Ok(ds)
    }
    #[test]
    fn cleanup_native_callbacks_can_cancel() -> Result<()> {
        let dir = tempfile::TempDir::new()?;
        let mut ds = source(3, 3, vec![1., 2., 3., 1., f64::NAN, 3., 1., 2., 3.])?;
        assert!(fill_nodata_with_progress(&mut ds, 100., 0, |_| false).is_err());
        assert!(sieve_with_progress(&mut ds, 2, false, |_| false).is_err());
        assert!(
            contour_to_path_with_progress(
                &ds,
                &dir.path().join("cancelled.gpkg"),
                1.,
                0.,
                "height",
                |_| false
            )
            .is_err()
        );
        Ok(())
    }

    #[test]
    fn contour_rejects_cancellation_and_panic_at_completion() -> Result<()> {
        let dir = tempfile::TempDir::new()?;
        let ds = source(2, 2, vec![0., 20., 0., 20.])?;
        for panics in [false, true] {
            let mut rejected = false;
            let result = contour_to_path_with_progress(
                &ds,
                &dir.path().join(format!("cancelled-{panics}.gpkg")),
                10.,
                5.,
                "height",
                |progress| {
                    if progress.fraction < 1.0 {
                        return true;
                    }
                    rejected = true;
                    assert!(!panics, "callback failed");
                    false
                },
            );
            assert!(rejected);
            assert!(result.is_err(), "panics={panics}");
        }
        Ok(())
    }

    #[test]
    fn dem_rejects_progress_cancellation_and_panic() -> Result<()> {
        let dir = tempfile::TempDir::new()?;
        let ds = source(3, 3, vec![1., 2., 3., 1., 2., 3., 1., 2., 3.])?;
        for panics in [false, true] {
            let mut called = false;
            let result = dem_to_path_with_progress(
                &ds,
                &dir.path().join(format!("dem-{panics}.tif")),
                DemMode::Hillshade,
                &[],
                |_| {
                    called = true;
                    assert!(!panics, "callback failed");
                    false
                },
            );
            assert!(called);
            assert!(result.is_err());
        }
        Ok(())
    }
    #[test]
    fn contour_native_preserves_crs_locations_elevations_and_nan_holes() -> Result<()> {
        let dir = tempfile::TempDir::new()?;
        let ds = source(2, 2, vec![0., 20., 0., 20.])?;
        let mut progress = Vec::new();
        let out = contour_to_path_with_progress(
            &ds,
            &dir.path().join("contour.gpkg"),
            10.,
            5.,
            "height",
            |p| {
                progress.push(p.fraction);
                true
            },
        )?;
        assert!(!progress.is_empty());
        let mut layer = out.layer(0)?;
        assert_eq!(layer.spatial_ref().unwrap().auth_code()?, 3857);
        let mut elevations = Vec::new();
        for feature in layer.features() {
            let elevation = feature.field_as_double(0)?.unwrap();
            elevations.push(elevation);
            let geometry = feature.geometry().unwrap();
            assert_eq!(geometry.geometry_name(), "LINESTRING");
            let bounds = geometry.envelope();
            let expected = 101. + elevation / 10.;
            assert!((bounds.MinX - expected).abs() < 1e-5);
            assert!((bounds.MaxX - expected).abs() < 1e-5);
        }
        elevations.sort_by(f64::total_cmp);
        assert_eq!(elevations, vec![5., 15.]);
        let ds = source(
            3,
            3,
            vec![0., 20., f64::NAN, 0., 20., f64::NAN, 0., 20., f64::NAN],
        )?;
        let out = contour_to_path_with_progress(
            &ds,
            &dir.path().join("masked.gpkg"),
            10.,
            5.,
            "height",
            |_| true,
        )?;
        let mut layer = out.layer(0)?;
        assert_eq!(layer.feature_count(), 2);
        for feature in layer.features() {
            let geometry = feature.geometry().unwrap();
            assert!(geometry.envelope().MaxX < 104.);
        }
        let ds = source(2, 2, vec![f64::NAN; 4])?;
        let out = contour_to_path_with_progress(
            &ds,
            &dir.path().join("empty.gpkg"),
            10.,
            0.,
            "height",
            |_| true,
        )?;
        assert_eq!(out.layer(0)?.feature_count(), 0);
        Ok(())
    }
}
