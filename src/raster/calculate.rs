use super::{DataType, execute::Job, options};
use crate::{
    dataset::{Kind, Source},
    gdal_ext::{
        algorithm::{Algorithm, Argument},
        execution::close,
        query::inspect_error,
    },
    operation::{Control, Error, Report, Result, Stage},
};
use gdal::{Dataset, DriverManager, raster::RasterCreationOptions};
use std::{collections::HashSet, path::Path};

/// Native expression parser; neither dialect executes Python or shell code.
#[derive(Debug, Clone, Copy, Default)]
pub enum ExpressionDialect {
    /// muparser formulas such as `A[1] + B[1]`.
    #[default]
    MuParser,
    /// GDAL built-in pixel functions.
    Builtin,
}
/// How stored NoData samples participate in expressions.
#[derive(Debug, Clone, Copy, Default)]
pub enum NoDataPolicy {
    /// Any referenced NoData input produces output NoData.
    #[default]
    Propagate,
    /// Evaluate stored sentinel values as ordinary numbers.
    EvaluateRawValues,
}
/// Evaluate expressions on strictly aligned rasters; creates a materialized GTiff by default.
#[derive(Clone)]
#[must_use]
pub struct Calculate {
    job: Job,
    names: Vec<String>,
    expressions: Vec<String>,
    dialect: ExpressionDialect,
    data_type: DataType,
    nodata: Option<f64>,
    policy: NoDataPolicy,
}
options::debug_builder!(Calculate);
impl Calculate {
    /// Describe a new output; add at least one named input and expression.
    pub fn new(output: impl AsRef<Path>) -> Self {
        Self {
            job: Job::multiple(Vec::<Source>::new(), output, "GTiff"),
            names: Vec::new(),
            expressions: Vec::new(),
            dialect: ExpressionDialect::default(),
            data_type: DataType::Float64,
            nodata: None,
            policy: NoDataPolicy::default(),
        }
    }
    /// Add a unique ASCII identifier and its independent source/open options.
    pub fn input(mut self, name: impl Into<String>, source: impl Into<Source>) -> Self {
        self.names.push(name.into());
        self.job.sources.push(source.into());
        self
    }
    /// Append an expression; native multiband expansion determines its output bands.
    pub fn expression(mut self, expression: impl Into<String>) -> Self {
        self.expressions.push(expression.into());
        self
    }
    /// Select the expression dialect explicitly.
    pub fn dialect(mut self, dialect: ExpressionDialect) -> Self {
        self.dialect = dialect;
        self
    }
    /// Select stored output type; arithmetic is floating point.
    pub fn data_type(mut self, data_type: DataType) -> Self {
        self.data_type = data_type;
        self
    }
    /// Set output NoData; NaN is allowed for floating outputs.
    pub fn nodata(mut self, value: f64) -> Self {
        self.nodata = Some(value);
        self
    }
    /// Select propagation or raw sentinel evaluation.
    pub fn nodata_policy(mut self, policy: NoDataPolicy) -> Self {
        self.policy = policy;
        self
    }
    options::common!();
    /// Run synchronously.
    pub fn run(self) -> Result<Report> {
        self.run_with(&mut Control::default())
    }
    /// Run with borrowed cancellation/progress.
    pub fn run_with(self, control: &mut Control<'_>) -> Result<Report> {
        let mut names = HashSet::new();
        for name in &self.names {
            let mut bytes = name.bytes();
            if !bytes
                .next()
                .is_some_and(|c| c.is_ascii_alphabetic() || c == b'_')
                || !bytes.all(|c| c.is_ascii_alphanumeric() || c == b'_')
                || !names.insert(name)
            {
                return Err(Error::invalid(
                    "input aliases must be unique ASCII identifiers",
                ));
            }
        }
        if self.expressions.is_empty() {
            return Err(Error::invalid("at least one expression is required"));
        }
        for expression in &self.expressions {
            crate::dataset::validate_text(expression, "expression")?;
        }
        validate_type(self.data_type)?;
        if let Some(value) = self.nodata {
            representable(value, self.data_type)?;
        }
        self.job
            .run(Kind::Raster, false, control, |prepared, progress| {
                let mut algorithm = Algorithm::new(&["raster", "calc"])?;
                validate_grid(&prepared.inputs)?;
                for input in &prepared.inputs {
                    validate_bands(input)?;
                }
                let driver = prepared.driver.as_ref().expect("typed output").short_name();
                if matches!(driver.as_str(), "VRT" | "GDALG") {
                    return Err(Error::unsupported(
                        "calculation output must be materialized",
                    ));
                }
                let calculated = prepared.temporary_path("calculated.vrt")?;
                algorithm.set(
                    "output",
                    Argument::String(
                        calculated
                            .to_str()
                            .ok_or_else(|| Error::unsupported("temporary path must be UTF-8"))?,
                    ),
                )?;
                algorithm.set("output-format", Argument::String("VRT"))?;
                algorithm.set("calc", Argument::Strings(&self.expressions))?;
                algorithm.set(
                    "dialect",
                    Argument::String(match self.dialect {
                        ExpressionDialect::MuParser => "muparser",
                        ExpressionDialect::Builtin => "builtin",
                    }),
                )?;
                algorithm.set("output-data-type", Argument::String(&self.data_type.name()))?;
                algorithm.set(
                    "propagate-nodata",
                    Argument::Boolean(matches!(self.policy, NoDataPolicy::Propagate)),
                )?;
                if let Some(value) = self.nodata {
                    algorithm.set("nodata", Argument::String(&value.to_string()))?;
                }
                // Calc reopens names internally; VRT serializes each already-open source's own options.
                let root = prepared
                    .temporary_path("input-0.vrt")?
                    .parent()
                    .expect("scratch parent")
                    .to_owned();
                let driver = DriverManager::get_driver_by_name("VRT").map_err(inspect_error)?;
                let mut descriptors = Vec::with_capacity(prepared.inputs.len());
                for (i, input) in prepared.inputs.iter().enumerate() {
                    let path = root.join(format!("input-{i}.vrt"));
                    let proxy = input
                        .create_copy(&driver, &path, &RasterCreationOptions::default())
                        .map_err(inspect_error)?;
                    let mut failure = None;
                    close(Some(proxy), Stage::CloseSource, &mut failure);
                    if let Some(error) = failure {
                        return Err(error);
                    }
                    descriptors.push(format!(
                        "{}={}",
                        self.names[i],
                        path.to_str()
                            .ok_or_else(|| Error::unsupported("temporary path must be UTF-8"))?
                    ));
                }
                algorithm.set("input", Argument::DatasetNames(&descriptors))?;
                algorithm.execute(progress).map_err(|error| {
                    use std::error::Error as _;
                    if error.source().is_some_and(|s| {
                        s.to_string()
                            .contains("is not supported by this GDAL build")
                    }) {
                        Error::unsupported(
                            "the selected expression engine is absent from this GDAL build",
                        )
                        .with_source(error)
                    } else {
                        error
                    }
                })?;
                let calculated = Source::new(&calculated).open_kind(Kind::Raster)?;
                let result = prepared.materialize(&calculated, progress);
                let mut failure = result.err();
                close(Some(calculated), Stage::CloseSource, &mut failure);
                failure.map_or(Ok(()), Err)
            })
    }
}
pub(super) fn validate_type(data_type: DataType) -> Result<()> {
    if matches!(
        data_type,
        DataType::Unknown | DataType::Int64 | DataType::UInt64
    ) {
        Err(Error::unsupported(
            "calculation and classification require a real 8/16/32-bit integer or floating output",
        ))
    } else {
        Ok(())
    }
}
pub(super) fn representable(value: f64, data_type: DataType) -> Result<()> {
    let valid = match data_type {
        DataType::UInt8 => value.fract() == 0. && (0. ..=u8::MAX as f64).contains(&value),
        DataType::Int8 => value.fract() == 0. && (i8::MIN as f64..=i8::MAX as f64).contains(&value),
        DataType::UInt16 => value.fract() == 0. && (0. ..=u16::MAX as f64).contains(&value),
        DataType::Int16 => {
            value.fract() == 0. && (i16::MIN as f64..=i16::MAX as f64).contains(&value)
        }
        DataType::UInt32 => value.fract() == 0. && (0. ..=u32::MAX as f64).contains(&value),
        DataType::Int32 => {
            value.fract() == 0. && (i32::MIN as f64..=i32::MAX as f64).contains(&value)
        }
        DataType::Float32 => value.is_nan() || (value.is_finite() && (value as f32).is_finite()),
        DataType::Float64 => value.is_finite() || value.is_nan(),
        _ => false,
    };
    if valid {
        Ok(())
    } else {
        Err(Error::invalid("value is outside the selected output type"))
    }
}
fn validate_bands(input: &Dataset) -> Result<()> {
    if input.raster_count() == 0 {
        return Err(Error::invalid("input has no raster bands"));
    }
    for i in 1..=input.raster_count() {
        validate_band(&input.rasterband(i).map_err(inspect_error)?)?;
    }
    Ok(())
}

pub(super) fn validate_band(band: &gdal::raster::RasterBand<'_>) -> Result<()> {
    validate_type(band.band_type())?;
    let flags = band.mask_flags().map_err(inspect_error)?;
    let per_band_nodata = flags.is_nodata() && !flags.is_per_dataset();
    if !flags.is_all_valid() && !per_band_nodata {
        return Err(Error::unsupported(
            "normalize shared NoData, independent masks and alpha to per-band NoData before raster algebra",
        ));
    }
    Ok(())
}
fn validate_grid(inputs: &[Dataset]) -> Result<()> {
    let first = inputs
        .first()
        .ok_or_else(|| Error::invalid("at least one input is required"))?;
    let first_srs = first.spatial_ref().map_err(|e| {
        Error::invalid("calculation requires an explicit source CRS").with_source(e)
    })?;
    let transform = first.geo_transform().map_err(inspect_error)?;
    let scale = transform[1]
        .hypot(transform[4])
        .min(transform[2].hypot(transform[5]));
    if !scale.is_finite() || scale <= 0. || transform.iter().any(|v| !v.is_finite()) {
        return Err(Error::invalid(
            "input requires a finite, invertible affine grid",
        ));
    }
    use gdal::GeoTransformEx;
    transform.invert().map_err(inspect_error)?;
    for input in &inputs[1..] {
        let srs = input.spatial_ref().map_err(inspect_error)?;
        let gt = input.geo_transform().map_err(inspect_error)?;
        if srs != first_srs
            || input.raster_size() != first.raster_size()
            || gt
                .iter()
                .zip(transform)
                .any(|(a, b)| !a.is_finite() || (a - b).abs() > scale * 1e-9)
        {
            return Err(Error::invalid(
                "calculation inputs must share CRS, dimensions and full affine grid; align explicitly with Warp",
            ));
        }
    }
    Ok(())
}
#[cfg(all(test, gdal_3_12))]
mod tests {
    use super::*;
    #[test]
    fn raster_algebra_rejects_dataset_nodata_tuples_before_writing() -> anyhow::Result<()> {
        use crate::operation::{ErrorKind, OutputState};
        use crate::raster::Reclassify;
        use gdal::{DriverManager, Metadata, raster::Buffer, spatial_ref::SpatialRef};
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("tuple.tif");
        let mut ds = DriverManager::get_driver_by_name("GTiff")?
            .create_with_band_type::<u8, _>(&path, 4, 1, 2)?;
        ds.set_spatial_ref(&SpatialRef::from_epsg(3857)?)?;
        ds.set_geo_transform(&[0., 1., 0., 1., 0., -1.])?;
        ds.set_metadata_item("NODATA_VALUES", "0 0", "")?;
        for (i, values) in [vec![0u8, 0, 1, 1], vec![0u8, 1, 0, 1]]
            .into_iter()
            .enumerate()
        {
            ds.rasterband(i + 1)?
                .write((0, 0), (4, 1), &mut Buffer::new((4, 1), values))?;
        }
        ds.close()?;
        let ds = Dataset::open(&path)?;
        assert_eq!(
            ds.rasterband(1)?
                .open_mask_band()?
                .read_band_as::<u8>()?
                .data(),
            [0, 255, 255, 255]
        );
        ds.close()?;
        let classified = dir.path().join("classified.tif");
        let calculated = dir.path().join("calculated.tif");
        for (result, out) in [
            (
                Reclassify::new(&path, &classified)
                    .band(1)
                    .value(0, 10)
                    .value(1, 20)
                    .run(),
                &classified,
            ),
            (
                Calculate::new(&calculated)
                    .input("A", &path)
                    .dialect(ExpressionDialect::Builtin)
                    .expression("sum(k=1)")
                    .run(),
                &calculated,
            ),
        ] {
            let error = result.expect_err("a per-band expression cannot preserve tuple validity");
            assert_eq!(
                (error.kind(), error.output_state()),
                (ErrorKind::Unsupported, OutputState::NotModified)
            );
            assert!(!out.exists());
        }
        Ok(())
    }
    #[test]
    fn multiple_expressions_and_callback_interruptions_close_native_outputs() -> anyhow::Result<()>
    {
        use crate::operation::{ErrorKind, OutputState};
        let dir = tempfile::tempdir()?;
        let source = dir.path().join("a.tif");
        super::super::test_support::fixture(&source, 2, 1, &[1u8, 2])?;
        let before = std::fs::read(&source)?;
        let out = dir.path().join("two.tif");
        let _ = Calculate::new(&out)
            .input("A", &source)
            .dialect(ExpressionDialect::Builtin)
            .expression("sum(k=1)")
            .expression("sum(k=2)")
            .run()?;
        let ds = Dataset::open(&out)?;
        assert_eq!(ds.raster_count(), 2);
        assert_eq!(ds.rasterband(2)?.read_band_as::<f64>()?.data(), [3., 4.]);
        ds.close()?;
        for panic in [false, true] {
            let path = dir.path().join(format!("cancel-{panic}.tif"));
            let mut callback = |_: &crate::operation::Progress| {
                assert!(!panic, "test callback panic");
                std::ops::ControlFlow::Break(())
            };
            let error = Calculate::new(&path)
                .input("A", &source)
                .dialect(ExpressionDialect::Builtin)
                .expression("sum(k=1)")
                .run_with(&mut Control::default().on_progress(&mut callback))
                .unwrap_err();
            assert_eq!(
                error.kind(),
                if panic {
                    ErrorKind::CallbackPanicked
                } else {
                    ErrorKind::Cancelled
                }
            );
            if error.output_state() == OutputState::NotModified {
                assert!(!path.exists());
            }
            if path.exists() {
                std::fs::remove_file(path)?;
            }
        }
        assert_eq!(std::fs::read(&source)?, before);
        assert!(!std::fs::read_dir(dir.path())?.any(|e| {
            e.unwrap()
                .file_name()
                .to_string_lossy()
                .contains("spatial-core")
        }));
        Ok(())
    }
    #[test]
    fn input_options_nodata_and_engine_capabilities_are_not_silently_changed() -> anyhow::Result<()>
    {
        use super::super::test_support::{fixture, pixels};
        let dir = tempfile::tempdir()?;
        let a = dir.path().join("a.tif");
        let b = dir.path().join("b.tif");
        fixture(&a, 4, 4, &(1u8..=16).collect::<Vec<_>>())?;
        fixture(&b, 2, 2, &[100u8; 4])?;
        let mut ds = Dataset::open_ex(
            &a,
            gdal::DatasetOptions {
                open_flags: gdal::GdalOpenFlags::GDAL_OF_UPDATE,
                ..Default::default()
            },
        )?;
        ds.build_overviews("NEAREST", &[2], &[])?;
        ds.close()?;
        let mut ds = Dataset::open_ex(
            &b,
            gdal::DatasetOptions {
                open_flags: gdal::GdalOpenFlags::GDAL_OF_UPDATE,
                ..Default::default()
            },
        )?;
        ds.set_geo_transform(&[0., 2., 0., 4., 0., -2.])?;
        ds.close()?;
        let out = dir.path().join("options.tif");
        let _ = Calculate::new(&out)
            .input(
                "A",
                Source::new(&a)
                    .allowed_drivers(["GTiff"])
                    .open_option("OVERVIEW_LEVEL", "0"),
            )
            .input("B", &b)
            .dialect(ExpressionDialect::Builtin)
            .expression("sum")
            .run()?;
        assert_eq!(pixels(&out)?, [101., 103., 109., 111.]);
        let source = dir.path().join("nodata.tif");
        fixture(&source, 2, 1, &[0u8, 2])?;
        let ds = Dataset::open_ex(
            &source,
            gdal::DatasetOptions {
                open_flags: gdal::GdalOpenFlags::GDAL_OF_UPDATE,
                ..Default::default()
            },
        )?;
        ds.rasterband(1)?.set_no_data_value(Some(0.))?;
        ds.close()?;
        for (policy, expected) in [
            (NoDataPolicy::Propagate, vec![-9999., 3.]),
            (NoDataPolicy::EvaluateRawValues, vec![1., 3.]),
        ] {
            let out = dir.path().join(format!("{policy:?}.tif"));
            let _ = Calculate::new(&out)
                .input("A", &source)
                .dialect(ExpressionDialect::Builtin)
                .expression("sum(k=1)")
                .nodata(-9999.)
                .nodata_policy(policy)
                .run()?;
            assert_eq!(pixels(&out)?, expected);
        }
        let out = dir.path().join("muparser.tif");
        match Calculate::new(&out)
            .input("A", &source)
            .expression("A[1]+1")
            .run()
        {
            Ok(_) => assert_eq!(pixels(&out)?[1], 3.),
            Err(error) => {
                assert_eq!(error.kind(), crate::operation::ErrorKind::Unsupported);
                assert_eq!(
                    error.output_state(),
                    crate::operation::OutputState::NotModified
                );
                assert!(!out.exists());
            }
        }
        Ok(())
    }
    #[test]
    fn multiple_inputs_nodata_and_shifted_grid_are_explicit() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let a = dir.path().join("a.tif");
        let b = dir.path().join("b.tif");
        super::super::test_support::fixture(&a, 2, 1, &[1u8, 2])?;
        super::super::test_support::fixture(&b, 2, 1, &[3u8, 4])?;
        let out = dir.path().join("sum.tif");
        let _ = Calculate::new(&out)
            .input("A", &a)
            .input("B", &b)
            .dialect(ExpressionDialect::Builtin)
            .expression("sum")
            .run()?;
        assert_eq!(super::super::test_support::pixels(&out)?, [4., 6.]);
        let mut ds = Dataset::open_ex(
            &b,
            gdal::DatasetOptions {
                open_flags: gdal::GdalOpenFlags::GDAL_OF_UPDATE,
                ..Default::default()
            },
        )?;
        let mut gt = ds.geo_transform()?;
        gt[0] += gt[1] / 2.;
        ds.set_geo_transform(&gt)?;
        ds.close()?;
        let bad = dir.path().join("bad.tif");
        let error = Calculate::new(&bad)
            .input("A", &a)
            .input("B", &b)
            .expression("A+B")
            .run()
            .unwrap_err();
        assert_eq!(
            error.output_state(),
            crate::operation::OutputState::NotModified
        );
        assert!(!bad.exists());
        Ok(())
    }
}
