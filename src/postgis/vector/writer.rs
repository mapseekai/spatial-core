use super::{VectorSchema, VectorSource, source};
use crate::postgis::{
    EmptyInputPolicy, PostGisClient, Result, TableName, WriteControl, WriteLimits, WriteMode,
    options::TargetOptions, transaction,
};
use std::{collections::BTreeMap, time::Duration};

/// Reusable vector options. Defaults preserve source CRS/names and create a new table.
#[derive(Debug, Clone)]
pub struct VectorWriteOptions {
    pub(crate) target: TargetOptions,
    pub(crate) limits: WriteLimits,
    pub(crate) schema: Option<VectorSchema>,
    pub(crate) target_srid: Option<i32>,
    pub(crate) renames: BTreeMap<String, String>,
    pub(crate) empty: EmptyInputPolicy,
    pub(super) spatial_index: bool,
    pub(super) cursor_index: bool,
}

impl Default for VectorWriteOptions {
    fn default() -> Self {
        Self {
            target: TargetOptions::default(),
            limits: WriteLimits::default(),
            schema: None,
            target_srid: None,
            renames: BTreeMap::new(),
            empty: EmptyInputPolicy::Reject,
            spatial_index: true,
            cursor_index: false,
        }
    }
}

impl VectorWriteOptions {
    /// Effect on an existing target.
    #[must_use]
    pub fn mode(mut self, mode: WriteMode) -> Self {
        self.target.mode = mode;
        self
    }
    /// Explicitly allow creating a missing schema.
    #[must_use]
    pub fn create_schema(mut self, create: bool) -> Self {
        self.target.create_schema = create;
        self
    }
    /// Maximum wait for each database lock; validated before DDL.
    #[must_use]
    pub fn lock_timeout(mut self, timeout: Duration) -> Self {
        self.target.lock_timeout = timeout;
        self
    }
    /// Resource limits for encoded records and the queue.
    #[must_use]
    pub fn limits(mut self, limits: WriteLimits) -> Self {
        self.limits = limits;
        self
    }
    /// Final output contract, including additional nullable columns.
    #[must_use]
    pub fn schema(mut self, schema: VectorSchema) -> Self {
        self.schema = Some(schema);
        self
    }
    /// Reproject coordinates to this EPSG/database SRID.
    #[must_use]
    pub fn target_srid(mut self, srid: i32) -> Self {
        self.target_srid = Some(srid);
        self
    }
    /// Rename a source attribute without changing its source index.
    #[must_use]
    pub fn rename_field(mut self, source: impl Into<String>, target: impl Into<String>) -> Self {
        self.renames.insert(source.into(), target.into());
        self
    }
    /// Permit an empty input only after successful source reading.
    #[must_use]
    pub fn empty_input(mut self, policy: EmptyInputPolicy) -> Self {
        self.empty = policy;
        self
    }
    /// Build a GiST geometry index for newly created tables (default true).
    #[must_use]
    pub fn spatial_index(mut self, enabled: bool) -> Self {
        self.spatial_index = enabled;
        self
    }
    /// Build the optional C-collation text identity cursor index.
    #[must_use]
    pub fn cursor_index(mut self, enabled: bool) -> Self {
        self.cursor_index = enabled;
        self
    }
}

/// One vector write. Consumed by `write`; a client can be reused across writes.
#[must_use]
pub struct PostGisVectorWriter<'a> {
    db: &'a PostGisClient,
    table: TableName,
    options: VectorWriteOptions,
}

impl<'a> PostGisVectorWriter<'a> {
    /// Configure a write with safe Create defaults.
    pub fn new(db: &'a PostGisClient, table: TableName) -> Self {
        Self {
            db,
            table,
            options: VectorWriteOptions::default(),
        }
    }
    /// Supply reusable options.
    pub fn options(mut self, options: VectorWriteOptions) -> Self {
        self.options = options;
        self
    }
    /// Select the effect on the target.
    pub fn mode(mut self, mode: WriteMode) -> Self {
        self.options = self.options.mode(mode);
        self
    }
    /// Explicit target reprojection.
    pub fn target_srid(mut self, srid: i32) -> Self {
        self.options = self.options.target_srid(srid);
        self
    }
    /// Final output schema.
    pub fn schema(mut self, schema: VectorSchema) -> Self {
        self.options = self.options.schema(schema);
        self
    }
    /// Import one source; all postprocessing precedes COMMIT.
    ///
    /// # Errors
    /// Returns typed validation/reader/database failures. Cancel then await to
    /// finish cleanup. A lost COMMIT response returns CommitOutcomeUnknown.
    pub async fn write(
        self,
        source: VectorSource,
        control: &WriteControl<'_>,
    ) -> Result<VectorWriteReport> {
        let (info, records, extent) = transaction::write(
            self.db,
            &self.table,
            &self.options.target,
            || source::start(source, self.options.clone()),
            false,
            control,
            |info, caps| super::table::plan(info, &self.options, &self.table, caps),
        )
        .await?;
        Ok(VectorWriteReport {
            table: self.table,
            schema: info.schema,
            feature_count: records,
            extent,
        })
    }
}

/// Only returned after COMMIT was acknowledged.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct VectorWriteReport {
    /// Written destination.
    pub table: TableName,
    /// Exact final schema.
    pub schema: VectorSchema,
    /// Confirmed final feature count after ordinary, deferred and DDL trigger effects.
    pub feature_count: u64,
    /// Final stored bounds after triggers; None for empty/NULL/EMPTY geometries.
    pub extent: Option<[f64; 4]>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::postgis::{
        ColumnName, PostGisError,
        test_support::{TestDb, failing_vector, json_file, points},
        vector::{
            ColumnSchema, CoordinateDimension, GeometryColumn, GeometryKind, PostgresType,
            read_metadata,
        },
    };

    #[tokio::test]
    #[ignore = "requires dedicated PostGIS test database"]
    async fn measured_geometry_and_empty_geometry_survive_the_database_roundtrip()
    -> anyhow::Result<()> {
        use gdal::vector::{Feature, Geometry, LayerAccess, LayerOptions};
        let db = TestDb::new().await?;
        let directory = tempfile::tempdir()?;
        let result = async {
            for (name, wkt, kind) in [
                (
                    "measured",
                    "POINT M (12 55 7)",
                    gdal::vector::OGRwkbGeometryType::wkbPointM,
                ),
                (
                    "zm",
                    "POINT ZM (12 55 9 7)",
                    gdal::vector::OGRwkbGeometryType::wkbPointZM,
                ),
                (
                    "empty_geom",
                    "POINT EMPTY",
                    gdal::vector::OGRwkbGeometryType::wkbPoint,
                ),
            ] {
                let path = directory.path().join(format!("{name}.gpkg"));
                let mut dataset =
                    gdal::DriverManager::get_driver_by_name("GPKG")?.create_vector_only(&path)?;
                let crs = gdal::spatial_ref::SpatialRef::from_epsg(4326)?;
                let layer = dataset.create_layer(LayerOptions {
                    name: "features",
                    srs: Some(&crs),
                    ty: kind,
                    ..Default::default()
                })?;
                let mut feature = Feature::new(layer.defn())?;
                feature.set_geometry(Geometry::from_wkt(wkt)?)?;
                feature.create(&layer)?;
                drop(feature);
                drop(dataset);
                let report = db
                    .vector(name)?
                    .write(VectorSource::new(path), &WriteControl::default())
                    .await?;
                let row = db
                    .pool
                    .get()
                    .await?
                    .query_one(
                        &format!(
                            "SELECT ST_M(geom),ST_Z(geom),ST_IsEmpty(geom) FROM {}.{name}",
                            db.schema
                        ),
                        &[],
                    )
                    .await?;
                if name == "empty_geom" {
                    assert!(row.get::<_, bool>(2));
                    assert_eq!(report.extent, None);
                } else {
                    assert_eq!(row.get::<_, f64>(0), 7.);
                    if name == "zm" {
                        assert_eq!(row.get::<_, f64>(1), 9.);
                    }
                }
            }
            Ok::<_, anyhow::Error>(())
        }
        .await;
        db.cleanup().await?;
        result
    }

    #[tokio::test]
    #[ignore = "requires dedicated PostGIS test database"]
    async fn shapefile_multi_normalization_preserves_zm_and_accepts_other_epsg_targets()
    -> anyhow::Result<()> {
        use gdal::vector::{Feature, Geometry, LayerAccess, LayerOptions, OGRwkbGeometryType};
        let directory = tempfile::tempdir()?;
        let db = TestDb::new().await?;
        let result = async {
            for (name, wkt, kind, expected) in [
                ("line", "LINESTRING ZM (12 55 9 7, 13 56 10 8)", OGRwkbGeometryType::wkbLineStringZM, "MULTILINESTRING"),
                ("polygon", "POLYGON ZM ((12 55 9 7, 13 55 9 7, 13 56 9 7, 12 55 9 7))", OGRwkbGeometryType::wkbPolygonZM, "MULTIPOLYGON"),
            ] {
                let path = directory.path().join(format!("{name}.shp"));
                let mut dataset = gdal::DriverManager::get_driver_by_name("ESRI Shapefile")?.create_vector_only(&path)?;
                let crs = gdal::spatial_ref::SpatialRef::from_epsg(4326)?;
                let layer = dataset.create_layer(LayerOptions { name, srs: Some(&crs), ty: kind, ..Default::default() })?;
                let mut feature = Feature::new(layer.defn())?;
                feature.set_geometry(Geometry::from_wkt(wkt)?)?;feature.create(&layer)?;
                drop(feature);drop(dataset);
                let report = db.vector(name)?.target_srid(3395).write(VectorSource::new(path), &WriteControl::default()).await?;
                assert_eq!(report.schema.geometry.srid, 3395);
                let row = db.pool.get().await?.query_one(&format!("SELECT GeometryType(geom),ST_Z((ST_DumpPoints(geom)).geom),ST_M((ST_DumpPoints(geom)).geom) FROM {}.{name} LIMIT 1", db.schema), &[]).await?;
                assert_eq!(row.get::<_, &str>(0), expected);assert_eq!(row.get::<_, f64>(1), 9.);assert_eq!(row.get::<_, f64>(2), 7.);
            }
            Ok::<_, anyhow::Error>(())
        }.await;
        db.cleanup().await?;
        result
    }

    #[tokio::test]
    #[ignore = "requires dedicated PostGIS test database"]
    async fn empty_inputs_custom_crs_and_explicit_name_mapping() -> anyhow::Result<()> {
        let db = TestDb::new().await?;
        let result = async {
            let empty = json_file(r#"{"type":"FeatureCollection","features":[]}"#)?;
            let schema = VectorSchema::new(vec![ColumnSchema::new("Value", PostgresType::BigInt)?], GeometryColumn::new("Geometry", GeometryKind::Point, CoordinateDimension::Xyz, 4326)?)?;
            let options = VectorWriteOptions::default().schema(schema).empty_input(EmptyInputPolicy::Allow);
            let report = db.vector("empty")?.options(options).write(VectorSource::new(empty.path()), &WriteControl::default()).await?;
            assert_eq!(report.feature_count,0); assert_eq!(report.extent,None);
            let metadata = read_metadata(&db.client,&db.table("empty")?,&ColumnName::new("Geometry")?,&WriteControl::default()).await?;
            assert_eq!(metadata.coordinate_dimension,Some(3)); assert_eq!(metadata.feature_count,0); assert_eq!(metadata.extent,None);
            let file = json_file(r#"{"type":"FeatureCollection","features":[{"type":"Feature","properties":{"_id":42,"a":1,"A":2},"geometry":{"type":"Point","coordinates":[12,55]}}]}"#)?;
            assert!(db.vector("named")?.write(VectorSource::new(file.path()),&WriteControl::default()).await.is_err());
            db.vector("named")?.options(VectorWriteOptions::default().rename_field("_id","source_id").cursor_index(true)).write(VectorSource::new(file.path()),&WriteControl::default()).await?;
            let row=db.pool.get().await?.query_one(&format!("SELECT source_id,a,\"A\" FROM {}.named",db.schema),&[]).await?;
            assert_eq!((row.get::<_,i32>(0),row.get::<_,i32>(1),row.get::<_,i32>(2)),(42,1,2));
            let vrt=tempfile::NamedTempFile::with_suffix(".vrt")?;
            std::fs::write(vrt.path(),format!(r#"<OGRVRTDataSource><OGRVRTLayer name="custom"><SrcDataSource>{}</SrcDataSource><SrcLayer>{}</SrcLayer><GeometryType>wkbPoint</GeometryType><LayerSRS>GEOGCS["Local survey geographic",DATUM["WGS_1984",SPHEROID["WGS 84",6378137,298.257223563]],PRIMEM["Greenwich",0],UNIT["degree",0.0174532925199433]]</LayerSRS></OGRVRTLayer></OGRVRTDataSource>"#,file.path().display(),file.path().file_stem().unwrap().to_str().unwrap()))?;
            let options=VectorWriteOptions::default().rename_field("_id","source_id");
            assert!(db.vector("custom")?.options(options.clone()).write(VectorSource::new(vrt.path()),&WriteControl::default()).await.is_err());
            let report=db.vector("custom")?.options(options.target_srid(3857)).write(VectorSource::new(vrt.path()),&WriteControl::default()).await?;
            let bounds=report.extent.unwrap();
            assert!((bounds[0]-1335833.8895192828).abs()<0.001);
            assert!((bounds[1]-7361866.113051188).abs()<0.001);
            Ok::<_,anyhow::Error>(())
        }.await;
        db.cleanup().await?;
        result
    }

    #[tokio::test]
    #[ignore = "requires dedicated PostGIS test database"]
    async fn create_replace_rows_identity_metadata_and_reprojection() -> anyhow::Result<()> {
        let db = TestDb::new().await?;
        let result = async {
            let report = db
                .vector("points")?
                .target_srid(3857)
                .write(VectorSource::new(points()), &WriteControl::default())
                .await?;
            assert_eq!(report.feature_count, 3);
            assert_eq!(report.schema.geometry.srid, 3857);
            assert!(report.extent.is_some());
            assert!(
                db.vector("points")?
                    .write(VectorSource::new(points()), &WriteControl::default())
                    .await
                    .is_err()
            );
            db.pool
                .get()
                .await?
                .batch_execute(&format!(
                    "ALTER TABLE {}.points ALTER COLUMN _id RESTART WITH 2147483648",
                    db.schema
                ))
                .await?;
            let report = db
                .vector("points")?
                .mode(WriteMode::ReplaceRows)
                .target_srid(3857)
                .write(VectorSource::new(points()), &WriteControl::default())
                .await?;
            assert_eq!(report.feature_count, 3);
            let row = db
                .pool
                .get()
                .await?
                .query_one(
                    &format!(
                        "SELECT min(_id),ST_SRID(geom) FROM {}.points GROUP BY ST_SRID(geom)",
                        db.schema
                    ),
                    &[],
                )
                .await?;
            assert_eq!(row.get::<_, i64>(0), 2147483648);
            assert_eq!(row.get::<_, i32>(1), 3857);
            let metadata = read_metadata(
                &db.client,
                &db.table("points")?,
                &ColumnName::new("geom")?,
                &WriteControl::default(),
            )
            .await?;
            assert_eq!(metadata.feature_count, 3);
            assert_eq!(metadata.srid, Some(3857));
            assert_eq!(metadata.columns[0].data_type, "bigint");
            assert!(
                db.vector("missing")?
                    .mode(WriteMode::ReplaceRows)
                    .write(VectorSource::new(points()), &WriteControl::default())
                    .await
                    .is_err()
            );
            Ok::<_, anyhow::Error>(())
        }
        .await;
        db.cleanup().await?;
        result
    }

    #[tokio::test]
    #[ignore = "requires dedicated PostGIS test database"]
    async fn exact_types_arrays_escaping_and_conversion_rollback() -> anyhow::Result<()> {
        let db = TestDb::new().await?;
        let file = json_file(
            r#"{"type":"FeatureCollection","features":[
            {"type":"Feature","properties":{"large":"9007199254740993","decimal":"123456789012345678.123456789012","path":[2,1,3],"text":"\\N\tline\n\\","enabled":"true","time":"2026-01-02T03:04:05.123456+08:00"},"geometry":{"type":"Point","coordinates":[116,39,17]}},
            {"type":"Feature","properties":{"path":[]},"geometry":{"type":"Point","coordinates":[117,40,18]}},
            {"type":"Feature","properties":{"path":null},"geometry":{"type":"Point","coordinates":[118,41,19]}}
        ]}"#,
        )?;
        let schema = VectorSchema::new(
            vec![
                ColumnSchema::new("large", PostgresType::BigInt)?,
                ColumnSchema::new(
                    "decimal",
                    PostgresType::Decimal {
                        precision: 30,
                        scale: 12,
                    },
                )?,
                ColumnSchema::new("path", PostgresType::IntegerArray)?,
                ColumnSchema::new("text", PostgresType::Text)?,
                ColumnSchema::new("enabled", PostgresType::Boolean)?,
                ColumnSchema::new("time", PostgresType::TimestampTz(6))?,
                ColumnSchema::new("added", PostgresType::Text)?,
            ],
            GeometryColumn::new("geom", GeometryKind::Point, CoordinateDimension::Xyz, 4326)?,
        )?;
        let result = async {
            db.vector("values")?.schema(schema.clone()).write(VectorSource::new(file.path()).open_option("DATE_AS_STRING=YES"), &WriteControl::default()).await?;
            let rows = db.pool.get().await?.query(&format!("SELECT large::text,decimal::text,path,text,enabled,ST_Z(geom),added,time FROM {}.values ORDER BY _id", db.schema), &[]).await?;
            assert_eq!(rows[0].get::<_, String>(0), "9007199254740993");
            assert_eq!(rows[0].get::<_, String>(1), "123456789012345678.123456789012");
            assert_eq!(rows[0].get::<_, Vec<i32>>(2), [2,1,3]);
            assert_eq!(rows[1].get::<_, Vec<i32>>(2), Vec::<i32>::new());
            assert_eq!(rows[2].get::<_, Option<Vec<i32>>>(2), None);
            assert_eq!(rows[0].get::<_, String>(3), "\\N\tline\n\\"); assert!(rows[0].get::<_, bool>(4));
            assert_eq!(rows[0].get::<_, f64>(5), 17.); assert!(rows[0].get::<_, Option<String>>(6).is_none());
            assert_eq!(rows[0].get::<_, chrono::DateTime<chrono::Utc>>(7).to_rfc3339(), "2026-01-01T19:04:05.123456+00:00");
            db.vector("values")?.mode(WriteMode::ReplaceRows).schema(schema.clone()).write(VectorSource::new(file.path()).open_option("DATE_AS_STRING=YES"), &WriteControl::default()).await?;
            let mut invalid_schema = schema; invalid_schema.columns[1].data_type = PostgresType::Decimal { precision: 30, scale: 5 };
            assert!(db.vector("values")?.mode(WriteMode::ReplaceTable).schema(invalid_schema).write(VectorSource::new(file.path()).open_option("DATE_AS_STRING=YES"), &WriteControl::default()).await.is_err());
            assert_eq!(db.pool.get().await?.query_one(&format!("SELECT count(*) FROM {}.values", db.schema), &[]).await?.get::<_, i64>(0), 3);
            Ok::<_, anyhow::Error>(())
        }.await;
        db.cleanup().await?;
        result
    }

    #[tokio::test]
    #[ignore = "requires dedicated PostGIS test database"]
    async fn native_read_failures_and_cancel_preserve_committed_target() -> anyhow::Result<()> {
        let db = TestDb::new().await?;
        let result = async {
            db.vector("points")?
                .write(VectorSource::new(points()), &WriteControl::default())
                .await?;
            for count in [0, 1] {
                let (_directory, path) = failing_vector(count)?;
                assert!(
                    db.vector("points")?
                        .options(TestDb::replace())
                        .write(VectorSource::new(path), &WriteControl::default())
                        .await
                        .is_err()
                );
                assert_eq!(
                    db.pool
                        .get()
                        .await?
                        .query_one(&format!("SELECT count(*) FROM {}.points", db.schema), &[])
                        .await?
                        .get::<_, i64>(0),
                    3
                );
            }
            let cancel = crate::control::Cancellation::default();
            let callback = |p: crate::postgis::WriteProgress| {
                if p.stage == crate::postgis::WriteStage::Copying {
                    cancel.cancel();
                }
            };
            let control = WriteControl::new(cancel.clone()).on_progress(&callback);
            let error = db
                .vector("points")?
                .options(TestDb::replace())
                .write(VectorSource::new(points()), &control)
                .await
                .unwrap_err();
            assert!(matches!(error.primary(), PostGisError::Cancelled));
            assert_eq!(
                db.pool
                    .get()
                    .await?
                    .query_one(&format!("SELECT count(*) FROM {}.points", db.schema), &[])
                    .await?
                    .get::<_, i64>(0),
                3
            );
            db.vector("points")?
                .options(TestDb::replace())
                .write(VectorSource::new(points()), &WriteControl::default())
                .await?;
            Ok::<_, anyhow::Error>(())
        }
        .await;
        db.cleanup().await?;
        result
    }

    #[tokio::test]
    #[ignore = "requires dedicated PostGIS test database"]
    async fn inherited_partitioned_and_rls_targets_are_never_cleared() -> anyhow::Result<()> {
        let db = TestDb::new().await?;
        let result = async {
            db.pool.get().await?.batch_execute(&format!("CREATE TABLE {s}.parent (value integer); CREATE TABLE {s}.child () INHERITS ({s}.parent); INSERT INTO {s}.child VALUES (42); CREATE TABLE {s}.partitioned (value integer) PARTITION BY RANGE(value); CREATE TABLE {s}.part PARTITION OF {s}.partitioned FOR VALUES FROM (0) TO (100); INSERT INTO {s}.part VALUES(43); CREATE TABLE {s}.protected(value integer); INSERT INTO {s}.protected VALUES(44); ALTER TABLE {s}.protected ENABLE ROW LEVEL SECURITY;", s=db.schema)).await?;
            for name in ["parent", "child", "partitioned", "part", "protected"] {
                for mode in [WriteMode::ReplaceRows, WriteMode::ReplaceTable] {
                    let error = db.vector(name)?.mode(mode).write(VectorSource::new(points()), &WriteControl::default()).await.unwrap_err();
                    assert!(matches!(error.primary(), PostGisError::Unsupported(_)), "{name}: {error:?}");
                }
            }
            for (name, expected) in [("child",42),("part",43),("protected",44)] {
                assert_eq!(db.pool.get().await?.query_one(&format!("SELECT value FROM {}.{name}", db.schema), &[]).await?.get::<_, i32>(0), expected);
            }
            Ok::<_, anyhow::Error>(())
        }.await;
        db.cleanup().await?;
        result
    }
    #[tokio::test]
    #[ignore = "requires dedicated PostGIS test database"]
    async fn lossy_text_float_conversion_rolls_back_existing_rows() -> anyhow::Result<()> {
        let db = TestDb::new().await?;
        let result = async {
            for (name, kind, invalid) in [
                ("double_values", PostgresType::DoublePrecision, ["9007199254740993", "1.0000000000000001"]),
                ("real_values", PostgresType::Real, ["16777217", "1.00000001"]),
            ] {
                let schema = VectorSchema::new(vec![ColumnSchema::new("value", kind)?], GeometryColumn::new("geom", GeometryKind::Point, CoordinateDimension::Xy, 4326)?)?;
                let valid = json_file(r#"{"type":"FeatureCollection","features":[{"type":"Feature","properties":{"value":"0.5"},"geometry":{"type":"Point","coordinates":[1,2]}}]}"#)?;
                db.vector(name)?.schema(schema.clone()).write(VectorSource::new(valid.path()), &WriteControl::default()).await?;
                for value in invalid {
                    let bad = json_file(&format!(r#"{{"type":"FeatureCollection","features":[{{"type":"Feature","properties":{{"value":"1.25"}},"geometry":{{"type":"Point","coordinates":[1,2]}}}},{{"type":"Feature","properties":{{"value":"{value}"}},"geometry":{{"type":"Point","coordinates":[1,2]}}}}]}}"#))?;
                    let error = db.vector(name)?.mode(WriteMode::ReplaceRows).schema(schema.clone()).write(VectorSource::new(bad.path()), &WriteControl::default()).await.unwrap_err();
                    assert!(matches!(error.primary(), PostGisError::InvalidInput(_)), "{error:?}");
                    let rows = db.pool.get().await?.query(&format!("SELECT value::float8 FROM {}", db.table(name)?.sql()), &[]).await?;
                    assert_eq!(rows.len(), 1);
                    assert_eq!(rows[0].get::<_, f64>(0), 0.5);
                }
            }
            Ok::<_, anyhow::Error>(())
        }.await;
        db.cleanup().await?;
        result
    }
}
