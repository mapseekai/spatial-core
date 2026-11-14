use super::{DemMode, execute::Job, options::debug_builder};
use crate::{
    config::gdal::GdalOptions,
    dataset::{Kind, Source},
    gdal_ext::{execution::validate_native_args, raster::utility},
    operation::{Control, Error, Report, Result},
};
use std::{
    ffi::CString,
    path::{Path, PathBuf},
};

// Constructors differ for single and multiple inputs; execution is identical.
macro_rules! native_methods {
    ($kind:ident,$input:ident) => {
        /// Apply thread-local configuration during open, processing and close.
        pub fn gdal_options(mut self, value: GdalOptions) -> Self {
            self.job.config = value;
            self
        }
        /// Execute native arguments and explicitly close all datasets.
        pub fn run(self) -> Result<Report> {
            self.run_with(&mut Control::default())
        }
        /// Execute with borrowed progress and cancellation control.
        pub fn run_with(self, control: &mut Control<'_>) -> Result<Report> {
            validate_native_args(&self.args)?;
            self.job
                .run(Kind::$input, false, control, move |prepared, progress| {
                    prepared.utility(utility::Kind::$kind, &self.args, None, progress)
                })
        }
    };
}

/// Native GDAL Translate arguments. Existing outputs may be modified.
/// Input/configuration flags belong on Source/GdalOptions, not in argv.
#[derive(Clone)]
#[must_use]
pub struct NativeTranslate {
    job: Job,
    args: Vec<String>,
}
debug_builder!(NativeTranslate);
impl NativeTranslate {
    /// Arguments contain utility options only, without filenames or an executable.
    pub fn new<I, S>(source: impl Into<Source>, output: impl AsRef<Path>, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let mut job = Job::new(source, output, "");
        job.native = true;
        Self {
            job,
            args: args.into_iter().map(Into::into).collect(),
        }
    }
    native_methods!(Translate, Raster);
}

/// Native GDAL Warp arguments. Existing outputs may be modified.
/// Input/configuration flags belong on Source/GdalOptions, not in argv.
#[derive(Clone)]
#[must_use]
pub struct NativeWarp {
    job: Job,
    args: Vec<String>,
}
debug_builder!(NativeWarp);
impl NativeWarp {
    /// Arguments contain utility options only, without filenames or an executable.
    pub fn new<I, S, A, V>(sources: I, output: impl AsRef<Path>, args: A) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<Source>,
        A: IntoIterator<Item = V>,
        V: Into<String>,
    {
        let mut job = Job::multiple(sources, output, "");
        job.native = true;
        Self {
            job,
            args: args.into_iter().map(Into::into).collect(),
        }
    }
    native_methods!(Warp, Raster);
}

/// Native GDAL Rasterize arguments. Existing outputs may be modified.
/// Input/configuration flags belong on Source/GdalOptions, not in argv.
#[derive(Clone)]
#[must_use]
pub struct NativeRasterize {
    job: Job,
    args: Vec<String>,
}
debug_builder!(NativeRasterize);
impl NativeRasterize {
    /// Arguments contain utility options only, without filenames or an executable.
    pub fn new<I, S>(source: impl Into<Source>, output: impl AsRef<Path>, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let mut job = Job::new(source, output, "");
        job.native = true;
        Self {
            job,
            args: args.into_iter().map(Into::into).collect(),
        }
    }
    native_methods!(Rasterize, Vector);
}

/// Native GDAL BuildVrt arguments. Existing outputs may be modified.
/// Input/configuration flags belong on Source/GdalOptions, not in argv.
/// Source open options are rejected before writing; persist them in a Translate VRT first.
#[derive(Clone)]
#[must_use]
pub struct NativeBuildVrt {
    job: Job,
    args: Vec<String>,
}
debug_builder!(NativeBuildVrt);
impl NativeBuildVrt {
    /// Arguments contain utility options only, without filenames or an executable.
    pub fn new<I, S, A, V>(sources: I, output: impl AsRef<Path>, args: A) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<Source>,
        A: IntoIterator<Item = V>,
        V: Into<String>,
    {
        let mut job = Job::multiple(sources, output, "");
        job.native = true;
        Self {
            job,
            args: args.into_iter().map(Into::into).collect(),
        }
    }
    native_methods!(BuildVrt, Raster);
}

/// Native DEM arguments with an explicit mode and optional color table.
#[derive(Clone)]
#[must_use]
pub struct NativeDem {
    job: Job,
    args: Vec<String>,
    mode: DemMode,
    colors: Option<PathBuf>,
}
debug_builder!(NativeDem);
impl NativeDem {
    /// Native arguments exclude the mode, input/output names and color table path.
    pub fn new<I, S>(
        source: impl Into<Source>,
        output: impl AsRef<Path>,
        mode: DemMode,
        args: I,
    ) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let mut job = Job::new(source, output, "");
        job.native = true;
        Self {
            job,
            args: args.into_iter().map(Into::into).collect(),
            mode,
            colors: None,
        }
    }
    /// Set the required color table for ColorRelief.
    pub fn color_file(mut self, path: impl AsRef<Path>) -> Self {
        self.colors = Some(path.as_ref().to_owned());
        self
    }
    /// Apply scoped GDAL configuration.
    pub fn gdal_options(mut self, value: GdalOptions) -> Self {
        self.job.config = value;
        self
    }
    /// Execute synchronously with checked dataset closure.
    pub fn run(self) -> Result<Report> {
        self.run_with(&mut Control::default())
    }
    /// Execute with borrowed progress and cancellation control.
    pub fn run_with(self, control: &mut Control<'_>) -> Result<Report> {
        validate_native_args(&self.args)?;
        if (self.mode == DemMode::ColorRelief) != self.colors.is_some() {
            return Err(Error::invalid(
                "ColorRelief requires a color file; other modes reject it",
            ));
        }
        self.job
            .run(Kind::Raster, false, control, move |prepared, progress| {
                let mode = CString::new(self.mode.name()).expect("static mode");
                let colors = self
                    .colors
                    .map(|p| crate::dataset::local_name(&p))
                    .transpose()?;
                prepared.utility(
                    utility::Kind::Dem,
                    &self.args,
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
    fn native_utilities_run_and_reserve_configuration_flags() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let src = dir.path().join("source.tif");
        fixture(&src, 3, 3, &[1f32, 2., 3., 1., 2., 3., 1., 2., 3.])?;
        let out = dir.path().join("native.tif");
        let _ = NativeTranslate::new(&src, &out, ["-of", "GTiff", "-outsize", "2", "2"]).run()?;
        assert_eq!(pixels(&out)?.len(), 4);
        let warp = dir.path().join("warp.tif");
        let _ = NativeWarp::new([&src], &warp, ["-of", "GTiff", "-t_srs", "EPSG:3857"]).run()?;
        let dem = dir.path().join("dem.tif");
        let _ = NativeDem::new(&src, &dem, DemMode::Slope, ["-of", "GTiff", "-s", "1"]).run()?;
        assert!((pixels(&dem)?[4] - 45.).abs() < 0.001);
        let vrt = dir.path().join("native.vrt");
        let _ = NativeBuildVrt::new([&src], &vrt, ["-strict"]).run()?;
        assert_eq!(pixels(&vrt)?, pixels(&src)?);
        for flag in [
            "-oo",
            "-IF",
            "--config=KEY=VALUE",
            "--thread-local-config",
            "--config-file",
        ] {
            assert!(
                NativeTranslate::new(&src, dir.path().join("blocked.tif"), [flag])
                    .run()
                    .is_err()
            );
            assert!(!dir.path().join("blocked.tif").exists());
        }
        let error =
            NativeTranslate::new(&src, dir.path().join("invalid.tif"), ["-not-a-gdal-option"])
                .run()
                .unwrap_err();
        assert_eq!(
            error.output_state(),
            crate::operation::OutputState::NotModified
        );
        assert!(!error.diagnostics().is_empty());
        Ok(())
    }
}
