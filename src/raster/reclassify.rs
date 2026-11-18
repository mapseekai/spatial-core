use super::{
    DataType, PixelValue,
    calculate::{representable, validate_band, validate_type},
    execute::{Job, processing_error},
    options,
};
use crate::{
    dataset::{Kind, Source},
    gdal_ext::{
        algorithm::{Algorithm, Argument},
        execution::close,
        query::{check_progress, inspect_error},
        raster::utility,
    },
    operation::{Control, Error, Report, Result, Stage},
};
use std::{
    ops::{Bound, RangeBounds},
    path::Path,
};

/// Behavior when no value or interval rule matches.
#[derive(Debug, Clone, Copy, Default)]
pub enum Unmatched {
    /// Fail instead of silently passing through unknown classes.
    #[default]
    Error,
    /// Write output NoData.
    NoData,
    /// Preserve the stored input value (subject to output conversion).
    Keep,
    /// Write a constant.
    Value(PixelValue),
}
#[derive(Debug, Clone)]
struct Rule {
    low: Bound<PixelValue>,
    high: Bound<PixelValue>,
    output: PixelValue,
}
/// Classify a selected band with nonoverlapping rules; creates GTiff/Float64 by default.
#[derive(Clone)]
#[must_use]
pub struct Reclassify {
    job: Job,
    band: usize,
    rules: Vec<Rule>,
    nodata: Option<PixelValue>,
    output_nodata: Option<PixelValue>,
    unmatched: Unmatched,
    data_type: DataType,
}
options::debug_builder!(Reclassify);
impl Reclassify {
    /// Describe classification without opening input/output.
    pub fn new(source: impl Into<Source>, output: impl AsRef<Path>) -> Self {
        Self {
            job: Job::new(source, output, "GTiff"),
            band: 1,
            rules: Vec::new(),
            nodata: None,
            output_nodata: None,
            unmatched: Unmatched::Error,
            data_type: DataType::Float64,
        }
    }
    /// Select a one-based source band.
    pub fn band(mut self, band: usize) -> Self {
        self.band = band;
        self
    }
    /// Map one exact stored value.
    pub fn value(mut self, input: impl Into<PixelValue>, output: impl Into<PixelValue>) -> Self {
        let input = input.into();
        self.rules.push(Rule {
            low: Bound::Included(input),
            high: Bound::Included(input),
            output: output.into(),
        });
        self
    }
    /// Map a Rust interval; endpoints retain their open/closed meaning.
    pub fn range<T: Copy + Into<PixelValue>>(
        mut self,
        input: impl RangeBounds<T>,
        output: impl Into<PixelValue>,
    ) -> Self {
        self.rules.push(Rule {
            low: input.start_bound().map(|v| (*v).into()),
            high: input.end_bound().map(|v| (*v).into()),
            output: output.into(),
        });
        self
    }
    /// Map source NoData to a valid class. Omission preserves invalidity.
    pub fn nodata(mut self, value: impl Into<PixelValue>) -> Self {
        self.nodata = Some(value.into());
        self
    }
    /// Set the stored sentinel and metadata together.
    pub fn output_nodata(mut self, value: impl Into<PixelValue>) -> Self {
        self.output_nodata = Some(value.into());
        self
    }
    /// Select the unmatched-value behavior; default is Error.
    pub fn unmatched(mut self, unmatched: Unmatched) -> Self {
        self.unmatched = unmatched;
        self
    }
    /// Select stored output type; wide integer arithmetic is not supported.
    pub fn data_type(mut self, data_type: DataType) -> Self {
        self.data_type = data_type;
        self
    }
    options::common!();
    /// Run synchronously.
    pub fn run(self) -> Result<Report> {
        self.run_with(&mut Control::default())
    }
    /// Run with borrowed progress and cancellation.
    pub fn run_with(self, control: &mut Control<'_>) -> Result<Report> {
        options::band(self.band)?;
        validate_type(self.data_type)?;
        let _ = Self::mapping(
            &self.rules,
            self.nodata,
            self.output_nodata,
            self.unmatched,
            self.data_type,
            None,
        )?;
        self.job
            .run(Kind::Raster, false, control, |prepared, progress| {
                let source = options::source_band(&prepared.inputs[0], self.band)?;
                validate_band(&source)?;
                let source_nodata = source.no_data_value();
                let needs_nodata = (source_nodata.is_some() && self.nodata.is_none())
                    || matches!(self.unmatched, Unmatched::NoData);
                let effective_nodata = match self.output_nodata.map(number).transpose()? {
                    Some(value) => Some(value),
                    None if needs_nodata => Some(match source_nodata {
                        Some(value) => value,
                        None if matches!(self.data_type, DataType::Float32 | DataType::Float64) => {
                            f64::NAN
                        }
                        None => return Err(Error::invalid(
                            "integer NoData output requires an explicit output_nodata sentinel",
                        )),
                    }),
                    None => None,
                };
                if let Some(sentinel) = effective_nodata {
                    representable(sentinel, self.data_type)?;
                    let unmatched = match self.unmatched {
                        Unmatched::Value(value) => Some(value),
                        _ => None,
                    };
                    for value in self.rules.iter()
                        .map(|r| r.output).chain(self.nodata).chain(unmatched)
                    {
                        if stored(number(value)?, self.data_type) == stored(sentinel, self.data_type) {
                            return Err(Error::invalid(
                                "classification output collides with output NoData after conversion",
                            ));
                        }
                    }
                }
                let mapping = Self::mapping(
                    &self.rules,
                    self.nodata,
                    self.output_nodata,
                    self.unmatched,
                    self.data_type,
                    source_nodata,
                )?;
                let mapping = mapping
                    .split(';')
                    .filter(|entry| source_nodata.is_some() || !entry.starts_with("NO_DATA="))
                    .map(|entry| {
                        let (input, output) = entry.split_once('=').expect("compiled mapping");
                        // Freeze source identity before assigning the derived output's different NoData metadata.
                        let input = if input == "NO_DATA" {
                            let value = source_nodata.expect("filtered NoData rule");
                            if value.is_nan() {
                                "NAN".to_owned()
                            } else {
                                value.to_string()
                            }
                        } else {
                            input.to_owned()
                        };
                        let output = if output == "NO_DATA" {
                            effective_nodata.expect("resolved NoData output").to_string()
                        } else {
                            output.to_owned()
                        };
                        format!("{input}={output}")
                    })
                    .collect::<Vec<_>>()
                    .join(";");
                let driver = prepared.driver.as_ref().expect("typed output").short_name();
                if matches!(driver.as_str(), "VRT" | "GDALG") {
                    return Err(Error::unsupported(
                        "classification output must be materialized",
                    ));
                }
                let path = prepared.temporary_path("reclassified.vrt")?;
                let view_path = prepared.temporary_path("selected.vrt")?;
                let view_name = crate::dataset::local_name(&view_path)?;
                let mut view_options = utility::Options::new(
                    utility::Kind::Translate,
                    &[
                        "-of".into(),
                        "VRT".into(),
                        "-b".into(),
                        self.band.to_string(),
                    ],
                )
                .map_err(processing_error)?;
                let mut outcome =
                    view_options.execute(&prepared.inputs, &view_name, None, progress);
                let mut view = outcome.output.take();
                let mut temporary = None;
                let result = (|| {
                    check_progress(progress)?;
                    if let Some(error) = outcome.error {
                        return Err(processing_error(error));
                    }
                    let view = view
                        .as_mut()
                        .ok_or_else(|| Error::invalid("cannot create band view"))?;
                    view.flush_cache().map_err(inspect_error)?;
                    let mut algorithm = Algorithm::new(&["raster", "reclassify"])?;
                    algorithm.bind("input", view)?;
                    algorithm.set(
                        "output",
                        Argument::String(
                            path.to_str().ok_or_else(|| {
                                Error::unsupported("temporary path must be UTF-8")
                            })?,
                        ),
                    )?;
                    algorithm.set("output-format", Argument::String("VRT"))?;
                    algorithm.set("output-data-type", Argument::String(&self.data_type.name()))?;
                    algorithm.set("mapping", Argument::String(&mapping))?;
                    algorithm.execute(progress)?;
                    temporary = Some(
                        gdal::Dataset::open_ex(
                            &path,
                            gdal::DatasetOptions {
                                open_flags: gdal::GdalOpenFlags::GDAL_OF_RASTER
                                    | gdal::GdalOpenFlags::GDAL_OF_UPDATE,
                                ..Default::default()
                            },
                        )
                        .map_err(inspect_error)?,
                    );
                    temporary
                        .as_ref()
                        .expect("temporary raster")
                        .rasterband(1)
                        .map_err(inspect_error)?
                        .set_no_data_value(effective_nodata.map(|value| stored(value, self.data_type)))
                        .map_err(inspect_error)?;
                    prepared.materialize(temporary.as_ref().expect("temporary raster"), progress)
                })();
                let mut failure = result.err();
                close(temporary, Stage::CloseSource, &mut failure);
                close(view.take(), Stage::CloseSource, &mut failure);
                failure.map_or(Ok(()), Err)
            })
    }
    fn mapping(
        rules: &[Rule],
        nodata: Option<PixelValue>,
        output_nodata: Option<PixelValue>,
        unmatched: Unmatched,
        data_type: DataType,
        source_nodata: Option<f64>,
    ) -> Result<String> {
        if rules.is_empty() {
            return Err(Error::invalid(
                "at least one classification rule is required",
            ));
        }
        let sentinel = output_nodata.map(number).transpose()?;
        if let Some(value) = sentinel {
            representable(value, data_type)?;
        }
        if sentinel.is_some() && matches!(unmatched, Unmatched::Keep) {
            return Err(Error::invalid(
                "Keep with a new NoData sentinel could invalidate unchanged values; classify them explicitly",
            ));
        }
        let check_output = |value| -> Result<()> {
            let value = number(value)?;
            representable(value, data_type)?;
            if sentinel.is_some_and(|s| s == value || s.is_nan() && value.is_nan()) {
                return Err(Error::invalid(
                    "classification output collides with output NoData",
                ));
            }
            if value.is_nan() {
                return Err(Error::invalid(
                    "use Unmatched::NoData rather than a NaN class",
                ));
            }
            Ok(())
        };
        let mut ranges = Vec::with_capacity(rules.len());
        let mut mappings = Vec::with_capacity(rules.len() + 2);
        for rule in rules {
            check_output(rule.output)?;
            let (low, low_closed) = endpoint(rule.low, f64::NEG_INFINITY)?;
            let (high, high_closed) = endpoint(rule.high, f64::INFINITY)?;
            if low > high || low == high && !(low_closed && high_closed) {
                return Err(Error::invalid(
                    "classification interval is empty or reversed",
                ));
            }
            ranges.push((low, high, low_closed, high_closed));
            // GDAL expands NO_DATA to a singleton interval, so exclude it from ordinary ranges.
            if let Some(nodata) = source_nodata.filter(|v| v.is_finite())
                && (nodata > low || nodata == low && low_closed)
                && (nodata < high || nodata == high && high_closed)
            {
                if low == high {
                    return Err(Error::invalid(
                        "map the source NoData value with nodata(), not value()",
                    ));
                }
                if low < nodata {
                    mappings.push(range_mapping(low, nodata, low_closed, false, rule.output));
                }
                if nodata < high {
                    mappings.push(range_mapping(nodata, high, false, high_closed, rule.output));
                }
            } else {
                mappings.push(range_mapping(
                    low,
                    high,
                    low_closed,
                    high_closed,
                    rule.output,
                ));
            }
        }
        ranges.sort_by(|a, b| a.0.total_cmp(&b.0));
        for pair in ranges.windows(2) {
            if pair[0].1 > pair[1].0 || pair[0].1 == pair[1].0 && pair[0].3 && pair[1].2 {
                return Err(Error::invalid(
                    "classification rules overlap or duplicate a value",
                ));
            }
        }
        let no_data = output_nodata.map_or_else(|| "NO_DATA".to_owned(), |v| v.to_string());
        let no_data_mapping = if let Some(value) = nodata {
            check_output(value)?;
            value.to_string()
        } else {
            no_data.clone()
        };
        mappings.push(format!("NO_DATA={no_data_mapping}"));
        match unmatched {
            Unmatched::Error => {}
            Unmatched::NoData => mappings.push(format!("DEFAULT={no_data}")),
            Unmatched::Keep => mappings.push("DEFAULT=PASS_THROUGH".into()),
            Unmatched::Value(value) => {
                check_output(value)?;
                mappings.push(format!("DEFAULT={value}"));
            }
        }
        Ok(mappings.join(";"))
    }
}
fn range_mapping(
    low: f64,
    high: f64,
    low_closed: bool,
    high_closed: bool,
    output: PixelValue,
) -> String {
    if low == high {
        format!("{low}={output}")
    } else {
        format!(
            "{}{low},{high}{}={output}",
            if low_closed { "[" } else { "(" },
            if high_closed { "]" } else { ")" }
        )
    }
}
fn endpoint(bound: Bound<PixelValue>, unbounded: f64) -> Result<(f64, bool)> {
    match bound {
        Bound::Unbounded => Ok((unbounded, false)),
        Bound::Included(value) | Bound::Excluded(value) => {
            let number = number(value)?;
            if !number.is_finite() {
                return Err(Error::invalid(
                    "classification endpoints must be finite; use an unbounded range instead",
                ));
            }
            Ok((number, matches!(bound, Bound::Included(_))))
        }
    }
}
fn number(value: PixelValue) -> Result<f64> {
    match value {
        PixelValue::Int(value) if value.unsigned_abs() <= (1u64 << 53) => Ok(value as f64),
        PixelValue::UInt(value) if value <= (1u64 << 53) => Ok(value as f64),
        PixelValue::Float(value) => Ok(value),
        _ => Err(Error::unsupported(
            "native classification cannot exactly represent this integer",
        )),
    }
}
fn stored(value: f64, data_type: DataType) -> f64 {
    if data_type == DataType::Float32 {
        f64::from(value as f32)
    } else {
        value
    }
}
#[cfg(all(test, gdal_3_12))]
mod tests {
    use super::*;
    #[test]
    fn reclassification_validates_only_the_selected_band() -> anyhow::Result<()> {
        use super::super::test_support::{fixture, pixels};
        use crate::operation::{ErrorKind, OutputState};
        let dir = tempfile::tempdir()?;
        fixture(&dir.path().join("data.tif"), 2, 1, &[1u8, 2])?;
        let simple = r#"<SimpleSource><SourceFilename relativeToVRT="1">data.tif</SourceFilename><SourceBand>1</SourceBand></SimpleSource>"#;
        for (name, second) in [
            (
                "wide",
                format!(r#"<VRTRasterBand dataType="UInt64" band="2">{simple}</VRTRasterBand>"#),
            ),
            (
                "mask",
                format!(
                    r#"<VRTRasterBand dataType="Byte" band="2">{simple}<MaskBand><VRTRasterBand dataType="Byte">{simple}</VRTRasterBand></MaskBand></VRTRasterBand>"#
                ),
            ),
        ] {
            let source = dir.path().join(format!("{name}.vrt"));
            std::fs::write(
                &source,
                format!(
                    r#"<VRTDataset rasterXSize="2" rasterYSize="1"><SRS>EPSG:3857</SRS><GeoTransform>0,1,0,1,0,-1</GeoTransform><VRTRasterBand dataType="Byte" band="1">{simple}</VRTRasterBand>{second}</VRTDataset>"#
                ),
            )?;
            let output = dir.path().join(format!("{name}.tif"));
            let classify = |band, path: &Path| {
                Reclassify::new(&source, path)
                    .band(band)
                    .value(1, 10)
                    .value(2, 20)
                    .run()
            };
            let _ = classify(1, &output)?;
            assert_eq!(pixels(&output)?, [10., 20.]);
            let rejected = dir.path().join("rejected.tif");
            for (band, kind) in [(2, ErrorKind::Unsupported), (3, ErrorKind::InvalidArgument)] {
                let error = classify(band, &rejected).unwrap_err();
                assert_eq!(
                    (error.kind(), error.output_state()),
                    (kind, OutputState::NotModified)
                );
                assert!(!rejected.exists());
            }
        }
        Ok(())
    }
    #[test]
    fn unmatched_policies_and_float32_sentinel_collisions_are_explicit() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let source = dir.path().join("classes.tif");
        super::super::test_support::fixture(&source, 3, 1, &[1u8, 2, 9])?;
        for (policy, values) in [
            (Unmatched::Keep, [10., 2., 9.]),
            (Unmatched::Value(7.into()), [10., 7., 7.]),
        ] {
            let out = dir.path().join(format!("{policy:?}.tif"));
            let _ = Reclassify::new(&source, &out)
                .value(1, 10)
                .unmatched(policy)
                .run()?;
            assert_eq!(super::super::test_support::pixels(&out)?, values);
        }
        let out = dir.path().join("error.tif");
        let error = Reclassify::new(&source, &out)
            .value(1, 10)
            .run()
            .unwrap_err();
        assert_eq!(
            error.output_state(),
            crate::operation::OutputState::MayHaveChanged
        );
        let out = dir.path().join("collision.tif");
        let error = Reclassify::new(&source, &out)
            .value(1, 16_777_217)
            .output_nodata(16_777_216)
            .data_type(DataType::Float32)
            .run()
            .unwrap_err();
        assert_eq!(
            error.output_state(),
            crate::operation::OutputState::NotModified
        );
        assert!(!out.exists());
        Ok(())
    }
    #[test]
    fn nodata_inside_or_at_range_endpoints_has_an_independent_mapping() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        for nodata in [0., 1., 2., f64::NAN] {
            let source = dir.path().join(format!("source-{nodata}.tif"));
            let mut ds = gdal::DriverManager::get_driver_by_name("GTiff")?
                .create_with_band_type::<f64, _>(&source, 4, 1, 1)?;
            ds.set_geo_transform(&[0., 1., 0., 1., 0., -1.])?;
            ds.rasterband(1)?.write(
                (0, 0),
                (4, 1),
                &mut gdal::raster::Buffer::new((4, 1), vec![-1., 0., 2., nodata]),
            )?;
            ds.rasterband(1)?.set_no_data_value(Some(nodata))?;
            ds.close()?;
            for explicit in [false, true] {
                let out = dir.path().join(format!("out-{nodata}-{explicit}.tif"));
                let mut builder = Reclassify::new(&source, &out)
                    .range(..=1., 10)
                    .range((Bound::Excluded(1.), Bound::Unbounded), 20)
                    .output_nodata(255)
                    .data_type(DataType::UInt8);
                if explicit {
                    builder = builder.nodata(7);
                }
                let _ = builder.run()?;
                let expected_nodata = if explicit { 7. } else { 255. };
                let expected = [-1., 0., 2., nodata].map(|v| {
                    if v == nodata || v.is_nan() {
                        expected_nodata
                    } else if v <= 1. {
                        10.
                    } else {
                        20.
                    }
                });
                assert_eq!(super::super::test_support::pixels(&out)?, expected);
            }
        }
        Ok(())
    }
    #[test]
    fn source_nodata_can_change_sentinel_without_invalidating_a_new_class() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let input = dir.path().join("source-nodata.tif");
        super::super::test_support::fixture(&input, 4, 1, &[0u8, 1, 2, 9])?;
        let ds = gdal::Dataset::open_ex(
            &input,
            gdal::DatasetOptions {
                open_flags: gdal::GdalOpenFlags::GDAL_OF_UPDATE,
                ..Default::default()
            },
        )?;
        ds.rasterband(1)?.set_no_data_value(Some(0.))?;
        ds.close()?;
        let output = dir.path().join("mapped.tif");
        let _ = Reclassify::new(&input, &output)
            .value(1, 0)
            .value(2, 20)
            .unmatched(Unmatched::NoData)
            .output_nodata(255)
            .data_type(DataType::UInt8)
            .run()?;
        assert_eq!(
            super::super::test_support::pixels(&output)?,
            [255., 0., 20., 255.]
        );
        let sample = super::super::PixelInfo::new(
            &output,
            super::super::SampleLocation::Pixel { column: 1, row: 0 },
        )
        .run()?;
        assert!(sample.value().bands[0].valid);
        let no_nodata = dir.path().join("no-nodata.tif");
        super::super::test_support::fixture(&no_nodata, 2, 1, &[1u8, 9])?;
        let floating = dir.path().join("floating.tif");
        let _ = Reclassify::new(&no_nodata, &floating)
            .value(1, 10)
            .unmatched(Unmatched::NoData)
            .run()?;
        let pixels = super::super::test_support::pixels(&floating)?;
        assert_eq!(pixels[0], 10.);
        assert!(pixels[1].is_nan());
        Ok(())
    }
    #[test]
    fn closed_ranges_and_output_nodata_change_pixels_and_metadata() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let input = dir.path().join("in.tif");
        let out = dir.path().join("out.tif");
        super::super::test_support::fixture(&input, 4, 1, &[1u8, 2, 5, 9])?;
        let _ = Reclassify::new(&input, &out)
            .value(1, 10)
            .range(2..=5, 20)
            .unmatched(Unmatched::NoData)
            .output_nodata(255)
            .data_type(DataType::UInt8)
            .run()?;
        assert_eq!(
            super::super::test_support::pixels(&out)?,
            [10., 20., 20., 255.]
        );
        let ds = gdal::Dataset::open(&out)?;
        assert_eq!(ds.rasterband(1)?.no_data_value(), Some(255.));
        ds.close()?;
        let bad = dir.path().join("bad.tif");
        assert!(
            Reclassify::new(&input, &bad)
                .range(1..=3, 10)
                .range(3..5, 20)
                .run()
                .is_err()
        );
        assert!(!bad.exists());
        Ok(())
    }
}
