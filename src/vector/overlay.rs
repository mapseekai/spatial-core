use super::{
    LayerSource,
    execute::{self, Output},
};
use crate::{
    config::gdal::GdalOptions,
    gdal_ext::{
        ffi::{checked_gdal_call, last_null_pointer_err},
        query::{self, inspect_error},
    },
    operation::{Control, Error, Report, Result, Stage},
};
use gdal::{
    spatial_ref::SpatialRef,
    vector::{LayerAccess, LayerOptions, OGRFieldType, OGRwkbGeometryType},
};
use std::{collections::HashSet, path::Path};

/// OGR layer overlay semantics; operand order matters.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum OverlayOperation {
    /// Common area, with both input schemas.
    #[default]
    Intersection,
    /// All areas, with both input schemas.
    Union,
    /// Areas belonging to only one side.
    SymDifference,
    /// Input split by method boundaries.
    Identity,
    /// Replace input areas with method areas, using the input schema.
    Update,
    /// Retain input areas inside method features.
    Clip,
    /// Retain input areas outside method features.
    Erase,
}
/// Overlay two independently opened layer cursors into a new GPKG by default.
#[derive(Debug, Clone)]
#[must_use]
pub struct Overlay {
    input: LayerSource,
    method: LayerSource,
    output: Output,
    operation: OverlayOperation,
    input_fields: Option<Vec<String>>,
    method_fields: Option<Vec<String>>,
    prefixes: (String, String),
    config: GdalOptions,
}
impl Overlay {
    /// Describe two operands and a new local destination.
    pub fn new(
        input: impl Into<LayerSource>,
        method: impl Into<LayerSource>,
        output: impl AsRef<Path>,
    ) -> Self {
        Self {
            input: input.into(),
            method: method.into(),
            output: Output::new(output, "overlay"),
            operation: OverlayOperation::Intersection,
            input_fields: None,
            method_fields: None,
            prefixes: ("input_".into(), "method_".into()),
            config: GdalOptions::default(),
        }
    }
    /// Select the native operation.
    pub fn operation(mut self, value: OverlayOperation) -> Self {
        self.operation = value;
        self
    }
    /// Select attributes from the input side.
    pub fn input_fields<I, S>(mut self, values: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.input_fields = Some(values.into_iter().map(Into::into).collect());
        self
    }
    /// Select method attributes for two-schema operations.
    pub fn method_fields<I, S>(mut self, values: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.method_fields = Some(values.into_iter().map(Into::into).collect());
        self
    }
    /// Set distinct prefixes used by two-schema operations.
    pub fn field_prefixes(mut self, input: impl Into<String>, method: impl Into<String>) -> Self {
        self.prefixes = (input.into(), method.into());
        self
    }
    /// Set the output driver; default GPKG.
    pub fn driver(mut self, value: impl Into<String>) -> Self {
        self.output.driver = value.into();
        self
    }
    /// Set the new layer's name.
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
    /// Scope native configuration.
    pub fn gdal_options(mut self, value: GdalOptions) -> Self {
        self.config = value;
        self
    }
    /// Execute and close output synchronously.
    pub fn run(self) -> Result<Report> {
        self.run_with(&mut Control::default())
    }
    /// Execute with borrowed progress/cancellation.
    pub fn run_with(self, control: &mut Control<'_>) -> Result<Report> {
        // OGR's public C++ layer interface is stable within a GDAL minor release.
        let runtime = gdal::version::version_info("VERSION_NUM")
            .parse::<u32>()
            .unwrap_or(0);
        // SAFETY: this function only returns the version in the adapter's GDAL headers.
        if runtime / 10_000 != unsafe { spatial_core_overlay_header_version() } / 10_000 {
            return Err(Error::unsupported(
                "overlay requires matching build and runtime GDAL major/minor versions",
            ));
        }
        execute::geos(0)?;
        let both = matches!(
            self.operation,
            OverlayOperation::Intersection
                | OverlayOperation::Union
                | OverlayOperation::SymDifference
                | OverlayOperation::Identity
        );
        if !both && self.method_fields.is_some() {
            return Err(Error::invalid(
                "Clip, Erase and Update use the input schema and do not accept method_fields",
            ));
        }
        for prefix in [&self.prefixes.0, &self.prefixes.1] {
            if prefix.contains('\0') {
                return Err(Error::invalid("field prefix contains NUL"));
            }
        }
        let sources = [self.input, self.method];
        execute::run(
            &sources,
            self.config,
            Some(&self.output),
            control,
            |datasets, destination, progress| {
                let mut input = sources[0].select(&datasets[0])?;
                let mut method = sources[1].select(&datasets[1])?;
                let srs = input.spatial_ref().ok_or_else(|| Error::invalid("overlay requires input CRS"))?;
                let method_srs = method.spatial_ref().ok_or_else(|| Error::invalid("overlay requires method CRS"))?;
                same_crs(&srs, &method_srs)?;
                let input_kind = inspect_layer(&mut input, progress)?;
                let method_kind = inspect_layer(&mut method, progress)?;
                if method_kind != 3 || (input_kind != 3 && !matches!(self.operation,
                    OverlayOperation::Intersection | OverlayOperation::Clip | OverlayOperation::Erase)) {
                    return Err(Error::unsupported("supported overlays are polygon/polygon, plus point-or-line/polygon Intersection, Clip and Erase"));
                }
                let mut fields = select_fields(&input, self.input_fields.as_deref(), if both { &self.prefixes.0 } else { "" })?;
                let input_count = fields.len();
                if both {
                    fields.extend(select_fields(&method, self.method_fields.as_deref(), &self.prefixes.1)?);
                }
                if self.operation == OverlayOperation::Update {
                    let expected = select_fields(&method, self.input_fields.as_deref(), "")?;
                    if fields.iter().map(|f| (&f.name, f.kind, f.subtype))
                        .ne(expected.iter().map(|f| (&f.name, f.kind, f.subtype))) {
                        return Err(Error::invalid("Update requires matching selected field names, types and subtypes"));
                    }
                }
                let mut names = HashSet::new();
                for field in &fields {
                    if !names.insert(field.name.to_lowercase()) {
                        return Err(Error::invalid("output field names collide"));
                    }
                }
                if self.output.driver.eq_ignore_ascii_case("ESRI Shapefile") && (input_kind != 1 || fields.is_empty()) {
                    return Err(Error::unsupported("Shapefile cannot preserve this Multi geometry or zero-field overlay schema"));
                }
                let values = self.output.layer_options.strings();
                let refs = values.iter().map(String::as_str).collect::<Vec<_>>();
                let output = destination.create()?;
                let ty = match input_kind {
                    1 => OGRwkbGeometryType::wkbMultiPoint,
                    2 => OGRwkbGeometryType::wkbMultiLineString,
                    _ => OGRwkbGeometryType::wkbMultiPolygon,
                };
                let result = output.create_layer(LayerOptions {
                    name: &self.output.layer, srs: Some(&srs), ty, options: Some(&refs),
                }).map_err(inspect_error)?;
                for field in &fields {
                    field.add_to(&result)?;
                }
                let expected = fields.iter().map(|f| (f.name.clone(), f.kind, f.subtype)).collect::<Vec<_>>();
                if result.defn().geometry_type() != ty {
                    return Err(Error::unsupported("output driver changed the requested Multi geometry type"));
                }
                if execute::field_schema(&result) != expected {
                    return Err(Error::unsupported("output driver changed requested field names, types or subtypes"));
                }
                let mut options = gdal::cpl::CslStringList::new();
                for value in ["SKIP_FAILURES=NO", "PROMOTE_TO_MULTI=YES", "KEEP_LOWER_DIMENSION_GEOMETRIES=NO"] {
                    options.add_string(value).map_err(inspect_error)?;
                }
                // A zero-field result otherwise tells OGR to recreate all input attributes.
                if fields.is_empty() {
                    options.add_string("ADD_FIELDS=NO").map_err(inspect_error)?;
                }
                let mut aliases = gdal::cpl::CslStringList::new();
                if both {
                    let (left, right) = (&self.prefixes.0, &self.prefixes.1);
                    let (a, b) = (left.to_lowercase(), right.to_lowercase());
                    // OGR maps every source field by prefix, even fields outside the projection.
                    let (native_left, native_right) = if a.starts_with(&b) || b.starts_with(&a) {
                        for (i, field) in fields.iter().enumerate() {
                            let (prefix, native) = if i < input_count { (left, "A_") } else { (right, "B_") };
                            aliases.add_string(&format!("{native}{}", &field.name[prefix.len()..])).map_err(inspect_error)?;
                        }
                        ("A_", "B_")
                    } else { (left.as_str(), right.as_str()) };
                    options.add_string(&format!("INPUT_PREFIX={native_left}")).map_err(inspect_error)?;
                    options.add_string(&format!("METHOD_PREFIX={native_right}")).map_err(inspect_error)?;
                }
                query::pulse(progress, 0., c"overlaying layers")?;
                let call = match self.operation {
                    OverlayOperation::Intersection => gdal_sys::OGR_L_Intersection,
                    OverlayOperation::Union => gdal_sys::OGR_L_Union,
                    OverlayOperation::SymDifference => gdal_sys::OGR_L_SymDifference,
                    OverlayOperation::Identity => gdal_sys::OGR_L_Identity,
                    OverlayOperation::Update => gdal_sys::OGR_L_Update,
                    OverlayOperation::Clip => gdal_sys::OGR_L_Clip,
                    OverlayOperation::Erase => gdal_sys::OGR_L_Erase,
                };
                let mut written = 0u64;
                // SAFETY: all layers, names, options, the counter and the callback live for this synchronous call.
                let status = checked_gdal_call(|| unsafe {
                    spatial_core_overlay(call, input.c_layer(), method.c_layer(), result.c_layer(), ty, aliases.as_ptr(), options.as_ptr(),
                        progress.native_callback(), progress.user_data(), &mut written)
                });
                query::check_progress(progress)?;
                if status.map_err(inspect_error)? != gdal_sys::OGRErr::OGRERR_NONE {
                    return Err(Error::native(Stage::Execute, last_null_pointer_err("OGR layer overlay")));
                }
                // Other formats may infer a different schema on reopen or simplify singleton Multi geometries.
                if !matches!(output.driver().short_name().as_str(), "GPKG" | "FlatGeobuf") {
                    let mut error = None;
                    crate::gdal_ext::execution::close(destination.dataset.take(), Stage::CloseOutput, &mut error);
                    if let Some(error) = error { return Err(error); }
                    verify_persisted(&self.output.path, ty, &expected, written, progress)?;
                }
                Ok(())
            },
        ).map(|result| result.into_parts().1)
    }
}
fn verify_persisted(
    output: &Path,
    geometry: OGRwkbGeometryType::Type,
    fields: &[(String, OGRFieldType::Type, gdal_sys::OGRFieldSubType::Type)],
    written: u64,
    progress: &mut crate::gdal_ext::progress::ProgressCallbackState<'_>,
) -> Result<()> {
    let dataset = crate::dataset::Source::from(output).open_kind(crate::dataset::Kind::Vector)?;
    let unsupported = |message| {
        Error::new(
            crate::operation::ErrorKind::Unsupported,
            Stage::Inspect,
            message,
        )
    };
    let result = (|| {
        let mut layer = dataset.layer(0).map_err(inspect_error)?;
        if layer.defn().geometry_type() != geometry || execute::field_schema(&layer) != fields {
            return Err(unsupported(
                "output format did not preserve the overlay schema on disk",
            ));
        }
        let mut features = layer.features();
        let mut count = 0u64;
        while let Some(feature) = checked_gdal_call(|| features.next()).map_err(inspect_error)? {
            if count.is_multiple_of(256) {
                query::pulse(progress, 1., c"checking stored overlay geometries")?;
            }
            if !execute::geometry(&feature)
                .is_some_and(|g| g.geometry_type() == geometry && !g.is_empty())
            {
                return Err(unsupported(
                    "output format did not preserve a Multi geometry on disk",
                ));
            }
            count += 1;
        }
        if count != written {
            return Err(unsupported(
                "output format did not preserve the overlay feature count",
            ));
        }
        Ok(())
    })();
    let mut error = result.err();
    crate::gdal_ext::execution::close(Some(dataset), Stage::CloseOutput, &mut error);
    error.map_or(Ok(()), Err)
}

type NativeOverlay = unsafe extern "C" fn(
    gdal_sys::OGRLayerH,
    gdal_sys::OGRLayerH,
    gdal_sys::OGRLayerH,
    gdal_sys::CSLConstList,
    gdal_sys::GDALProgressFunc,
    *mut std::ffi::c_void,
) -> gdal_sys::OGRErr::Type;

unsafe extern "C" {
    fn spatial_core_overlay_header_version() -> u32;
    fn spatial_core_overlay(
        operation: NativeOverlay,
        input: gdal_sys::OGRLayerH,
        method: gdal_sys::OGRLayerH,
        output: gdal_sys::OGRLayerH,
        geometry_type: OGRwkbGeometryType::Type,
        aliases: gdal_sys::CSLConstList,
        options: gdal_sys::CSLConstList,
        progress: gdal_sys::GDALProgressFunc,
        user_data: *mut std::ffi::c_void,
        written: *mut u64,
    ) -> gdal_sys::OGRErr::Type;
}
struct Field {
    name: String,
    kind: OGRFieldType::Type,
    subtype: gdal_sys::OGRFieldSubType::Type,
    width: i32,
    precision: i32,
}
impl Field {
    fn add_to(&self, layer: &impl LayerAccess) -> Result<()> {
        let name = std::ffi::CString::new(self.name.as_str())
            .map_err(|_| Error::invalid("field name contains NUL"))?;
        // SAFETY: the temporary definition is destroyed on every path; CreateField copies it synchronously.
        let status = checked_gdal_call(|| unsafe {
            let field = gdal_sys::OGR_Fld_Create(name.as_ptr(), self.kind);
            if field.is_null() {
                return gdal_sys::OGRErr::OGRERR_NOT_ENOUGH_MEMORY;
            }
            gdal_sys::OGR_Fld_SetSubType(field, self.subtype);
            gdal_sys::OGR_Fld_SetWidth(field, self.width);
            gdal_sys::OGR_Fld_SetPrecision(field, self.precision);
            let status = gdal_sys::OGR_L_CreateField(layer.c_layer(), field, 0);
            gdal_sys::OGR_Fld_Destroy(field);
            status
        })
        .map_err(inspect_error)?;
        if status != gdal_sys::OGRErr::OGRERR_NONE {
            return Err(Error::native(
                Stage::OpenOutput,
                last_null_pointer_err("OGR_L_CreateField"),
            ));
        }
        Ok(())
    }
}
fn select_fields(
    layer: &impl LayerAccess,
    selection: Option<&[String]>,
    prefix: &str,
) -> Result<Vec<Field>> {
    let all = layer
        .defn()
        .fields()
        .enumerate()
        .map(|(index, f)| Field {
            name: f.name(),
            kind: f.field_type(),
            subtype: execute::field_subtype(layer.defn(), index),
            width: f.width(),
            precision: f.precision(),
        })
        .collect::<Vec<_>>();
    let names = selection.map_or_else(
        || all.iter().map(|f| f.name.clone()).collect(),
        |v| v.to_vec(),
    );
    names
        .iter()
        .map(|name| {
            let field = all
                .iter()
                .find(|f| &f.name == name)
                .ok_or_else(|| Error::invalid("selected overlay field does not exist"))?;
            Ok(Field {
                name: format!("{prefix}{name}"),
                kind: field.kind,
                subtype: field.subtype,
                width: field.width,
                precision: field.precision,
            })
        })
        .collect()
}
fn same_crs(a: &SpatialRef, b: &SpatialRef) -> Result<()> {
    // SAFETY: both spatial references are live, and epoch access does not transfer ownership.
    let same_epoch = unsafe {
        gdal_sys::OSRGetCoordinateEpoch(a.to_c_hsrs())
            == gdal_sys::OSRGetCoordinateEpoch(b.to_c_hsrs())
    };
    if a != b || a.axis_mapping_strategy() != b.axis_mapping_strategy() || !same_epoch {
        Err(Error::invalid(
            "overlay CRS and coordinate epochs must match; reproject explicitly",
        ))
    } else {
        Ok(())
    }
}
fn inspect_layer(
    layer: &mut impl LayerAccess,
    progress: &mut crate::gdal_ext::progress::ProgressCallbackState<'_>,
) -> Result<u32> {
    if layer.defn().geom_fields().count() != 1 {
        return Err(Error::unsupported(
            "overlay requires exactly one geometry field",
        ));
    }
    let declared = layer
        .defn()
        .geom_fields()
        .next()
        .expect("one geometry field")
        .field_type();
    if !matches!(
        declared,
        OGRwkbGeometryType::wkbUnknown
            | OGRwkbGeometryType::wkbPoint
            | OGRwkbGeometryType::wkbMultiPoint
            | OGRwkbGeometryType::wkbLineString
            | OGRwkbGeometryType::wkbMultiLineString
            | OGRwkbGeometryType::wkbPolygon
            | OGRwkbGeometryType::wkbMultiPolygon
    ) {
        return Err(Error::unsupported(
            "overlay requires a linear XY layer schema",
        ));
    }
    let mut family = geometry_family(declared);
    let mut index = 0u64;
    let mut features = layer.features();
    loop {
        let Some(feature) = checked_gdal_call(|| features.next()).map_err(inspect_error)? else {
            break;
        };
        index += 1;
        if index % 256 == 1 {
            query::pulse(progress, 0., c"checking overlay inputs")?;
        }
        let geometry = execute::geometry(&feature)
            .ok_or_else(|| Error::invalid("overlay does not accept NULL geometries"))?;
        execute::linear_xy(geometry)?;
        if geometry.is_empty() || !execute::valid_geometry(geometry)? {
            return Err(Error::invalid("overlay requires valid nonempty geometries"));
        }
        let actual = geometry_family(geometry.geometry_type())
            .ok_or_else(|| Error::unsupported("unsupported overlay geometry family"))?;
        if family.is_some_and(|f| f != actual) {
            return Err(Error::unsupported(
                "mixed geometry families require explicit normalization",
            ));
        }
        family = Some(actual);
    }
    drop(features);
    layer.reset_feature_reading();
    family.ok_or_else(|| Error::unsupported("empty layers need a concrete geometry type"))
}
fn geometry_family(kind: OGRwkbGeometryType::Type) -> Option<u32> {
    // SAFETY: type flattening only inspects the numeric code.
    match unsafe { gdal_sys::OGR_GT_Flatten(kind) } {
        OGRwkbGeometryType::wkbPoint | OGRwkbGeometryType::wkbMultiPoint => Some(1),
        OGRwkbGeometryType::wkbLineString | OGRwkbGeometryType::wkbMultiLineString => Some(2),
        OGRwkbGeometryType::wkbPolygon | OGRwkbGeometryType::wkbMultiPolygon => Some(3),
        _ => None,
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn filters_boolean_fields_and_empty_intersections_keep_their_semantics() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let a = dir.path().join("a.geojson");
        std::fs::write(
            &a,
            r#"{"type":"FeatureCollection","features":[{"type":"Feature","properties":{"enabled":true},"geometry":{"type":"Polygon","coordinates":[[[0,0],[2,0],[2,2],[0,2],[0,0]]]}}]}"#,
        )?;
        let out = dir.path().join("bool.gpkg");
        let _ = Overlay::new(LayerSource::new(&a).where_clause("enabled = 1"), &a, &out).run()?;
        let info = super::super::Info::new(&out).run()?;
        assert_eq!(info.value().layers[0].feature_count, Some(1));
        assert!(
            info.value().layers[0]
                .fields
                .iter()
                .all(|f| f.field_subtype == gdal_sys::OGRFieldSubType::OFSTBoolean)
        );
        let empty = dir.path().join("empty.gpkg");
        let _ = Overlay::new(LayerSource::new(&a).where_clause("enabled = 0"), &a, &empty).run()?;
        assert_eq!(
            super::super::Info::new(&empty).run()?.value().layers[0].feature_count,
            Some(0)
        );
        let collision = dir.path().join("collision.gpkg");
        let error = Overlay::new(&a, &a, &collision)
            .field_prefixes("same_", "SAME_")
            .run()
            .unwrap_err();
        assert_eq!(
            error.output_state(),
            crate::operation::OutputState::NotModified
        );
        assert!(!collision.exists());
        let b = dir.path().join("b.geojson");
        std::fs::write(
            &b,
            r#"{"type":"FeatureCollection","features":[{"type":"Feature","properties":{},"geometry":{"type":"Polygon","coordinates":[[[2,0],[4,0],[4,2],[2,2],[2,0]]]}}]}"#,
        )?;
        let touching = dir.path().join("touching.gpkg");
        let _ = Overlay::new(&a, &b, &touching).run()?;
        assert_eq!(
            super::super::Info::new(&touching).run()?.value().layers[0].feature_count,
            Some(0)
        );
        Ok(())
    }

    #[test]
    fn point_line_inputs_and_explicit_multilayer_selection_are_supported() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let polygons = dir.path().join("areas.gpkg");
        super::super::analysis_test_support::polygons(
            &polygons,
            &["POLYGON ((0 0,2 0,2 2,0 2,0 0))"],
        )?;
        let mut ds = gdal::Dataset::open_ex(
            &polygons,
            gdal::DatasetOptions {
                open_flags: gdal::GdalOpenFlags::GDAL_OF_UPDATE,
                ..Default::default()
            },
        )?;
        let srs = SpatialRef::from_epsg(3857)?;
        for (name, ty, wkt) in [
            ("points", OGRwkbGeometryType::wkbPoint, "POINT (1 1)"),
            (
                "lines",
                OGRwkbGeometryType::wkbLineString,
                "LINESTRING (-1 1,3 1)",
            ),
        ] {
            let layer = ds.create_layer(LayerOptions {
                name,
                srs: Some(&srs),
                ty,
                ..Default::default()
            })?;
            let mut f = gdal::vector::Feature::new(layer.defn())?;
            f.set_geometry(gdal::vector::Geometry::from_wkt(wkt)?)?;
            f.create(&layer)?;
        }
        ds.close()?;
        let bad = dir.path().join("ambiguous.gpkg");
        assert!(Overlay::new(&polygons, &polygons, &bad).run().is_err());
        assert!(!bad.exists());
        for name in ["points", "lines"] {
            let out = dir.path().join(format!("{name}.gpkg"));
            let _ = Overlay::new(
                LayerSource::new(&polygons).layer(name),
                LayerSource::new(&polygons).layer("parcels"),
                &out,
            )
            .operation(OverlayOperation::Clip)
            .run()?;
            assert_eq!(
                super::super::Info::new(&out).run()?.value().layers[0].feature_count,
                Some(1)
            );
        }
        let cancelled = dir.path().join("cancelled.gpkg");
        let mut callback = |_: &crate::operation::Progress| std::ops::ControlFlow::Break(());
        let error = Overlay::new(
            LayerSource::new(&polygons).layer("parcels"),
            LayerSource::new(&polygons).layer("parcels"),
            &cancelled,
        )
        .run_with(&mut Control::default().on_progress(&mut callback))
        .unwrap_err();
        assert_eq!(error.kind(), crate::operation::ErrorKind::Cancelled);
        assert!(!cancelled.exists());
        Ok(())
    }
    fn integer(
        feature: &gdal::vector::Feature<'_>,
        index: usize,
    ) -> gdal::errors::Result<Option<i32>> {
        // Some drivers reopen an absent field as unset rather than explicitly NULL.
        // SAFETY: index is from the live feature definition.
        if unsafe { gdal_sys::OGR_F_IsFieldSetAndNotNull(feature.c_feature(), index as i32) } == 0 {
            return Ok(None);
        }
        feature.field_as_integer(index)
    }

    fn attributed(path: &Path, geometry: &str, value: i32) -> anyhow::Result<()> {
        std::fs::write(
            path,
            format!(
                r#"{{"type":"Feature","properties":{{"keep":{value},"secret":"private"}},"geometry":{geometry}}}"#
            ),
        )?;
        Ok(())
    }

    const SQUARE: &str = r#"{"type":"Polygon","coordinates":[[[0,0],[2,0],[2,2],[0,2],[0,0]]]}"#;
    const OVERLAP: &str = r#"{"type":"Polygon","coordinates":[[[1,0],[3,0],[3,2],[1,2],[1,0]]]}"#;

    #[test]
    fn projected_schema_and_values_survive_the_native_overlay() -> anyhow::Result<()> {
        use OverlayOperation::{Clip, Intersection, Update};

        let dir = tempfile::tempdir()?;
        let input = dir.path().join("input.geojson");
        let method = dir.path().join("method.geojson");
        attributed(&input, SQUARE, 42)?;
        attributed(&method, OVERLAP, 84)?;
        for (i, (op, left, right, names, mut expected)) in [
            (Intersection, false, false, vec![], vec![vec![]]),
            (
                Intersection,
                true,
                false,
                vec!["input_keep"],
                vec![vec![Some(42)]],
            ),
            (
                Intersection,
                false,
                true,
                vec!["method_keep"],
                vec![vec![Some(84)]],
            ),
            (
                Intersection,
                true,
                true,
                vec!["input_keep", "method_keep"],
                vec![vec![Some(42), Some(84)]],
            ),
            (Clip, false, false, vec![], vec![vec![]]),
            (Update, false, false, vec![], vec![vec![], vec![]]),
            (
                Update,
                true,
                false,
                vec!["keep"],
                vec![vec![Some(42)], vec![Some(84)]],
            ),
        ]
        .into_iter()
        .enumerate()
        {
            let output = dir.path().join(format!("result-{i}.gpkg"));
            let mut op_builder = Overlay::new(&input, &method, &output)
                .operation(op)
                .input_fields(left.then_some("keep"));
            if op == Intersection {
                op_builder = op_builder.method_fields(right.then_some("keep"));
            }
            let _ = op_builder.run()?;
            let ds = gdal::Dataset::open(output)?;
            let mut layer = ds.layer(0)?;
            assert_eq!(
                layer.defn().fields().map(|f| f.name()).collect::<Vec<_>>(),
                names,
                "{op:?}"
            );
            let mut rows = Vec::new();
            for f in layer.features() {
                rows.push(
                    (0..names.len())
                        .map(|i| integer(&f, i))
                        .collect::<gdal::errors::Result<Vec<_>>>()?,
                );
            }
            rows.sort();
            expected.sort();
            assert_eq!(rows, expected, "{op:?}");
        }
        Ok(())
    }

    #[test]
    fn strict_driver_receives_promoted_update_rows_and_null_fields() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let input = dir.path().join("input.geojson");
        let method = dir.path().join("method.geojson");
        attributed(&input, SQUARE, 42)?;
        attributed(&method, OVERLAP, 84)?;
        for (op, mut expected) in [
            (
                OverlayOperation::Update,
                vec![vec![Some(42)], vec![Some(84)]],
            ),
            (
                OverlayOperation::Union,
                vec![
                    vec![Some(42), None],
                    vec![Some(42), Some(84)],
                    vec![None, Some(84)],
                ],
            ),
        ] {
            let output = dir.path().join(format!("{op:?}.fgb"));
            let mut builder = Overlay::new(&input, &method, &output)
                .operation(op)
                .driver("FlatGeobuf")
                .input_fields(["keep"]);
            if op == OverlayOperation::Union {
                builder = builder.method_fields(["keep"]);
            }
            let _ = builder.run()?;
            let ds = gdal::Dataset::open(output)?;
            let mut layer = ds.layer(0)?;
            let fields = layer.defn().fields().count();
            let mut rows = Vec::new();
            for f in layer.features() {
                assert_eq!(
                    f.geometry().unwrap().geometry_type(),
                    OGRwkbGeometryType::wkbMultiPolygon
                );
                rows.push(
                    (0..fields)
                        .map(|i| integer(&f, i))
                        .collect::<gdal::errors::Result<Vec<_>>>()?,
                );
            }
            rows.sort();
            expected.sort();
            assert_eq!(rows, expected);
        }
        Ok(())
    }

    #[test]
    fn unselected_side_never_fills_an_output_field_with_the_same_name() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let input = dir.path().join("input.geojson");
        let method = dir.path().join("method.geojson");
        attributed(&input, SQUARE, 42)?;
        attributed(&method, OVERLAP, 84)?;
        for select_input in [true, false] {
            let output = dir.path().join(format!("{select_input}.gpkg"));
            let _ = Overlay::new(&input, &method, &output)
                .operation(OverlayOperation::Union)
                .input_fields(select_input.then_some("keep"))
                .method_fields((!select_input).then_some("keep"))
                .field_prefixes("", "")
                .run()?;
            let ds = gdal::Dataset::open(output)?;
            let mut values: Vec<_> = ds
                .layer(0)?
                .features()
                .map(|f| integer(&f, 0))
                .collect::<gdal::errors::Result<_>>()?;
            values.sort();
            let selected = if select_input { 42 } else { 84 };
            assert_eq!(values, vec![None, Some(selected), Some(selected)]);
        }
        Ok(())
    }

    #[test]
    fn formats_must_preserve_the_stored_multi_schema() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let input = dir.path().join("input.geojson");
        let method = dir.path().join("method.geojson");
        attributed(&input, SQUARE, 42)?;
        attributed(&method, OVERLAP, 84)?;
        let shape = dir.path().join("result.shp");
        let error = Overlay::new(&input, &method, &shape)
            .driver("ESRI Shapefile")
            .run()
            .unwrap_err();
        assert_eq!(
            error.output_state(),
            crate::operation::OutputState::NotModified
        );
        assert!(!shape.exists());
        let json = dir.path().join("result.geojson");
        let _ = Overlay::new(&input, &method, &json)
            .driver("GeoJSON")
            .run()?;
        let ds = gdal::Dataset::open(json)?;
        assert_eq!(
            ds.layer(0)?
                .features()
                .next()
                .unwrap()
                .geometry()
                .unwrap()
                .geometry_type(),
            OGRwkbGeometryType::wkbMultiPolygon
        );
        let empty = dir.path().join("empty.geojson");
        let error = Overlay::new(
            LayerSource::new(&input).where_clause("keep = 0"),
            &method,
            &empty,
        )
        .driver("GeoJSON")
        .run()
        .unwrap_err();
        assert_eq!(error.stage(), Stage::Inspect);
        assert_eq!(error.kind(), crate::operation::ErrorKind::Unsupported);
        assert_eq!(
            error.output_state(),
            crate::operation::OutputState::MayHaveChanged
        );
        Ok(())
    }

    #[test]
    fn touching_geometries_never_write_lower_dimensional_features() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let method = dir.path().join("method.geojson");
        attributed(&method, SQUARE, 84)?;
        for (i, (geometry, clip_count, erase_count, multi)) in [
            (
                r#"{"type":"Polygon","coordinates":[[[2,0],[4,0],[4,2],[2,2],[2,0]]]}"#,
                0,
                1,
                OGRwkbGeometryType::wkbMultiPolygon,
            ),
            (
                r#"{"type":"LineString","coordinates":[[-1,0],[0,0]]}"#,
                0,
                1,
                OGRwkbGeometryType::wkbMultiLineString,
            ),
            (
                r#"{"type":"Point","coordinates":[2,1]}"#,
                1,
                0,
                OGRwkbGeometryType::wkbMultiPoint,
            ),
            // One part crosses the polygon and the other only touches a corner.
            (
                r#"{"type":"MultiLineString","coordinates":[[[-1,1],[3,1]], [[-2,0],[0,0]]]}"#,
                1,
                1,
                OGRwkbGeometryType::wkbMultiLineString,
            ),
        ]
        .into_iter()
        .enumerate()
        {
            let input = dir.path().join(format!("input-{i}.geojson"));
            attributed(&input, geometry, 42)?;
            for (op, count) in [
                (OverlayOperation::Clip, clip_count),
                (OverlayOperation::Intersection, clip_count),
                (OverlayOperation::Erase, erase_count),
            ] {
                let output = dir.path().join(format!("{i}-{op:?}.gpkg"));
                let _ = Overlay::new(&input, &method, &output).operation(op).run()?;
                let ds = gdal::Dataset::open(output)?;
                let mut layer = ds.layer(0)?;
                assert_eq!(layer.defn().geometry_type(), multi);
                let mut types = Vec::new();
                for feature in layer.features() {
                    let geometry = feature.geometry().unwrap();
                    types.push(Some(geometry.geometry_type()));
                    if i == 3 {
                        assert_eq!(
                            geometry.length(),
                            if op == OverlayOperation::Erase {
                                4.
                            } else {
                                2.
                            }
                        );
                    }
                }
                assert_eq!(types, vec![Some(multi); count], "{i}:{op:?}");
            }
        }
        Ok(())
    }

    #[test]
    fn seven_overlays_preserve_schema_areas_and_inputs() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let a = dir.path().join("a.gpkg");
        let b = dir.path().join("b.gpkg");
        super::super::analysis_test_support::polygons(&a, &["POLYGON ((0 0,2 0,2 2,0 2,0 0))"])?;
        super::super::analysis_test_support::polygons(&b, &["POLYGON ((1 0,3 0,3 2,1 2,1 0))"])?;
        let before = std::fs::read(&a)?;
        for (operation, area, fields) in [
            (OverlayOperation::Intersection, 2., 2),
            (OverlayOperation::Union, 6., 2),
            (OverlayOperation::SymDifference, 4., 2),
            (OverlayOperation::Identity, 4., 2),
            (OverlayOperation::Update, 6., 1),
            (OverlayOperation::Clip, 2., 1),
            (OverlayOperation::Erase, 2., 1),
        ] {
            let out = dir.path().join(format!("{operation:?}.gpkg"));
            let _ = Overlay::new(&a, &b, &out).operation(operation).run()?;
            assert_eq!(
                super::super::analysis_test_support::area(&out)?,
                area,
                "{operation:?}"
            );
            let ds = gdal::Dataset::open(&out)?;
            let mut layer = ds.layer(0)?;
            assert_eq!(layer.defn().fields().count(), fields);
            assert!(
                layer.features().all(|f| f
                    .geometry()
                    .is_some_and(|g| g.geometry_type() == OGRwkbGeometryType::wkbMultiPolygon)),
                "{operation:?}"
            );
            ds.close()?;
        }
        assert_eq!(std::fs::read(&a)?, before);
        Ok(())
    }
}
