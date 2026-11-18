use super::{
    LayerSource,
    execute::{self, Output},
};
use crate::{
    config::gdal::GdalOptions,
    gdal_ext::{
        ffi::checked_gdal_call,
        query::{self, inspect_error},
    },
    operation::{Control, Error, Outcome, Result},
};
use gdal::vector::{
    Feature, FieldDefn, Geometry, LayerAccess, LayerOptions, OGRFieldType, OGRwkbGeometryType,
};
use std::path::Path;

/// Whether NULL/EMPTY geometries count as issues.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum GeometryPolicy {
    /// Count as a data-quality issue.
    #[default]
    Reject,
    /// Count separately without invalidating the check.
    Allow,
}
/// A geometrical data-quality issue, distinct from an execution error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GeometryIssueKind {
    /// No geometry.
    Null,
    /// Empty geometry.
    Empty,
    /// Native validity test failed.
    Invalid,
    /// Native simplicity test failed.
    NonSimple,
    /// Coverage algorithm reported an invalid edge without classifying its cause.
    InvalidEdge,
}
/// Bounded issue identity; FIDs need not be consecutive or unique.
#[derive(Debug, Clone)]
pub struct GeometryIssue {
    /// Source FID, when supplied by the driver.
    pub fid: Option<u64>,
    /// One-based position in this selected scan.
    pub ordinal: u64,
    /// Observed issue.
    pub kind: GeometryIssueKind,
}
/// The selected dataset layer and filters whose features were checked.
#[derive(Debug, Clone)]
pub struct CheckScope {
    /// Actual source layer name.
    pub layer: String,
    /// Whether an attribute filter restricted the check.
    pub attribute_filter: bool,
    /// Optional source-CRS spatial filter.
    pub spatial_filter: Option<[f64; 4]>,
}
/// Complete scan summary with a bounded set of issue samples.
#[derive(Debug)]
pub struct GeometryCheck {
    /// Scope of this scan.
    pub scope: CheckScope,
    /// Number of features scanned.
    pub checked: u64,
    /// Features with at least one rejected geometry condition.
    pub invalid: u64,
    /// NULL geometries, including allowed ones.
    pub null: u64,
    /// EMPTY geometries, including allowed ones.
    pub empty: u64,
    /// Non-simple geometries when requested.
    pub non_simple: u64,
    /// Retained problem samples.
    pub issues: Vec<GeometryIssue>,
    /// More issues were found than retained.
    pub truncated: bool,
}
impl GeometryCheck {
    /// True only when the completed selected scan found no rejected conditions.
    pub fn is_valid(&self) -> bool {
        self.invalid == 0
    }
}
/// Validate a complete selected layer; invalid data is returned in the summary.
#[derive(Debug, Clone)]
#[must_use]
pub struct CheckGeometry {
    source: LayerSource,
    simple: bool,
    null: GeometryPolicy,
    empty: GeometryPolicy,
    limit: usize,
    output: Option<Output>,
    config: GdalOptions,
}
impl CheckGeometry {
    /// Describe a single layer check.
    pub fn new(source: impl Into<LayerSource>) -> Self {
        Self {
            source: source.into(),
            simple: false,
            null: GeometryPolicy::Reject,
            empty: GeometryPolicy::Reject,
            limit: 100,
            output: None,
            config: GdalOptions::default(),
        }
    }
    /// Additionally check geometry simplicity.
    pub fn check_simple(mut self, value: bool) -> Self {
        self.simple = value;
        self
    }
    /// Select the NULL geometry policy.
    pub fn null_geometry(mut self, value: GeometryPolicy) -> Self {
        self.null = value;
        self
    }
    /// Select the EMPTY geometry policy.
    pub fn empty_geometry(mut self, value: GeometryPolicy) -> Self {
        self.empty = value;
        self
    }
    /// Bound retained samples; the scan still completes.
    pub fn sample_limit(mut self, value: usize) -> Self {
        self.limit = value;
        self
    }
    /// Write all issues to a new GPKG, including geometry and source identity.
    pub fn issues_output(mut self, path: impl AsRef<Path>) -> Self {
        self.output = Some(Output::new(path, "geometry_issues"));
        self
    }
    /// Apply scoped GDAL configuration.
    pub fn gdal_options(mut self, value: GdalOptions) -> Self {
        self.config = value;
        self
    }
    /// Scan synchronously.
    pub fn run(self) -> Result<Outcome<GeometryCheck>> {
        self.run_with(&mut Control::default())
    }
    /// Scan with borrowed progress/cancellation; interruption is an error.
    pub fn run_with(self, control: &mut Control<'_>) -> Result<Outcome<GeometryCheck>> {
        if self.limit > 1_000_000 {
            return Err(Error::invalid("sample limit exceeds 1000000"));
        }
        execute::geos(0)?;
        execute::run(
            std::slice::from_ref(&self.source),
            self.config,
            self.output.as_ref(),
            control,
            |inputs, destination, progress| {
                let mut layer = self.source.select(&inputs[0])?;
                if layer.defn().geom_fields().count() != 1 {
                    return Err(Error::unsupported(
                        "geometry checks require exactly one geometry field",
                    ));
                }
                let scope = self.source.scope(&layer);
                let mut summary = GeometryCheck {
                    scope,
                    checked: 0,
                    invalid: 0,
                    null: 0,
                    empty: 0,
                    non_simple: 0,
                    issues: Vec::new(),
                    truncated: false,
                };
                let mut issues = if destination.spec.is_some() {
                    let output = destination.create()?;
                    let layer = output
                        .create_layer(LayerOptions {
                            name: "geometry_issues",
                            srs: layer.spatial_ref().as_ref(),
                            ty: OGRwkbGeometryType::wkbUnknown,
                            ..Default::default()
                        })
                        .map_err(inspect_error)?;
                    issue_fields(&layer)?;
                    Some(layer)
                } else {
                    None
                };
                let mut features = layer.features();
                loop {
                    let feature = checked_gdal_call(|| features.next()).map_err(inspect_error)?;
                    let Some(feature) = feature else { break };
                    summary.checked += 1;
                    if summary.checked % 256 == 1 {
                        query::pulse(progress, 0., c"checking geometries")?;
                    }
                    let mut kinds = Vec::with_capacity(2);
                    match execute::geometry(&feature) {
                        None => {
                            summary.null += 1;
                            if self.null == GeometryPolicy::Reject {
                                kinds.push(GeometryIssueKind::Null);
                            }
                        }
                        Some(geometry) if geometry.is_empty() => {
                            summary.empty += 1;
                            if self.empty == GeometryPolicy::Reject {
                                kinds.push(GeometryIssueKind::Empty);
                            }
                        }
                        Some(geometry) => {
                            execute::linear_xy(geometry)?;
                            if !execute::valid_geometry(geometry)? {
                                kinds.push(GeometryIssueKind::Invalid);
                            }
                            // SAFETY: GDAL borrows the live geometry for this predicate.
                            if self.simple
                                && !checked_gdal_call(|| {
                                    // SAFETY: the feature owns this live geometry throughout the predicate.
                                    unsafe { gdal_sys::OGR_G_IsSimple(geometry.c_geometry()) != 0 }
                                })
                                .map_err(inspect_error)?
                            {
                                summary.non_simple += 1;
                                kinds.push(GeometryIssueKind::NonSimple);
                            }
                        }
                    }
                    if !kinds.is_empty() {
                        summary.invalid += 1;
                    }
                    for kind in kinds {
                        let issue = GeometryIssue {
                            fid: feature.fid(),
                            ordinal: summary.checked,
                            kind,
                        };
                        if let Some(layer) = &mut issues {
                            write_issue(layer, &issue, execute::geometry(&feature))?;
                        }
                        if summary.issues.len() < self.limit {
                            summary.issues.push(issue);
                        } else {
                            summary.truncated = true;
                        }
                    }
                }
                query::pulse(progress, 1., c"geometry check complete")?;
                Ok(summary)
            },
        )
    }
}
pub(super) fn issue_fields(layer: &impl LayerAccess) -> Result<()> {
    for (name, ty) in [
        ("source_fid", OGRFieldType::OFTString),
        ("ordinal", OGRFieldType::OFTInteger64),
        ("issue", OGRFieldType::OFTString),
    ] {
        FieldDefn::new(name, ty)
            .and_then(|f| f.add_to_layer(layer))
            .map_err(inspect_error)?;
    }
    Ok(())
}
pub(super) fn write_issue(
    layer: &mut impl LayerAccess,
    issue: &GeometryIssue,
    geometry: Option<&Geometry>,
) -> Result<()> {
    let mut feature = Feature::new(layer.defn()).map_err(inspect_error)?;
    if let Some(fid) = issue.fid {
        feature
            .set_field_string(0, &fid.to_string())
            .map_err(inspect_error)?;
    }
    feature
        .set_field_integer64(
            1,
            i64::try_from(issue.ordinal)
                .map_err(|_| Error::invalid("feature ordinal exceeds i64"))?,
        )
        .map_err(inspect_error)?;
    feature
        .set_field_string(2, &format!("{:?}", issue.kind))
        .map_err(inspect_error)?;
    if let Some(geometry) = geometry {
        feature
            .set_geometry(geometry.clone())
            .map_err(inspect_error)?;
    }
    feature.create(layer).map_err(inspect_error)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn empty_policy_and_line_simplicity_are_independent_of_validity() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let source = dir.path().join("empty.gpkg");
        super::super::analysis_test_support::polygons(&source, &["POLYGON EMPTY", ""])?;
        let result = CheckGeometry::new(&source).run()?;
        assert_eq!(result.value().empty, 1);
        assert_eq!(result.value().null, 1);
        assert_eq!(result.value().invalid, 2);
        assert!(
            CheckGeometry::new(&source)
                .empty_geometry(GeometryPolicy::Allow)
                .null_geometry(GeometryPolicy::Allow)
                .run()?
                .value()
                .is_valid()
        );
        let line = dir.path().join("line.geojson");
        std::fs::write(
            &line,
            r#"{"type":"FeatureCollection","features":[{"type":"Feature","properties":{},"geometry":{"type":"LineString","coordinates":[[0,0],[2,2],[0,2],[2,0]]}}]}"#,
        )?;
        assert!(CheckGeometry::new(&line).run()?.value().is_valid());
        let result = CheckGeometry::new(&line).check_simple(true).run()?;
        assert_eq!(result.value().non_simple, 1);
        assert!(!result.value().is_valid());
        Ok(())
    }
    #[test]
    fn invalid_null_and_bounded_samples_are_results_not_execution_errors() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let source = dir.path().join("invalid.gpkg");
        let issues = dir.path().join("issues.gpkg");
        super::super::analysis_test_support::polygons(
            &source,
            &["POLYGON ((0 0,2 2,0 2,2 0,0 0))", ""],
        )?;
        let result = CheckGeometry::new(&source)
            .sample_limit(1)
            .issues_output(&issues)
            .run()?;
        assert_eq!(result.value().checked, 2);
        assert_eq!(result.value().invalid, 2);
        assert_eq!(result.value().null, 1);
        assert!(result.value().truncated);
        assert_eq!(result.value().issues.len(), 1);
        let ds = gdal::Dataset::open(&issues)?;
        assert_eq!(ds.layer(0)?.feature_count(), 2);
        ds.close()?;
        Ok(())
    }
}
