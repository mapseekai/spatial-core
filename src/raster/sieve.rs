use super::{
    Connectivity, Mask,
    execute::{Job, check_status, processing_error},
    options::{self, common, debug_builder},
};
use crate::{
    dataset::{Kind, Source},
    operation::{Control, Error, Report, Result},
};
use gdal::raster::GdalDataType;
use std::{path::Path, ptr};

/// Copy to a new GTiff, then replace small connected integer regions.
/// Accepts 8-, 16- and 32-bit category bands. Other bands are unchanged.
#[derive(Clone)]
#[must_use]
pub struct Sieve {
    job: Job,
    band: usize,
    mask: Mask,
    threshold: u32,
    connectivity: Connectivity,
}
debug_builder!(Sieve);
impl Sieve {
    /// Select the minimum surviving region size in pixels (positive).
    pub fn new(source: impl Into<Source>, output: impl AsRef<Path>, threshold: u32) -> Self {
        Self {
            job: Job::new(source, output, "GTiff"),
            band: 1,
            mask: Mask::default(),
            threshold,
            connectivity: Connectivity::default(),
        }
    }
    common!();
    /// Select a one-based category band.
    pub fn band(mut self, value: usize) -> Self {
        self.band = value;
        self
    }
    /// Set the validity mask used by the algorithm.
    pub fn mask(mut self, value: Mask) -> Self {
        self.mask = value;
        self
    }
    /// Select edge or edge-and-corner connectivity.
    pub fn connectivity(mut self, value: Connectivity) -> Self {
        self.connectivity = value;
        self
    }
    /// Copy, process and close the output; never edits the source.
    pub fn run(self) -> Result<Report> {
        self.run_with(&mut Control::default())
    }
    /// Execute with borrowed progress and cancellation.
    pub fn run_with(self, control: &mut Control<'_>) -> Result<Report> {
        options::band(self.band)?;
        if self.threshold == 0 || self.threshold > i32::MAX as u32 {
            return Err(Error::invalid(
                "sieve threshold must be positive and fit a GDAL integer",
            ));
        }
        self.job
            .run(Kind::Raster, false, control, move |prepared, progress| {
                let source = &prepared.inputs[0];
                let band = options::source_band(source, self.band)?;
                if !matches!(
                    band.band_type(),
                    GdalDataType::UInt8
                        | GdalDataType::Int8
                        | GdalDataType::UInt16
                        | GdalDataType::Int16
                        | GdalDataType::UInt32
                        | GdalDataType::Int32
                ) {
                    return Err(Error::invalid(
                        "sieve requires 8-, 16- or 32-bit integer category codes",
                    ));
                }
                let _ = options::mask(source, &band, self.mask)?;
                prepared.copy(progress)?;
                let source = &prepared.inputs[0];
                let band = options::source_band(source, self.band)?;
                let mask = options::mask(source, &band, self.mask)?;
                let target = prepared
                    .output
                    .as_ref()
                    .expect("copied output")
                    .rasterband(self.band)
                    .map_err(processing_error)?;
                // SAFETY: borrowed input/mask/output bands remain live and the
                // callback is synchronous; no Rust pointer is retained after return.
                let status = unsafe {
                    gdal_sys::GDALSieveFilter(
                        band.c_rasterband(),
                        mask.as_ref().map_or(ptr::null_mut(), |m| m.c_rasterband()),
                        target.c_rasterband(),
                        self.threshold as i32,
                        self.connectivity.number(),
                        ptr::null_mut(),
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
    fn removes_small_regions_without_mutating_source() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let src = dir.path().join("classes.tif");
        fixture(&src, 3, 3, &[1u8, 1, 1, 1, 9, 1, 1, 1, 1])?;
        let before = std::fs::read(&src)?;
        let out = dir.path().join("clean.tif");
        let _ = Sieve::new(&src, &out, 2).run()?;
        assert_eq!(pixels(&out)?, [1.; 9]);
        assert_eq!(before, std::fs::read(src)?);
        let float = dir.path().join("float.tif");
        fixture(&float, 1, 1, &[1.5f32])?;
        assert!(
            Sieve::new(&float, dir.path().join("bad.tif"), 2)
                .run()
                .is_err()
        );
        Ok(())
    }

    #[test]
    fn signed_byte_categories_are_preserved() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let src = dir.path().join("signed.tif");
        fixture(&src, 3, 3, &[1i8, 1, 1, 1, -9, 1, 1, 1, 1])?;
        let before = std::fs::read(&src)?;
        let out = dir.path().join("sieved.tif");
        let _ = Sieve::new(&src, &out, 2).run()?;
        assert_eq!(pixels(&out)?, [1.; 9]);
        assert_eq!(
            gdal::Dataset::open(out)?.rasterband(1)?.band_type(),
            GdalDataType::Int8
        );
        assert_eq!(before, std::fs::read(src)?);
        Ok(())
    }
}
