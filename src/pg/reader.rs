use super::{Geometry, GeometryType, PgCopy, PgField, PgType, PgValue, Result, VectorIngestError};
use crate::gdal_ext::ffi::checked_gdal_call;
use chrono::Datelike;
use gdal::{
    Dataset,
    spatial_ref::{AxisMappingStrategy, CoordTransform, SpatialRef},
    vector::{
        Feature, FieldValue, Geometry as GdalGeometry, Layer, LayerAccess, OGRFieldType,
        OGRwkbGeometryType,
    },
};

impl PgCopy {
    pub(super) fn source_spatial_ref(layer: &Layer<'_>) -> Result<SpatialRef> {
        let spatial_ref = layer
            .spatial_ref()
            .ok_or(VectorIngestError::MissingSourceCrs)?;
        // Older GDAL GeoPackage drivers materialize srs_id 0/-1 as named
        // placeholders. The geographic placeholder can even transform as WGS84,
        // so reject it before treating the input coordinates as a known CRS.
        if spatial_ref.auth_name().is_none()
            && matches!(
                spatial_ref.name().as_deref(),
                Some("Undefined geographic SRS" | "Undefined Cartesian SRS")
            )
        {
            return Err(VectorIngestError::MissingSourceCrs.into());
        }
        Ok(spatial_ref)
    }

    pub(super) fn require_geometry(geometry: Option<&GdalGeometry>) -> Result<&GdalGeometry> {
        let geometry = geometry.ok_or(VectorIngestError::MissingGeometry)?;
        // gdal 0.19 Feature::geometry can return Some(Geometry) wrapping a null
        // OGR handle. Check it before any native geometry operation.
        // SAFETY: has_gdal_ptr checks that the wrapper is initialized; obtaining
        // the pointer does not dereference it, and the feature owns its lifetime.
        if !geometry.has_gdal_ptr() || unsafe { geometry.c_geometry() }.is_null() {
            return Err(VectorIngestError::MissingGeometry.into());
        }
        Ok(geometry)
    }

    pub(super) fn ensure_usable_geometry_count(count: u64) -> Result<()> {
        if count == 0 {
            return Err(VectorIngestError::NoUsableGeometry.into());
        }
        Ok(())
    }

    pub(super) fn coordinate_transform(
        mut source_srs: SpatialRef,
        target_srid: u32,
    ) -> Result<(CoordTransform, String)> {
        if !matches!(target_srid, 3857 | 4326) {
            return Err(VectorIngestError::UnsupportedTargetSrid(target_srid).into());
        }

        let mut target_srs = SpatialRef::from_epsg(target_srid)
            .map_err(|err| VectorIngestError::CoordinateTransform(err.to_string()))?;
        source_srs.set_axis_mapping_strategy(AxisMappingStrategy::TraditionalGisOrder);
        target_srs.set_axis_mapping_strategy(AxisMappingStrategy::TraditionalGisOrder);
        let transform = CoordTransform::new(&source_srs, &target_srs)
            .map_err(|err| VectorIngestError::CoordinateTransform(err.to_string()))?;
        let source_crs = source_srs
            .auth_name()
            .and_then(|name| {
                source_srs
                    .auth_code()
                    .ok()
                    .map(|code| format!("{name}:{code}"))
            })
            .unwrap_or_else(|| "custom".to_string());

        Ok((transform, source_crs))
    }

    #[cfg(test)]
    pub(super) fn transform_geometry(
        geometry: &GdalGeometry,
        source_srs: SpatialRef,
        target_srid: u32,
    ) -> Result<GdalGeometry> {
        let (transform, _) = Self::coordinate_transform(source_srs, target_srid)?;
        geometry
            .transform(&transform)
            .map_err(|err| VectorIngestError::GeometryTransform(err.to_string()).into())
    }

    pub(super) fn wkb_to_ewkb(wkb: &[u8], srid: u32) -> Vec<u8> {
        if wkb.len() < 5 {
            return wkb.to_vec();
        }
        let is_little_endian = wkb[0] == 1;
        let mut type_bytes = [0u8; 4];
        type_bytes.copy_from_slice(&wkb[1..5]);
        let mut wkb_type = if is_little_endian {
            u32::from_le_bytes(type_bytes)
        } else {
            u32::from_be_bytes(type_bytes)
        };
        wkb_type |= 0x20000000;
        let mut ewkb = Vec::with_capacity(wkb.len() + 4);
        ewkb.push(wkb[0]);
        if is_little_endian {
            ewkb.extend_from_slice(&wkb_type.to_le_bytes());
            ewkb.extend_from_slice(&srid.to_le_bytes());
        } else {
            ewkb.extend_from_slice(&wkb_type.to_be_bytes());
            ewkb.extend_from_slice(&srid.to_be_bytes());
        }
        ewkb.extend_from_slice(&wkb[5..]);
        ewkb
    }

    pub fn read_fields(
        dataset: &Dataset,
        layer_idx: Option<usize>,
        lowercase: bool,
        target_srid: u32,
    ) -> Result<Vec<PgField>> {
        let idx = layer_idx.unwrap_or(0);
        // Lazy drivers can return a layer wrapper even when initializing its
        // source failed. Catch that error before later metadata calls obscure it.
        let layer = checked_gdal_call(|| dataset.layer(idx))??;
        let declared = layer.defn().geometry_type();
        let geom_type =
            Self::ogr_geometry_to_pg(if dataset.driver().short_name() == "ESRI Shapefile" {
                Self::shapefile_geometry_type(declared)
            } else {
                declared
            })?;
        let defns = layer.defn();
        let mut fields: Vec<PgField> = vec![];
        for field_defn in defns.fields() {
            let field_name = if lowercase {
                field_defn.name().to_lowercase()
            } else {
                field_defn.name()
            };
            if field_name.is_empty() {
                return Err(VectorIngestError::InvalidFieldName.into());
            }
            let field_type = Self::ogr_field_type_to_pg(field_defn.field_type())?;
            fields.push(PgField {
                field_name,
                field_type,
            });
        }
        fields.push(PgField {
            field_name: "geom".to_string(),
            field_type: PgType::Geometry(Geometry {
                srid: target_srid,
                geometry: geom_type,
            }),
        });
        Ok(fields)
    }

    pub(super) fn shapefile_geometry_type(
        kind: OGRwkbGeometryType::Type,
    ) -> OGRwkbGeometryType::Type {
        use gdal::vector::{
            geometry_type_flatten, geometry_type_has_m, geometry_type_has_z,
            geometry_type_set_modifier,
        };
        let multi = match geometry_type_flatten(kind) {
            OGRwkbGeometryType::wkbPolygon => OGRwkbGeometryType::wkbMultiPolygon,
            OGRwkbGeometryType::wkbLineString => OGRwkbGeometryType::wkbMultiLineString,
            _ => return kind,
        };
        geometry_type_set_modifier(multi, geometry_type_has_z(kind), geometry_type_has_m(kind))
    }

    pub(super) fn ogr_geometry_to_pg(ogr_type: OGRwkbGeometryType::Type) -> Result<GeometryType> {
        match ogr_type {
            OGRwkbGeometryType::wkbPoint => Ok(GeometryType::Point),
            OGRwkbGeometryType::wkbLineString => Ok(GeometryType::LineString),
            OGRwkbGeometryType::wkbPolygon => Ok(GeometryType::Polygon),
            OGRwkbGeometryType::wkbMultiPoint => Ok(GeometryType::MultiPoint),
            OGRwkbGeometryType::wkbMultiLineString => Ok(GeometryType::MultiLineString),
            OGRwkbGeometryType::wkbMultiPolygon => Ok(GeometryType::MultiPolygon),
            OGRwkbGeometryType::wkbPoint25D => Ok(GeometryType::PointZ),
            OGRwkbGeometryType::wkbLineString25D => Ok(GeometryType::LineStringZ),
            OGRwkbGeometryType::wkbPolygon25D => Ok(GeometryType::PolygonZ),
            OGRwkbGeometryType::wkbMultiPoint25D => Ok(GeometryType::MultiPointZ),
            OGRwkbGeometryType::wkbMultiLineString25D => Ok(GeometryType::MultiLineStringZ),
            OGRwkbGeometryType::wkbMultiPolygon25D => Ok(GeometryType::MultiPolygonZ),
            OGRwkbGeometryType::wkbPointZM => Ok(GeometryType::PointZM),
            OGRwkbGeometryType::wkbLineStringZM => Ok(GeometryType::LineStringZM),
            OGRwkbGeometryType::wkbPolygonZM => Ok(GeometryType::PolygonZM),
            OGRwkbGeometryType::wkbMultiPointZM => Ok(GeometryType::MultiPointZM),
            OGRwkbGeometryType::wkbMultiLineStringZM => Ok(GeometryType::MultiLineStringZM),
            OGRwkbGeometryType::wkbMultiPolygonZM => Ok(GeometryType::MultiPolygonZM),
            _ => {
                let kind = match ogr_type {
                    OGRwkbGeometryType::wkbNone => "wkbNone".to_string(),
                    OGRwkbGeometryType::wkbUnknown => "wkbUnknown".to_string(),
                    OGRwkbGeometryType::wkbGeometryCollection => {
                        "wkbGeometryCollection".to_string()
                    }
                    _ => ogr_type.to_string(),
                };
                Err(VectorIngestError::UnsupportedGeometryType(kind).into())
            }
        }
    }

    pub(super) fn ogr_field_type_to_pg(field_type: OGRFieldType::Type) -> Result<PgType> {
        match field_type {
            OGRFieldType::OFTInteger => Ok(PgType::Integer),
            OGRFieldType::OFTIntegerList => Ok(PgType::IntegerArray),
            OGRFieldType::OFTInteger64 => Ok(PgType::Bigint),
            OGRFieldType::OFTReal => Ok(PgType::DoublePrecision),
            OGRFieldType::OFTString | OGRFieldType::OFTWideString => Ok(PgType::Text),
            OGRFieldType::OFTDate => Ok(PgType::Date),
            _ => Err(VectorIngestError::UnsupportedFieldType(field_type).into()),
        }
    }

    pub(super) fn field_value_to_pg_value(
        value: Option<FieldValue>,
        field_type: &PgType,
    ) -> PgValue {
        match value {
            Some(FieldValue::IntegerValue(v)) => PgValue::Integer(v),
            Some(FieldValue::IntegerListValue(v)) => PgValue::IntegerArray(v),
            Some(FieldValue::Integer64Value(v)) => PgValue::Bigint(v),
            Some(FieldValue::RealValue(v)) => PgValue::DoublePrecision(v),
            Some(FieldValue::StringValue(v)) => PgValue::Text(v),
            Some(FieldValue::DateValue(v)) => {
                chrono::NaiveDate::from_ymd_opt(v.year(), v.month(), v.day())
                    .map(PgValue::Date)
                    .unwrap_or(PgValue::Null)
            }
            None => Self::null_value_for_type(field_type),
            _ => Self::null_value_for_type(field_type),
        }
    }

    pub(super) fn read_field_value(
        feature: &Feature<'_>,
        index: usize,
        field_type: &PgType,
    ) -> Result<PgValue> {
        if *field_type != PgType::IntegerArray {
            return Ok(Self::field_value_to_pg_value(
                feature.field(index)?,
                field_type,
            ));
        }
        anyhow::ensure!(index < feature.field_count(), "field index out of range");
        let index = i32::try_from(index)?;
        // gdal 0.19 calls from_raw_parts even when an empty list has a null
        // pointer. Read integer lists directly so [] stays distinct from NULL
        // without constructing an invalid Rust slice.
        // SAFETY: the live feature owns the handle and index is in its schema.
        let handle = unsafe { feature.c_feature() };
        // SAFETY: handle and index were validated above; this does not mutate them.
        if unsafe { gdal_sys::OGR_F_IsFieldSetAndNotNull(handle, index) } == 0 {
            return Ok(PgValue::Null);
        }
        let mut count = 0;
        // SAFETY: GDAL borrows the list from this live feature and initializes count.
        let values = unsafe { gdal_sys::OGR_F_GetFieldAsIntegerList(handle, index, &mut count) };
        anyhow::ensure!(count >= 0, "negative integer list length");
        let values = if count == 0 {
            Vec::new()
        } else {
            anyhow::ensure!(!values.is_null(), "missing integer list values");
            // SAFETY: GDAL provides count i32 elements owned by the borrowed
            // feature. The non-null, non-empty slice is copied before it changes.
            unsafe { std::slice::from_raw_parts(values, count as usize) }.to_vec()
        };
        Ok(PgValue::IntegerArray(values))
    }

    pub(super) fn null_value_for_type(_field_type: &PgType) -> PgValue {
        PgValue::Null
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gdal::{
        DriverManager,
        vector::{FieldDefn, LayerOptions},
    };

    const UNDEFINED_GEOGRAPHIC: &str = r#"GEOGCS["Undefined geographic SRS",DATUM["unknown",SPHEROID["unknown",6378137,298.257223563]],PRIMEM["Greenwich",0],UNIT["degree",0.0174532925199433]]"#;
    const UNDEFINED_CARTESIAN: &str = r#"LOCAL_CS["Undefined Cartesian SRS",UNIT["metre",1]]"#;

    fn read_memory_spatial_ref(srs: Option<&SpatialRef>) -> Result<SpatialRef> {
        let driver = DriverManager::get_driver_by_name("Memory")?;
        let mut dataset = driver.create_vector_only("")?;
        let layer = dataset.create_layer(LayerOptions {
            srs,
            ty: OGRwkbGeometryType::wkbPoint,
            ..Default::default()
        })?;
        PgCopy::source_spatial_ref(&layer)
    }

    #[test]
    fn absent_and_undefined_source_crs_are_typed_failures() -> Result<()> {
        let geographic = SpatialRef::from_wkt(UNDEFINED_GEOGRAPHIC)?;
        let cartesian = SpatialRef::from_wkt(UNDEFINED_CARTESIAN)?;
        for srs in [None, Some(&geographic), Some(&cartesian)] {
            let error = read_memory_spatial_ref(srs).expect_err("unknown CRS must fail");
            assert!(matches!(
                error.downcast_ref::<VectorIngestError>(),
                Some(VectorIngestError::MissingSourceCrs)
            ));
        }
        Ok(())
    }

    #[test]
    fn valid_custom_and_authority_source_crs_remain_accepted() -> Result<()> {
        let custom = SpatialRef::from_wkt(
            &UNDEFINED_GEOGRAPHIC.replace("Undefined geographic SRS", "Local survey geographic"),
        )?;
        assert!(custom.auth_name().is_none());
        let actual = read_memory_spatial_ref(Some(&custom))?;
        assert_eq!(actual.name().as_deref(), Some("Local survey geographic"));
        let epsg = SpatialRef::from_epsg(4326)?;
        assert_eq!(read_memory_spatial_ref(Some(&epsg))?.auth_code()?, 4326);
        Ok(())
    }

    #[test]
    fn empty_source_field_is_a_typed_input_failure() -> Result<()> {
        let driver = DriverManager::get_driver_by_name("Memory")?;
        let mut dataset = driver.create_vector_only("")?;
        let layer = dataset.create_layer(LayerOptions {
            ty: OGRwkbGeometryType::wkbPoint,
            ..Default::default()
        })?;
        FieldDefn::new("", OGRFieldType::OFTString)?.add_to_layer(&layer)?;
        let error =
            PgCopy::read_fields(&dataset, Some(0), false, 4326).expect_err("empty field rejected");
        assert!(matches!(
            error.downcast_ref::<VectorIngestError>(),
            Some(VectorIngestError::InvalidFieldName)
        ));
        Ok(())
    }

    #[test]
    fn integer_lists_preserve_paths_empty_and_null_values() -> Result<()> {
        let driver = DriverManager::get_driver_by_name("Memory")?;
        let mut dataset = driver.create_vector_only("")?;
        {
            let layer = dataset.create_layer(LayerOptions {
                ty: OGRwkbGeometryType::wkbPoint,
                ..Default::default()
            })?;
            FieldDefn::new("gis_part_path", OGRFieldType::OFTIntegerList)?.add_to_layer(&layer)?;
        }
        let fields = PgCopy::read_fields(&dataset, None, true, 3857)?;
        assert_eq!(fields[0].field_type, PgType::IntegerArray);
        let layer = dataset.layer(0)?;
        let mut feature = Feature::new(layer.defn())?;
        for path in [vec![1], vec![2, 1, 3], vec![], vec![i32::MIN, i32::MAX]] {
            feature.set_field_integer_list(0, &path)?;
            assert!(matches!(
                PgCopy::read_field_value(&feature, 0, &PgType::IntegerArray)?,
                PgValue::IntegerArray(actual) if actual == path
            ));
            assert!(matches!(
                PgCopy::field_value_to_pg_value(Some(FieldValue::IntegerListValue(path.clone())), &PgType::IntegerArray),
                PgValue::IntegerArray(actual) if actual == path
            ));
        }
        feature.set_field_null(0)?;
        assert!(matches!(
            PgCopy::read_field_value(&feature, 0, &PgType::IntegerArray)?,
            PgValue::Null
        ));
        feature.unset_field(0)?;
        assert!(matches!(
            PgCopy::read_field_value(&feature, 0, &PgType::IntegerArray)?,
            PgValue::Null
        ));
        assert!(matches!(
            PgCopy::field_value_to_pg_value(None, &PgType::IntegerArray),
            PgValue::Null
        ));
        assert!(PgCopy::read_field_value(&feature, 1, &PgType::IntegerArray).is_err());
        Ok(())
    }

    #[test]
    fn other_list_types_remain_explicit_input_failures() {
        for kind in [
            OGRFieldType::OFTRealList,
            OGRFieldType::OFTStringList,
            OGRFieldType::OFTInteger64List,
        ] {
            let error = PgCopy::ogr_field_type_to_pg(kind).unwrap_err();
            assert!(
                matches!(error.downcast_ref::<VectorIngestError>(), Some(VectorIngestError::UnsupportedFieldType(actual)) if *actual == kind)
            );
        }
    }
}
