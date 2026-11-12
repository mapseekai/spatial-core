mod empty_vector;
pub use empty_vector::{VectorCopyContract, copy_empty_vector, restore_vector_copy_fields};
mod raster;
mod reader;
mod vector;

use crate::control::Cancellation;
use anyhow::Result;
use anyhow::anyhow;
use bytes::BytesMut;
use deadpool_postgres::{GenericClient, Object, Pool, Transaction};

use postgres_types::ToSql;
use tokio_postgres::types::{IsNull, Type, to_sql_checked};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PgType {
    Integer,
    IntegerArray,
    Bigint,
    DoublePrecision,
    Text,
    Varchar,
    Date,
    Geometry(Geometry),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Geometry {
    srid: u32,
    geometry: GeometryType,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GeometryType {
    Point,
    LineString,
    Polygon,
    MultiPoint,
    MultiLineString,
    MultiPolygon,
    PointZ,
    LineStringZ,
    PolygonZ,
    MultiPointZ,
    MultiLineStringZ,
    MultiPolygonZ,
    PointZM,
    LineStringZM,
    PolygonZM,
    MultiPointZM,
    MultiLineStringZM,
    MultiPolygonZM,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PgField {
    field_name: String,
    field_type: PgType,
}

#[derive(Debug, Clone)]
pub enum PgValue {
    Integer(i32),
    IntegerArray(Vec<i32>),
    Bigint(i64),
    DoublePrecision(f64),
    Text(String),
    Date(chrono::NaiveDate),
    Geometry(Vec<u8>),
    Null,
}

#[derive(Debug, Clone)]
pub struct RasterCopyRow {
    pub rast_hex: String,
}

#[derive(Debug, Clone, Copy)]
pub struct RasterCopyBatchStats {
    pub committed_rows: usize,
    pub committed_batches: usize,
}

impl ToSql for PgValue {
    fn to_sql(
        &self,
        ty: &Type,
        out: &mut BytesMut,
    ) -> std::result::Result<IsNull, Box<dyn std::error::Error + Sync + Send>> {
        match self {
            PgValue::Integer(v) => v.to_sql(ty, out),
            PgValue::IntegerArray(v) => v.to_sql_checked(ty, out),
            PgValue::Bigint(v) => v.to_sql(ty, out),
            PgValue::DoublePrecision(v) => v.to_sql(ty, out),
            PgValue::Text(v) => v.to_sql(ty, out),
            PgValue::Date(v) => v.to_sql(ty, out),
            PgValue::Geometry(v) => v.to_sql(ty, out),
            PgValue::Null => Ok(IsNull::Yes),
        }
    }

    fn accepts(ty: &Type) -> bool {
        matches!(
            *ty,
            Type::INT4
                | Type::INT4_ARRAY
                | Type::INT8
                | Type::FLOAT8
                | Type::TEXT
                | Type::VARCHAR
                | Type::DATE
                | Type::BYTEA
        )
    }

    to_sql_checked!();
}

pub struct PgCopy;

#[derive(Debug, thiserror::Error)]
pub enum VectorIngestError {
    #[error("source vector field name is empty")]
    InvalidFieldName,
    #[error("source vector layer has unsupported field type: {0}")]
    UnsupportedFieldType(u32),
    #[error("source vector layer has no usable CRS")]
    MissingSourceCrs,
    #[error("unsupported target SRID: {0}")]
    UnsupportedTargetSrid(u32),
    #[error("cannot construct coordinate transformation: {0}")]
    CoordinateTransform(String),
    #[error("cannot reproject geometry: {0}")]
    GeometryTransform(String),
    #[error("source feature has no geometry")]
    MissingGeometry,
    #[error("source layer has unsupported geometry type: {0}")]
    UnsupportedGeometryType(String),
    #[error("source vector layer has no non-empty geometry")]
    NoUsableGeometry,
}

pub(super) fn should_report_vector_progress(
    written: u64,
    total: Option<u64>,
    usable_geometry_count: u64,
) -> bool {
    total != Some(written) || usable_geometry_count > 0
}

pub fn quote_ident(ident: &str) -> String {
    format!("\"{}\"", ident.replace('"', "\"\""))
}

pub(crate) async fn acquire_connection(pool: &Pool, cancel: &Cancellation) -> Result<Object> {
    cancel.check()?;
    tokio::select! {
        biased;
        _ = cancel.cancelled() => Err(crate::control::Cancelled.into()),
        result = pool.get() => result.map_err(|error| anyhow!("failed to acquire database connection: {error}")),
    }
}

/// Stop an in-flight query before rolling back, then let the caller join its
/// reader. Dropping a SQL future alone leaves server-side work running.
/// Keep COMMIT outside this select; after dropping transaction guards, callers
/// must await finish_database_result to synchronize the connection on errors.
pub(crate) async fn run_with_cancellation<T>(
    cancel: &Cancellation,
    query: tokio_postgres::CancelToken,
    operation: impl std::future::Future<Output = Result<T>>,
) -> Result<T> {
    cancel.check()?;
    tokio::select! {
        biased;
        _ = cancel.cancelled() => {
            // Cancellation is best effort if the server has already closed the
            // connection; the typed cancellation remains the activity outcome.
            if let Err(err) = query.cancel_query(tokio_postgres::NoTls).await {
                tracing::warn!(%err, "failed to cancel PostgreSQL query");
            }
            cancel.check()?;
            unreachable!("cancelled waiter returned without cancellation")
        }
        result = operation => result,
    }
}

/// Transaction::drop queues ROLLBACK but does not await it. An ordered
/// round trip on the same connection waits for COPY termination, ROLLBACK and
/// ReadyForQuery before the connection or activity permit can be released.
pub(crate) async fn finish_database_result<T>(client: &Object, result: Result<T>) -> Result<T> {
    if result.is_err()
        && let Err(cleanup) = client.batch_execute("SELECT 1").await
    {
        return result
            .map_err(|error| error.context(format!("database cleanup failed: {cleanup}")));
    }
    result
}

impl PgCopy {
    pub async fn create_table(
        client: &impl GenericClient,
        schema: &str,
        table: &str,
        fields: &[PgField],
        gid_column: Option<&str>,
    ) -> Result<()> {
        let gid_col = gid_column.unwrap_or("_id");
        let mut field_definitions: Vec<String> = vec![format!(
            "{} BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY",
            quote_ident(gid_col)
        )];
        field_definitions.extend(fields.iter().map(|field| {
            let type_str = Self::pg_type_to_sql(&field.field_type);
            format!("{} {}", quote_ident(&field.field_name), type_str)
        }));

        let sql = format!(
            "CREATE TABLE IF NOT EXISTS {}.{} ({})",
            quote_ident(schema),
            quote_ident(table),
            field_definitions.join(", ")
        );

        client
            .execute(&sql, &[])
            .await
            .map_err(|e| anyhow!("Failed to create table {}.{}: {:?}", schema, table, e))?;
        Ok(())
    }

    pub async fn delete_table(client: &Object, schema: &str, table: &str) -> Result<()> {
        let sql = format!(
            "DROP TABLE IF EXISTS {}.{}",
            quote_ident(schema),
            quote_ident(table)
        );
        client.execute(&sql, &[]).await?;
        Ok(())
    }

    pub async fn create_geom_index(
        client: &Transaction<'_>,
        schema: &str,
        table: &str,
        geometry_column: Option<&str>,
    ) -> Result<()> {
        let geom_column = geometry_column.unwrap_or("geom");
        let index_name = format!("{}_{}_geom_idx", table, geom_column);
        let sql = format!(
            "CREATE INDEX IF NOT EXISTS {} ON {}.{} USING GIST({})",
            quote_ident(&index_name),
            quote_ident(schema),
            quote_ident(table),
            quote_ident(geom_column)
        );
        client.execute(&sql, &[]).await?;
        Ok(())
    }

    pub(super) async fn create_cursor_index(
        client: &Transaction<'_>,
        schema: &str,
        table: &str,
        gid_column: Option<&str>,
    ) -> Result<()> {
        // GeoLedger pages by lexical IDs; a bigint primary key cannot serve this ordering.
        client
            .batch_execute(&format!(
                "CREATE INDEX ON {}.{} (({}::text COLLATE \"C\"))",
                quote_ident(schema),
                quote_ident(table),
                quote_ident(gid_column.unwrap_or("_id")),
            ))
            .await?;
        Ok(())
    }

    pub async fn create_gid(
        client: &Object,
        schema: &str,
        table: &str,
        gid_column: Option<&str>,
    ) -> Result<()> {
        let gid_col = gid_column.unwrap_or("_id");
        let check_sql = r#"
            SELECT EXISTS (
                SELECT 1
                FROM information_schema.columns
                WHERE table_schema = $1
                AND table_name = $2
                AND column_name = $3
            )"#;
        let exists: bool = client
            .query_one(check_sql, &[&schema, &table, &gid_col])
            .await?
            .get(0);
        if !exists {
            let add_column_sql = format!(
                "ALTER TABLE {}.{} ADD COLUMN {} BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY",
                quote_ident(schema),
                quote_ident(table),
                quote_ident(gid_col)
            );
            client.execute(&add_column_sql, &[]).await?;
        }
        Ok(())
    }

    fn pg_type_to_sql(pg_type: &PgType) -> String {
        match pg_type {
            PgType::Integer => "INTEGER".to_string(),
            PgType::IntegerArray => "INTEGER[]".to_string(),
            PgType::Bigint => "BIGINT".to_string(),
            PgType::DoublePrecision => "DOUBLE PRECISION".to_string(),
            PgType::Text => "TEXT".to_string(),
            PgType::Varchar => "VARCHAR".to_string(),
            PgType::Date => "DATE".to_string(),
            PgType::Geometry(geom) => {
                let geom_type = match geom.geometry {
                    GeometryType::Point => "POINT",
                    GeometryType::LineString => "LINESTRING",
                    GeometryType::Polygon => "POLYGON",
                    GeometryType::MultiPoint => "MULTIPOINT",
                    GeometryType::MultiLineString => "MULTILINESTRING",
                    GeometryType::MultiPolygon => "MULTIPOLYGON",
                    GeometryType::PointZ => "POINTZ",
                    GeometryType::LineStringZ => "LINESTRINGZ",
                    GeometryType::PolygonZ => "POLYGONZ",
                    GeometryType::MultiPointZ => "MULTIPOINTZ",
                    GeometryType::MultiLineStringZ => "MULTILINESTRINGZ",
                    GeometryType::MultiPolygonZ => "MULTIPOLYGONZ",
                    GeometryType::PointZM => "POINTZM",
                    GeometryType::LineStringZM => "LINESTRINGZM",
                    GeometryType::PolygonZM => "POLYGONZM",
                    GeometryType::MultiPointZM => "MULTIPOINTZM",
                    GeometryType::MultiLineStringZM => "MULTILINESTRINGZM",
                    GeometryType::MultiPolygonZM => "MULTIPOLYGONZM",
                };
                format!("GEOMETRY({},{})", geom_type, geom.srid)
            }
        }
    }

    fn pg_type_to_postgres_type(pg_type: &PgType) -> Type {
        match pg_type {
            PgType::Integer => Type::INT4,
            PgType::IntegerArray => Type::INT4_ARRAY,
            PgType::Bigint => Type::INT8,
            PgType::DoublePrecision => Type::FLOAT8,
            PgType::Text | PgType::Varchar => Type::TEXT,
            PgType::Date => Type::DATE,
            PgType::Geometry(_) => Type::BYTEA,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{PgCopy, PgType, PgValue, VectorIngestError, should_report_vector_progress};
    use anyhow::Result;
    use bytes::BytesMut;
    use gdal::spatial_ref::SpatialRef;
    use gdal::vector::{
        Feature as GdalFeature, Geometry as GdalGeometry, LayerAccess, LayerOptions,
    };
    use gdal::{Dataset, DriverManager};
    use gdal_sys::OGRwkbGeometryType::{wkbGeometryCollection, wkbNone, wkbPoint, wkbUnknown};
    use std::path::PathBuf;
    use tokio_postgres::types::{FromSql, IsNull, ToSql, Type};
    use uuid::Uuid;

    #[test]
    fn integer_array_schema_and_binary_values_preserve_empty_and_null() -> Result<()> {
        assert_eq!(PgCopy::pg_type_to_sql(&PgType::IntegerArray), "INTEGER[]");
        assert_eq!(
            PgCopy::pg_type_to_postgres_type(&PgType::IntegerArray),
            Type::INT4_ARRAY
        );
        assert!(<PgValue as ToSql>::accepts(&Type::INT4_ARRAY));
        assert!(
            PgValue::IntegerArray(vec![1])
                .to_sql_checked(&Type::INT4, &mut BytesMut::new())
                .is_err()
        );
        for path in [vec![1], vec![2, 1, 3], vec![], vec![i32::MIN, i32::MAX]] {
            let mut bytes = BytesMut::new();
            assert!(matches!(
                PgValue::IntegerArray(path.clone())
                    .to_sql_checked(&Type::INT4_ARRAY, &mut bytes)
                    .map_err(|e| anyhow::anyhow!(e))?,
                IsNull::No
            ));
            assert_eq!(
                Vec::<i32>::from_sql(&Type::INT4_ARRAY, &bytes).map_err(|e| anyhow::anyhow!(e))?,
                path
            );
        }
        let mut bytes = BytesMut::new();
        assert!(matches!(
            PgValue::Null
                .to_sql_checked(&Type::INT4_ARRAY, &mut bytes)
                .map_err(|e| anyhow::anyhow!(e))?,
            IsNull::Yes
        ));
        assert!(bytes.is_empty());
        Ok(())
    }

    #[tokio::test]
    #[ignore = "requires SPATIAL_CORE_TEST_POSTGRES_DSN with PostGIS"]
    async fn copy_in_preserves_integer_arrays_metadata_json_and_retry() -> Result<()> {
        use deadpool_postgres::{Manager, Pool};
        use tokio_postgres::NoTls;
        let dsn = std::env::var("SPATIAL_CORE_TEST_POSTGRES_DSN")?;
        let pool = Pool::builder(Manager::new(dsn.parse()?, NoTls))
            .max_size(2)
            .build()?;
        let mut client = pool.get().await?;
        let schema = format!("integer_array_test_{}", Uuid::new_v4().simple());
        let file = tempfile::NamedTempFile::with_suffix(".geojson")?;
        std::fs::write(
            file.path(),
            r#"{"type":"FeatureCollection","features":[
            {"type":"Feature","properties":{"gis_part_path":[2,1,3]},"geometry":{"type":"Point","coordinates":[12,55]}},
            {"type":"Feature","properties":{"gis_part_path":[]},"geometry":{"type":"Point","coordinates":[13,56]}},
            {"type":"Feature","properties":{"gis_part_path":null},"geometry":{"type":"Point","coordinates":[14,57]}},
            {"type":"Feature","properties":{},"geometry":{"type":"Point","coordinates":[15,58]}}
        ]}"#,
        )?;
        let result: Result<()> = async {
            for _ in 0..2 {
                PgCopy::copy_in(&mut client, &schema, "features", file.path(), 3857).await?;
                let rows = client.query(&format!("SELECT gis_part_path, pg_typeof(gis_part_path)::text, to_jsonb(gis_part_path), ST_SRID(geom) FROM {}.features ORDER BY _id", super::quote_ident(&schema)), &[]).await?;
                assert_eq!(rows.len(), 4, "retry must replace instead of append");
                for (row, expected) in rows.iter().zip([Some(vec![2, 1, 3]), Some(vec![]), None, None]) {
                    assert_eq!(row.get::<_, Option<Vec<i32>>>(0), expected);
                    assert_eq!(row.get::<_, String>(1), "integer[]");
                    assert_eq!(row.get::<_, Option<serde_json::Value>>(2), expected.map(|v| serde_json::json!(v)));
                    assert_eq!(row.get::<_, i32>(3), 3857);
                }
                let metadata = crate::vector::metadata::compute_vector_metadata(&pool, &schema, "features", &crate::control::Cancellation::default()).await?;
                assert_eq!(metadata.feature_count, 4);
                assert_eq!(metadata.attributes_schema["gis_part_path"], "integer[]");
            }
            Ok(())
        }.await;
        client
            .batch_execute(&format!(
                "DROP SCHEMA IF EXISTS {} CASCADE",
                super::quote_ident(&schema)
            ))
            .await?;
        result
    }

    #[test]
    fn reprojects_wgs84_point_to_web_mercator() -> Result<()> {
        let source = SpatialRef::from_epsg(4326)?;
        let geometry = GdalGeometry::from_wkt("POINT (180 0)")?;

        let transformed = PgCopy::transform_geometry(&geometry, source, 3857)?;
        let (x, y, _) = transformed.get_point(0);

        assert!((x - 20_037_508.342_789_244).abs() < 0.01);
        assert!(y.abs() < 0.01);
        Ok(())
    }

    #[test]
    fn reprojects_web_mercator_point_to_wgs84() -> Result<()> {
        let source = SpatialRef::from_epsg(3857)?;
        let geometry = GdalGeometry::from_wkt("POINT (20037508.342789244 0)")?;

        let transformed = PgCopy::transform_geometry(&geometry, source, 4326)?;
        let (x, y, _) = transformed.get_point(0);

        assert!((x - 180.0).abs() < 1e-7);
        assert!(y.abs() < 1e-7);
        Ok(())
    }

    #[test]
    fn same_crs_transform_preserves_coordinates() -> Result<()> {
        let source = SpatialRef::from_epsg(4326)?;
        let geometry = GdalGeometry::from_wkt("POINT (120.5 30.25)")?;

        let transformed = PgCopy::transform_geometry(&geometry, source, 4326)?;
        let (x, y, _) = transformed.get_point(0);

        assert!((x - 120.5).abs() < 1e-10);
        assert!((y - 30.25).abs() < 1e-10);
        Ok(())
    }

    #[test]
    fn rejects_unsupported_target_srid() -> Result<()> {
        let source = SpatialRef::from_epsg(4326)?;
        let geometry = GdalGeometry::from_wkt("POINT (120 30)")?;

        let err = PgCopy::transform_geometry(&geometry, source, 4490)
            .expect_err("unsupported target must fail");

        assert!(matches!(
            err.downcast_ref::<VectorIngestError>(),
            Some(VectorIngestError::UnsupportedTargetSrid(4490))
        ));
        Ok(())
    }

    #[test]
    fn missing_source_crs_is_rejected() -> Result<()> {
        let driver = DriverManager::get_driver_by_name("GPKG")?;
        let path = format!("/vsimem/missing-crs-{}.gpkg", Uuid::new_v4());
        let mut dataset = driver.create_vector_only(&path)?;
        dataset.create_layer(LayerOptions {
            name: "features",
            srs: None,
            ty: wkbPoint,
            ..Default::default()
        })?;
        drop(dataset);
        let dataset = Dataset::open(&path)?;
        let layer = dataset.layer(0)?;

        assert!(matches!(
            PgCopy::source_spatial_ref(&layer),
            Err(err) if err.downcast_ref::<VectorIngestError>()
                .is_some_and(|err| matches!(err, VectorIngestError::MissingSourceCrs))
        ));
        Ok(())
    }

    #[test]
    fn read_fields_uses_target_srid_instead_of_source_srid() -> Result<()> {
        let driver = DriverManager::get_driver_by_name("GPKG")?;
        let path = format!("/vsimem/target-fields-{}.gpkg", Uuid::new_v4());
        let mut dataset = driver.create_vector_only(&path)?;
        let source = SpatialRef::from_epsg(4326)?;
        dataset.create_layer(LayerOptions {
            name: "features",
            srs: Some(&source),
            ty: wkbPoint,
            ..Default::default()
        })?;

        let fields = PgCopy::read_fields(&dataset, Some(0), true, 3857)?;
        let geometry = fields
            .iter()
            .find(|field| field.field_name == "geom")
            .expect("geometry field");

        assert!(matches!(
            geometry.field_type,
            PgType::Geometry(super::Geometry { srid: 3857, .. })
        ));
        Ok(())
    }

    #[test]
    fn missing_feature_geometry_is_typed() {
        let err = PgCopy::require_geometry(None::<&GdalGeometry>)
            .expect_err("missing geometry must fail");

        assert!(matches!(
            err.downcast_ref::<VectorIngestError>(),
            Some(VectorIngestError::MissingGeometry)
        ));
    }

    #[test]
    fn missing_native_geometry_handle_is_typed_before_ffi() {
        // SAFETY: this non-owning shell is not used by any native operation.
        let geometry = unsafe { GdalGeometry::lazy_feature_geometry() };
        assert!(
            PgCopy::require_geometry(Some(&geometry))
                .unwrap_err()
                .is::<VectorIngestError>()
        );
        // SAFETY: initialize the same non-owning shell to model GDAL's null
        // feature handle; require_geometry must reject it without dereferencing.
        unsafe {
            geometry.set_c_geometry(std::ptr::null_mut());
        }
        let error = PgCopy::require_geometry(Some(&geometry)).unwrap_err();
        assert!(matches!(
            error.downcast_ref::<VectorIngestError>(),
            Some(VectorIngestError::MissingGeometry)
        ));
    }

    #[test]
    fn read_fields_rejects_layer_without_spatial_geometry() -> Result<()> {
        let driver = DriverManager::get_driver_by_name("GPKG")?;
        let path = format!("/vsimem/non-spatial-{}.gpkg", Uuid::new_v4());
        let mut dataset = driver.create_vector_only(&path)?;
        let source = SpatialRef::from_epsg(4326)?;
        dataset.create_layer(LayerOptions {
            name: "attributes",
            srs: Some(&source),
            ty: wkbNone,
            ..Default::default()
        })?;

        let err = PgCopy::read_fields(&dataset, Some(0), true, 3857)
            .expect_err("non-spatial layer must fail");

        assert!(matches!(
            err.downcast_ref::<VectorIngestError>(),
            Some(VectorIngestError::UnsupportedGeometryType(kind)) if kind == "wkbNone"
        ));
        Ok(())
    }

    #[test]
    fn rejects_unknown_and_collection_geometry_types() {
        for (ogr_type, expected) in [
            (wkbUnknown, "wkbUnknown"),
            (wkbGeometryCollection, "wkbGeometryCollection"),
        ] {
            let err = PgCopy::ogr_geometry_to_pg(ogr_type)
                .expect_err("unsupported geometry type must fail");
            assert!(matches!(
                err.downcast_ref::<VectorIngestError>(),
                Some(VectorIngestError::UnsupportedGeometryType(kind)) if kind == expected
            ));
        }
    }

    #[test]
    fn zero_usable_geometries_is_typed_and_suppresses_completion_progress() {
        let err =
            PgCopy::ensure_usable_geometry_count(0).expect_err("zero usable geometries must fail");
        assert!(matches!(
            err.downcast_ref::<VectorIngestError>(),
            Some(VectorIngestError::NoUsableGeometry)
        ));
        assert!(PgCopy::ensure_usable_geometry_count(1).is_ok());

        assert!(!should_report_vector_progress(2, Some(2), 0));
        assert!(should_report_vector_progress(1, Some(2), 0));
        assert!(should_report_vector_progress(2, Some(2), 1));
    }

    #[tokio::test]
    #[ignore = "requires SPATIAL_CORE_TEST_POSTGRES_DSN with PostGIS"]
    async fn copy_in_imports_geojson_fixture() -> Result<()> {
        use deadpool_postgres::{Manager, Pool};
        use tokio_postgres::NoTls;

        let fixture_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("testdata")
            .join("points.geojson");
        let dsn = std::env::var("SPATIAL_CORE_TEST_POSTGRES_DSN")?;
        let pg_cfg = dsn.parse()?;
        let pool = Pool::builder(Manager::new(pg_cfg, NoTls))
            .max_size(5)
            .build()?;
        let mut client = pool.get().await?;

        let schema = "public";
        let table = format!("copy_in_test_{}", Uuid::new_v4().simple());

        let run_result: Result<()> = async {
            PgCopy::copy_in(&mut client, schema, &table, &fixture_path, 4326).await?;

            let count_sql = format!(
                "SELECT COUNT(*) FROM {}.{}",
                super::quote_ident(schema),
                super::quote_ident(&table)
            );
            let count: i64 = client.query_one(&count_sql, &[]).await?.get(0);
            assert_eq!(count, 3, "all synthetic fixture rows must be imported");

            Ok(())
        }
        .await;

        let _ = PgCopy::delete_table(&client, schema, &table).await;
        run_result
    }

    #[tokio::test]
    #[ignore = "requires SPATIAL_CORE_TEST_POSTGRES_DSN"]
    async fn generated_ids_exceed_int32() -> Result<()> {
        use deadpool_postgres::{Manager, Pool};
        use tokio_postgres::NoTls;
        let dsn = std::env::var("SPATIAL_CORE_TEST_POSTGRES_DSN")?;
        let pool = Pool::builder(Manager::new(dsn.parse()?, NoTls))
            .max_size(1)
            .build()?;
        let client = pool.get().await?;
        let schema = format!("identity_test_{}", Uuid::new_v4().simple());
        client
            .batch_execute(&format!("CREATE SCHEMA {}", super::quote_ident(&schema)))
            .await?;
        let result: Result<()> = async {
            PgCopy::create_table(&client, &schema, "generated", &[], None).await?;
            client
                .batch_execute(&format!(
                    "CREATE TABLE {}.existing (name text)",
                    super::quote_ident(&schema)
                ))
                .await?;
            PgCopy::create_gid(&client, &schema, "existing", None).await?;
            PgCopy::create_gid(&client, &schema, "existing", None).await?;
            for table in ["generated", "existing"] {
                let qualified = format!(
                    "{}.{}",
                    super::quote_ident(&schema),
                    super::quote_ident(table)
                );
                client
                    .batch_execute(&format!(
                        "ALTER TABLE {qualified} ALTER COLUMN _id RESTART WITH 2147483648"
                    ))
                    .await?;
                let id: i64 = client
                    .query_one(
                        &format!("INSERT INTO {qualified} DEFAULT VALUES RETURNING _id"),
                        &[],
                    )
                    .await?
                    .get(0);
                assert_eq!(id, 2_147_483_648);
            }
            Ok(())
        }
        .await;
        client
            .batch_execute(&format!(
                "DROP SCHEMA {} CASCADE",
                super::quote_ident(&schema)
            ))
            .await?;
        result
    }

    #[tokio::test]
    #[ignore = "requires SPATIAL_CORE_TEST_POSTGRES_DSN"]
    async fn cursor_index_supports_lexical_pagination() -> Result<()> {
        use deadpool_postgres::{Manager, Pool};
        use tokio_postgres::NoTls;
        let dsn = std::env::var("SPATIAL_CORE_TEST_POSTGRES_DSN")?;
        let pool = Pool::builder(Manager::new(dsn.parse()?, NoTls))
            .max_size(1)
            .build()?;
        let mut client = pool.get().await?;
        let tx = client.transaction().await?;
        let schema = format!("cursor_test_{}", Uuid::new_v4().simple());
        tx.batch_execute(&format!("CREATE SCHEMA {}", super::quote_ident(&schema)))
            .await?;
        let table = "quoted\"table";
        let id = "feature id";
        PgCopy::create_table(&tx, &schema, table, &[], Some(id)).await?;
        let qualified = format!(
            "{}.{}",
            super::quote_ident(&schema),
            super::quote_ident(table)
        );
        tx.batch_execute(&format!(
            "INSERT INTO {qualified} OVERRIDING SYSTEM VALUE SELECT generate_series(1,10000); ANALYZE {qualified}"
        ))
        .await?;
        PgCopy::create_cursor_index(&tx, &schema, table, Some(id)).await?;
        let query = format!(
            "SELECT {id}::text FROM {qualified} WHERE {id}::text COLLATE \"C\">'5000' ORDER BY {id}::text COLLATE \"C\" LIMIT 10",
            id = super::quote_ident(id),
        );
        let plan = tx
            .query(&format!("EXPLAIN (COSTS OFF) {query}"), &[])
            .await?;
        let plan = plan
            .iter()
            .map(|row| row.get::<_, String>(0))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(plan.contains("Index Scan"), "{plan}");
        let ids = tx
            .query(&query, &[])
            .await?
            .iter()
            .map(|row| row.get::<_, String>(0))
            .collect::<Vec<_>>();
        assert_eq!(
            ids,
            [
                "5001", "5002", "5003", "5004", "5005", "5006", "5007", "5008", "5009", "501"
            ]
        );
        tx.rollback().await?;
        Ok(())
    }

    #[tokio::test]
    #[ignore = "requires SPATIAL_CORE_TEST_POSTGRES_DSN with PostGIS"]
    async fn copy_in_preserves_source_ids_without_column_collisions() -> Result<()> {
        use deadpool_postgres::{Manager, Pool};
        use tokio_postgres::NoTls;
        let dsn = std::env::var("SPATIAL_CORE_TEST_POSTGRES_DSN")?;
        let pool = Pool::builder(Manager::new(dsn.parse()?, NoTls))
            .max_size(1)
            .build()?;
        let mut client = pool.get().await?;
        let schema = format!("source_id_test_{}", Uuid::new_v4().simple());
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("source.geojson");
        std::fs::write(
            &path,
            r#"{"type":"FeatureCollection","features":[
            {"type":"Feature","properties":{"_id":77,"source_id":88,"source_id_2":99},"geometry":{"type":"Point","coordinates":[120,30]}},
            {"type":"Feature","properties":{"_id":77,"source_id":89,"source_id_2":100},"geometry":{"type":"Point","coordinates":[121,31]}}
        ]}"#,
        )?;
        let result: Result<()> = async {
            for _ in 0..2 {
                PgCopy::copy_in(&mut client, &schema, "features", &path, 3857).await?;
                let rows = client.query(&format!(
                    "SELECT _id,source_id,source_id_2,source_id_3,ST_SRID(geom) FROM {}.features ORDER BY _id",
                    super::quote_ident(&schema)
                ), &[]).await?;
                assert_eq!(rows.len(), 2, "retry must not append rows");
                for (index, row) in rows.iter().enumerate() {
                    assert_eq!(row.get::<_, i64>(0), index as i64 + 1);
                    assert_eq!(row.get::<_, i32>(1), index as i32 + 88);
                    assert_eq!(row.get::<_, i32>(2), index as i32 + 99);
                    assert_eq!(row.get::<_, i32>(3), 77);
                    assert_eq!(row.get::<_, i32>(4), 3857);
                }
            }
            Ok(())
        }.await;
        client
            .batch_execute(&format!(
                "DROP SCHEMA IF EXISTS {} CASCADE",
                super::quote_ident(&schema)
            ))
            .await?;
        result
    }

    #[tokio::test]
    #[ignore = "requires SPATIAL_CORE_TEST_POSTGRES_DSN with PostGIS"]
    async fn copy_in_reprojects_and_rolls_back_missing_crs() -> Result<()> {
        use deadpool_postgres::{Manager, Pool};
        use tokio_postgres::NoTls;

        let dsn = std::env::var("SPATIAL_CORE_TEST_POSTGRES_DSN")?;
        let pg_config = dsn.parse()?;
        let pool = Pool::builder(Manager::new(pg_config, NoTls))
            .max_size(1)
            .build()?;
        let mut client = pool.get().await?;
        let schema_name = format!("copy_fresh_{}", Uuid::new_v4().simple());
        let schema = schema_name.as_str();
        let table = format!("copy_reproject_{}", Uuid::new_v4().simple());
        let missing_table = format!("copy_missing_crs_{}", Uuid::new_v4().simple());
        let temp_dir = tempfile::tempdir()?;

        let run_result: Result<()> = async {
            let source_path = temp_dir.path().join("wgs84.gpkg");
            let driver = DriverManager::get_driver_by_name("GPKG")?;
            let mut dataset = driver.create_vector_only(&source_path)?;
            let source_srs = SpatialRef::from_epsg(4326)?;
            {
                let mut layer = dataset.create_layer(LayerOptions {
                    name: "features",
                    srs: Some(&source_srs),
                    ty: wkbPoint,
                    ..Default::default()
                })?;
                layer.create_feature(GdalGeometry::from_wkt("POINT (120 30)")?)?;
            }
            drop(dataset);

            PgCopy::copy_in(&mut client, schema, &table, &source_path, 3857).await?;
            let query = format!(
                "SELECT ST_SRID(geom), ST_X(geom), ST_Y(geom) FROM {}.{} LIMIT 1",
                super::quote_ident(schema),
                super::quote_ident(&table)
            );
            let row = client.query_one(&query, &[]).await?;
            let srid: i32 = row.get(0);
            let x: f64 = row.get(1);
            let y: f64 = row.get(2);
            assert_eq!(srid, 3857);
            assert!((x - 13_358_338.895_192_828).abs() < 0.01);
            assert!((y - 3_503_549.843_504_375_3).abs() < 0.01);

            // Bulk imports publish only after statistics exist, and generated
            // identifiers must remain usable beyond the signed 32-bit range.
            let qualified = format!("{}.{}", super::quote_ident(schema), super::quote_ident(&table));
            let id_type: String = client.query_one(
                "SELECT data_type FROM information_schema.columns WHERE table_schema = $1 AND table_name = $2 AND column_name = '_id'",
                &[&schema, &table],
            ).await?.get(0);
            assert_eq!(id_type, "bigint");
            let estimated_rows: f32 = client.query_one(
                "SELECT reltuples FROM pg_class WHERE oid = $1::text::regclass", &[&qualified],
            ).await?.get(0);
            assert_eq!(estimated_rows, 1.0, "ANALYZE must populate statistics before publish");
            client.batch_execute(&format!("ALTER TABLE {qualified} ALTER COLUMN _id RESTART WITH 2147483648")).await?;
            let large_id: i64 = client.query_one(
                &format!("INSERT INTO {qualified}(geom) VALUES (ST_SetSRID(ST_Point(0,0),3857)) RETURNING _id"), &[],
            ).await?.get(0);
            assert_eq!(large_id, 2_147_483_648);

            // Re-running the same import must replace the previously committed
            // table instead of appending duplicate features.
            PgCopy::copy_in(&mut client, schema, &table, &source_path, 3857).await?;
            let count_query = format!(
                "SELECT COUNT(*) FROM {}.{}",
                super::quote_ident(schema),
                super::quote_ident(&table)
            );
            let count: i64 = client.query_one(&count_query, &[]).await?.get(0);
            assert_eq!(count, 1, "retry must replace, not append, vector rows");

            let missing_path = temp_dir.path().join("missing.gpkg");
            let mut missing_dataset = driver.create_vector_only(&missing_path)?;
            {
                let mut missing_layer = missing_dataset.create_layer(LayerOptions {
                    name: "features",
                    srs: None,
                    ty: wkbPoint,
                    ..Default::default()
                })?;
                missing_layer.create_feature(GdalGeometry::from_wkt("POINT (1 2)")?)?;
            }
            drop(missing_dataset);

            let err = PgCopy::copy_in(&mut client, schema, &missing_table, &missing_path, 3857)
                .await
                .expect_err("missing source CRS must fail");
            assert!(matches!(
                err.downcast_ref::<VectorIngestError>(),
                Some(VectorIngestError::MissingSourceCrs)
            ));

            let qualified = format!("{schema}.{missing_table}");
            let row = client
                .query_one("SELECT to_regclass($1)::text", &[&qualified])
                .await?;
            let relation: Option<String> = row.get(0);
            assert!(relation.is_none(), "failed ingest left table {qualified}");
            Ok(())
        }
        .await;

        client
            .batch_execute(&format!(
                "DROP SCHEMA IF EXISTS {} CASCADE",
                super::quote_ident(schema)
            ))
            .await?;
        run_result
    }

    #[tokio::test]
    #[ignore = "requires SPATIAL_CORE_TEST_POSTGRES_DSN with PostGIS"]
    async fn copy_in_rolls_back_invalid_geometry_datasets() -> Result<()> {
        use deadpool_postgres::{Manager, Pool};
        use tokio_postgres::NoTls;

        let dsn = std::env::var("SPATIAL_CORE_TEST_POSTGRES_DSN")?;
        let pool = Pool::builder(Manager::new(dsn.parse()?, NoTls))
            .max_size(1)
            .build()?;
        let mut client = pool.get().await?;
        let schema = "public";
        let zero_table = format!("copy_zero_{}", Uuid::new_v4().simple());
        let empty_table = format!("copy_empty_{}", Uuid::new_v4().simple());
        let missing_table = format!("copy_no_geom_{}", Uuid::new_v4().simple());
        let temp_dir = tempfile::tempdir()?;
        let driver = DriverManager::get_driver_by_name("GPKG")?;
        let source_srs = SpatialRef::from_epsg(4326)?;

        let run_result: Result<()> = async {
            let zero_path = temp_dir.path().join("zero.gpkg");
            let mut zero_dataset = driver.create_vector_only(&zero_path)?;
            zero_dataset.create_layer(LayerOptions {
                name: "features",
                srs: Some(&source_srs),
                ty: wkbPoint,
                ..Default::default()
            })?;
            drop(zero_dataset);

            let zero_err = PgCopy::copy_in(&mut client, schema, &zero_table, &zero_path, 3857)
                .await
                .expect_err("zero-feature dataset must fail");
            assert!(matches!(
                zero_err.downcast_ref::<VectorIngestError>(),
                Some(VectorIngestError::NoUsableGeometry)
            ));

            let empty_path = temp_dir.path().join("empty.gpkg");
            let mut empty_dataset = driver.create_vector_only(&empty_path)?;
            {
                let mut layer = empty_dataset.create_layer(LayerOptions {
                    name: "features",
                    srs: Some(&source_srs),
                    ty: wkbPoint,
                    ..Default::default()
                })?;
                layer.create_feature(GdalGeometry::empty(wkbPoint)?)?;
            }
            drop(empty_dataset);

            let empty_err = PgCopy::copy_in(&mut client, schema, &empty_table, &empty_path, 3857)
                .await
                .expect_err("all-empty dataset must fail");
            assert!(
                matches!(
                    empty_err.downcast_ref::<VectorIngestError>(),
                    Some(VectorIngestError::NoUsableGeometry)
                ),
                "unexpected empty fixture error: {empty_err:#}"
            );

            let missing_path = temp_dir.path().join("missing-geometry.gpkg");
            let mut missing_dataset = driver.create_vector_only(&missing_path)?;
            {
                let layer = missing_dataset.create_layer(LayerOptions {
                    name: "features",
                    srs: Some(&source_srs),
                    ty: wkbPoint,
                    ..Default::default()
                })?;
                GdalFeature::new(layer.defn())?.create(&layer)?;
            }
            drop(missing_dataset);

            let missing_err =
                PgCopy::copy_in(&mut client, schema, &missing_table, &missing_path, 3857)
                    .await
                    .expect_err("feature without geometry must fail");
            assert!(matches!(
                missing_err.downcast_ref::<VectorIngestError>(),
                Some(VectorIngestError::MissingGeometry)
            ));

            for table in [&zero_table, &empty_table, &missing_table] {
                let qualified = format!("{schema}.{table}");
                let row = client
                    .query_one("SELECT to_regclass($1)::text", &[&qualified])
                    .await?;
                let relation: Option<String> = row.get(0);
                assert!(relation.is_none(), "failed ingest left table {qualified}");
            }
            Ok(())
        }
        .await;

        let _ = PgCopy::delete_table(&client, schema, &zero_table).await;
        let _ = PgCopy::delete_table(&client, schema, &empty_table).await;
        let _ = PgCopy::delete_table(&client, schema, &missing_table).await;
        run_result
    }

    #[test]
    fn wkb_to_ewkb_accepts_slice_and_sets_srid_flag() {
        let wkb: [u8; 21] = [
            1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        ];
        let ewkb = PgCopy::wkb_to_ewkb(&wkb, 4326);
        assert_eq!(ewkb[0], 1);
        let mut type_bytes = [0u8; 4];
        type_bytes.copy_from_slice(&ewkb[1..5]);
        let typ = u32::from_le_bytes(type_bytes);
        assert_eq!(typ, 0x20000001);
    }

    #[test]
    fn wkb_to_ewkb_preserves_big_endian_payload_and_sets_srid() {
        let wkb: [u8; 21] = [
            0, 0, 0, 0, 1, 64, 94, 0, 0, 0, 0, 0, 0, 64, 36, 0, 0, 0, 0, 0, 0,
        ];

        let ewkb = PgCopy::wkb_to_ewkb(&wkb, 4326);

        assert_eq!(&ewkb[0..5], &[0, 0x20, 0, 0, 1]);
        assert_eq!(&ewkb[5..9], &4326_u32.to_be_bytes());
        assert_eq!(&ewkb[9..], &wkb[5..]);
    }

    #[test]
    fn quote_ident_escapes_embedded_quotes() {
        assert_eq!(super::quote_ident("normal"), "\"normal\"");
        assert_eq!(super::quote_ident("a\"b"), "\"a\"\"b\"");
        let ident = super::quote_ident("public\";DROP");
        assert_eq!(ident, "\"public\"\";DROP\"");
    }
    #[tokio::test]
    #[ignore = "requires SPATIAL_CORE_TEST_POSTGRES_DSN"]
    async fn cancellation_stops_inflight_query_and_connection_remains_usable() -> Result<()> {
        use deadpool_postgres::{Manager, Pool};
        let dsn = std::env::var("SPATIAL_CORE_TEST_POSTGRES_DSN")?;
        let pool = Pool::builder(Manager::new(dsn.parse()?, tokio_postgres::NoTls))
            .max_size(1)
            .build()?;
        let client = pool.get().await?;
        let cancel = crate::control::Cancellation::default();
        let trigger = cancel.clone();
        let trigger = tokio::task::spawn_blocking(move || {
            std::thread::sleep(std::time::Duration::from_millis(100));
            trigger.cancel();
        });
        let started = std::time::Instant::now();
        let error = super::run_with_cancellation(&cancel, client.cancel_token(), async {
            client.batch_execute("SELECT pg_sleep(10)").await?;
            Ok(())
        })
        .await
        .unwrap_err();
        trigger.await?;
        assert!(error.downcast_ref::<crate::control::Cancelled>().is_some());
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
        assert_eq!(
            client
                .query_one("SELECT 1::int", &[])
                .await?
                .get::<_, i32>(0),
            1
        );
        // The sole pool slot remains held by `client`, so this acquisition
        // cannot finish unless cancellation is observed while waiting.
        let waiting = crate::control::Cancellation::default();
        let trigger = waiting.clone();
        let trigger = tokio::task::spawn_blocking(move || {
            std::thread::sleep(std::time::Duration::from_millis(100));
            trigger.cancel();
        });
        let error = super::acquire_connection(&pool, &waiting)
            .await
            .unwrap_err();
        trigger.await?;
        assert!(error.downcast_ref::<crate::control::Cancelled>().is_some());
        Ok(())
    }
    pub(super) fn failing_vector_fixture(
        valid_rows: usize,
    ) -> Result<(tempfile::TempDir, PathBuf)> {
        use crate::gdal_ext::vector::vector_translate::vector_translate;
        let directory = tempfile::tempdir()?;
        let source = Dataset::open(
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("testdata/points.geojson"),
        )?;
        let geopackage = directory.path().join("points.gpkg");
        let converted = vector_translate(
            &source,
            geopackage.as_path().try_into()?,
            Some(vec!["-f", "GPKG", "-nln", "points"].try_into()?),
        )?;
        drop(converted);
        let path = directory.path().join("broken.vrt");
        std::fs::write(
            &path,
            format!(
                r#"<OGRVRTDataSource><OGRVRTLayer name="broken">
<SrcDataSource relativeToVRT="1">points.gpkg</SrcDataSource>
<SrcSQL dialect="SQLITE">SELECT geom, json_extract(CASE WHEN fid &gt; {valid_rows} THEN 'invalid-json' ELSE '{{"a":1}}' END, '$.a') AS value FROM points</SrcSQL>
<GeometryType>wkbPoint</GeometryType><LayerSRS>EPSG:4326</LayerSRS>
<Field name="value" type="Integer"/>
</OGRVRTLayer></OGRVRTDataSource>"#
            ),
        )?;
        Ok((directory, path))
    }
}
