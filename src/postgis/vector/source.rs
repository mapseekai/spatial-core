use super::{
    ColumnSchema, CoordinateDimension, GeometryColumn, GeometryKind, PostgresType, VectorSchema,
    VectorWriteOptions, copy, ewkb,
};
use crate::{
    config::gdal::GdalOptions,
    gdal_ext::ffi::checked_gdal_call,
    postgis::{PostGisError, Result, stream::SourceStream},
};
use gdal::{
    Dataset,
    spatial_ref::{AxisMappingStrategy, CoordTransform, SpatialRef},
    vector::{
        Feature, FieldValue, LayerAccess, OGRFieldType, OGRwkbGeometryType, geometry_type_flatten,
        geometry_type_has_m, geometry_type_has_z, geometry_type_set_modifier,
    },
};
use std::path::{Path, PathBuf};

/// Owned input description; live GDAL handles stay in the blocking reader.
#[derive(Debug, Clone)]
pub struct VectorSource {
    path: PathBuf,
    layer: Option<LayerSelector>,
    options: GdalOptions,
    open_options: Vec<String>,
}
#[derive(Debug, Clone)]
enum LayerSelector {
    Name(String),
    Index(usize),
}

impl VectorSource {
    /// Input path or GDAL virtual filesystem URI.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            layer: None,
            options: GdalOptions::default(),
            open_options: Vec::new(),
        }
    }
    /// Select a layer by name. Multiple layers require explicit selection.
    #[must_use]
    pub fn layer_name(mut self, name: impl Into<String>) -> Self {
        self.layer = Some(LayerSelector::Name(name.into()));
        self
    }
    /// Select a layer by zero-based index.
    #[must_use]
    pub fn layer_index(mut self, index: usize) -> Self {
        self.layer = Some(LayerSelector::Index(index));
        self
    }
    /// Apply explicit thread-local options for the worker lifetime.
    #[must_use]
    pub fn gdal_options(mut self, options: GdalOptions) -> Self {
        self.options = options;
        self
    }
    /// Driver open option, for example `DATE_AS_STRING=YES` for exact GeoJSON time text.
    #[must_use]
    pub fn open_option(mut self, option: impl Into<String>) -> Self {
        self.open_options.push(option.into());
        self
    }
}

pub(crate) struct SourceInfo {
    pub schema: VectorSchema,
    pub total: Option<u64>,
}

fn input(message: &str) -> PostGisError {
    PostGisError::InvalidInput(message.into())
}

pub(crate) fn start(source: VectorSource, options: VectorWriteOptions) -> SourceStream<SourceInfo> {
    SourceStream::start(options.limits, move |metadata, sender| {
        let _scope = source.options.enter();
        let dataset = open_dataset(&source.path, &source.open_options)
            .map_err(|e| PostGisError::source(0, e))?;
        let mut layer = checked_gdal_call(|| match &source.layer {
            Some(LayerSelector::Name(name)) => dataset.layer_by_name(name),
            Some(LayerSelector::Index(index)) => dataset.layer(*index),
            None => dataset.layer(0),
        })
        .map_err(|e| PostGisError::source(0, e))?
        .map_err(|e| PostGisError::source(0, e))?;
        if source.layer.is_none() && dataset.layer_count() != 1 {
            return Err(input("a multi-layer source requires an explicit layer"));
        }
        let mut source_crs = layer
            .spatial_ref()
            .ok_or_else(|| input("source has no usable CRS"))?;
        if source_crs.auth_name().is_none()
            && matches!(
                source_crs.name().as_deref(),
                Some("Undefined geographic SRS" | "Undefined Cartesian SRS")
            )
        {
            return Err(input("source has no usable CRS"));
        }
        let source_srid = (source_crs.auth_name().as_deref() == Some("EPSG"))
            .then(|| source_crs.auth_code().ok())
            .flatten();
        let explicit_srid = options
            .target_srid
            .or(options.schema.as_ref().map(|s| s.geometry.srid));
        let srid = explicit_srid
            .or(source_srid)
            .ok_or_else(|| input("source CRS has no EPSG identifier; specify a target SRID"))?;
        if !(1..=998999).contains(&srid) {
            return Err(input("invalid target SRID"));
        }
        let mut target_crs =
            SpatialRef::from_epsg(srid as u32).map_err(|e| PostGisError::source(0, e))?;
        source_crs.set_axis_mapping_strategy(AxisMappingStrategy::TraditionalGisOrder);
        target_crs.set_axis_mapping_strategy(AxisMappingStrategy::TraditionalGisOrder);
        if explicit_srid.is_none() && source_crs != target_crs {
            return Err(input(
                "source CRS differs from its EPSG authority; specify an explicit target",
            ));
        }
        let transform = CoordTransform::new(&source_crs, &target_crs)
            .map_err(|e| PostGisError::source(0, e))?;
        let shapefile = dataset.driver().short_name() == "ESRI Shapefile";
        let declared = layer.defn().geometry_type();
        let normalized = normalize_kind(declared, shapefile);
        let fields = layer
            .defn()
            .fields()
            .map(|f| (f.name(), f.field_type()))
            .collect::<Vec<_>>();
        for source in options.renames.keys() {
            if !fields.iter().any(|(name, _)| name == source) {
                return Err(input("field rename refers to an absent source field"));
            }
        }
        let mut schema = if let Some(schema) = &options.schema {
            schema.clone()
        } else {
            let (kind, dimension) = geometry_schema(normalized)?;
            let columns = fields
                .iter()
                .map(|(name, kind)| {
                    ColumnSchema::new(
                        options.renames.get(name).unwrap_or(name).clone(),
                        field_type(*kind)?,
                    )
                })
                .collect::<Result<Vec<_>>>()?;
            VectorSchema::new(columns, GeometryColumn::new("geom", kind, dimension, srid)?)?
        };
        if options.target_srid.is_some() {
            schema.geometry.srid = srid;
        }
        schema.validate()?;
        let mut mapping = vec![None; schema.columns.len()];
        for (index, (name, _)) in fields.iter().enumerate() {
            let target = options.renames.get(name).unwrap_or(name);
            let position = schema
                .columns
                .iter()
                .position(|c| c.name.as_str() == target)
                .ok_or_else(|| input("source attribute is absent from the output schema"))?;
            if mapping[position].replace(index).is_some() {
                return Err(input("source field names collide after mapping"));
            }
        }
        let total = checked_gdal_call(|| layer.try_feature_count())
            .map_err(|e| PostGisError::source(0, e))?;
        metadata
            .send(SourceInfo {
                schema: schema.clone(),
                total,
            })
            .map_err(|_| PostGisError::Cancelled)?;
        sender.validate_srid()?;
        let mut features =
            checked_gdal_call(|| layer.features()).map_err(|e| PostGisError::source(0, e))?;
        let mut record = 0;
        loop {
            if sender.cancellation().is_cancelled() {
                return Err(PostGisError::Cancelled);
            }
            let Some(feature) = checked_gdal_call(|| features.next())
                .map_err(|e| PostGisError::source(record + 1, e))?
            else {
                break;
            };
            record += 1;
            let mut row = Vec::new();
            for (column, source_index) in schema.columns.iter().zip(&mapping) {
                let value = match source_index {
                    Some(index) => read_value(&feature, *index, fields[*index].1)
                        .map_err(|e| PostGisError::source(record, e))?,
                    None => None,
                };
                copy::push_field(&mut row, value, column, options.limits.max_record_bytes)?;
                row.push(b'\t');
                if row.len() > options.limits.max_record_bytes {
                    return Err(PostGisError::ResourceLimit(
                        "encoded attributes exceed record budget".into(),
                    ));
                }
            }
            let geometry = feature.geometry().filter(|geometry| {
                // SAFETY: retrieving the owned feature's pointer does not dereference it.
                geometry.has_gdal_ptr() && !unsafe { geometry.c_geometry() }.is_null()
            });
            match geometry {
                None if schema.geometry.nullable => row.extend_from_slice(b"\\N"),
                None => return Err(input("source feature has NULL geometry")),
                Some(geometry) => {
                    let transformed = if geometry.is_empty() {
                        geometry.clone()
                    } else {
                        geometry
                            .transform(&transform)
                            .map_err(|e| PostGisError::source(record, e))?
                    };
                    let normalized_kind = normalize_kind(transformed.geometry_type(), shapefile);
                    let transformed = if normalized_kind != transformed.geometry_type() {
                        let mut multi = gdal::vector::Geometry::empty(normalized_kind)
                            .map_err(|e| PostGisError::source(record, e))?;
                        multi
                            .add_geometry(transformed)
                            .map_err(|e| PostGisError::source(record, e))?;
                        multi
                    } else {
                        transformed
                    };
                    let wkb = transformed
                        .wkb()
                        .map_err(|e| PostGisError::source(record, e))?;
                    if wkb
                        .len()
                        .checked_add(4)
                        .and_then(|n| n.checked_mul(2))
                        .and_then(|n| n.checked_add(row.len() + 1))
                        .is_none_or(|n| n > options.limits.max_record_bytes)
                    {
                        return Err(PostGisError::ResourceLimit(
                            "encoded geometry exceeds record budget".into(),
                        ));
                    }
                    let encoded = ewkb::encode(&wkb, &schema.geometry)?;
                    row.extend_from_slice(encoded.as_bytes());
                }
            }
            row.push(b'\n');
            sender.send(row)?;
        }
        Ok(())
    })
}

fn field_type(kind: OGRFieldType::Type) -> Result<PostgresType> {
    match kind {
        OGRFieldType::OFTInteger => Ok(PostgresType::Integer),
        OGRFieldType::OFTInteger64 => Ok(PostgresType::BigInt),
        OGRFieldType::OFTReal => Ok(PostgresType::DoublePrecision),
        OGRFieldType::OFTString | OGRFieldType::OFTWideString => Ok(PostgresType::Text),
        OGRFieldType::OFTDate => Ok(PostgresType::Date),
        OGRFieldType::OFTIntegerList => Ok(PostgresType::IntegerArray),
        // OGR DateTime may have an unknown timezone. Explicit text contracts avoid guessing.
        other => Err(PostGisError::Unsupported(format!("OGR field type {other}"))),
    }
}

fn geometry_schema(kind: OGRwkbGeometryType::Type) -> Result<(GeometryKind, CoordinateDimension)> {
    let family = match geometry_type_flatten(kind) {
        1 => GeometryKind::Point,
        2 => GeometryKind::LineString,
        3 => GeometryKind::Polygon,
        4 => GeometryKind::MultiPoint,
        5 => GeometryKind::MultiLineString,
        6 => GeometryKind::MultiPolygon,
        _ => {
            return Err(PostGisError::Unsupported(
                "geometry family needs an explicit supported schema".into(),
            ));
        }
    };
    let dimension = match (geometry_type_has_z(kind), geometry_type_has_m(kind)) {
        (false, false) => CoordinateDimension::Xy,
        (true, false) => CoordinateDimension::Xyz,
        (false, true) => CoordinateDimension::Xym,
        (true, true) => CoordinateDimension::Xyzm,
    };
    Ok((family, dimension))
}

fn normalize_kind(kind: OGRwkbGeometryType::Type, shapefile: bool) -> OGRwkbGeometryType::Type {
    if !shapefile {
        return kind;
    }
    let flat = geometry_type_flatten(kind);
    let multi = match flat {
        2 => 5,
        3 => 6,
        _ => return kind,
    };
    geometry_type_set_modifier(multi, geometry_type_has_z(kind), geometry_type_has_m(kind))
}

fn read_value(
    feature: &Feature<'_>,
    index: usize,
    kind: OGRFieldType::Type,
) -> anyhow::Result<Option<FieldValue>> {
    anyhow::ensure!(
        index < feature.field_count(),
        "source field index is out of range"
    );
    let raw_index = i32::try_from(index)?;
    // SAFETY: index is in the schema and the live feature owns its handle.
    let handle = unsafe { feature.c_feature() };
    // SAFETY: same validated live handle/index; no mutation.
    if unsafe { gdal_sys::OGR_F_IsFieldSetAndNotNull(handle, raw_index) } == 0 {
        return Ok(None);
    }
    // Check unset as well as NULL before GDAL can turn missing values into defaults.
    field_type(kind)?;
    if kind != OGRFieldType::OFTIntegerList {
        return Ok(feature.field(index)?);
    }
    let mut count = 0;
    // SAFETY: GDAL initializes count and borrows storage from the live feature.
    let values = unsafe { gdal_sys::OGR_F_GetFieldAsIntegerList(handle, raw_index, &mut count) };
    anyhow::ensure!(
        count >= 0 && (count == 0 || !values.is_null()),
        "invalid integer list"
    );
    let values = if count == 0 {
        Vec::new()
    } else {
        // SAFETY: GDAL provides count non-null i32 elements, copied while feature lives.
        unsafe { std::slice::from_raw_parts(values, count as usize) }.to_vec()
    };
    Ok(Some(FieldValue::IntegerListValue(values)))
}

fn open_dataset(path: &Path, options: &[String]) -> anyhow::Result<Dataset> {
    let options = options.iter().map(String::as_str).collect::<Vec<_>>();
    let open = |path: &Path| {
        Dataset::open_ex(
            path,
            gdal::DatasetOptions {
                open_options: Some(&options),
                ..Default::default()
            },
        )
    };
    let error = match open(path) {
        Ok(dataset) => return Ok(dataset),
        Err(error) => error,
    };
    if !path.to_string_lossy().starts_with("/vsizip/") {
        return Err(error.into());
    }
    let entries = gdal::vsi::read_dir(path, true).map_err(|_| error)?;
    let mut files = entries.iter().filter(|p| {
        p.extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("shp"))
            && !p.components().any(|part| part.as_os_str() == "__MACOSX")
            && !p
                .file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with("._"))
    });
    let first = files
        .next()
        .ok_or_else(|| anyhow::anyhow!("ZIP contains no Shapefile"))?;
    anyhow::ensure!(
        files.next().is_none(),
        "ZIP must contain exactly one Shapefile"
    );
    Ok(open(&path.join(first))?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{postgis::test_support::points, vector::NativeTranslate};

    #[test]
    fn zip_files_and_archive_roots_open_without_treating_members_as_directories()
    -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let zip = directory.path().join("points.shp.zip");
        let _ = NativeTranslate::new(points(), &zip, ["-f", "ESRI Shapefile", "-nln", "points"])
            .run()?;
        let root = PathBuf::from(format!("/vsizip/{}", zip.display()));
        for path in [&root, &root.join("points.shp")] {
            assert_eq!(open_dataset(path, &[])?.layer(0)?.feature_count(), 3);
        }
        let nested = PathBuf::from(format!(
            "/vsizip/{}",
            directory.path().join("nested.zip").display()
        ));
        for entry in gdal::vsi::read_dir(&root, false)? {
            let from = std::ffi::CString::new(root.join(&entry).to_string_lossy().as_bytes())?;
            let to = std::ffi::CString::new(
                nested
                    .join("shapes")
                    .join(&entry)
                    .to_string_lossy()
                    .as_bytes(),
            )?;
            assert_eq!(
                // SAFETY: both CString paths remain valid throughout this synchronous VSI copy.
                unsafe { gdal_sys::CPLCopyFile(to.as_ptr(), from.as_ptr()) },
                0
            );
        }
        for path in [
            &nested,
            &nested.join("shapes"),
            &nested.join("shapes/points.shp"),
        ] {
            assert_eq!(open_dataset(path, &[])?.layer(0)?.feature_count(), 3);
        }
        let member = PathBuf::from(format!(
            "/vsizip/{}/nested/points.geojson",
            directory.path().join("points.zip").display()
        ));
        let _ = NativeTranslate::new(points(), &member, ["-f", "GeoJSON"]).run()?;
        assert_eq!(open_dataset(&member, &[])?.layer(0)?.feature_count(), 3);
        assert!(open_dataset(&member.with_file_name("absent.geojson"), &[]).is_err());
        Ok(())
    }
}
