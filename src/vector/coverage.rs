use super::{
    CheckScope, GeometryIssue, GeometryIssueKind, LayerSource,
    check_geometry::{issue_fields, write_issue},
    execute::{self, Output},
};
use crate::{
    config::gdal::GdalOptions,
    gdal_ext::{
        algorithm::{Algorithm, Argument},
        execution::close,
        ffi::checked_gdal_call,
        query::{self, inspect_error},
    },
    operation::{Control, Error, Outcome, OutputState, Result, Stage},
};
use gdal::{
    DriverManager,
    vector::{
        Feature, FieldDefn, Geometry, LayerAccess, LayerOptions, OGRFieldType, OGRwkbGeometryType,
    },
};

/// Input-size guards; these are not a hard limit on GEOS process memory.
#[derive(Debug, Clone, Copy, Default)]
pub struct CoverageLimits {
    /// Maximum selected feature count.
    pub max_features: Option<u64>,
    /// Maximum selected vertex count, including ring closing vertices.
    pub max_vertices: Option<u64>,
}
/// Complete selected coverage check; causes are not guessed from invalid edges.
#[derive(Debug)]
pub struct CoverageCheck {
    /// Selected layer and filters.
    pub scope: CheckScope,
    /// Number of input features checked.
    pub checked: u64,
    /// Features with NULL, EMPTY, or invalid geometries.
    pub invalid_geometries: u64,
    /// Native invalid-edge features, not individual segment count.
    pub invalid_edges: u64,
    /// Bounded source identities for geometry and coverage issues.
    pub issues: Vec<GeometryIssue>,
    /// Additional issues were found beyond the sample limit.
    pub truncated: bool,
}
impl CoverageCheck {
    /// True for a completed check without invalid input geometries or edges.
    pub fn is_valid(&self) -> bool {
        self.invalid_geometries == 0 && self.invalid_edges == 0
    }
}
/// Native policy for assigning overlapping areas during coverage cleaning.
#[derive(Debug, Clone, Copy, Default)]
pub enum MergeStrategy {
    /// Assign to the neighbor sharing the longest border.
    #[default]
    LongestBorder,
    /// Assign to the largest polygon.
    MaxArea,
    /// Assign to the smallest polygon.
    MinArea,
    /// Assign by native input order.
    MinIndex,
}
impl MergeStrategy {
    fn name(self) -> &'static str {
        match self {
            Self::LongestBorder => "longest-border",
            Self::MaxArea => "max-area",
            Self::MinArea => "min-area",
            Self::MinIndex => "min-index",
        }
    }
}
#[derive(Debug, Clone)]
pub(super) struct Settings {
    pub source: LayerSource,
    pub config: GdalOptions,
    pub limits: CoverageLimits,
    pub geographic: bool,
}
impl Settings {
    pub fn new(source: impl Into<LayerSource>) -> Self {
        Self {
            source: source.into(),
            config: GdalOptions::default(),
            limits: CoverageLimits::default(),
            geographic: false,
        }
    }
}
pub(super) enum Action {
    Check {
        gap: f64,
        limit: usize,
        issues: Option<Output>,
    },
    Clean {
        snap: f64,
        gap: f64,
        merge: MergeStrategy,
    },
    Simplify {
        tolerance: f64,
        preserve: bool,
    },
}
pub(super) fn run(
    settings: Settings,
    action: Action,
    output: Option<Output>,
    control: &mut Control<'_>,
) -> Result<Outcome<CoverageCheck>> {
    execute::geos(if matches!(action, Action::Clean { .. }) {
        14
    } else {
        12
    })?;
    let (distance, sample_limit) = match &action {
        Action::Check { gap, limit, .. } => (*gap, *limit),
        Action::Clean { snap, gap, .. } => (snap.max(*gap), 0),
        Action::Simplify { tolerance, .. } => (*tolerance, 0),
    };
    if !distance.is_finite() || distance < 0. || sample_limit > 1_000_000 {
        return Err(Error::invalid(
            "coverage distances must be finite/nonnegative and sample_limit <= 1000000",
        ));
    }
    // Validate each distance separately: max() alone would hide a NaN.
    if let Action::Clean { snap, gap, .. } = &action
        && (!snap.is_finite() || *snap < 0. || !gap.is_finite() || *gap < 0.)
    {
        return Err(Error::invalid(
            "cleaning distances must be finite and nonnegative",
        ));
    }
    let destination = match &action {
        Action::Check { issues, .. } => issues.as_ref(),
        _ => output.as_ref(),
    };
    execute::run(
        std::slice::from_ref(&settings.source),
        settings.config.clone(),
        destination,
        control,
        |inputs, destination, progress| {
            let mut algorithm = Algorithm::new(&[
                "vector",
                match action {
                    Action::Clean { .. } => "clean-coverage",
                    Action::Simplify { .. } => "simplify-coverage",
                    Action::Check { .. } => "check-coverage",
                },
            ])?;
            let mut layer = settings.source.select(&inputs[0])?;
            let srs = layer
                .spatial_ref()
                .ok_or_else(|| Error::invalid("coverage requires an explicit CRS"))?;
            if !srs.is_projected() && !settings.geographic && distance > 0. {
                return Err(Error::invalid(
                    "positive coverage distances require a projected CRS or explicit allow_geographic(true)",
                ));
            }
            let mut summary = CoverageCheck {
                scope: settings.source.scope(&layer),
                checked: 0,
                invalid_geometries: 0,
                invalid_edges: 0,
                issues: Vec::new(),
                truncated: false,
            };
            let configured =
                gdal::config::get_config_option("CPL_TMPDIR", "").map_err(inspect_error)?;
            let mut builder = tempfile::Builder::new();
            builder.prefix(".spatial-core-coverage-");
            let directory = if configured.is_empty() {
                builder.tempdir()
            } else {
                builder.tempdir_in(configured)
            }
            .map_err(|e| {
                Error::invalid("cannot create coverage scratch directory").with_source(e)
            })?;
            let mut selected = None;
            let mut invalid_edges = None;
            let result = (|| {
                let mut issues =
                    if matches!(action, Action::Check { .. }) && destination.spec.is_some() {
                        let ds = destination.create()?;
                        let layer = ds
                            .create_layer(LayerOptions {
                                name: "coverage_issues",
                                srs: Some(&srs),
                                ty: OGRwkbGeometryType::wkbUnknown,
                                ..Default::default()
                            })
                            .map_err(inspect_error)?;
                        issue_fields(&layer)?;
                        Some(layer)
                    } else {
                        None
                    };
                let path = directory.path().join("selected.gpkg");
                selected = Some(
                    DriverManager::get_driver_by_name("GPKG")
                        .and_then(|d| d.create_vector_only(&path))
                        .map_err(inspect_error)?,
                );
                let selected_ds = selected.as_mut().expect("snapshot");
                let mut tx = selected_ds.start_transaction().map_err(inspect_error)?;
                let fields = execute::field_schema(&layer);
                if layer.defn().geom_fields().count() != 1 {
                    return Err(Error::unsupported(
                        "coverage requires exactly one geometry field",
                    ));
                }
                let declared = layer.defn().geometry_type();
                if !matches!(
                    declared,
                    OGRwkbGeometryType::wkbPolygon
                        | OGRwkbGeometryType::wkbMultiPolygon
                        | OGRwkbGeometryType::wkbUnknown
                ) {
                    return Err(Error::unsupported(
                        "coverage requires a linear XY polygon layer schema",
                    ));
                }
                let unique = |base: &str| {
                    let mut name = base.to_owned();
                    while fields.iter().any(|f| f.0.eq_ignore_ascii_case(&name)) {
                        name.push('_');
                    }
                    name
                };
                let fid_name = unique("__source_fid");
                let ordinal_name = unique("__source_ordinal");
                let storage_fid = unique("__rowid");
                let storage_geom = unique("__geometry");
                let layer_options = [
                    format!("FID={storage_fid}"),
                    format!("GEOMETRY_NAME={storage_geom}"),
                ];
                let options = layer_options.iter().map(String::as_str).collect::<Vec<_>>();
                let staged = tx
                    .create_layer(LayerOptions {
                        name: "selected",
                        srs: Some(&srs),
                        ty: declared,
                        options: Some(&options),
                    })
                    .map_err(inspect_error)?;
                for index in 0..fields.len() {
                    // SAFETY: GDAL copies the live source field definition into the destination schema.
                    let status = checked_gdal_call(|| unsafe {
                        gdal_sys::OGR_L_CreateField(
                            staged.c_layer(),
                            gdal_sys::OGR_FD_GetFieldDefn(layer.defn().c_defn(), index as i32),
                            0,
                        )
                    })
                    .map_err(inspect_error)?;
                    if status != gdal_sys::OGRErr::OGRERR_NONE {
                        return Err(Error::unsupported(
                            "coverage scratch driver cannot preserve the source schema",
                        ));
                    }
                }
                let actual = execute::field_schema(&staged);
                if actual != fields {
                    return Err(Error::unsupported(
                        "coverage scratch driver changed attribute names/types/subtypes",
                    ));
                }
                let identity = matches!(action, Action::Check { .. } | Action::Simplify { .. });
                if identity {
                    FieldDefn::new(&fid_name, OGRFieldType::OFTString)
                        .and_then(|f| f.add_to_layer(&staged))
                        .map_err(inspect_error)?;
                    FieldDefn::new(&ordinal_name, OGRFieldType::OFTInteger64)
                        .and_then(|f| f.add_to_layer(&staged))
                        .map_err(inspect_error)?;
                }
                let mut vertices = 0u64;
                let mut features = layer.features();
                loop {
                    let Some(feature) =
                        checked_gdal_call(|| features.next()).map_err(inspect_error)?
                    else {
                        break;
                    };
                    summary.checked += 1;
                    if settings
                        .limits
                        .max_features
                        .is_some_and(|limit| summary.checked > limit)
                    {
                        return Err(Error::invalid("coverage feature budget exceeded"));
                    }
                    if summary.checked % 256 == 1 {
                        query::pulse(progress, 0., c"checking and snapshotting coverage")?;
                    }
                    let issue = match execute::geometry(&feature) {
                        None => Some(GeometryIssueKind::Null),
                        Some(g) if g.is_empty() => Some(GeometryIssueKind::Empty),
                        Some(g) => {
                            execute::linear_xy(g)?;
                            // SAFETY: flattening inspects only the numeric geometry type.
                            if !matches!(
                                // SAFETY: flattening inspects only the geometry type code.
                                unsafe { gdal_sys::OGR_GT_Flatten(g.geometry_type()) },
                                OGRwkbGeometryType::wkbPolygon
                                    | OGRwkbGeometryType::wkbMultiPolygon
                            ) {
                                return Err(Error::unsupported(
                                    "coverage requires polygon or multipolygon geometries",
                                ));
                            }
                            vertices = vertices
                                .checked_add(vertex_count(g)?)
                                .ok_or_else(|| Error::invalid("coverage vertex count overflow"))?;
                            if settings
                                .limits
                                .max_vertices
                                .is_some_and(|limit| vertices > limit)
                            {
                                return Err(Error::invalid("coverage vertex budget exceeded"));
                            }
                            if execute::valid_geometry(g)? {
                                None
                            } else {
                                Some(GeometryIssueKind::Invalid)
                            }
                        }
                    };
                    if let Some(kind) = issue {
                        summary.invalid_geometries += 1;
                        let issue = GeometryIssue {
                            fid: feature.fid(),
                            ordinal: summary.checked,
                            kind,
                        };
                        if let Some(layer) = &mut issues {
                            write_issue(layer, &issue, execute::geometry(&feature))?;
                        }
                        retain(&mut summary, issue, sample_limit);
                    }
                    let mut copied = Feature::new(staged.defn()).map_err(inspect_error)?;
                    // SAFETY: both features are live; SetFrom copies fields/geometry without transferring ownership.
                    let status = checked_gdal_call(|| unsafe {
                        gdal_sys::OGR_F_SetFrom(copied.c_feature(), feature.c_feature(), 0)
                    })
                    .map_err(inspect_error)?;
                    if status != gdal_sys::OGRErr::OGRERR_NONE {
                        return Err(Error::invalid("cannot copy selected coverage feature"));
                    }
                    if identity {
                        if let Some(fid) = feature.fid() {
                            copied
                                .set_field_string(fields.len(), &fid.to_string())
                                .map_err(inspect_error)?;
                        }
                        copied
                            .set_field_integer64(
                                fields.len() + 1,
                                i64::try_from(summary.checked)
                                    .map_err(|_| Error::invalid("coverage ordinal exceeds i64"))?,
                            )
                            .map_err(inspect_error)?;
                    }
                    copied.create(&staged).map_err(inspect_error)?;
                }
                drop(features);
                tx.commit().map_err(inspect_error)?;
                selected_ds.flush_cache().map_err(inspect_error)?;
                if summary.invalid_geometries != 0 {
                    if matches!(action, Action::Check { .. }) {
                        return Ok(());
                    }
                    return Err(Error::invalid(
                        "coverage processing requires valid nonempty input geometries; inspect CheckGeometry first",
                    ));
                }
                if matches!(action, Action::Check { .. } | Action::Simplify { .. }) {
                    let mut check = Algorithm::new(&["vector", "check-coverage"])?;
                    check.bind("input", selected_ds)?;
                    check.set("output", Argument::String("stream"))?;
                    check.set("output-format", Argument::String("stream"))?;
                    check.set(
                        "maximum-gap-width",
                        Argument::Double(match action {
                            Action::Check { gap, .. } => gap,
                            _ => 0.,
                        }),
                    )?;
                    invalid_edges = Some(check.execute_dataset(progress)?);
                    let mut edges = invalid_edges
                        .as_ref()
                        .expect("edges output")
                        .layer(0)
                        .map_err(inspect_error)?;
                    let selected_layer = selected_ds.layer(0).map_err(inspect_error)?;
                    let fid_index = selected_layer
                        .defn()
                        .field_index(&fid_name)
                        .map_err(inspect_error)?;
                    let ordinal_index = selected_layer
                        .defn()
                        .field_index(&ordinal_name)
                        .map_err(inspect_error)?;
                    let mut features = edges.features();
                    loop {
                        let Some(feature) =
                            checked_gdal_call(|| features.next()).map_err(inspect_error)?
                        else {
                            break;
                        };
                        if execute::geometry(&feature).is_none_or(|g| g.is_empty()) {
                            continue;
                        }
                        summary.invalid_edges += 1;
                        let source_fid = feature.fid().ok_or_else(|| {
                            Error::invalid("native coverage output lost source identity")
                        })?;
                        let source = checked_gdal_call(|| selected_layer.feature(source_fid))
                            .map_err(inspect_error)?
                            .ok_or_else(|| {
                                Error::invalid("coverage source identity cannot be resolved")
                            })?;
                        let fid = source
                            .field_as_string(fid_index)
                            .map_err(inspect_error)?
                            .and_then(|s| s.parse().ok());
                        let ordinal = source
                            .field_as_integer64(ordinal_index)
                            .map_err(inspect_error)?
                            .ok_or_else(|| Error::invalid("coverage result lost source ordinal"))?;
                        let issue = GeometryIssue {
                            fid,
                            ordinal: u64::try_from(ordinal)
                                .map_err(|_| Error::invalid("invalid coverage source ordinal"))?,
                            kind: GeometryIssueKind::InvalidEdge,
                        };
                        if let Some(layer) = &mut issues {
                            write_issue(layer, &issue, execute::geometry(&feature))?;
                        }
                        retain(&mut summary, issue, sample_limit);
                        if summary.invalid_edges % 256 == 1 {
                            query::pulse(progress, 0., c"summarizing coverage issues")?;
                        }
                    }
                    drop(features);
                    let mut failure = None;
                    close(invalid_edges.take(), Stage::CloseSource, &mut failure);
                    if let Some(error) = failure {
                        return Err(error);
                    }
                    if matches!(action, Action::Simplify { .. }) && !summary.is_valid() {
                        return Err(Error::invalid(
                            "simplification requires a valid coverage; inspect or clean it first",
                        ));
                    }
                }
                if !matches!(action, Action::Check { .. }) {
                    // Remove temporary source-identity fields before exposing a transformed dataset.
                    if identity {
                        let layer = selected_ds.layer(0).map_err(inspect_error)?;
                        for index in [fields.len() + 1, fields.len()] {
                            // SAFETY: this is our owned scratch layer; deleting in reverse preserves indices.
                            let result = checked_gdal_call(|| unsafe {
                                gdal_sys::OGR_L_DeleteField(layer.c_layer(), index as i32)
                            })
                            .map_err(inspect_error)?;
                            if result != gdal_sys::OGRErr::OGRERR_NONE {
                                return Err(Error::invalid(
                                    "cannot remove scratch identity fields",
                                ));
                            }
                        }
                    }
                    algorithm.bind("input", selected_ds)?;
                    let spec = output.as_ref().expect("processing destination");
                    spec.set_algorithm(&mut algorithm)?;
                    match action {
                        Action::Clean { snap, gap, merge } => {
                            algorithm.set("snapping-distance", Argument::Double(snap))?;
                            algorithm.set("maximum-gap-width", Argument::Double(gap))?;
                            algorithm.set("merge-strategy", Argument::String(merge.name()))?;
                        }
                        Action::Simplify {
                            tolerance,
                            preserve,
                        } => {
                            algorithm.set("tolerance", Argument::Double(tolerance))?;
                            algorithm.set("preserve-boundary", Argument::Boolean(preserve))?;
                        }
                        Action::Check { .. } => unreachable!(),
                    }
                    destination.state = OutputState::MayHaveChanged;
                    destination.dataset = Some(algorithm.execute_dataset(progress)?);
                    let actual = execute::field_schema(
                        &destination
                            .dataset
                            .as_ref()
                            .expect("coverage output")
                            .layer(0)
                            .map_err(inspect_error)?,
                    );
                    if actual != fields {
                        return Err(Error::unsupported(
                            "output driver changed attribute names/types/subtypes",
                        ));
                    }
                }
                Ok(())
            })();
            let result =
                result.and_then(|()| query::pulse(progress, 1., c"coverage operation complete"));
            let mut failure = result.err();
            close(invalid_edges, Stage::CloseSource, &mut failure);
            close(selected, Stage::CloseSource, &mut failure);
            drop(directory);
            failure.map_or(Ok(summary), Err)
        },
    )
}
fn vertex_count(geometry: &Geometry) -> Result<u64> {
    let mut count = geometry.point_count() as u64;
    for index in 0..geometry.geometry_count() {
        count = count
            .checked_add(vertex_count(&geometry.get_geometry(index))?)
            .ok_or_else(|| Error::invalid("vertex count overflow"))?;
    }
    Ok(count)
}
fn retain(summary: &mut CoverageCheck, issue: GeometryIssue, limit: usize) {
    if summary.issues.len() < limit {
        summary.issues.push(issue);
    } else {
        summary.truncated = true;
    }
}
macro_rules! settings_methods {
    () => {
        /// Limit selected features/vertices before the native all-layer operation.
        pub fn limits(mut self, value: CoverageLimits) -> Self {
            self.settings.limits = value;
            self
        }
        /// Permit positive distances in nonprojected source units; default false.
        pub fn allow_geographic(mut self, value: bool) -> Self {
            self.settings.geographic = value;
            self
        }
        /// Scope native configuration through resource close.
        pub fn gdal_options(mut self, value: GdalOptions) -> Self {
            self.settings.config = value;
            self
        }
    };
}
pub(super) use settings_methods;
macro_rules! output_methods {
    () => {
        /// Select output driver; default GPKG.
        pub fn driver(mut self, value: impl Into<String>) -> Self {
            self.output.driver = value.into();
            self
        }
        /// Set the new layer name.
        pub fn layer_name(mut self, value: impl Into<String>) -> Self {
            self.output.layer = value.into();
            self
        }
        /// Set a dataset creation option.
        pub fn creation_option(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
            self.output.creation.set(key.into(), value.into());
            self
        }
        /// Set a layer creation option.
        pub fn layer_option(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
            self.output.layer_options.set(key.into(), value.into());
            self
        }
    };
}
pub(super) use output_methods;
#[cfg(all(test, gdal_3_12))]
mod tests {
    use super::super::{CheckCoverage, CleanCoverage, SimplifyCoverage};
    use super::*;
    #[test]
    fn coverage_cancellation_at_snapshot_and_completion_never_returns_success() -> anyhow::Result<()>
    {
        use crate::{
            control::Cancellation,
            operation::{ErrorKind, OutputState, Progress},
        };
        use std::ops::ControlFlow;
        let dir = tempfile::tempdir()?;
        for (index, (count, invalid)) in [(3, true), (256, true), (257, true), (3, false)]
            .into_iter()
            .enumerate()
        {
            let source = dir.path().join(format!("source{index}.gpkg"));
            let wkt = if invalid {
                "POLYGON ((0 0,2 2,0 2,2 0,0 0))"
            } else {
                "POLYGON ((0 0,2 0,2 2,0 2,0 0))"
            };
            super::super::analysis_test_support::polygons(&source, &vec![wkt; count])?;
            for (case, (message, stop, write)) in [
                ("checking and snapshotting coverage", false, false),
                ("checking and snapshotting coverage", true, true),
                ("coverage operation complete", false, true),
            ]
            .into_iter()
            .enumerate()
            {
                let target = dir.path().join(format!("issues{index}_{case}.gpkg"));
                let token = Cancellation::default();
                let mut seen = false;
                let mut callback = |p: &Progress| {
                    if p.message == message {
                        seen = true;
                        if stop {
                            return ControlFlow::Break(());
                        }
                        token.cancel();
                    }
                    ControlFlow::Continue(())
                };
                let mut op = CheckCoverage::new(&source);
                if write {
                    op = op.issues_output(&target);
                }
                let outcome = op.run_with(
                    &mut Control::default()
                        .cancellation(&token)
                        .on_progress(&mut callback),
                );
                assert!(
                    seen,
                    "missing callback: {message}, count={count}, invalid={invalid}"
                );
                let error = outcome.unwrap_err();
                assert_eq!(error.kind(), ErrorKind::Cancelled);
                assert_eq!(error.stage(), Stage::Execute);
                assert_eq!(
                    error.output_state(),
                    if write {
                        OutputState::MayHaveChanged
                    } else {
                        OutputState::NotModified
                    }
                );
                if write {
                    gdal::Dataset::open(&target)?.close()?;
                }
            }
        }
        Ok(())
    }
    #[test]
    fn empty_layers_native_distance_units_and_cancellation_are_explicit() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let empty = dir.path().join("empty.gpkg");
        super::super::analysis_test_support::polygons(&empty, &[])?;
        let result = CheckCoverage::new(&empty).run()?;
        assert!(result.value().is_valid());
        assert_eq!(result.value().checked, 0);
        let simple = dir.path().join("simple.gpkg");
        let _ = SimplifyCoverage::new(&empty, &simple).tolerance(0.).run()?;
        assert_eq!(
            super::super::Info::new(&simple).run()?.value().layers[0].feature_count,
            Some(0)
        );
        let source = dir.path().join("geographic.geojson");
        std::fs::write(
            &source,
            r#"{"type":"FeatureCollection","features":[{"type":"Feature","properties":{"enabled":true},"geometry":{"type":"Polygon","coordinates":[[[0,0],[2,0],[2,2],[0,2],[0,0]]]}}]}"#,
        )?;
        assert!(
            CheckCoverage::new(&source)
                .maximum_gap_width(1.)
                .run()
                .is_err()
        );
        assert!(
            CheckCoverage::new(&source)
                .maximum_gap_width(1.)
                .allow_geographic(true)
                .run()?
                .value()
                .is_valid()
        );
        let out = dir.path().join("out.gpkg");
        let scratch = tempfile::tempdir()?;
        let mut saw_snapshot = false;
        let mut callback = |_: &crate::operation::Progress| {
            saw_snapshot = std::fs::read_dir(scratch.path()).unwrap().count() == 1;
            std::ops::ControlFlow::Break(())
        };
        let error = SimplifyCoverage::new(&source, &out)
            .tolerance(0.)
            .gdal_options(
                GdalOptions::default().set("CPL_TMPDIR", scratch.path().to_str().unwrap())?,
            )
            .run_with(&mut Control::default().on_progress(&mut callback))
            .unwrap_err();
        assert_eq!(error.kind(), crate::operation::ErrorKind::Cancelled);
        assert!(!out.exists());
        assert!(saw_snapshot);
        assert_eq!(std::fs::read_dir(scratch.path())?.count(), 0);
        let _ = SimplifyCoverage::new(&source, &out).tolerance(0.).run()?;
        assert_eq!(
            super::super::Info::new(&out).run()?.value().layers[0].fields[0].field_subtype,
            gdal_sys::OGRFieldSubType::OFSTBoolean
        );
        Ok(())
    }
    #[test]
    fn narrow_gaps_require_an_explicit_threshold() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let source = dir.path().join("gap.gpkg");
        super::super::analysis_test_support::polygons(
            &source,
            &[
                "POLYGON ((0 0,2 0,2 2,0 2,0 0))",
                "POLYGON ((2.05 0,4 0,4 2,2.05 2,2.05 0))",
            ],
        )?;
        assert!(CheckCoverage::new(&source).run()?.value().is_valid());
        let result = CheckCoverage::new(&source).maximum_gap_width(0.1).run()?;
        assert!(!result.value().is_valid());
        assert_eq!(result.value().invalid_edges, 2);
        Ok(())
    }
    #[test]
    fn checks_filters_budgets_cleaning_and_shared_edge_simplification() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let good = dir.path().join("good.gpkg");
        let overlap = dir.path().join("overlap.gpkg");
        super::super::analysis_test_support::polygons(
            &good,
            &[
                "POLYGON ((0 0,2 0,2 2,0 2,0 0))",
                "POLYGON ((2 0,4 0,4 2,2 2,2 0))",
            ],
        )?;
        super::super::analysis_test_support::polygons(
            &overlap,
            &[
                "POLYGON ((0 0,2 0,2 2,0 2,0 0))",
                "POLYGON ((1 0,3 0,3 2,1 2,1 0))",
            ],
        )?;
        let before = std::fs::read(&good)?;
        let check = CheckCoverage::new(&good).run()?;
        assert!(check.value().is_valid());
        assert_eq!(check.value().checked, 2);
        let issues = dir.path().join("issues.gpkg");
        let check = CheckCoverage::new(&overlap)
            .sample_limit(1)
            .issues_output(&issues)
            .run()?;
        assert!(!check.value().is_valid());
        assert!(check.value().invalid_edges > 0);
        assert_eq!(check.value().issues.len(), 1);
        assert_eq!(check.value().issues[0].fid, Some(1));
        assert_eq!(check.value().issues[0].ordinal, 1);
        let subset =
            CheckCoverage::new(LayerSource::new(&overlap).where_clause("id = 10")).run()?;
        assert!(subset.value().is_valid());
        assert_eq!(subset.value().checked, 1);
        assert!(subset.value().scope.attribute_filter);
        assert!(
            CheckCoverage::new(&good)
                .limits(CoverageLimits {
                    max_features: Some(1),
                    max_vertices: None
                })
                .run()
                .is_err()
        );
        let bad = dir.path().join("bad.gpkg");
        assert!(
            SimplifyCoverage::new(&overlap, &bad)
                .tolerance(0.1)
                .run()
                .is_err()
        );
        assert!(!bad.exists());
        let simple = dir.path().join("simple.gpkg");
        let _ = SimplifyCoverage::new(&good, &simple)
            .tolerance(0.2)
            .preserve_boundary(true)
            .run()?;
        assert!(CheckCoverage::new(&simple).run()?.value().is_valid());
        assert_eq!(super::super::analysis_test_support::area(&simple)?, 8.);
        let clean = dir.path().join("clean.gpkg");
        let result = CleanCoverage::new(&overlap, &clean)
            .snapping_distance(0.)
            .run();
        if execute::geos(14).is_ok() {
            let _ = result?;
            assert!(CheckCoverage::new(&clean).run()?.value().is_valid());
        } else {
            assert_eq!(
                result.unwrap_err().kind(),
                crate::operation::ErrorKind::Unsupported
            );
            assert!(!clean.exists());
        }
        assert_eq!(std::fs::read(&good)?, before);
        Ok(())
    }
}
