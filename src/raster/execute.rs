use crate::{
    config::gdal::GdalOptions,
    dataset::{self, Kind, Options, Source},
    gdal_ext::{
        diagnostics::Diagnostics,
        execution::{close, supported_version},
        output,
        progress::ProgressCallbackState,
        raster::utility,
    },
    operation::{Control, Error, ErrorKind, OutputState, Report, Result, Stage},
};
use gdal::{Dataset, Driver, DriverManager};
use std::{
    ffi::{CStr, CString},
    ops::ControlFlow,
    path::{Path, PathBuf},
};

#[derive(Clone, Debug)]
pub(super) struct Job {
    pub sources: Vec<Source>,
    pub path: PathBuf,
    pub driver: String,
    pub creation: Options,
    pub config: GdalOptions,
    pub native: bool,
}
impl Job {
    pub fn new(source: impl Into<Source>, output: impl AsRef<Path>, driver: &str) -> Self {
        Self::multiple([source], output, driver)
    }
    pub fn multiple<I, S>(sources: I, output: impl AsRef<Path>, driver: &str) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<Source>,
    {
        Self {
            sources: sources.into_iter().map(Into::into).collect(),
            path: output.as_ref().to_owned(),
            driver: driver.into(),
            creation: Options::default(),
            config: GdalOptions::default(),
            native: false,
        }
    }
    pub fn run(
        self,
        input_kind: Kind,
        vector_output: bool,
        control: &mut Control<'_>,
        execute: impl FnOnce(&mut Prepared, &mut ProgressCallbackState<'_>) -> Result<()>,
    ) -> Result<Report> {
        supported_version()?;
        control.check()?;
        if self.sources.is_empty() || self.sources.len() > i32::MAX as usize {
            return Err(Error::invalid(
                "one or more inputs are required, within GDAL's count limit",
            ));
        }
        for source in &self.sources {
            source.validate()?;
        }
        self.creation.validate()?;
        let diagnostics = Diagnostics::enter();
        let config = self.config.enter();
        let mut prepared = Prepared {
            inputs: Vec::with_capacity(self.sources.len()),
            auxiliary: Vec::new(),
            temporary: None,
            output: None,
            name: dataset::local_name(&self.path)?,
            driver: None,
            creation: self.creation,
            state: OutputState::NotModified,
            aliases: output::AliasCheck::new(&self.path)?,
            source_open_options: self.sources.iter().any(Source::has_open_options),
        };
        let result = (|| {
            if !self.native {
                let path = output::absolute_output(&self.path)?;
                let driver = DriverManager::get_driver_by_name(&self.driver).map_err(|e| {
                    Error::unsupported("output driver is not installed").with_source(e)
                })?;
                if vector_output {
                    output::check_vector_creation(&path, &driver, 1)?;
                } else {
                    output::check_raster_creation(&path, &driver)?;
                }
                prepared.name = dataset::local_name(&path)?;
                prepared.aliases.add_driver(Some(&driver.short_name()))?;
                prepared.driver = Some(driver);
            }
            prepared.open_inputs(self.sources, input_kind, control)?;
            control.check()?;
            let failures = diagnostics.failures();
            let active = control.is_active();
            let mut callback = |progress| {
                if control
                    .cancellation
                    .is_some_and(crate::control::Cancellation::is_cancelled)
                {
                    return false;
                }
                control
                    .callback
                    .as_mut()
                    .is_none_or(|callback| matches!(callback(&progress), ControlFlow::Continue(())))
            };
            let mut progress =
                ProgressCallbackState::optional(if active { Some(&mut callback) } else { None });
            let result = execute(&mut prepared, &mut progress);
            check_progress(&progress)?;
            result?;
            if diagnostics.failures() != failures {
                return Err(Error::new(
                    ErrorKind::Processing,
                    Stage::Execute,
                    "GDAL reported a failure; inspect diagnostics",
                ));
            }
            Ok(())
        })();
        let mut failure = result.err();
        close(prepared.output.take(), Stage::CloseOutput, &mut failure);
        for input in prepared
            .inputs
            .drain(..)
            .chain(prepared.auxiliary.drain(..))
        {
            close(Some(input), Stage::CloseSource, &mut failure);
        }
        drop(prepared.temporary.take());
        drop(config);
        let report = diagnostics.take_report();
        if let Some(mut error) = failure {
            error.output_state = prepared.state;
            error.report = report;
            Err(error)
        } else {
            Ok(report)
        }
    }
}

pub(super) struct Prepared {
    pub output: Option<Dataset>,
    pub inputs: Vec<Dataset>,
    auxiliary: Vec<Dataset>,
    temporary: Option<(crate::config::gdal::ThreadOptions, tempfile::TempDir)>,
    pub name: CString,
    pub driver: Option<Driver>,
    pub creation: Options,
    pub state: OutputState,
    aliases: output::AliasCheck,
    source_open_options: bool,
}
impl Prepared {
    fn open_inputs(
        &mut self,
        sources: impl IntoIterator<Item = Source>,
        kind: Kind,
        control: &Control<'_>,
    ) -> Result<()> {
        for source in sources {
            control.check()?;
            self.aliases.check_source(&source)?;
            self.inputs.push(source.open_kind(kind)?);
            self.aliases
                .check_dataset(self.inputs.last().expect("opened source"))?;
        }
        Ok(())
    }

    pub fn utility(
        &mut self,
        kind: utility::Kind,
        args: &[String],
        dem: Option<(&CStr, Option<&CStr>)>,
        progress: &mut ProgressCallbackState<'_>,
    ) -> Result<()> {
        // shortcut: GDALBuildVRT drops per-source open options; reject until it can persist them.
        if matches!(kind, utility::Kind::BuildVrt) && self.source_open_options {
            return Err(Error::unsupported(
                "BuildVrt cannot persist source open options; first use Translate with VRT output",
            ));
        }
        let mut options =
            utility::Options::new(kind, args).map_err(|e| Error::native(Stage::Prepare, e))?;
        let driver = args
            .windows(2)
            .rev()
            .find(|pair| pair[0].eq_ignore_ascii_case("-of") || pair[0].eq_ignore_ascii_case("-f"))
            .map(|pair| {
                DriverManager::get_driver_by_name(&pair[1])
                    .map(|driver| driver.short_name())
                    .map_err(|e| {
                        Error::unsupported("output driver is not installed").with_source(e)
                    })
            })
            .transpose()?;
        self.aliases.add_driver(driver.as_deref())?;
        if let Some((_, Some(colors))) = dem {
            let colors = native_path(colors)?;
            self.aliases.check_path(colors)?;
        }
        if matches!(kind, utility::Kind::Warp) {
            for pair in args
                .windows(2)
                .filter(|pair| pair[0].eq_ignore_ascii_case("-cutline"))
            {
                let cutline = Path::new(&pair[1]);
                if cutline.exists() {
                    self.aliases.check_path(cutline)?;
                    self.auxiliary
                        .push(Source::new(cutline).open_kind(Kind::Vector)?);
                    self.aliases
                        .check_dataset(self.auxiliary.last().expect("opened cutline"))?;
                }
            }
        }
        if driver.as_deref() == Some("COG") {
            self.isolate_scratch()?;
        }
        self.state = OutputState::MayHaveChanged;
        let outcome = options.execute(&self.inputs, &self.name, dem, progress);
        self.output = outcome.output;
        check_progress(progress)?;
        if outcome.usage_error {
            return Err(Error::invalid("GDAL rejected the operation arguments"));
        }
        if let Some(error) = outcome.error {
            return Err(processing_error(error));
        }
        Ok(())
    }
    pub fn isolate_scratch(&mut self) -> Result<()> {
        // COG and FillNodata unlink predictable scratch names; isolate caller-owned files.
        let configured = gdal::config::get_config_option("CPL_TMPDIR", "")
            .map_err(|e| Error::native(Stage::Prepare, e))?;
        let mut builder = tempfile::Builder::new();
        builder.prefix(".spatial-core-gdal-");
        let path = native_path(&self.name)?;
        let directory = if !configured.is_empty() {
            builder.tempdir_in(&configured)
        } else if let Some(parent) = path.parent().filter(|parent| parent.is_dir()) {
            builder.tempdir_in(parent)
        } else {
            builder.tempdir()
        }
        .map_err(|e| {
            Error::invalid("cannot create isolated GDAL scratch directory").with_source(e)
        })?;
        let name = directory
            .path()
            .to_str()
            .ok_or_else(|| Error::unsupported("GDAL scratch directory must be UTF-8"))?;
        let scope = GdalOptions::default()
            .set("CPL_TMPDIR", name)
            .expect("filesystem path and static key contain no NUL")
            .enter();
        self.temporary = Some((scope, directory));
        Ok(())
    }
    pub fn create_vector(&mut self) -> Result<()> {
        let options = string_list(&self.creation)?;
        let driver = self.driver.as_ref().expect("typed output");
        self.state = OutputState::MayHaveChanged;
        // SAFETY: driver, filename and option strings remain live; a successful
        // zero-band GDALCreate transfers one new vector dataset handle.
        let raw = unsafe {
            gdal_sys::GDALCreate(
                driver.c_driver(),
                self.name.as_ptr(),
                0,
                0,
                0,
                gdal_sys::GDALDataType::GDT_Unknown,
                options.as_ptr(),
            )
        };
        if raw.is_null() {
            return Err(processing_error(
                crate::gdal_ext::ffi::last_null_pointer_err("GDALCreate"),
            ));
        }
        // SAFETY: the non-null handle is exclusively owned by this execution.
        self.output = Some(unsafe { Dataset::from_c_dataset(raw) });
        Ok(())
    }
    pub fn copy(&mut self, progress: &mut ProgressCallbackState<'_>) -> Result<()> {
        let driver = self.driver.as_ref().expect("typed output").short_name();
        if driver != "GTiff" {
            return Err(Error::unsupported(
                "copy-then-edit algorithms require GTiff output",
            ));
        }
        let source = &self.inputs[0];
        let first = source.rasterband(1).map_err(processing_error)?;
        for index in 2..=source.raster_count() {
            let band = source.rasterband(index).map_err(processing_error)?;
            if band.band_type() != first.band_type() {
                return Err(Error::unsupported(
                    "GTiff copies require the same pixel type in every band",
                ));
            }
        }
        let mut args = vec!["-of".into(), driver];
        self.creation.append_args("-co", &mut args);
        self.utility(utility::Kind::Translate, &args, None, progress)
    }
}

fn native_path(name: &CStr) -> Result<&Path> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        Ok(Path::new(std::ffi::OsStr::from_bytes(name.to_bytes())))
    }
    #[cfg(not(unix))]
    {
        name.to_str()
            .map(Path::new)
            .map_err(|e| Error::unsupported("native path must be UTF-8").with_source(e))
    }
}

pub(super) fn check_progress(progress: &ProgressCallbackState<'_>) -> Result<()> {
    if let Some(kind) = progress.interruption() {
        Err(Error::new(
            kind,
            Stage::Execute,
            "operation interrupted by progress control",
        ))
    } else {
        Ok(())
    }
}
pub(super) fn processing_error(error: gdal::errors::GdalError) -> Error {
    Error::new(
        ErrorKind::Processing,
        Stage::Execute,
        "GDAL processing failed; inspect source and diagnostics",
    )
    .with_source(error)
}
pub(super) fn check_status(status: gdal_sys::CPLErr::Type) -> Result<()> {
    if status == gdal_sys::CPLErr::CE_None {
        Ok(())
    } else {
        Err(Error::new(
            ErrorKind::Processing,
            Stage::Execute,
            "GDAL algorithm failed; inspect diagnostics",
        ))
    }
}

pub(super) fn string_list(options: &Options) -> Result<gdal::cpl::CslStringList> {
    options.validate()?;
    let mut list = gdal::cpl::CslStringList::new();
    for value in options.strings() {
        list.add_string(&value)
            .map_err(|e| Error::native(Stage::Prepare, e))?;
    }
    Ok(list)
}
pub(super) fn spatial_ref(input: &Dataset) -> Result<Option<gdal::spatial_ref::SpatialRef>> {
    if input.projection().is_empty() {
        Ok(None)
    } else {
        input.spatial_ref().map(Some).map_err(processing_error)
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        operation::{Control, ErrorKind},
        raster::test_support::fixture,
        raster::{
            BuildVrt, Contour, Dem, DemMode, FillNodata, NativeTranslate, Polygonize, Rasterize,
            Sieve, Translate, Warp,
        },
    };
    use std::ops::ControlFlow;

    #[test]
    fn deferred_vrt_write_failure_is_not_successful_close() -> anyhow::Result<()> {
        use crate::{
            control::Cancellation,
            operation::{OutputState, Severity, Stage},
            raster::{NativeBuildVrt, test_support::pixels},
        };
        let dir = tempfile::tempdir()?;
        let src = dir.path().join("source.tif");
        fixture(&src, 2, 2, &[1u8, 2, 3, 4])?;
        for active in [false, true] {
            let out = dir.path().join(format!("directory-{active}.vrt"));
            std::fs::create_dir(&out)?;
            let cancellation = Cancellation::default();
            let mut control = if active {
                Control::default().cancellation(&cancellation)
            } else {
                Control::default()
            };
            // VRT defers its write until close, whose return code can still be CE_None.
            let error = NativeBuildVrt::new([&src], &out, ["-strict"])
                .run_with(&mut control)
                .unwrap_err();
            assert_eq!(error.kind(), ErrorKind::Close);
            assert_eq!(error.stage(), Stage::CloseOutput);
            assert_eq!(error.output_state(), OutputState::MayHaveChanged);
            assert_eq!(error.cleanup_errors().len(), 1);
            assert_eq!(error.cleanup_errors()[0].stage, Stage::CloseOutput);
            assert!(error.diagnostics().iter().any(|diagnostic| matches!(
                diagnostic.severity,
                Severity::Failure | Severity::Fatal
            )));
            assert_eq!(std::fs::read_dir(&out)?.count(), 0);
            assert_eq!(pixels(&src)?, [1., 2., 3., 4.]);
        }
        Ok(())
    }

    #[test]
    fn invalid_band_selection_has_a_consistent_error_contract() -> anyhow::Result<()> {
        use crate::{
            operation::{OutputState, Stage},
            raster::Mask,
        };
        let dir = tempfile::tempdir()?;
        let src = dir.path().join("one-band.tif");
        fixture(&src, 3, 3, &[1u8; 9])?;
        for index in [0, 2, usize::MAX] {
            for operation in 0..10 {
                let out = dir.path().join(format!("invalid-{operation}-{index}"));
                let error = match operation {
                    0 => Translate::new(&src, &out).bands([index]).run(),
                    1 => BuildVrt::new([&src], &out).bands([index]).run(),
                    2 => Dem::new(&src, &out, DemMode::Slope).band(index).run(),
                    3 => FillNodata::new(&src, &out).band(index).run(),
                    4 => Sieve::new(&src, &out, 2).band(index).run(),
                    5 => Polygonize::new(&src, &out).band(index).run(),
                    6 => Contour::new(&src, &out).interval(1.).band(index).run(),
                    7 => FillNodata::new(&src, &out).mask(Mask::Band(index)).run(),
                    8 => Sieve::new(&src, &out, 2).mask(Mask::Band(index)).run(),
                    _ => Polygonize::new(&src, &out).mask(Mask::Band(index)).run(),
                }
                .unwrap_err();
                assert_eq!(
                    error.kind(),
                    ErrorKind::InvalidArgument,
                    "operation {operation}, band {index}"
                );
                assert_eq!(
                    error.stage(),
                    if index == 2 {
                        Stage::Inspect
                    } else {
                        Stage::Prepare
                    }
                );
                assert_eq!(error.output_state(), OutputState::NotModified);
                assert!(!out.exists());
            }
        }
        Ok(())
    }

    #[test]
    fn cancellation_between_inputs_does_not_open_the_next_source() -> anyhow::Result<()> {
        use super::*;
        let dir = tempfile::tempdir()?;
        let src = dir.path().join("source.tif");
        fixture(&src, 1, 1, &[7u8])?;
        let out = dir.path().join("out.vrt");
        let cancellation = crate::control::Cancellation::default();
        let control = Control::default().cancellation(&cancellation);
        let mut prepared = Prepared {
            inputs: Vec::new(),
            auxiliary: Vec::new(),
            temporary: None,
            output: None,
            name: dataset::local_name(&out)?,
            driver: None,
            creation: Options::default(),
            state: OutputState::NotModified,
            aliases: output::AliasCheck::new(&out)?,
            source_open_options: false,
        };
        let sources = [
            Source::new(&src),
            Source::new(dir.path().join("must-not-open")),
        ]
        .into_iter()
        .enumerate()
        .map(|(index, source)| {
            if index == 1 {
                cancellation.cancel();
            }
            source
        });
        let error = prepared
            .open_inputs(sources, Kind::Raster, &control)
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Cancelled);
        assert_eq!(error.stage(), Stage::Prepare);
        assert_eq!(prepared.inputs.len(), 1);
        assert_eq!(prepared.state, OutputState::NotModified);
        assert!(!out.exists());
        for input in prepared.inputs {
            input.close()?;
        }
        Ok(())
    }

    #[test]
    fn native_progress_is_enabled_only_for_active_control() -> anyhow::Result<()> {
        use super::*;
        let dir = tempfile::tempdir()?;
        let src = dir.path().join("source.tif");
        fixture(&src, 1, 1, &[7u8])?;
        let token = crate::control::Cancellation::default();
        for active in [false, true] {
            let mut control = if active {
                Control::default().cancellation(&token)
            } else {
                Control::default()
            };
            let _ = Job::new(&src, dir.path().join("output.tif"), "GTiff").run(
                Kind::Raster,
                false,
                &mut control,
                |_, progress| {
                    assert_eq!(progress.native_callback().is_some(), active);
                    assert_eq!(!progress.user_data().is_null(), active);
                    Ok(())
                },
            )?;
        }
        Ok(())
    }

    #[test]
    fn mixed_band_types_are_rejected_before_copying() -> anyhow::Result<()> {
        use crate::{operation::OutputState, raster::Mask};
        let dir = tempfile::tempdir()?;
        fixture(&dir.path().join("byte.tif"), 3, 1, &[1u8; 3])?;
        fixture(
            &dir.path().join("wide.tif"),
            3,
            1,
            &[1234.5f64, 99999.5, 0.125],
        )?;
        let src = dir.path().join("mixed.vrt");
        std::fs::write(
            &src,
            r#"<VRTDataset rasterXSize="3" rasterYSize="1">
          <VRTRasterBand dataType="Byte" band="1"><SimpleSource><SourceFilename relativeToVRT="1">byte.tif</SourceFilename><SourceBand>1</SourceBand></SimpleSource></VRTRasterBand>
          <VRTRasterBand dataType="Float64" band="2"><SimpleSource><SourceFilename relativeToVRT="1">wide.tif</SourceFilename><SourceBand>1</SourceBand></SimpleSource></VRTRasterBand>
        </VRTDataset>"#,
        )?;
        for sieve in [false, true] {
            let out = dir.path().join(format!("copy-{sieve}.tif"));
            let error = if sieve {
                Sieve::new(&src, &out, 1).run()
            } else {
                FillNodata::new(&src, &out).mask(Mask::None).run()
            }
            .unwrap_err();
            assert_eq!(error.kind(), ErrorKind::Unsupported);
            assert_eq!(error.output_state(), OutputState::NotModified);
            assert!(!out.exists());
        }
        assert_eq!(
            super::super::test_support::pixels(&dir.path().join("wide.tif"))?,
            [1234.5, 99999.5, 0.125]
        );
        Ok(())
    }

    #[test]
    fn explicit_masks_reject_byte_conversion_that_changes_validity() -> anyhow::Result<()> {
        use crate::{operation::OutputState, raster::Mask};
        let dir = tempfile::tempdir()?;
        for floating in [false, true] {
            let src = dir.path().join(format!("mask-{floating}.tif"));
            if floating {
                fixture(&src, 2, 2, &[-1f32, 0., 0.1, 1.])?;
            } else {
                fixture(&src, 2, 2, &[-1i16, 0, -1, 1])?;
            }
            for operation in 0..3 {
                let out = dir.path().join(format!("bad-{floating}-{operation}"));
                let error = match operation {
                    0 => FillNodata::new(&src, &out).mask(Mask::Band(1)).run(),
                    1 => Sieve::new(&src, &out, 2).mask(Mask::Band(1)).run(),
                    _ => Polygonize::new(&src, &out)
                        .floating_values(floating)
                        .mask(Mask::Band(1))
                        .run(),
                }
                .unwrap_err();
                assert_eq!(error.output_state(), OutputState::NotModified);
                assert!(!out.exists());
            }
        }
        let src = dir.path().join("byte-mask.tif");
        fixture(&src, 2, 2, &[7u8, 0, 7, 1])?;
        let out = dir.path().join("valid.gpkg");
        let _ = Polygonize::new(&src, &out).mask(Mask::Band(1)).run()?;
        use gdal::vector::LayerAccess;
        let ds = gdal::Dataset::open(out)?;
        let area: f64 = ds
            .layer(0)?
            .features()
            .map(|f| f.geometry().unwrap().area())
            .sum();
        assert_eq!(area, 3.);
        ds.close()?;
        Ok(())
    }

    #[test]
    fn native_driver_case_cannot_bypass_source_worldfile_protection() -> anyhow::Result<()> {
        use crate::operation::OutputState;
        let dir = tempfile::tempdir()?;
        let src = dir.path().join("world.tif");
        let ds = gdal::DriverManager::get_driver_by_name("GTiff")?
            .create_with_band_type::<u8, _>(&src, 3, 1, 1)?;
        ds.rasterband(1)?.fill(7., None)?;
        ds.close()?;
        let world = dir.path().join("world.tfw");
        let before = "1\n0\n0\n-1\n0.5\n0.5\n";
        std::fs::write(&world, before)?;
        let out = dir.path().join("world.unknown");
        for driver in ["GTiff", "gtiff", "gTiFf"] {
            let error = NativeTranslate::new(
                &src,
                &out,
                [
                    "-of", driver, "-co", "TFW=YES", "-a_ullr", "100", "100", "103", "99",
                ],
            )
            .run()
            .unwrap_err();
            assert_eq!(error.output_state(), OutputState::NotModified);
            assert!(!out.exists());
            assert_eq!(std::fs::read_to_string(&world)?, before);
        }
        Ok(())
    }

    #[test]
    fn auxiliary_color_and_cutline_inputs_cannot_be_overwritten() -> anyhow::Result<()> {
        use crate::{
            operation::OutputState,
            raster::{NativeDem, NativeWarp},
        };
        let dir = tempfile::tempdir()?;
        let src = dir.path().join("source.tif");
        fixture(&src, 3, 3, &[1u8; 9])?;
        let colors = dir.path().join("colors.txt");
        std::fs::write(&colors, "0 255 0 0\n1 0 0 255\n")?;
        let before = std::fs::read(&colors)?;
        let error = NativeDem::new(&src, &colors, DemMode::ColorRelief, ["-of", "GTiff"])
            .color_file(&colors)
            .run()
            .unwrap_err();
        assert_eq!(error.output_state(), OutputState::NotModified);
        assert_eq!(before, std::fs::read(&colors)?);
        let cutline = dir.path().join("cut.geojson");
        std::fs::write(
            &cutline,
            r#"{"type":"Polygon","coordinates":[[[0,0],[3,0],[3,3],[0,3],[0,0]]]}"#,
        )?;
        let before = std::fs::read(&cutline)?;
        let error = NativeWarp::new(
            [&src],
            &cutline,
            ["-of", "GTiff", "-cutline", cutline.to_str().unwrap()],
        )
        .run()
        .unwrap_err();
        assert_eq!(error.output_state(), OutputState::NotModified);
        assert_eq!(before, std::fs::read(&cutline)?);
        // Also protect files belonging to an auxiliary dataset, not just its main name.
        let shp = dir.path().join("cut.shp");
        let _ = crate::vector::Translate::new(&cutline, &shp)
            .driver("ESRI Shapefile")
            .run()?;
        let dbf = shp.with_extension("dbf");
        let before = std::fs::read(&dbf)?;
        let error = NativeWarp::new(
            [&src],
            &dbf,
            ["-of", "GTiff", "-cutline", shp.to_str().unwrap()],
        )
        .run()
        .unwrap_err();
        assert_eq!(error.output_state(), OutputState::NotModified);
        assert_eq!(before, std::fs::read(&dbf)?);
        let out = dir.path().join("clipped.tif");
        let _ = Warp::new([&src], &out).cutline(&shp).run()?;
        assert_eq!(super::super::test_support::pixels(&out)?, [1.; 9]);
        Ok(())
    }
    #[test]
    fn algorithms_preserve_callback_failure_categories() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let src = dir.path().join("source.tif");
        fixture(&src, 8, 8, &(0..64u8).collect::<Vec<_>>())?;
        let geojson = dir.path().join("input.geojson");
        std::fs::write(
            &geojson,
            r#"{"type":"FeatureCollection","features":[{"type":"Feature","properties":{},"geometry":{"type":"Point","coordinates":[1,1]}}]}"#,
        )?;
        for panics in [false, true] {
            for operation in 0..8 {
                let path = dir.path().join(format!("out-{panics}-{operation}"));
                let mut callback = |_: &crate::operation::Progress| {
                    assert!(!panics, "callback panicked");
                    ControlFlow::Break(())
                };
                let mut control = Control::default().on_progress(&mut callback);
                let error = match operation {
                    0 => FillNodata::new(&src, &path).run_with(&mut control),
                    1 => Sieve::new(&src, &path, 2).run_with(&mut control),
                    2 => Polygonize::new(&src, &path).run_with(&mut control),
                    3 => Contour::new(&src, &path)
                        .interval(2.)
                        .run_with(&mut control),
                    4 => Dem::new(&src, &path, DemMode::Hillshade).run_with(&mut control),
                    5 => Warp::new([&src], &path).run_with(&mut control),
                    6 => BuildVrt::new([&src], &path).run_with(&mut control),
                    _ => Rasterize::new(&geojson, &path)
                        .bounds([0., 0., 2., 2.])
                        .size(8, 8)
                        .run_with(&mut control),
                }
                .unwrap_err();
                assert_eq!(
                    error.kind(),
                    if panics {
                        ErrorKind::CallbackPanicked
                    } else {
                        ErrorKind::Cancelled
                    },
                    "operation {operation}: {error:?}"
                );
            }
        }
        Ok(())
    }

    #[test]
    fn cog_scratch_files_are_isolated_and_configuration_is_restored() -> anyhow::Result<()> {
        use crate::config::gdal::GdalOptions;
        let dir = tempfile::tempdir()?;
        let scratch = tempfile::tempdir()?;
        let src = dir.path().join("output.tif.ovr.tmp");
        fixture(&src, 1024, 1024, &vec![7u8; 1024 * 1024])?;
        let before = std::fs::read(&src)?;
        let sentinel = scratch
            .path()
            .join(format!("output_{}_1.ovr.tmp", std::process::id()));
        std::fs::write(&sentinel, "caller owned")?;
        let original = gdal::config::get_thread_local_config_option("CPL_TMPDIR", "missing")?;
        for custom in [false, true] {
            let out = dir
                .path()
                .join(if custom { "custom.tif" } else { "output.tif" });
            let mut job = Translate::new(&src, &out).driver("COG");
            if custom {
                job = job.gdal_options(
                    GdalOptions::default().set("CPL_TMPDIR", scratch.path().to_str().unwrap())?,
                );
            }
            let mut saw_private_directory = false;
            let mut callback = |_: &crate::operation::Progress| {
                let current =
                    gdal::config::get_thread_local_config_option("CPL_TMPDIR", "").unwrap();
                let path = std::path::Path::new(&current);
                assert!(
                    path.file_name()
                        .unwrap()
                        .to_str()
                        .unwrap()
                        .starts_with(".spatial-core-gdal-")
                );
                assert!(path.is_dir());
                if custom {
                    assert_eq!(path.parent(), Some(scratch.path()));
                }
                saw_private_directory = true;
                ControlFlow::Continue(())
            };
            let _ = job.run_with(&mut Control::default().on_progress(&mut callback))?;
            assert!(saw_private_directory);
            assert_eq!(before, std::fs::read(&src)?);
            assert_eq!(std::fs::read_to_string(&sentinel)?, "caller owned");
            assert_eq!(
                gdal::config::get_thread_local_config_option("CPL_TMPDIR", "missing")?,
                original
            );
            assert_eq!(std::fs::read_dir(scratch.path())?.count(), 1);
            assert_eq!(
                super::super::test_support::pixels(&out)?,
                vec![7.; 1024 * 1024]
            );
        }
        Ok(())
    }

    #[test]
    fn copy_then_edit_algorithms_cancel_and_catch_panics_after_copy() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let scratch = tempfile::tempdir()?;
        let src = dir.path().join("holes.tif");
        let mut values = vec![7u8; 32 * 32];
        values[100] = 0;
        fixture(&src, 32, 32, &values)?;
        let ds = gdal::Dataset::open_ex(
            &src,
            gdal::DatasetOptions {
                open_flags: gdal::GdalOpenFlags::GDAL_OF_UPDATE,
                ..Default::default()
            },
        )?;
        ds.rasterband(1)?.set_no_data_value(Some(0.))?;
        ds.close()?;
        let before = std::fs::read(&src)?;
        let sentinel = scratch
            .path()
            .join(format!("_{}_1fill_y_work.tif", std::process::id()));
        std::fs::write(&sentinel, "caller owned")?;
        for sieve in [false, true] {
            for panics in [false, true] {
                let mut copy_finished = false;
                let mut reached_algorithm = false;
                let mut callback = |progress: &crate::operation::Progress| {
                    if copy_finished && progress.fraction < 1. {
                        reached_algorithm = true;
                        assert!(!panics, "algorithm callback panicked");
                        return ControlFlow::Break(());
                    }
                    copy_finished |= progress.fraction >= 1.;
                    ControlFlow::Continue(())
                };
                let out = dir
                    .path()
                    .join(format!("second-stage-{sieve}-{panics}.tif"));
                let mut control = Control::default().on_progress(&mut callback);
                let error = if sieve {
                    Sieve::new(&src, &out, 2).run_with(&mut control)
                } else {
                    FillNodata::new(&src, &out)
                        .gdal_options(
                            crate::config::gdal::GdalOptions::default()
                                .set("CPL_TMPDIR", scratch.path().to_str().unwrap())?,
                        )
                        .run_with(&mut control)
                }
                .unwrap_err();
                assert!(reached_algorithm);
                assert_eq!(
                    error.kind(),
                    if panics {
                        ErrorKind::CallbackPanicked
                    } else {
                        ErrorKind::Cancelled
                    }
                );
                assert_eq!(
                    error.output_state(),
                    crate::operation::OutputState::MayHaveChanged
                );
                assert_eq!(before, std::fs::read(&src)?);
                assert_eq!(std::fs::read_to_string(&sentinel)?, "caller owned");
                assert_eq!(std::fs::read_dir(scratch.path())?.count(), 1);
            }
        }
        Ok(())
    }
    #[test]
    fn sidecars_and_symlinks_cannot_overwrite_source_data() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let src = dir.path().join("source.tif");
        fixture(&src, 2, 2, &[1u8, 2, 3, 4])?;
        let out = dir.path().join("out.tif");
        let sidecar = dir.path().join("out.tfw");
        std::fs::write(&sidecar, "existing sidecar")?;
        assert_eq!(
            Translate::new(&src, &out)
                .creation_option("TFW", "YES")
                .run()
                .unwrap_err()
                .kind(),
            ErrorKind::OutputExists
        );
        assert_eq!(std::fs::read_to_string(&sidecar)?, "existing sidecar");
        #[cfg(unix)]
        {
            let link = dir.path().join("alias.tif");
            std::os::unix::fs::symlink(&src, &link)?;
            assert!(
                NativeTranslate::new(&src, &link, ["-of", "GTiff"])
                    .run()
                    .is_err()
            );
            let hard = dir.path().join("hard.tif");
            std::fs::hard_link(&src, &hard)?;
            assert!(
                NativeTranslate::new(&src, &hard, ["-of", "GTiff"])
                    .run()
                    .is_err()
            );
        }
        Ok(())
    }
    #[test]
    fn invalid_crs_restores_prior_native_error_and_retains_diagnostics() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let src = dir.path().join("source.tif");
        fixture(&src, 1, 1, &[7u8])?;
        // SAFETY: the handler and C string remain live through this synchronous test.
        unsafe {
            gdal_sys::CPLPushErrorHandler(Some(gdal_sys::CPLQuietErrorHandler));
            gdal_sys::CPLError(gdal_sys::CPLErr::CE_Warning, 1, c"prior state".as_ptr());
        }
        let result = Translate::new(&src, dir.path().join("bad.tif"))
            .assign_srs("not-a-crs")
            .run();
        // SAFETY: this reads GDAL's thread-local error and pops the handler we pushed.
        let state = unsafe {
            let state = std::ffi::CStr::from_ptr(gdal_sys::CPLGetLastErrorMsg())
                .to_string_lossy()
                .into_owned();
            gdal_sys::CPLPopErrorHandler();
            gdal_sys::CPLErrorReset();
            state
        };
        assert_eq!(state, "prior state");
        assert!(result.is_err());
        assert!(!dir.path().join("bad.tif").exists());
        Ok(())
    }
}
