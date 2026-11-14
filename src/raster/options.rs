use crate::operation::{Error, Result};

pub use gdal::raster::GdalDataType as DataType;

/// Interpolation or aggregation used by GDAL raster utilities.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum Resampling {
    /// Nearest source pixel, suitable for category codes.
    #[default]
    Nearest,
    /// Bilinear interpolation.
    Bilinear,
    /// Cubic convolution.
    Cubic,
    /// Cubic spline interpolation.
    CubicSpline,
    /// Lanczos interpolation.
    Lanczos,
    /// Mean of contributing pixels.
    Average,
    /// Most frequent category.
    Mode,
    /// Root mean square.
    Rms,
    /// Minimum contributing value (Warp only).
    Min,
    /// Maximum contributing value (Warp only).
    Max,
    /// Median contributing value (Warp only).
    Median,
    /// First quartile (Warp only).
    Q1,
    /// Third quartile (Warp only).
    Q3,
    /// Weighted sum (Warp only).
    Sum,
}
impl Resampling {
    pub(super) fn name(self) -> &'static str {
        match self {
            Self::Nearest => "near",
            Self::Bilinear => "bilinear",
            Self::Cubic => "cubic",
            Self::CubicSpline => "cubicspline",
            Self::Lanczos => "lanczos",
            Self::Average => "average",
            Self::Mode => "mode",
            Self::Rms => "rms",
            Self::Min => "min",
            Self::Max => "max",
            Self::Median => "med",
            Self::Q1 => "q1",
            Self::Q3 => "q3",
            Self::Sum => "sum",
        }
    }
    pub(super) fn validate_translate(self) -> Result<()> {
        if matches!(
            self,
            Self::Min | Self::Max | Self::Median | Self::Q1 | Self::Q3 | Self::Sum
        ) {
            Err(Error::invalid(
                "this resampling algorithm is only supported by Warp",
            ))
        } else {
            Ok(())
        }
    }
}

/// Pixel neighbors considered connected in category algorithms.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Connectivity {
    /// Shared edges only.
    #[default]
    Four,
    /// Shared edges or corners.
    Eight,
}
impl Connectivity {
    pub(super) fn number(self) -> i32 {
        match self {
            Self::Four => 4,
            Self::Eight => 8,
        }
    }
}

/// Validity mask used by band algorithms.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mask {
    /// GDAL's default validity mask, including NoData or alpha if present.
    #[default]
    Default,
    /// Treat every pixel as valid.
    None,
    /// Use a UInt8 source band as mask; zero means invalid, nonzero means valid.
    Band(usize),
}

pub(super) fn mask<'a>(
    dataset: &'a gdal::Dataset,
    band: &'a gdal::raster::RasterBand<'a>,
    choice: Mask,
) -> Result<Option<gdal::raster::RasterBand<'a>>> {
    match choice {
        Mask::Default => band
            .open_mask_band()
            .map(Some)
            .map_err(super::execute::processing_error),
        Mask::None => Ok(None),
        Mask::Band(index) => {
            let mask = source_band(dataset, index)?;
            if mask.band_type() != gdal::raster::GdalDataType::UInt8 {
                return Err(Error::invalid("explicit mask bands must have UInt8 pixels"));
            }
            Ok(Some(mask))
        }
    }
}

pub(super) fn source_band(
    dataset: &gdal::Dataset,
    index: usize,
) -> Result<gdal::raster::RasterBand<'_>> {
    use crate::operation::{ErrorKind, Stage};
    band(index)?;
    if index > dataset.raster_count() {
        return Err(Error::new(
            ErrorKind::InvalidArgument,
            Stage::Inspect,
            "selected band is outside the source raster",
        ));
    }
    dataset.rasterband(index).map_err(|error| {
        Error::new(
            ErrorKind::Processing,
            Stage::Inspect,
            "cannot access source band",
        )
        .with_source(error)
    })
}

pub(super) fn positive(value: f64, label: &str) -> Result<()> {
    if value.is_finite() && value > 0.0 {
        Ok(())
    } else {
        Err(Error::invalid(format!(
            "{label} must be positive and finite"
        )))
    }
}
pub(super) fn finite(value: f64, label: &str) -> Result<()> {
    if value.is_finite() {
        Ok(())
    } else {
        Err(Error::invalid(format!("{label} must be finite")))
    }
}
pub(super) fn bounds(values: [f64; 4]) -> Result<()> {
    if values.into_iter().all(f64::is_finite) && values[0] < values[2] && values[1] < values[3] {
        Ok(())
    } else {
        Err(Error::invalid(
            "bounds must have positive area and finite coordinates",
        ))
    }
}
pub(super) fn band(index: usize) -> Result<()> {
    if index == 0 || index > i32::MAX as usize {
        Err(Error::invalid(
            "band index must be positive and fit a GDAL integer",
        ))
    } else {
        Ok(())
    }
}
pub(super) fn size(width: u32, height: u32) -> Result<()> {
    if width == 0 || height == 0 || width > i32::MAX as u32 || height > i32::MAX as u32 {
        Err(Error::invalid(
            "raster dimensions must be positive and fit GDAL integers",
        ))
    } else {
        Ok(())
    }
}
pub(super) fn pair(args: &mut Vec<String>, key: &str, value: impl ToString) {
    args.extend([key.to_owned(), value.to_string()]);
}
pub(super) fn numbers<const N: usize>(args: &mut Vec<String>, key: &str, values: [f64; N]) {
    args.push(key.to_owned());
    args.extend(values.map(|v| v.to_string()));
}
pub(super) fn srs(definition: &str) -> Result<()> {
    crate::dataset::validate_text(definition, "spatial reference")?;
    gdal::spatial_ref::SpatialRef::from_definition(definition)
        .map(|_| ())
        .map_err(|e| Error::invalid("invalid spatial reference").with_source(e))
}

macro_rules! common {
    () => {
        /// Set the output driver. Default is documented on the builder.
        pub fn driver(mut self, driver: impl Into<String>) -> Self {
            self.job.driver = driver.into();
            self
        }
        /// Set a driver creation option, replacing the same case-insensitive key.
        pub fn creation_option(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
            self.job.creation.set(key.into(), value.into());
            self
        }
        /// Apply thread-local GDAL configuration until all datasets are closed.
        pub fn gdal_options(mut self, options: crate::config::gdal::GdalOptions) -> Self {
            self.job.config = options;
            self
        }
    };
}
pub(super) use common;
macro_rules! debug_builder {
    ($name:ident) => {
        impl std::fmt::Debug for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.debug_struct(stringify!($name))
                    .field("source_count", &self.job.sources.len())
                    .field("driver", &self.job.driver)
                    .finish_non_exhaustive()
            }
        }
    };
}
pub(super) use debug_builder;
