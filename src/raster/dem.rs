use super::{
    execute::Job,
    options::{self, common, debug_builder, pair},
};
use crate::{
    dataset::{Kind, Source},
    gdal_ext::raster::utility,
    operation::{Control, Error, Report, Result},
};
use std::{
    ffi::CString,
    path::{Path, PathBuf},
};

/// Native GDAL terrain operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum DemMode {
    /// Shaded relief.
    Hillshade,
    /// Slope angle, degrees by default.
    Slope,
    /// Aspect angle, compass convention by default.
    Aspect,
    /// Local elevation range.
    Roughness,
    /// Topographic position index.
    Tpi,
    /// Terrain ruggedness index.
    Tri,
    /// Apply a color table file to elevations.
    ColorRelief,
}
impl DemMode {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Hillshade => "hillshade",
            Self::Slope => "slope",
            Self::Aspect => "aspect",
            Self::Roughness => "roughness",
            Self::Tpi => "TPI",
            Self::Tri => "TRI",
            Self::ColorRelief => "color-relief",
        }
    }
}
/// Derivative or terrain ruggedness calculation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum DemAlgorithm {
    /// Horn derivatives (hillshade, slope, aspect).
    Horn,
    /// Zevenbergen-Thorne derivatives (hillshade, slope, aspect).
    ZevenbergenThorne,
    /// Riley terrain ruggedness.
    Riley,
    /// Wilson terrain ruggedness.
    Wilson,
}
impl DemAlgorithm {
    fn name(self) -> &'static str {
        match self {
            Self::Horn => "Horn",
            Self::ZevenbergenThorne => "ZevenbergenThorne",
            Self::Riley => "Riley",
            Self::Wilson => "Wilson",
        }
    }
}

/// Derive terrain data into a new raster (GTiff by default).
#[derive(Clone)]
#[must_use]
pub struct Dem {
    job: Job,
    mode: DemMode,
    band: usize,
    colors: Option<PathBuf>,
    scale: Option<f64>,
    azimuth: Option<f64>,
    altitude: Option<f64>,
    edges: bool,
    percent: bool,
    trigonometric: bool,
    zero_for_flat: bool,
    algorithm: Option<DemAlgorithm>,
}
debug_builder!(Dem);
impl Dem {
    /// Describe one terrain operation without opening data.
    pub fn new(source: impl Into<Source>, output: impl AsRef<Path>, mode: DemMode) -> Self {
        Self {
            job: Job::new(source, output, "GTiff"),
            mode,
            band: 1,
            colors: None,
            scale: None,
            azimuth: None,
            altitude: None,
            edges: false,
            percent: false,
            trigonometric: false,
            zero_for_flat: false,
            algorithm: None,
        }
    }
    common!();
    /// Select a one-based elevation band.
    pub fn band(mut self, value: usize) -> Self {
        self.band = value;
        self
    }
    /// Set the color table path; required only for ColorRelief.
    pub fn color_file(mut self, path: impl AsRef<Path>) -> Self {
        self.colors = Some(path.as_ref().to_owned());
        self
    }
    /// Set vertical-to-horizontal units scale for hillshade or slope.
    pub fn scale(mut self, value: f64) -> Self {
        self.scale = Some(value);
        self
    }
    /// Set hillshade illumination azimuth, in degrees from north.
    pub fn azimuth(mut self, value: f64) -> Self {
        self.azimuth = Some(value);
        self
    }
    /// Set hillshade illumination altitude above the horizon, in degrees.
    pub fn altitude(mut self, value: f64) -> Self {
        self.altitude = Some(value);
        self
    }
    /// Compute edges and interpolate missing neighbors where GDAL supports it.
    pub fn compute_edges(mut self, enabled: bool) -> Self {
        self.edges = enabled;
        self
    }
    /// Return percent slope instead of degrees. Valid only for Slope.
    pub fn slope_percent(mut self, enabled: bool) -> Self {
        self.percent = enabled;
        self
    }
    /// Use trigonometric aspect angles. Valid only for Aspect.
    pub fn trigonometric(mut self, enabled: bool) -> Self {
        self.trigonometric = enabled;
        self
    }
    /// Set flat aspect values to zero. Valid only for Aspect.
    pub fn zero_for_flat(mut self, enabled: bool) -> Self {
        self.zero_for_flat = enabled;
        self
    }
    /// Choose an algorithm valid for the selected terrain mode.
    pub fn algorithm(mut self, value: DemAlgorithm) -> Self {
        self.algorithm = Some(value);
        self
    }
    /// Execute and check explicit output closure.
    pub fn run(self) -> Result<Report> {
        self.run_with(&mut Control::default())
    }
    /// Execute with borrowed progress and cancellation.
    pub fn run_with(self, control: &mut Control<'_>) -> Result<Report> {
        options::band(self.band)?;
        if (self.mode == DemMode::ColorRelief) != self.colors.is_some() {
            return Err(Error::invalid(
                "ColorRelief requires a color file; other modes reject it",
            ));
        }
        if (self.azimuth.is_some() || self.altitude.is_some()) && self.mode != DemMode::Hillshade
            || self.percent && self.mode != DemMode::Slope
            || (self.trigonometric || self.zero_for_flat) && self.mode != DemMode::Aspect
            || self.scale.is_some() && !matches!(self.mode, DemMode::Slope | DemMode::Hillshade)
        {
            return Err(Error::invalid(
                "terrain parameter is incompatible with the selected mode",
            ));
        }
        let mut args = vec!["-of".into(), self.job.driver.clone()];
        pair(&mut args, "-b", self.band);
        if let Some(scale) = self.scale {
            options::positive(scale, "terrain scale")?;
            pair(&mut args, "-s", scale);
        }
        for (flag, value, max) in [("-az", self.azimuth, 360.), ("-alt", self.altitude, 90.)] {
            if let Some(value) = value {
                if !value.is_finite() || !(0.0..=max).contains(&value) {
                    return Err(Error::invalid(
                        "illumination angle is outside its valid range",
                    ));
                }
                pair(&mut args, flag, value);
            }
        }
        for (enabled, flag) in [
            (self.edges, "-compute_edges"),
            (self.percent, "-p"),
            (self.trigonometric, "-trigonometric"),
            (self.zero_for_flat, "-zero_for_flat"),
        ] {
            if enabled {
                args.push(flag.into());
            }
        }
        if let Some(algorithm) = self.algorithm {
            let compatible = match algorithm {
                DemAlgorithm::Horn | DemAlgorithm::ZevenbergenThorne => matches!(
                    self.mode,
                    DemMode::Hillshade | DemMode::Slope | DemMode::Aspect
                ),
                DemAlgorithm::Riley | DemAlgorithm::Wilson => self.mode == DemMode::Tri,
            };
            if !compatible {
                return Err(Error::invalid(
                    "terrain algorithm is incompatible with the selected mode",
                ));
            }
            pair(&mut args, "-alg", algorithm.name());
        }
        self.job.creation.append_args("-co", &mut args);
        self.job
            .run(Kind::Raster, false, control, move |prepared, progress| {
                options::source_band(&prepared.inputs[0], self.band)?;
                let mode = CString::new(self.mode.name()).expect("static mode");
                let colors = self
                    .colors
                    .map(|path| Source::new(path).name())
                    .transpose()?;
                prepared.utility(
                    utility::Kind::Dem,
                    &args,
                    Some((&mode, colors.as_deref())),
                    progress,
                )
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::raster::test_support::{fixture, pixels};
    #[test]
    fn computes_slope_and_color_relief() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let src = dir.path().join("dem.tif");
        fixture(&src, 3, 3, &[1f32, 2., 3., 1., 2., 3., 1., 2., 3.])?;
        let out = dir.path().join("slope.tif");
        let _ = Dem::new(&src, &out, DemMode::Slope).scale(1.).run()?;
        assert!((pixels(&out)?[4] - 45.).abs() < 0.001);
        let colors = dir.path().join("colors.txt");
        std::fs::write(&colors, "1 255 0 0\n3 0 0 255\n")?;
        let rgb = dir.path().join("rgb.tif");
        let _ = Dem::new(&src, &rgb, DemMode::ColorRelief)
            .color_file(&colors)
            .run()?;
        let ds = gdal::Dataset::open(rgb)?;
        assert_eq!(ds.raster_count(), 3);
        assert_eq!(ds.rasterband(1)?.read_band_as::<u8>()?.data()[0], 255);
        ds.close()?;
        assert!(
            Dem::new(&src, dir.path().join("bad.tif"), DemMode::Slope)
                .azimuth(90.)
                .run()
                .is_err()
        );
        Ok(())
    }
}
