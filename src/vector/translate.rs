use super::{Dimensions, ErrorPolicy, SqlDialect, WriteMode};
use super::{native, options::TranslateOptions, target};
#[cfg(test)]
use crate::gdal_ext::execution::record_close;
use crate::gdal_ext::execution::{close, supported_version};
use crate::{
    config::gdal::GdalOptions,
    dataset::{self, Source},
    gdal_ext::{
        diagnostics::Diagnostics, progress::ProgressCallbackState, vector::translate as ffi,
    },
    operation::{Control, Error, ErrorKind, OutputState, Progress, Report, Result, Stage},
};
#[cfg(test)]
use gdal::Dataset;
use std::{
    fmt,
    ops::ControlFlow,
    path::{Path, PathBuf},
};

/// A synchronous vector conversion with explicit, validated write semantics.
///
/// ```no_run
/// use spatial_core::vector::{Translate, Dimensions};
/// # fn main() -> spatial_core::operation::Result<()> {
/// let report = Translate::new("roads.shp", "roads.gpkg")
///     .driver("GPKG")
///     .reproject_to("EPSG:3857")
///     .dimensions(Dimensions::Xy)
///     .run()?;
/// # Ok(()) }
/// ```
#[derive(Clone)]
#[must_use]
pub struct Translate {
    source: Source,
    output: PathBuf,
    options: TranslateOptions,
    config: GdalOptions,
}

impl fmt::Debug for Translate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Translate")
            .field("source", &self.source)
            .field("driver", &self.options.driver)
            .field("write_mode", &self.options.write_mode)
            .finish_non_exhaustive()
    }
}

impl Translate {
    /// Describe a conversion to a new local file or directory; performs no I/O.
    pub fn new(source: impl Into<Source>, output: impl AsRef<Path>) -> Self {
        Self {
            source: source.into(),
            output: output.as_ref().to_owned(),
            options: TranslateOptions::default(),
            config: GdalOptions::default(),
        }
    }
    /// Select an installed GDAL driver by its short name; default is unambiguous inference.
    pub fn driver(mut self, name: impl Into<String>) -> Self {
        self.options.driver = Some(name.into());
        self
    }
    /// Replace the source layer selection. Omission selects every source layer.
    pub fn layers<I, S>(mut self, names: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.options.layers = Some(names.into_iter().map(Into::into).collect());
        self
    }
    /// Select one SQL result. Cannot be combined with layers, where, or typed append.
    pub fn sql(mut self, query: impl Into<String>, dialect: SqlDialect) -> Self {
        self.options.sql = Some((query.into(), dialect));
        self
    }
    /// Set a GDAL attribute filter expression on each selected input layer.
    pub fn where_clause(mut self, expression: impl Into<String>) -> Self {
        self.options.where_clause = Some(expression.into());
        self
    }
    /// Replace field selection. Names are escaped by the library; empty selects geometry only.
    pub fn fields<I, S>(mut self, names: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.options.fields = Some(names.into_iter().map(Into::into).collect());
        self
    }
    /// Filter by a source-CRS bounding box; this does not clip geometries.
    pub fn spatial_filter(mut self, bounds: [f64; 4]) -> Self {
        self.options.spatial_filter = Some(bounds);
        self
    }
    /// Name one output layer. Required in all update modes.
    pub fn layer_name(mut self, name: impl Into<String>) -> Self {
        self.options.layer_name = Some(name.into());
        self
    }
    /// Override source CRS for an explicitly requested reprojection.
    pub fn source_srs(mut self, definition: impl Into<String>) -> Self {
        self.options.source_srs = Some(definition.into());
        self
    }
    /// Transform coordinates to the given CRS.
    pub fn reproject_to(mut self, definition: impl Into<String>) -> Self {
        self.options.reproject_to = Some(definition.into());
        self
    }
    /// Change the CRS label without transforming coordinates.
    pub fn assign_srs(mut self, definition: impl Into<String>) -> Self {
        self.options.assign_srs = Some(definition.into());
        self
    }
    /// Explicitly select coordinate dimensions.
    pub fn dimensions(mut self, dimensions: Dimensions) -> Self {
        self.options.dimensions = Some(dimensions);
        self
    }
    /// Clip before reprojection, in source CRS units.
    pub fn clip_source(mut self, clip: super::Clip) -> Self {
        self.options.clip_source = Some(clip);
        self
    }
    /// Clip after reprojection, in destination CRS units.
    pub fn clip_destination(mut self, clip: super::Clip) -> Self {
        self.options.clip_destination = Some(clip);
        self
    }
    /// Simplify in source CRS units before reprojection; conflicts with segmentize.
    pub fn simplify(mut self, tolerance: f64) -> Self {
        self.options.simplify = Some(tolerance);
        self
    }
    /// Add vertices to limit segment length in source CRS units before reprojection.
    pub fn segmentize(mut self, max_distance: f64) -> Self {
        self.options.segmentize = Some(max_distance);
        self
    }
    /// Write collection members as separate features; output FIDs may change.
    pub fn explode_collections(mut self, enabled: bool) -> Self {
        self.options.explode_collections = enabled;
        self
    }
    /// Explicitly convert output geometry type.
    /// Concrete types require `dimensions` to avoid implicit loss of Z/M coordinates.
    pub fn geometry_type(mut self, geometry_type: super::GeometryType) -> Self {
        self.options.geometry_type = Some(geometry_type);
        self
    }
    /// Opt in to GDAL's geometry repair operation.
    pub fn make_valid(mut self, enabled: bool) -> Self {
        self.options.make_valid = enabled;
        self
    }
    /// Select native error handling; the default is Abort.
    pub fn error_policy(mut self, policy: ErrorPolicy) -> Self {
        self.options.error_policy = policy;
        self
    }
    /// Select the exact creation/update contract.
    pub fn write_mode(mut self, mode: WriteMode) -> Self {
        self.options.write_mode = mode;
        self
    }
    /// Set a dataset creation option, replacing the same case-insensitive key.
    pub fn dataset_option(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.options.dataset_options.set(key.into(), value.into());
        self
    }
    /// Set a layer creation option, replacing the same case-insensitive key.
    pub fn layer_option(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.options.layer_options.set(key.into(), value.into());
        self
    }
    /// Set an option used when opening an existing output in update mode.
    pub fn output_open_option(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.options
            .output_open_options
            .set(key.into(), value.into());
        self
    }
    /// Apply thread-local GDAL configuration through open, execution and close.
    pub fn gdal_options(mut self, options: GdalOptions) -> Self {
        self.config = options;
        self
    }
    /// Execute synchronously; success includes explicit dataset closure.
    pub fn run(self) -> Result<Report> {
        self.run_with(&mut Control::default())
    }
    /// Execute with borrowed progress and cancellation control.
    pub fn run_with(self, control: &mut Control<'_>) -> Result<Report> {
        execute(
            self.source,
            self.output,
            Request::Typed(Box::new(self.options)),
            self.config,
            control,
        )
    }
}

pub(super) enum Request {
    Typed(Box<TranslateOptions>),
    Native(Vec<String>),
}

pub(super) fn execute(
    source: Source,
    output_path: PathBuf,
    request: Request,
    config: GdalOptions,
    control: &mut Control<'_>,
) -> Result<Report> {
    source.validate()?;
    dataset::local_name(&output_path)?;
    control.check()?;
    let version = supported_version()?;
    match &request {
        Request::Typed(options) => options.validate(version)?,
        Request::Native(args) => native::validate_args(args)?,
    }
    let diagnostics = Diagnostics::enter();
    let config_guard = config.enter();
    let mut input = None;
    let mut output = None;
    let mut output_state = OutputState::NotModified;
    let result = (|| {
        let active = control.is_active();
        // Native parsing may read @files, so it belongs inside both guards.
        let mut native_options = match &request {
            Request::Native(args) => {
                target::reject_alias(&source, &output_path)?;
                let mut args = args.clone();
                if active {
                    args.push("-progress".into());
                }
                Some(ffi::Options::new(&args).map_err(|e| Error::native(Stage::Prepare, e))?)
            }
            Request::Typed(_) => None,
        };
        input = Some(source.open()?);
        let input = input.as_mut().expect("opened source");
        target::reject_dataset_alias(input, &output_path)?;
        let destination = match &request {
            Request::Typed(options) => {
                let target = target::prepare(&source, input, &output_path, options)?;
                native_options = Some(
                    ffi::Options::new(&options.args(
                        &target.driver,
                        &target.layers,
                        active,
                        version,
                    ))
                    .map_err(|e| Error::native(Stage::Prepare, e))?,
                );
                control.check()?;
                if options.write_mode != WriteMode::CreateDataset {
                    output_state = OutputState::MayHaveChanged;
                    output = Some(
                        dataset::open(&target.name, &[], &options.output_open_options, true)
                            .map_err(|e| Error::native(Stage::OpenOutput, e))?,
                    );
                    target::check_update(
                        input,
                        output.as_ref().expect("opened output"),
                        &target,
                        options,
                    )?;
                }
                target.name
            }
            Request::Native(_) => dataset::local_name(&output_path)?,
        };
        control.check()?;
        let failure_count = diagnostics.failures();
        let mut callback = |progress: Progress| {
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
        let mut progress = ProgressCallbackState::new(&mut callback);
        output_state = OutputState::MayHaveChanged;
        let outcome = ffi::translate(
            input,
            &destination,
            output.as_mut(),
            native_options.as_mut().expect("parsed options"),
            active.then_some(&mut progress),
        );
        if let Some(created) = outcome.created {
            output = Some(created);
        }
        if let Some(reason) = progress.interruption() {
            return Err(Error::new(
                reason,
                Stage::Execute,
                "conversion interrupted by progress control",
            ));
        }
        if outcome.usage_error {
            return Err(Error::new(
                ErrorKind::InvalidArgument,
                Stage::Execute,
                "GDAL rejected the conversion arguments",
            ));
        }
        if let Some(error) = outcome.error {
            return Err(Error::native(Stage::Execute, error));
        }
        if matches!(&request, Request::Typed(options) if options.error_policy == ErrorPolicy::Abort)
            && diagnostics.failures() != failure_count
        {
            return Err(Error::new(
                ErrorKind::Translate,
                Stage::Execute,
                "GDAL reported a failure; inspect diagnostics",
            ));
        }
        Ok(())
    })();
    let mut failure = result.err();
    close(output, Stage::CloseOutput, &mut failure);
    close(input, Stage::CloseSource, &mut failure);
    drop(config_guard);
    let report = diagnostics.take_report();
    if let Some(mut error) = failure {
        error.output_state = output_state;
        error.report = report;
        Err(error)
    } else {
        Ok(report)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{control::Cancellation, vector::NativeTranslate};
    use gdal::vector::LayerAccess;

    #[test]
    fn explicit_gpkg_fid_preserves_a_conflicting_source_attribute() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let json = dir.path().join("input.geojson");
        std::fs::write(
            &json,
            r#"{"type":"FeatureCollection","name":"points","features":[{"type":"Feature","id":1,"properties":{"fid":100},"geometry":{"type":"Point","coordinates":[1,2]}},{"type":"Feature","id":2,"properties":{"fid":200},"geometry":{"type":"Point","coordinates":[3,4]}}]}"#,
        )?;
        let source = dir.path().join("input.fgb");
        let _ = Translate::new(&json, &source).run()?;
        let output = dir.path().join("output.gpkg");
        let _ = Translate::new(&source, &output)
            .layer_option("FID", "row_id")
            .run()?;
        let ds = Dataset::open(output)?;
        let mut layer = ds.layer(0)?;
        let index = layer.defn().field_index("fid")?;
        let mut values = layer
            .features()
            .map(|f| f.field_as_integer64(index).map(Option::unwrap))
            .collect::<std::result::Result<Vec<_>, _>>()?;
        values.sort_unstable();
        assert_eq!(values, [100, 200]);
        Ok(())
    }

    #[test]
    fn native_creation_and_update_protect_shared_shapefile_companions() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let input = points(dir.path());
        // A matching GeoJSON stem is independent, not a Shapefile companion.
        let shape = input.with_extension("shp");
        let _ = Translate::new(&input, &shape)
            .layer_option("ENCODING", "UTF-8")
            .run()?;
        assert_points(&input)?;
        let dbf = dir.path().join("attributes.dbf");
        let _ =
            NativeTranslate::new(&input, &dbf, ["-f", "ESRI Shapefile", "-nlt", "NONE"]).run()?;
        let original = std::fs::read(&dbf)?;
        let error = NativeTranslate::new(
            &dbf,
            dbf.with_extension("shp"),
            ["-f", "ESRI Shapefile", "-select", "name"],
        )
        .run()
        .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::InvalidArgument);
        assert_eq!(std::fs::read(&dbf)?, original);
        #[cfg(unix)]
        {
            let target = dir.path().join("target.shp");
            let _ = Translate::new(&input, &target).run()?;
            std::fs::remove_file(target.with_extension("dbf"))?;
            std::fs::hard_link(shape.with_extension("dbf"), target.with_extension("dbf"))?;
            let original = std::fs::read(shape.with_extension("dbf"))?;
            let typed = Translate::new(&shape, &target)
                .layer_name("target")
                .write_mode(WriteMode::AppendFeatures)
                .run()
                .unwrap_err();
            assert_eq!(typed.kind(), ErrorKind::InvalidArgument);
            let native = NativeTranslate::new(&shape, &target, ["-append", "-nln", "target"])
                .run()
                .unwrap_err();
            assert_eq!(native.kind(), ErrorKind::InvalidArgument);
            assert_eq!(std::fs::read(shape.with_extension("dbf"))?, original);
        }
        Ok(())
    }

    fn points(directory: &Path) -> PathBuf {
        let path = directory.join("points.geojson");
        std::fs::write(&path, r#"{"type":"FeatureCollection","name":"points","features":[
          {"type":"Feature","properties":{"name":"甲","value":9007199254740991,"nullable":"present","time":"2024-01-02T03:04:05Z"},"geometry":{"type":"Point","coordinates":[120,30,9]}},
          {"type":"Feature","properties":{"name":"乙","value":2,"nullable":null,"time":"2024-01-03T03:04:05Z"},"geometry":{"type":"Point","coordinates":[121,31,10]}},
          {"type":"Feature","properties":{"name":"丙","value":3,"nullable":null,"time":null},"geometry":{"type":"Point","coordinates":[122,32,11]}}
        ]}"#).unwrap();
        path
    }

    fn count(path: &Path, layer: &str) -> u64 {
        let ds = Dataset::open(path).unwrap();
        ds.layer_by_name(layer).unwrap().feature_count()
    }

    fn assert_points(path: &Path) -> anyhow::Result<()> {
        check_points(path, true)
    }

    fn check_points(path: &Path, preserves_null: bool) -> anyhow::Result<()> {
        let ds = Dataset::open(path)?;
        let mut layer = ds.layer(0)?;
        assert_eq!(layer.feature_count(), 3);
        // Shapefile's ESRI WKT may omit authority IDs, so check actual CRS behavior.
        use gdal::spatial_ref::{AxisMappingStrategy, CoordTransform, SpatialRef};
        let mut source_srs = layer.spatial_ref().unwrap();
        let mut wgs84 = SpatialRef::from_epsg(4979)?;
        source_srs.set_axis_mapping_strategy(AxisMappingStrategy::TraditionalGisOrder);
        wgs84.set_axis_mapping_strategy(AxisMappingStrategy::TraditionalGisOrder);
        let (mut x, mut y, mut z) = ([120.0], [30.0], [9.0]);
        CoordTransform::new(&source_srs, &wgs84)?.transform_coords(&mut x, &mut y, &mut z)?;
        assert!(
            (x[0] - 120.0).abs() < 1e-9 && (y[0] - 30.0).abs() < 1e-9 && (z[0] - 9.0).abs() < 1e-9
        );
        let name = layer.defn().field_index("name")?;
        let value = layer.defn().field_index("value")?;
        let nullable = layer.defn().field_index("nullable")?;
        let mut features = layer.features().collect::<Vec<_>>();
        features.sort_by_key(|f| f.field_as_integer64(value).unwrap().unwrap());
        if preserves_null {
            assert!(
                features[0].field(nullable)?.is_none(),
                "{}: {:?}",
                path.display(),
                features[0].field(nullable)?
            );
        } else {
            // Some drivers materialize unset string attributes as empty strings.
            assert!(
                features[0]
                    .field_as_string(nullable)?
                    .is_none_or(|value| value.is_empty())
            );
        }
        let first = features.last().unwrap();
        assert_eq!(first.field_as_string(name)?.as_deref(), Some("甲"));
        assert_eq!(first.field_as_integer64(value)?, Some(9007199254740991));
        assert_eq!(first.geometry().unwrap().get_point(0), (120.0, 30.0, 9.0));
        Ok(())
    }

    #[test]
    fn real_formats_roundtrip_fields_geometry_crs_and_nulls() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let input = points(dir.path());
        let gpkg = dir.path().join("base.gpkg");
        let _ = Translate::new(&input, &gpkg).run()?;
        assert_points(&gpkg)?;
        for (driver, extension) in [
            ("GeoJSON", "geojson"),
            ("FlatGeobuf", "fgb"),
            ("ESRI Shapefile", "shp"),
        ] {
            let output = dir
                .path()
                .join(format!("converted-{extension}.{extension}"));
            let job = Translate::new(&gpkg, &output)
                .driver(driver)
                .layer_name("points");
            let job = if driver == "ESRI Shapefile" {
                job.layer_option("ENCODING", "UTF-8")
            } else {
                job
            };
            let _ = job.run()?;
            check_points(&output, driver == "GeoJSON")?;
            let back = dir.path().join(format!("{driver}.gpkg"));
            let _ = Translate::new(&output, &back).layer_name("points").run()?;
            check_points(&back, driver == "GeoJSON")?;
        }
        Ok(())
    }

    #[test]
    fn write_modes_preserve_other_layers_and_reject_missing_or_existing_targets()
    -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let input = points(dir.path());
        let output = dir.path().join("target.gpkg");
        let _ = Translate::new(&input, &output).run()?;
        assert_eq!(
            Translate::new(&input, &output).run().unwrap_err().kind(),
            ErrorKind::OutputExists
        );
        let job = || Translate::new(&input, &output).layer_name("other");
        for mode in [WriteMode::ReplaceLayer, WriteMode::AppendFeatures] {
            assert_eq!(
                job().write_mode(mode).run().unwrap_err().kind(),
                ErrorKind::LayerNotFound
            );
        }
        let _ = job().write_mode(WriteMode::CreateLayer).run()?;
        assert_eq!(
            job()
                .write_mode(WriteMode::CreateLayer)
                .run()
                .unwrap_err()
                .kind(),
            ErrorKind::LayerExists
        );
        let _ = job().write_mode(WriteMode::AppendFeatures).run()?;
        assert_eq!(count(&output, "other"), 6);
        let _ = job()
            .write_mode(WriteMode::ReplaceLayer)
            .where_clause("value = 2")
            .run()?;
        assert_eq!(count(&output, "other"), 1);
        assert_eq!(count(&output, "points"), 3);
        let multiple = dir.path().join("multiple.gpkg");
        let _ = Translate::new(&output, &multiple)
            .layers(["other", "points"])
            .run()?;
        assert_eq!(Dataset::open(&multiple)?.layer_count(), 2);
        let single = dir.path().join("single.geojson");
        assert_eq!(
            Translate::new(&output, &single).run().unwrap_err().kind(),
            ErrorKind::Unsupported
        );
        assert!(!single.exists());
        Ok(())
    }

    #[test]
    fn append_rejects_schema_geometry_and_crs_mismatch_before_inserting() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let input = points(dir.path());
        let output = dir.path().join("mercator.gpkg");
        let _ = Translate::new(&input, &output)
            .reproject_to("EPSG:3857")
            .run()?;
        let job = || {
            Translate::new(&input, &output)
                .write_mode(WriteMode::AppendFeatures)
                .layer_name("points")
        };
        for job in [
            job(),
            job().reproject_to("EPSG:4326"),
            job().assign_srs("EPSG:4326"),
            job().dimensions(Dimensions::Xy),
        ] {
            assert_eq!(job.run().unwrap_err().kind(), ErrorKind::InvalidArgument);
            assert_eq!(count(&output, "points"), 3);
        }
        let _ = job().reproject_to("EPSG:3857").run()?;
        assert_eq!(count(&output, "points"), 6);
        let changed = dir.path().join("changed.geojson");
        std::fs::write(
            &changed,
            std::fs::read_to_string(&input)?.replace("\"value\"", "\"extra\""),
        )?;
        assert!(
            Translate::new(&changed, &output)
                .layer_name("points")
                .write_mode(WriteMode::AppendFeatures)
                .reproject_to("EPSG:3857")
                .run()
                .is_err()
        );
        assert_eq!(count(&output, "points"), 6);
        Ok(())
    }

    #[test]
    fn filters_sql_special_fields_and_invalid_where_are_not_silently_ignored() -> anyhow::Result<()>
    {
        let dir = tempfile::tempdir()?;
        let input = points(dir.path());
        let output = dir.path().join("filtered.gpkg");
        let _ = Translate::new(&input, &output)
            .where_clause("value = 2")
            .fields(["name"])
            .spatial_filter([120.5, 30.5, 121.5, 31.5])
            .run()?;
        assert_eq!(count(&output, "points"), 1);
        assert_eq!(Dataset::open(&output)?.layer(0)?.defn().fields().count(), 1);
        let invalid = dir.path().join("invalid.gpkg");
        assert!(
            Translate::new(&input, &invalid)
                .where_clause("missing = (")
                .error_policy(ErrorPolicy::SkipFailures)
                .run()
                .is_err()
        );
        assert!(!invalid.exists());
        let special = dir.path().join("special.geojson");
        std::fs::write(
            &special,
            r#"{"type":"FeatureCollection","name":"special","features":[{"type":"Feature","properties":{"中文":1,"with space":2,"a,b":3,"a\"b":4,"a\\b":5},"geometry":{"type":"Point","coordinates":[1,2]}}]}"#,
        )?;
        let selected = dir.path().join("selected.gpkg");
        let names = ["中文", "with space", "a,b", "a\"b", "a\\b"];
        let selection = Translate::new(&special, &selected).fields(names).run();
        if supported_version()? >= 3_090_000 {
            let _ = selection?;
            assert_eq!(
                Dataset::open(&selected)?
                    .layer(0)?
                    .defn()
                    .fields()
                    .map(|f| f.name())
                    .collect::<Vec<_>>(),
                names
            );
        } else {
            assert_eq!(selection.unwrap_err().kind(), ErrorKind::Unsupported);
            assert!(!selected.exists());
        }
        let sql = dir.path().join("sql.geojson");
        let _ = Translate::new(&input, &sql)
            .sql("SELECT name FROM points WHERE value = 2", SqlDialect::Ogr)
            .run()?;
        assert_eq!(Dataset::open(&sql)?.layer(0)?.feature_count(), 1);
        let bad_sql = dir.path().join("bad_sql.gpkg");
        let error = Translate::new(&input, &bad_sql)
            .sql("SELECT * FROM missing", SqlDialect::Ogr)
            .run()
            .unwrap_err();
        assert_eq!(error.output_state(), OutputState::MayHaveChanged);
        assert!(!error.diagnostics().is_empty());
        Ok(())
    }

    #[test]
    fn assignment_and_reprojection_have_distinct_coordinate_effects() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let input = points(dir.path());
        for reproject in [false, true] {
            let output = dir.path().join(format!("crs-{reproject}.gpkg"));
            let job = Translate::new(&input, &output).dimensions(Dimensions::Xy);
            let job = if reproject {
                job.reproject_to("EPSG:3857")
            } else {
                job.assign_srs("EPSG:3857")
            };
            let _ = job.run()?;
            let ds = Dataset::open(output)?;
            let mut layer = ds.layer(0)?;
            assert_eq!(layer.spatial_ref().unwrap().auth_code()?, 3857);
            let feature = layer.features().next().unwrap();
            let (x, y, z) = feature.geometry().unwrap().get_point(0);
            assert_eq!(z, 0.0);
            if reproject {
                assert!((x - 13358338.895192828).abs() < 0.01 && y > 3_000_000.0);
            } else {
                assert_eq!((x, y), (120.0, 30.0));
            }
        }
        Ok(())
    }

    #[test]
    fn native_boundary_and_known_driver_options_cannot_bypass_typed_contracts() -> anyhow::Result<()>
    {
        let dir = tempfile::tempdir()?;
        let input = points(dir.path());
        let output = dir.path().join("target.gpkg");
        for flag in [
            "-OO",
            "-IF",
            "--CONFIG",
            "--config=key",
            "--thread-local-config=value",
        ] {
            let error = NativeTranslate::new(&input, &output, [flag, "value"])
                .run()
                .unwrap_err();
            assert_eq!(error.kind(), ErrorKind::InvalidArgument);
            assert!(!output.exists());
        }
        for job in [
            Translate::new(&input, &output).layer_option("OVERWRITE", "NO"),
            Translate::new(&input, &output).layer_option("LAUNDER", "YES"),
            Translate::new(&input, &output).layers(["-skipfailures"]),
            Translate::new(&input, &output).layer_name("-skipfailures"),
            Translate::new(&input, &output).sql("@query.sql", SqlDialect::Native),
            Translate::new(&input, &output)
                .output_open_option("prelude_statements", "DELETE FROM points")
                .write_mode(WriteMode::CreateLayer)
                .layer_name("other"),
        ] {
            assert!(job.run().is_err());
            assert!(!output.exists());
        }
        let _ = NativeTranslate::new(&input, &output, ["-f", "GPKG", "-nln", "points"]).run()?;
        assert_points(&output)?;
        assert!(
            NativeTranslate::new(&input, dir.path().join("unknown.gpkg"), ["-unknown-flag"])
                .run()
                .is_err()
        );
        Ok(())
    }

    #[test]
    fn creation_protects_companions_directories_and_input_aliases() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let input = points(dir.path());
        let dbf = dir.path().join("my.data.dbf");
        std::fs::write(&dbf, "original")?;
        assert_eq!(
            Translate::new(&input, dbf.with_extension("shp"))
                .run()
                .unwrap_err()
                .kind(),
            ErrorKind::OutputExists
        );
        assert_eq!(std::fs::read_to_string(&dbf)?, "original");
        let gpkg = dir.path().join("out.gpkg");
        std::fs::write(dir.path().join("out.gpkg-wal"), "original")?;
        assert_eq!(
            Translate::new(&input, &gpkg).run().unwrap_err().kind(),
            ErrorKind::OutputExists
        );
        assert!(Translate::new(&input, &input).run().is_err());
        assert!(
            NativeTranslate::new(&input, &input, ["-f", "GeoJSON"])
                .run()
                .is_err()
        );
        let folder = dir.path().join("existing");
        std::fs::create_dir(&folder)?;
        assert_eq!(
            Translate::new(&input, &folder)
                .driver("ESRI Shapefile")
                .run()
                .unwrap_err()
                .kind(),
            ErrorKind::OutputExists
        );
        #[cfg(unix)]
        {
            let symlink = dir.path().join("symlink.geojson");
            std::os::unix::fs::symlink(&input, &symlink)?;
            let hardlink = dir.path().join("hardlink.geojson");
            std::fs::hard_link(&input, &hardlink)?;
            for alias in [symlink, hardlink] {
                assert_eq!(
                    Translate::new(&input, &alias).run().unwrap_err().kind(),
                    ErrorKind::InvalidArgument
                );
            }
        }
        assert_points(&input)?;
        Ok(())
    }

    #[test]
    fn cancellation_panic_and_handled_callback_errors_have_distinct_results() -> anyhow::Result<()>
    {
        let dir = tempfile::tempdir()?;
        let input = points(dir.path());
        let cancelled = Cancellation::default();
        cancelled.cancel();
        let output = dir.path().join("precancelled.gpkg");
        assert_eq!(
            Translate::new(&input, &output)
                .run_with(&mut Control::default().cancellation(&cancelled))
                .unwrap_err()
                .kind(),
            ErrorKind::Cancelled
        );
        assert!(!output.exists());
        for panic in [false, true] {
            let output = dir.path().join(format!("interrupt-{panic}.gpkg"));
            let mut calls = 0;
            let mut callback = |_: &Progress| {
                calls += 1;
                assert!(!panic, "user callback");
                ControlFlow::Break(())
            };
            let error = Translate::new(&input, &output)
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
            assert_eq!(calls, 1);
            assert_eq!(error.output_state(), OutputState::MayHaveChanged);
        }
        let output = dir.path().join("handled.gpkg");
        let mut callback = |_: &Progress| {
            assert!(Dataset::open("/definitely/absent/spatial-core-callback").is_err());
            ControlFlow::Continue(())
        };
        let report = Translate::new(&input, &output)
            .run_with(&mut Control::default().on_progress(&mut callback))?;
        assert!(!report.diagnostics().iter().any(|d| matches!(
            d.severity,
            crate::operation::Severity::Failure | crate::operation::Severity::Fatal
        )));
        assert_points(&output)?;
        Ok(())
    }

    #[test]
    fn close_errors_preserve_the_primary_failure_and_both_dataset_origins() {
        fn fault() -> gdal::errors::Result<()> {
            Err(gdal::errors::GdalError::CplError {
                class: gdal_sys::CPLErr::CE_Failure,
                number: 3,
                msg: "injected close failure".into(),
            })
        }
        let mut primary = Some(Error::new(
            ErrorKind::Cancelled,
            Stage::Execute,
            "cancelled",
        ));
        record_close(fault(), Stage::CloseOutput, &mut primary);
        record_close(fault(), Stage::CloseSource, &mut primary);
        let error = primary.unwrap();
        assert_eq!(error.kind(), ErrorKind::Cancelled);
        assert_eq!(
            error
                .cleanup_errors()
                .iter()
                .map(|e| e.stage)
                .collect::<Vec<_>>(),
            [Stage::CloseOutput, Stage::CloseSource]
        );
        let mut primary = None;
        record_close(fault(), Stage::CloseOutput, &mut primary);
        assert_eq!(primary.unwrap().kind(), ErrorKind::Close);
        fn send_sync<T: Send + Sync>() {}
        send_sync::<Error>();
        send_sync::<Report>();
        send_sync::<Translate>();
        send_sync::<NativeTranslate>();
    }

    #[test]
    fn driver_options_are_applied_at_their_own_stage_and_warnings_are_reported()
    -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let input = points(dir.path());
        let output = dir.path().join("options.gpkg");
        let source = Source::new(&input)
            .allowed_drivers(["GeoJSON"])
            .open_option("DATE_AS_STRING", "YES");
        let report = Translate::new(source, &output)
            .dataset_option("VERSION", "1.2")
            .layer_option("GEOMETRY_NAME", "shape")
            .layer_option("SPATIAL_INDEX", "NO")
            .layer_option("SPATIAL_CORE_UNKNOWN_OPTION", "value")
            .run()?;
        let ds = Dataset::open(&output)?;
        assert_eq!(
            ds.layer(0)?.defn().geom_fields().next().unwrap().name(),
            "shape"
        );
        assert_eq!(
            ds.layer(0)?
                .defn()
                .fields()
                .find(|f| f.name() == "time")
                .unwrap()
                .field_type(),
            gdal_sys::OGRFieldType::OFTString
        );
        assert!(
            report
                .diagnostics()
                .iter()
                .any(|d| d.message.contains("SPATIAL_CORE_UNKNOWN_OPTION"))
        );
        assert_eq!(ds.driver().short_name(), "GPKG");
        ds.close()?;
        let _ = Translate::new(&input, &output)
            .layer_name("other")
            .write_mode(WriteMode::CreateLayer)
            .output_open_option("LIST_ALL_TABLES", "YES")
            .run()?;
        Ok(())
    }

    #[test]
    fn empty_layers_scalar_tables_and_measured_geometry_are_supported() -> anyhow::Result<()> {
        use gdal::{
            DriverManager,
            spatial_ref::SpatialRef,
            vector::{Feature, Geometry, LayerOptions},
        };
        let dir = tempfile::tempdir()?;
        let input = dir.path().join("schema.gpkg");
        let mut dataset = DriverManager::get_driver_by_name("GPKG")?.create_vector_only(&input)?;
        let srs = SpatialRef::from_epsg(4326)?;
        let layer = dataset.create_layer(LayerOptions {
            name: "empty",
            srs: Some(&srs),
            ty: gdal_sys::OGRwkbGeometryType::wkbPoint,
            ..Default::default()
        })?;
        layer.create_defn_fields(&[("value", gdal_sys::OGRFieldType::OFTInteger)])?;
        let layer = dataset.create_layer(LayerOptions {
            name: "scalar",
            ty: gdal_sys::OGRwkbGeometryType::wkbNone,
            ..Default::default()
        })?;
        layer.create_defn_fields(&[("value", gdal_sys::OGRFieldType::OFTInteger)])?;
        let mut feature = Feature::new(layer.defn())?;
        feature.set_field_integer(0, 7)?;
        feature.create(&layer)?;
        drop(feature);
        let mut layer = dataset.create_layer(LayerOptions {
            name: "measured",
            srs: Some(&srs),
            ty: gdal_sys::OGRwkbGeometryType::wkbPointZM,
            ..Default::default()
        })?;
        layer.create_feature(Geometry::from_wkt("POINT ZM (1 2 3 4)")?)?;
        layer.create_feature(Geometry::from_wkt("POINT ZM EMPTY")?)?;
        dataset.close()?;
        let output = dir.path().join("copy.gpkg");
        let _ = Translate::new(&input, &output).run()?;
        assert_eq!(count(&output, "empty"), 0);
        let copy = Dataset::open(&output)?;
        assert_eq!(copy.layer_by_name("empty")?.defn().fields().count(), 1);
        assert_eq!(
            copy.layer_by_name("scalar")?.defn().geom_fields().count(),
            0
        );
        let mut measured = copy.layer_by_name("measured")?;
        assert_eq!(
            measured.defn().geom_fields().next().unwrap().field_type(),
            gdal_sys::OGRwkbGeometryType::wkbPointZM
        );
        let mut rows = measured.features();
        assert_eq!(
            rows.next().unwrap().geometry().unwrap().get_point_zm(0),
            (1.0, 2.0, 3.0, 4.0)
        );
        assert!(rows.next().unwrap().geometry().unwrap().is_empty());
        drop(rows);
        copy.close()?;
        let _ = Translate::new(&input, &output)
            .layers(["scalar"])
            .layer_name("scalar")
            .write_mode(WriteMode::AppendFeatures)
            .run()?;
        assert_eq!(count(&output, "scalar"), 2);
        Ok(())
    }

    #[test]
    fn native_skipping_and_geometry_repair_are_explicit_opt_ins() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let input = dir.path().join("mixed.geojson");
        std::fs::write(
            &input,
            r#"{"type":"FeatureCollection","features":[{"type":"Feature","properties":{},"geometry":{"type":"Point","coordinates":[0,0]}},{"type":"Feature","properties":{},"geometry":{"type":"Polygon","coordinates":[[[0,0],[2,2],[0,2],[2,0],[0,0]]]}}]}"#,
        )?;
        assert!(
            Translate::new(&input, dir.path().join("abort.shp"))
                .run()
                .is_err()
        );
        let skipped = dir.path().join("skipped.shp");
        let report = Translate::new(&input, &skipped)
            .error_policy(ErrorPolicy::SkipFailures)
            .run()?;
        assert_eq!(Dataset::open(&skipped)?.layer(0)?.feature_count(), 1);
        assert!(
            report
                .diagnostics()
                .iter()
                .any(|d| d.severity == crate::operation::Severity::Failure)
        );
        for repair in [false, true] {
            let output = dir.path().join(format!("repair-{repair}.gpkg"));
            let _ = Translate::new(&input, &output).make_valid(repair).run()?;
            let ds = Dataset::open(&output)?;
            let mut layer = ds.layer(0)?;
            let polygon = layer.features().nth(1).unwrap();
            assert_eq!(polygon.geometry().unwrap().is_valid(), repair);
        }
        Ok(())
    }

    #[test]
    fn nested_operations_and_parallel_configuration_restore_the_calling_thread()
    -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let input = points(dir.path());
        let key = "SPATIAL_CORE_VECTOR_CONFIG_TEST";
        std::thread::scope(|scope| -> anyhow::Result<()> {
            let handles: Vec<_> = (0..2)
                .map(|i| {
                    let input = &input;
                    let output = dir.path().join(format!("parallel-{i}.gpkg"));
                    let nested = dir.path().join(format!("nested-{i}.gpkg"));
                    scope.spawn(move || -> anyhow::Result<()> {
                        gdal::config::set_thread_local_config_option(key, "prior")?;
                        let expected = format!("worker-{i}");
                        let options = GdalOptions::default().set(key, &expected)?;
                        let mut nested_run = false;
                        let mut callback = |_: &Progress| {
                            assert_eq!(
                                gdal::config::get_thread_local_config_option(key, "").unwrap(),
                                expected
                            );
                            if !nested_run {
                                let _ = Translate::new(input, &nested)
                                    .gdal_options(
                                        GdalOptions::default().set(key, "nested").unwrap(),
                                    )
                                    .run()
                                    .unwrap();
                                nested_run = true;
                            }
                            ControlFlow::Continue(())
                        };
                        let _ = Translate::new(input, &output)
                            .gdal_options(options.clone())
                            .run_with(&mut Control::default().on_progress(&mut callback))?;
                        assert!(nested_run);
                        assert_eq!(
                            gdal::config::get_thread_local_config_option(key, "")?,
                            "prior"
                        );
                        assert!(
                            Translate::new(input, &output)
                                .gdal_options(options)
                                .run()
                                .is_err()
                        );
                        assert_eq!(
                            gdal::config::get_thread_local_config_option(key, "")?,
                            "prior"
                        );
                        gdal::config::clear_thread_local_config_option(key)?;
                        Ok(())
                    })
                })
                .collect();
            for handle in handles {
                handle.join().unwrap()?;
            }
            Ok(())
        })?;
        Ok(())
    }

    #[test]
    #[ignore = "requires optional GDAL Parquet driver; run explicitly when installed"]
    fn optional_geoparquet_roundtrip() -> anyhow::Result<()> {
        gdal::DriverManager::get_driver_by_name("Parquet")?;
        let dir = tempfile::tempdir()?;
        let input = points(dir.path());
        let output = dir.path().join("points.parquet");
        let _ = Translate::new(&input, &output).driver("Parquet").run()?;
        assert_points(&output)
    }

    #[test]
    fn pg_dump_exports_sql_without_executing_it() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let input = points(dir.path());
        let output = dir.path().join("points.sql");
        let _ = Translate::new(&input, &output)
            .driver("PGDump")
            .layer_name("points")
            .layer_option("SCHEMA", "export_schema")
            .layer_option("GEOMETRY_NAME", "shape")
            .run()?;
        let sql = std::fs::read_to_string(output)?;
        assert!(
            sql.contains("export_schema") && sql.contains("shape") && sql.contains("CREATE TABLE")
        );
        Ok(())
    }

    #[test]
    fn shapefile_aliases_and_layer_name_laundering_never_replace_existing_data()
    -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let input = points(dir.path());
        let shapes = dir.path().join("shapes");
        std::fs::create_dir(&shapes)?;
        let source = shapes.join("points.shp");
        let _ = Translate::new(&input, &source).run()?;
        for alias in [&shapes, &source.with_extension("dbf")] {
            let error = Translate::new(&source, alias)
                .layer_name("points")
                .write_mode(WriteMode::ReplaceLayer)
                .where_clause("value=2")
                .run()
                .unwrap_err();
            assert_eq!(error.kind(), ErrorKind::InvalidArgument);
            let error = NativeTranslate::new(
                &source,
                alias,
                ["-overwrite", "-nln", "points", "-where", "value=2"],
            )
            .run()
            .unwrap_err();
            assert_eq!(error.kind(), ErrorKind::InvalidArgument);
            assert_eq!(count(&source, "points"), 3);
        }
        let other = shapes.join("a_b.shp");
        let _ = Translate::new(&input, &other)
            .where_clause("value=2")
            .run()?;
        for (destination, name) in [(&source, "a_b"), (&shapes, "a/b"), (&shapes, "a_b")] {
            assert!(
                Translate::new(&input, destination)
                    .layer_name(name)
                    .write_mode(WriteMode::CreateLayer)
                    .run()
                    .is_err()
            );
            assert_eq!(count(&other, "a_b"), 1);
        }
        let _ = Translate::new(&input, &shapes)
            .layer_name("new_layer")
            .write_mode(WriteMode::CreateLayer)
            .run()?;
        assert_eq!(count(&shapes.join("new_layer.shp"), "new_layer"), 3);
        Ok(())
    }

    #[test]
    fn alternate_option_key_separators_cannot_enable_destructive_driver_options()
    -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let input = points(dir.path());
        let output = dir.path().join("target.gpkg");
        let _ = Translate::new(&input, &output).layer_name("a_b").run()?;
        for key in ["OVERWRITE:", "LAUNDER:", " OVERWRITE", "OVERWRITE\t"] {
            let error = Translate::new(&input, &output)
                .layer_name("a/b")
                .write_mode(WriteMode::CreateLayer)
                .layer_option(key, "YES")
                .where_clause("value=2")
                .run()
                .unwrap_err();
            assert_eq!(error.kind(), ErrorKind::InvalidArgument);
            assert_eq!(error.output_state(), OutputState::NotModified);
            assert_eq!(count(&output, "a_b"), 3);
        }
        assert!(
            Translate::new(&input, &output)
                .layer_name("other")
                .write_mode(WriteMode::CreateLayer)
                .output_open_option("PRELUDE_STATEMENTS:", "DELETE FROM a_b")
                .run()
                .is_err()
        );
        assert_eq!(count(&output, "a_b"), 3);
        Ok(())
    }

    #[test]
    fn append_requires_matching_geometry_names_order_and_crs() -> anyhow::Result<()> {
        use gdal::{
            DriverManager,
            spatial_ref::SpatialRef,
            vector::{Feature, Geometry, LayerOptions},
        };
        fn fixture(
            path: &Path,
            names: [&str; 2],
            codes: [u32; 2],
            rows: bool,
        ) -> anyhow::Result<()> {
            let mut ds = DriverManager::get_driver_by_name("SQLite")?.create_vector_only(path)?;
            let layer = ds.create_layer(LayerOptions {
                name: "points",
                ty: gdal_sys::OGRwkbGeometryType::wkbNone,
                ..Default::default()
            })?;
            let fields = if rows { ["a", "b"] } else { ["b", "a"] };
            for field in fields {
                layer.create_defn_fields(&[(field, gdal_sys::OGRFieldType::OFTInteger)])?;
            }
            for (name, code) in names.into_iter().zip(codes) {
                let name = std::ffi::CString::new(name)?;
                let srs = SpatialRef::from_epsg(code)?;
                // SAFETY: the new field borrows live strings/SRS; the layer copies the definition.
                unsafe {
                    let field = gdal_sys::OGR_GFld_Create(
                        name.as_ptr(),
                        gdal_sys::OGRwkbGeometryType::wkbPoint,
                    );
                    gdal_sys::OGR_GFld_SetSpatialRef(field, srs.to_c_hsrs());
                    let result = gdal_sys::OGR_L_CreateGeomField(layer.c_layer(), field, 0);
                    gdal_sys::OGR_GFld_Destroy(field);
                    assert_eq!(result, 0);
                }
            }
            if rows {
                let mut feature = Feature::new(layer.defn())?;
                feature.set_field_integer(0, 7)?;
                feature.set_field_integer(1, 8)?;
                for (i, text) in ["POINT(120 30)", "POINT(13358338 3503549)"]
                    .into_iter()
                    .enumerate()
                {
                    let geometry = Geometry::from_wkt(text)?;
                    // SAFETY: the feature copies a live geometry into a valid geometry field.
                    assert_eq!(
                        // SAFETY: the feature copies the live geometry into an existing field.
                        unsafe {
                            gdal_sys::OGR_F_SetGeomField(
                                feature.c_feature(),
                                i as i32,
                                geometry.c_geometry(),
                            )
                        },
                        0
                    );
                }
                feature.create(&layer)?;
            }
            ds.close()?;
            Ok(())
        }
        let dir = tempfile::tempdir()?;
        let source = dir.path().join("source.sqlite");
        fixture(&source, ["g1", "g2"], [4326, 3857], true)?;
        for (i, names, codes) in [
            (0, ["g2", "g1"], [4326, 3857]),
            (1, ["h1", "h2"], [4326, 3857]),
            (2, ["g2", "g1"], [3857, 4326]),
            (3, ["g1", "g2"], [4326, 3857]),
        ] {
            let target = dir.path().join(format!("target-{i}.sqlite"));
            fixture(&target, names, codes, false)?;
            let result = Translate::new(&source, &target)
                .layer_name("points")
                .write_mode(WriteMode::AppendFeatures)
                .run();
            if i < 3 {
                assert_eq!(result.unwrap_err().kind(), ErrorKind::InvalidArgument);
                assert_eq!(count(&target, "points"), 0);
            } else {
                let _ = result?;
                let ds = Dataset::open(&target)?;
                let mut layer = ds.layer(0)?;
                let feature = layer.features().next().unwrap();
                assert_eq!(
                    feature.geometry_by_index(0)?.get_point(0),
                    (120.0, 30.0, 0.0)
                );
                assert_eq!(
                    feature.geometry_by_index(1)?.get_point(0),
                    (13358338.0, 3503549.0, 0.0)
                );
            }
        }
        Ok(())
    }

    #[test]
    fn configuration_diagnostics_are_captured_and_prior_cpl_state_is_restored() -> anyhow::Result<()>
    {
        let dir = tempfile::tempdir()?;
        let input = points(dir.path());
        let outer = crate::gdal_ext::diagnostics::UserCallbackScope::enter();
        let debug = GdalOptions::default().set("CPL_DEBUG", "ON")?;
        let debug_scope = debug.enter();
        // SAFETY: set only the current thread's CPL state with a static message.
        unsafe {
            gdal_sys::CPLErrorSetState(gdal_sys::CPLErr::CE_Warning, 42, c"prior-state".as_ptr())
        };
        let options = GdalOptions::default().set("SPATIAL_CORE_UNKNOWN_CONFIG", "value")?;
        let report = Translate::new(&input, dir.path().join("output.gpkg"))
            .gdal_options(options)
            .run()?;
        if supported_version()? >= 3_110_000 {
            assert!(
                report
                    .diagnostics()
                    .iter()
                    .any(|d| d.message.contains("SPATIAL_CORE_UNKNOWN_CONFIG"))
            );
        }
        // SAFETY: read only the current thread's CPL state while the outer guard is alive.
        unsafe {
            assert_eq!(gdal_sys::CPLGetLastErrorNo(), 42);
            assert_eq!(
                std::ffi::CStr::from_ptr(gdal_sys::CPLGetLastErrorMsg()),
                c"prior-state"
            );
        }
        drop(debug_scope);
        drop(outer);
        Ok(())
    }
    #[test]
    fn geometry_processing_clips_simplifies_segmentizes_and_explodes() -> anyhow::Result<()> {
        use crate::vector::{Clip, GeometryType};
        let dir = tempfile::tempdir()?;
        let src = dir.path().join("lines.geojson");
        std::fs::write(
            &src,
            r#"{"type":"FeatureCollection","features":[{"type":"Feature","properties":{"label":"road"},"geometry":{"type":"LineString","coordinates":[[0,0],[1,0.1],[2,0]]}}]}"#,
        )?;
        let clipped = dir.path().join("clipped.gpkg");
        let _ = Translate::new(&src, &clipped)
            .clip_source(Clip::Bounds([0.5, -0.5, 1.5, 0.5]))
            .run()?;
        let ds = Dataset::open(clipped)?;
        let mut layer = ds.layer(0)?;
        let f = layer.features().next().unwrap();
        let envelope = f.geometry().unwrap().envelope();
        assert_eq!((envelope.MinX, envelope.MaxX), (0.5, 1.5));
        drop(f);
        ds.close()?;
        let simple = dir.path().join("simple.gpkg");
        let _ = Translate::new(&src, &simple).simplify(0.2).run()?;
        let ds = Dataset::open(simple)?;
        let mut layer = ds.layer(0)?;
        let f = layer.features().next().unwrap();
        assert_eq!(f.geometry().unwrap().point_count(), 2);
        drop(f);
        ds.close()?;
        let dense = dir.path().join("dense.gpkg");
        let _ = Translate::new(&src, &dense).segmentize(0.5).run()?;
        let ds = Dataset::open(dense)?;
        let mut layer = ds.layer(0)?;
        let f = layer.features().next().unwrap();
        assert!(f.geometry().unwrap().point_count() > 3);
        drop(f);
        ds.close()?;
        let collection = dir.path().join("multi.geojson");
        std::fs::write(
            &collection,
            r#"{"type":"FeatureCollection","features":[{"type":"Feature","properties":{"name":"pair"},"geometry":{"type":"MultiPoint","coordinates":[[0,0],[1,1]]}}]}"#,
        )?;
        let single = dir.path().join("single.gpkg");
        let _ = Translate::new(&collection, &single)
            .explode_collections(true)
            .geometry_type(GeometryType::Point)
            .dimensions(Dimensions::Xy)
            .run()?;
        assert_eq!(Dataset::open(single)?.layer(0)?.feature_count(), 2);
        assert!(
            Translate::new(&src, dir.path().join("invalid.gpkg"))
                .simplify(1.)
                .segmentize(1.)
                .run()
                .is_err()
        );
        assert!(
            Translate::new(&src, dir.path().join("not-wkt.gpkg"))
                .clip_source(Clip::Wkt(src.to_string_lossy().into_owned()))
                .run()
                .is_err()
        );
        Ok(())
    }
    #[test]
    fn sql_spatial_processing_and_destination_clip_are_read_back() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let src = dir.path().join("points.geojson");
        std::fs::write(
            &src,
            r#"{"type":"FeatureCollection","name":"points","features":[{"type":"Feature","properties":{"name":"p"},"geometry":{"type":"Point","coordinates":[1,1]}}]}"#,
        )?;
        let buffered = dir.path().join("buffer.gpkg");
        let _ = Translate::new(&src, &buffered)
            .sql(
                "SELECT ST_Buffer(geometry, 0.1) AS geometry, name FROM points",
                SqlDialect::Sqlite,
            )
            .run()?;
        let ds = Dataset::open(buffered)?;
        let mut layer = ds.layer(0)?;
        let f = layer.features().next().unwrap();
        let area = f.geometry().unwrap().area();
        assert!(area > 0.03 && area < 0.032);
        drop(f);
        ds.close()?;
        let clipped = dir.path().join("clip.gpkg");
        let _ = Translate::new(&src, &clipped)
            .reproject_to("EPSG:3857")
            .clip_destination(crate::vector::Clip::Bounds([
                110000., 110000., 112000., 112000.,
            ]))
            .run()?;
        assert_eq!(Dataset::open(&clipped)?.layer(0)?.feature_count(), 1);
        Ok(())
    }

    #[test]
    fn concrete_geometry_types_require_and_preserve_explicit_dimensions() -> anyhow::Result<()> {
        use crate::vector::GeometryType;
        use gdal::{
            DriverManager,
            vector::{Geometry, LayerOptions},
        };
        let dir = tempfile::tempdir()?;
        for (name, dimensions, wkt, expected) in [
            ("z", Dimensions::Xyz, "POINT Z (1 2 3)", (1., 2., 3., 0.)),
            ("m", Dimensions::Xym, "POINT M (1 2 4)", (1., 2., 0., 4.)),
            (
                "zm",
                Dimensions::Xyzm,
                "POINT ZM (1 2 3 4)",
                (1., 2., 3., 4.),
            ),
        ] {
            let src = dir.path().join(format!("{name}.gpkg"));
            let mut ds = DriverManager::get_driver_by_name("GPKG")?.create_vector_only(&src)?;
            let geometry = Geometry::from_wkt(wkt)?;
            ds.create_layer(LayerOptions {
                name: "points",
                ty: geometry.geometry_type(),
                ..Default::default()
            })?
            .create_feature(geometry)?;
            ds.close()?;
            let missing = dir.path().join(format!("{name}-missing.gpkg"));
            let error = Translate::new(&src, &missing)
                .geometry_type(GeometryType::MultiPoint)
                .run()
                .unwrap_err();
            assert_eq!(error.output_state(), OutputState::NotModified);
            assert!(!missing.exists());
            for (label, job, expected) in [
                (
                    "concrete",
                    Translate::new(&src, dir.path().join(format!("{name}-concrete.gpkg")))
                        .geometry_type(GeometryType::MultiPoint)
                        .dimensions(dimensions),
                    expected,
                ),
                (
                    "semantic",
                    Translate::new(&src, dir.path().join(format!("{name}-semantic.gpkg")))
                        .geometry_type(GeometryType::PromoteToMulti),
                    expected,
                ),
                (
                    "xy",
                    Translate::new(&src, dir.path().join(format!("{name}-xy.gpkg")))
                        .geometry_type(GeometryType::MultiPoint)
                        .dimensions(Dimensions::Xy),
                    (1., 2., 0., 0.),
                ),
            ] {
                let _ = job.run()?;
                let ds = Dataset::open(dir.path().join(format!("{name}-{label}.gpkg")))?;
                let mut layer = ds.layer(0)?;
                let feature = layer.features().next().unwrap();
                assert_eq!(
                    feature.geometry().unwrap().get_geometry(0).get_point_zm(0),
                    expected
                );
                drop(feature);
                ds.close()?;
            }
        }
        Ok(())
    }
}
