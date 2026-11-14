use super::{
    Mask,
    execute::{Job, check_status, processing_error, string_list},
    options::{self, common, debug_builder},
};
use crate::{
    dataset::{Kind, Options, Source},
    operation::{Control, Error, Report, Result},
};
use std::{path::Path, ptr};

/// Copy a raster to a new GTiff, then interpolate invalid pixels in one band.
/// Other bands and independent validity masks retain their copied values.
/// Interpolation requires a Float32, 8-bit or 16-bit band: GDAL's working buffer
/// otherwise rounds even valid pixels. Explicitly convert wider types first.
#[derive(Clone)]
#[must_use]
pub struct FillNodata {
    job: Job,
    band: usize,
    mask: Mask,
    distance: f64,
    smoothing: u32,
    options: Options,
}
debug_builder!(FillNodata);
impl FillNodata {
    /// Default: band 1, validity mask, 100-pixel search radius, no smoothing.
    pub fn new(source: impl Into<Source>, output: impl AsRef<Path>) -> Self {
        Self {
            job: Job::new(source, output, "GTiff"),
            band: 1,
            mask: Mask::default(),
            distance: 100.,
            smoothing: 0,
            options: Options::default(),
        }
    }
    common!();
    /// Select a one-based target band.
    pub fn band(mut self, value: usize) -> Self {
        self.band = value;
        self
    }
    /// Select validity from the original input; None treats all pixels as valid.
    pub fn mask(mut self, value: Mask) -> Self {
        self.mask = value;
        self
    }
    /// Set the maximum interpolation radius in pixels.
    pub fn max_distance(mut self, value: f64) -> Self {
        self.distance = value;
        self
    }
    /// Set smoothing iterations; values must fit a native integer.
    pub fn smoothing_iterations(mut self, value: u32) -> Self {
        self.smoothing = value;
        self
    }
    /// Set a GDALFillNodata algorithm option, such as INTERPOLATION (GDAL 3.9+).
    pub fn algorithm_option(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.options.set(key.into(), value.into());
        self
    }
    /// Copy, process and close the output; never edits the source.
    pub fn run(self) -> Result<Report> {
        self.run_with(&mut Control::default())
    }
    /// Execute with borrowed progress and cancellation.
    pub fn run_with(self, control: &mut Control<'_>) -> Result<Report> {
        options::band(self.band)?;
        options::positive(self.distance, "search distance")?;
        self.options.validate()?;
        if self.smoothing > i32::MAX as u32 {
            return Err(Error::invalid(
                "smoothing iterations exceed GDAL's integer limit",
            ));
        }
        if self.options.get("INTERPOLATION").is_some()
            && crate::gdal_ext::execution::supported_version()? < 3_090_000
        {
            return Err(Error::unsupported(
                "INTERPOLATION requires GDAL 3.9 or later",
            ));
        }
        self.job
            .run(Kind::Raster, false, control, move |prepared, progress| {
                let band = options::source_band(&prepared.inputs[0], self.band)?;
                let _ = options::mask(&prepared.inputs[0], &band, self.mask)?;
                // An all-valid band needs no interpolation or Float32 round-trip.
                let all_valid = self.mask == Mask::None
                    || self.mask == Mask::Default && band.mask_flags().map_err(processing_error)?.is_all_valid();
                if !all_valid && !matches!(band.band_type(),
                    gdal::raster::GdalDataType::UInt8 | gdal::raster::GdalDataType::Int8
                    | gdal::raster::GdalDataType::UInt16 | gdal::raster::GdalDataType::Int16
                    | gdal::raster::GdalDataType::Float32)
                {
                    return Err(Error::unsupported("fill-nodata's Float32 buffer cannot preserve this pixel type; convert it explicitly first"));
                }
                let args = string_list(&self.options)?;
                if !all_valid {
                    prepared.isolate_scratch()?;
                }
                prepared.copy(progress)?;
                if all_valid {
                    return Ok(());
                }
                let source = &prepared.inputs[0];
                let input = options::source_band(source, self.band)?;
                let mask = options::mask(source, &input, self.mask)?;
                let band = prepared
                    .output
                    .as_ref()
                    .expect("copied output")
                    .rasterband(self.band)
                    .map_err(processing_error)?;
                // SAFETY: source mask and writable output band are distinct owned
                // datasets; options and callback remain live until GDAL returns.
                let status = unsafe {
                    gdal_sys::GDALFillNodata(
                        band.c_rasterband(),
                        mask.as_ref().map_or(ptr::null_mut(), |m| m.c_rasterband()),
                        self.distance,
                        0,
                        self.smoothing as i32,
                        args.as_ptr(),
                        progress.native_callback(),
                        progress.user_data(),
                    )
                };
                check_status(status)
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::raster::test_support::{fixture, pixels};
    #[test]
    fn fills_a_copy_and_leaves_original_bytes_unchanged() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let src = dir.path().join("hole.tif");
        fixture(&src, 3, 3, &[1f32, 2., 3., 1., -9999., 3., 1., 2., 3.])?;
        let ds = gdal::Dataset::open_ex(
            &src,
            gdal::DatasetOptions {
                open_flags: gdal::GdalOpenFlags::GDAL_OF_UPDATE,
                ..Default::default()
            },
        )?;
        ds.rasterband(1)?.set_no_data_value(Some(-9999.))?;
        ds.close()?;
        let before = std::fs::read(&src)?;
        let out = dir.path().join("filled.tif");
        let _ = FillNodata::new(&src, &out).max_distance(10.).run()?;
        let data = pixels(&out)?;
        assert!(data[4] > 1. && data[4] < 3.);
        assert_eq!(data[0], 1.);
        assert_eq!(before, std::fs::read(&src)?);
        let untouched = dir.path().join("untouched.tif");
        let _ = FillNodata::new(&src, &untouched).mask(Mask::None).run()?;
        assert_eq!(pixels(&untouched)?[4], -9999.);
        Ok(())
    }

    #[test]
    fn wider_pixels_are_not_silently_rounded_by_interpolation() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let src = dir.path().join("precise.tif");
        fixture(&src, 3, 1, &[16777217f64; 3])?;
        let out = dir.path().join("valid.tif");
        let _ = FillNodata::new(&src, &out).run()?;
        assert_eq!(pixels(&out)?, [16777217.; 3]);
        let ds = gdal::Dataset::open_ex(
            &src,
            gdal::DatasetOptions {
                open_flags: gdal::GdalOpenFlags::GDAL_OF_UPDATE,
                ..Default::default()
            },
        )?;
        ds.rasterband(1)?.set_no_data_value(Some(-9999.))?;
        ds.close()?;
        let before = std::fs::read(&src)?;
        let out = dir.path().join("rejected.tif");
        let error = FillNodata::new(&src, &out).run().unwrap_err();
        assert_eq!(error.kind(), crate::operation::ErrorKind::Unsupported);
        assert_eq!(
            error.output_state(),
            crate::operation::OutputState::NotModified
        );
        assert!(!out.exists());
        assert_eq!(before, std::fs::read(src)?);
        Ok(())
    }
}
