use super::{RasterSchema, RasterSource, RasterStorage, TileLayout, source};
use crate::postgis::{
    ColumnName, PostGisClient, Result, TableName, WriteControl, WriteLimits, WriteMode,
    options::TargetOptions, transaction,
};
use std::time::Duration;

/// Resampling applied only when raster reprojection is requested.
#[derive(Debug, Clone, Copy, Default)]
pub enum Resampling {
    /// Preserve categorical values (default).
    #[default]
    Nearest,
    /// Bilinear interpolation.
    Bilinear,
    /// Cubic interpolation.
    Cubic,
}
impl Resampling {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Nearest => "near",
            Self::Bilinear => "bilinear",
            Self::Cubic => "cubic",
        }
    }
}

/// Raster output options. Defaults: Create, InDB, 256×256, source CRS.
#[derive(Debug, Clone)]
pub struct RasterWriteOptions {
    pub(crate) target: TargetOptions,
    pub(crate) limits: WriteLimits,
    pub(crate) layout: TileLayout,
    pub(crate) storage: RasterStorage,
    pub(crate) target_srid: Option<i32>,
    pub(crate) resampling: Resampling,
    pub(crate) identity: ColumnName,
    pub(crate) column: ColumnName,
}

impl Default for RasterWriteOptions {
    fn default() -> Self {
        Self {
            target: TargetOptions::default(),
            limits: WriteLimits {
                max_records: 1_000_000,
                ..WriteLimits::default()
            },
            layout: TileLayout::default(),
            storage: RasterStorage::InDb,
            target_srid: None,
            resampling: Resampling::Nearest,
            identity: ColumnName::new("_id").expect("valid literal"),
            column: ColumnName::new("rast").expect("valid literal"),
        }
    }
}

impl RasterWriteOptions {
    /// Target table write mode.
    #[must_use]
    pub fn mode(mut self, mode: WriteMode) -> Self {
        self.target.mode = mode;
        self
    }
    /// Allow creating a missing schema.
    #[must_use]
    pub fn create_schema(mut self, create: bool) -> Self {
        self.target.create_schema = create;
        self
    }
    /// Maximum wait for each database lock.
    #[must_use]
    pub fn lock_timeout(mut self, timeout: Duration) -> Self {
        self.target.lock_timeout = timeout;
        self
    }
    /// Encoding, queue, and tile-count bounds.
    #[must_use]
    pub fn limits(mut self, limits: WriteLimits) -> Self {
        self.limits = limits;
        self
    }
    /// Tile dimensions and overlap, or Whole.
    #[must_use]
    pub fn tile_layout(mut self, layout: TileLayout) -> Self {
        self.layout = layout;
        self
    }
    /// Pixel storage, with a separate OutDB server reference.
    #[must_use]
    pub fn storage(mut self, storage: RasterStorage) -> Self {
        self.storage = storage;
        self
    }
    /// Explicit target EPSG/database SRID.
    #[must_use]
    pub fn target_srid(mut self, srid: i32) -> Self {
        self.target_srid = Some(srid);
        self
    }
    /// Reprojection resampling; no resampling occurs for an unchanged CRS.
    #[must_use]
    pub fn resampling(mut self, resampling: Resampling) -> Self {
        self.resampling = resampling;
        self
    }
    /// Explicit identity and raster column names.
    #[must_use]
    pub fn columns(mut self, identity: ColumnName, raster: ColumnName) -> Self {
        self.identity = identity;
        self.column = raster;
        self
    }
}

/// Configures and executes one raster write.
#[must_use]
pub struct PostGisRasterWriter<'a> {
    db: &'a PostGisClient,
    table: TableName,
    options: RasterWriteOptions,
}

impl<'a> PostGisRasterWriter<'a> {
    /// Create a writer with Create/InDB/tiled defaults.
    pub fn new(db: &'a PostGisClient, table: TableName) -> Self {
        Self {
            db,
            table,
            options: RasterWriteOptions::default(),
        }
    }
    /// Reusable options.
    pub fn options(mut self, options: RasterWriteOptions) -> Self {
        self.options = options;
        self
    }
    /// Target write mode.
    pub fn mode(mut self, mode: WriteMode) -> Self {
        self.options = self.options.mode(mode);
        self
    }
    /// Tile layout.
    pub fn tile_layout(mut self, layout: TileLayout) -> Self {
        self.options = self.options.tile_layout(layout);
        self
    }
    /// Import a raster; the report is returned only after confirmed COMMIT.
    /// # Errors
    /// Rejects unsupported pixels, invalid grids, budget excess, target mismatch,
    /// inaccessible inputs and database failures. OutDB requires server-side access.
    pub async fn write(
        self,
        source: RasterSource,
        control: &WriteControl<'_>,
    ) -> Result<RasterWriteReport> {
        if self.options.identity == self.options.column {
            return Err(crate::postgis::PostGisError::InvalidInput(
                "identity and raster column names collide".into(),
            ));
        }
        let (info, tile_count, extent) = transaction::write(
            self.db,
            &self.table,
            &self.options.target,
            || source::start(source, self.options.clone()),
            true,
            control,
            |info, caps| super::table::plan(info, &self.options, &self.table, caps),
        )
        .await?;
        Ok(RasterWriteReport {
            table: self.table,
            schema: info.schema,
            source_size: info.source_size,
            output_size: info.output_size,
            tile_count,
            extent,
        })
    }
}

/// A raster write whose COMMIT was acknowledged.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct RasterWriteReport {
    /// Written destination.
    pub table: TableName,
    /// Verified band/SRID/scale/skew contract and the planned GDAL grid origin.
    pub schema: RasterSchema,
    /// Input dimensions before any warp.
    pub source_size: (usize, usize),
    /// GDAL output dimensions after warp, used to plan tiles; not recomputed from stored tiles.
    pub output_size: (usize, usize),
    /// Confirmed final tile count after ordinary, deferred and DDL trigger effects.
    pub tile_count: u64,
    /// Final stored footprint bounds after triggers, including NoData pixels.
    pub extent: Option<[f64; 4]>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::postgis::{
        PostGisError,
        test_support::{TestDb, raster_file},
    };

    #[tokio::test]
    #[ignore = "requires dedicated PostGIS Raster database"]
    async fn every_supported_pixel_type_and_nodata_roundtrip() -> anyhow::Result<()> {
        fn fixture<T: gdal::raster::GdalType + Copy>(
            values: [T; 2],
            nodata: Option<f64>,
        ) -> anyhow::Result<tempfile::NamedTempFile> {
            let file = tempfile::NamedTempFile::with_suffix(".tif")?;
            let mut dataset = gdal::DriverManager::get_driver_by_name("GTiff")?
                .create_with_band_type::<T, _>(file.path(), 2, 1, 1)?;
            dataset.set_spatial_ref(&gdal::spatial_ref::SpatialRef::from_epsg(4326)?)?;
            dataset.set_geo_transform(&[10., 0.1, 0., 20., 0., -0.1])?;
            dataset.rasterband(1)?.write(
                (0, 0),
                (2, 1),
                &mut gdal::raster::Buffer::new((2, 1), values.to_vec()),
            )?;
            dataset.rasterband(1)?.set_no_data_value(nodata)?;
            dataset.flush_cache()?;
            crate::postgis::test_support::make_server_readable(file.path())?;
            Ok(file)
        }
        let fixtures = [
            (fixture([42u8, 255], Some(255.))?, "8BUI", 42., Some(255.)),
            (
                fixture([60000u16, 65535], Some(65535.))?,
                "16BUI",
                60000.,
                Some(65535.),
            ),
            (
                fixture([-123i16, -32768], Some(-32768.))?,
                "16BSI",
                -123.,
                Some(-32768.),
            ),
            (
                fixture([4_000_000_000u32, u32::MAX], Some(u32::MAX as f64))?,
                "32BUI",
                4_000_000_000.,
                Some(u32::MAX as f64),
            ),
            (
                fixture([-2_000_000_000i32, i32::MIN], Some(i32::MIN as f64))?,
                "32BSI",
                -2_000_000_000.,
                Some(i32::MIN as f64),
            ),
            (
                fixture([1.25f32, f32::NAN], Some(f64::NAN))?,
                "32BF",
                1.25,
                Some(f64::NAN),
            ),
            (
                fixture([1.234567890123f64, 2.], None)?,
                "64BF",
                1.234567890123,
                None,
            ),
        ];
        let db = TestDb::new().await?;
        let result = async {
            db.pool.get().await?.batch_execute("SET postgis.enable_outdb_rasters = true; SET postgis.gdal_enabled_drivers = 'GTiff'").await?;
            for (index, (file, pixel_type, expected, nodata)) in fixtures.iter().enumerate() {
              for (outdb, storage) in [(false, RasterStorage::InDb), (true, RasterStorage::OutDb { server_path: file.path().to_str().unwrap().into() })] {
                let table = db.table(&format!("pixels_{index}_{outdb}"))?;
                let options = RasterWriteOptions::default().storage(storage).tile_layout(TileLayout::Whole);
                PostGisRasterWriter::new(&db.client, table.clone()).options(options).write(RasterSource::new(file.path()), &WriteControl::default()).await?;
                let row = db.pool.get().await?.query_one(&format!("SELECT ST_BandPixelType(rast,1),ST_Value(rast,1,1,1),ST_Value(rast,1,2,1),ST_BandNoDataValue(rast,1) FROM {}", table.sql()), &[]).await?;
                assert_eq!(row.get::<_, &str>(0), *pixel_type);
                assert_eq!(row.get::<_, f64>(1), *expected);
                let actual = row.get::<_, Option<f64>>(3);
                if let Some(nodata) = nodata {
                    assert!(row.get::<_, Option<f64>>(2).is_none());
                    assert!(actual.is_some_and(|value| value == *nodata || value.is_nan() && nodata.is_nan()));
                } else {
                    assert_eq!(row.get::<_, f64>(2), 2.);
                    assert!(actual.is_none());
                }
              }
            }
            Ok::<_, anyhow::Error>(())
        }.await;
        db.cleanup().await?;
        result
    }

    #[tokio::test]
    #[ignore = "requires dedicated PostGIS Raster database with OutDB permission"]
    async fn indb_outdb_whole_and_tiles_read_back_real_pixels() -> anyhow::Result<()> {
        let db = TestDb::new().await?;
        let file = raster_file()?;
        let result = async {
            db.pool.get().await?.batch_execute("SET postgis.enable_outdb_rasters = true; SET postgis.gdal_enabled_drivers = 'GTiff'").await?;
            for (index, storage) in [RasterStorage::InDb, RasterStorage::OutDb { server_path: file.path().to_str().unwrap().into() }].into_iter().enumerate() {
                for (tiled, layout) in [(false, TileLayout::Whole), (true, TileLayout::tiled(2,2)?)] {
                    let name = format!("raster_{index}_{tiled}");
                    let options = RasterWriteOptions::default().storage(storage.clone()).tile_layout(layout);
                    let report = PostGisRasterWriter::new(&db.client, db.table(&name)?).options(options).write(RasterSource::new(file.path()), &WriteControl::default()).await?;
                    assert_eq!(report.tile_count, if tiled { 6 } else { 1 });
                    assert_eq!(report.output_size, (5,3));
                    let rows = db.pool.get().await?.query(&format!("SELECT ST_Width(rast),ST_Height(rast),ST_Value(rast,1,1,1,false),ST_Value(rast,2,1,1,false),ST_BandNoDataValue(rast,1),ST_UpperLeftX(rast),ST_UpperLeftY(rast),ST_SkewX(rast),ST_SkewY(rast) FROM {}.{name} ORDER BY _id", db.schema), &[]).await?;
                    for (tile, row) in rows.iter().enumerate() {
                        let (x,y) = if tiled { ((tile%3)*2,(tile/3)*2) } else { (0,0) };
                        assert_eq!(row.get::<_, f64>(2), 100. + (y*5+x) as f64);
                        assert_eq!(row.get::<_, f64>(3), 200. + (y*5+x) as f64);
                        assert_eq!(row.get::<_, f64>(4), 65535.);
                        assert!((row.get::<_, f64>(5) - (100.+x as f64*0.1+y as f64*0.02)).abs() < 1e-10);
                        assert!((row.get::<_, f64>(6) - (30.+x as f64*0.01-y as f64*0.1)).abs() < 1e-10);
                        assert_eq!(row.get::<_, f64>(7), 0.02); assert_eq!(row.get::<_, f64>(8), 0.01);
                    }
                    let catalog = db.pool.get().await?.query_one("SELECT srid,num_bands,pixel_types,blocksize_x,blocksize_y FROM raster_columns WHERE r_table_schema=$1 AND r_table_name=$2", &[&db.schema,&name]).await?;
                    assert_eq!(catalog.get::<_, i32>(0),4326);
                    assert_eq!(catalog.get::<_, i32>(1),2);
                    assert_eq!(catalog.get::<_, Vec<String>>(2),["16BUI","16BUI"]);
                    if tiled { assert!(catalog.get::<_, Option<i32>>(3).is_none()); }
                    let metadata = super::super::read_metadata(&db.client, &db.table(&name)?, &ColumnName::new("rast")?, &WriteControl::default()).await?;
                    assert_eq!(metadata.tile_count, report.tile_count); assert_eq!(metadata.srids, [4326]);
                    assert_eq!(metadata.heterogeneous, tiled); // edge tile dimensions vary
                }
            }
            Ok::<_, anyhow::Error>(())
        }.await;
        db.cleanup().await?;
        result
    }

    #[tokio::test]
    #[ignore = "requires dedicated PostGIS Raster test database"]
    async fn raster_cancel_budget_and_old_constraints_preserve_existing_rows() -> anyhow::Result<()>
    {
        let db = TestDb::new().await?;
        let file = raster_file()?;
        let result = async {
            let table = db.table("tiles")?;
            let options = RasterWriteOptions::default().tile_layout(TileLayout::tiled(2, 2)?);
            PostGisRasterWriter::new(&db.client, table.clone())
                .options(options.clone())
                .write(RasterSource::new(file.path()), &WriteControl::default())
                .await?;
            let cancel = crate::control::Cancellation::default();
            let callback = |p: crate::postgis::WriteProgress| {
                if p.stage == crate::postgis::WriteStage::Copying {
                    cancel.cancel();
                }
            };
            let error = PostGisRasterWriter::new(&db.client, table.clone())
                .options(options.clone().mode(WriteMode::ReplaceRows))
                .write(
                    RasterSource::new(file.path()),
                    &WriteControl::new(cancel.clone()).on_progress(&callback),
                )
                .await
                .unwrap_err();
            assert!(matches!(error.primary(), PostGisError::Cancelled));
            let error = PostGisRasterWriter::new(&db.client, table.clone())
                .options(
                    options
                        .clone()
                        .mode(WriteMode::ReplaceTable)
                        .limits(WriteLimits::new(32, 64, 10)?),
                )
                .write(RasterSource::new(file.path()), &WriteControl::default())
                .await
                .unwrap_err();
            assert!(matches!(error.primary(), PostGisError::ResourceLimit(_)));
            // Existing extent/scale constraints survive ReplaceRows and reject a changed grid.
            let error = PostGisRasterWriter::new(&db.client, table.clone())
                .options(
                    options
                        .clone()
                        .mode(WriteMode::ReplaceRows)
                        .target_srid(3857),
                )
                .write(RasterSource::new(file.path()), &WriteControl::default())
                .await
                .unwrap_err();
            assert!(matches!(error.primary(), PostGisError::Database { .. }));
            assert_eq!(
                db.pool
                    .get()
                    .await?
                    .query_one(&format!("SELECT count(*) FROM {}.tiles", db.schema), &[])
                    .await?
                    .get::<_, i64>(0),
                6
            );
            PostGisRasterWriter::new(&db.client, table)
                .options(options.mode(WriteMode::ReplaceRows))
                .write(RasterSource::new(file.path()), &WriteControl::default())
                .await?;
            Ok::<_, anyhow::Error>(())
        }
        .await;
        db.cleanup().await?;
        result
    }
    #[tokio::test]
    #[ignore = "requires dedicated PostGIS Raster database"]
    async fn precise_scale_nodata_and_reprojection_survive_catalog_rounding() -> anyhow::Result<()>
    {
        let db = TestDb::new().await?;
        let result = async {
            for (name, scale, nodata, target) in [
                ("control", 0.1, None, None),
                ("precise", 0.0000235176759123, Some(0.1234567890123), None),
                ("warped", 0.1, None, Some(3857)),
            ] {
                let file = tempfile::NamedTempFile::with_suffix(".tif")?;
                let mut dataset = gdal::DriverManager::get_driver_by_name("GTiff")?.create_with_band_type::<f64, _>(file.path(), 2, 2, 1)?;
                dataset.set_spatial_ref(&gdal::spatial_ref::SpatialRef::from_epsg(4326)?)?;
                dataset.set_geo_transform(&[0., scale, 0., 2., 0., -scale])?;
                let mut band = dataset.rasterband(1)?;
                band.set_no_data_value(nodata)?;
                band.write((0, 0), (2, 2), &mut gdal::raster::Buffer::new((2, 2), vec![1., 2., 3., 4.]))?;
                drop(dataset);
                let mut options = RasterWriteOptions::default().tile_layout(TileLayout::Whole);
                if let Some(srid) = target { options = options.target_srid(srid); }
                let table = db.table(name)?;
                let report = PostGisRasterWriter::new(&db.client, table.clone()).options(options).write(RasterSource::new(file.path()), &WriteControl::default()).await?;
                let row = db.pool.get().await?.query_one(&format!("SELECT ST_ScaleX(rast),ST_ScaleY(rast),ST_BandNoDataValue(rast,1),ST_Value(rast,1,1,1),ST_SRID(rast) FROM {}", table.sql()), &[]).await?;
                assert_eq!(report.tile_count, 1);
                assert_eq!(row.get::<_, f64>(0), report.schema.geo_transform[1]);
                assert_eq!(row.get::<_, f64>(1), report.schema.geo_transform[5]);
                assert_eq!(row.get::<_, Option<f64>>(2), nodata);
                assert_eq!(row.get::<_, f64>(3), 1.);
                assert_eq!(row.get::<_, i32>(4), target.unwrap_or(4326));
                if target.is_none() {
                    assert_eq!(row.get::<_, f64>(0), scale);
                    assert_eq!(row.get::<_, f64>(1), -scale);
                }
                if name == "precise" {
                    let catalog = db.pool.get().await?.query_one("SELECT scale_x,nodata_values FROM raster_columns WHERE r_table_schema=$1 AND r_table_name=$2", &[&db.schema, &name]).await?;
                    assert_ne!(catalog.get::<_, f64>(0), scale);
                    assert_ne!(catalog.get::<_, Vec<Option<f64>>>(1)[0], nodata);
                }
            }
            Ok::<_, anyhow::Error>(())
        }.await;
        db.cleanup().await?;
        result
    }
    #[tokio::test]
    #[ignore = "requires dedicated PostGIS Raster database"]
    async fn replace_rows_checks_precise_headers_after_all_triggers() -> anyhow::Result<()> {
        let db = TestDb::new().await?;
        let result = async {
            let file = tempfile::NamedTempFile::with_suffix(".tif")?;
            let mut dataset = gdal::DriverManager::get_driver_by_name("GTiff")?.create_with_band_type::<f64, _>(file.path(), 2, 2, 1)?;
            dataset.set_spatial_ref(&gdal::spatial_ref::SpatialRef::from_epsg(4326)?)?;
            dataset.set_geo_transform(&[0., 0.1, 0., 2., 0., -0.1])?;
            dataset.rasterband(1)?.set_no_data_value(Some(0.1234567890123))?;
            drop(dataset);
            let table = db.table("headers")?;
            let sql = table.sql();
            for deferred in [false, true] {
                PostGisRasterWriter::new(&db.client, table.clone()).mode(WriteMode::ReplaceTable).write(RasterSource::new(file.path()), &WriteControl::default()).await?;
                let action = if deferred { format!("UPDATE {sql} SET rast=ST_SetBandNoDataValue(rast,1,0.1234567890124) WHERE _id=NEW._id") } else { "NEW.rast := ST_SetBandNoDataValue(NEW.rast,1,0.1234567890124)".into() };
                let trigger = if deferred { "CONSTRAINT TRIGGER change_header AFTER INSERT" } else { "TRIGGER change_header BEFORE INSERT" };
                let deferrable = if deferred { "DEFERRABLE INITIALLY DEFERRED" } else { "" };
                db.pool.get().await?.batch_execute(&format!("CREATE OR REPLACE FUNCTION {}.change_header() RETURNS trigger LANGUAGE plpgsql AS $$BEGIN {action}; RETURN NEW; END$$; CREATE {trigger} ON {sql} {deferrable} FOR EACH ROW EXECUTE FUNCTION {}.change_header()", db.schema, db.schema)).await?;
                let error = PostGisRasterWriter::new(&db.client, table.clone()).mode(WriteMode::ReplaceRows).write(RasterSource::new(file.path()), &WriteControl::default()).await.unwrap_err();
                assert!(matches!(error.primary(), PostGisError::SchemaMismatch(_)), "{error:?}");
                let row = db.pool.get().await?.query_one(&format!("SELECT ST_BandNoDataValue(rast,1),(SELECT count(*) FROM {sql}) FROM {sql}"), &[]).await?;
                assert_eq!(row.get::<_, f64>(0), 0.1234567890123);
                assert_eq!(row.get::<_, i64>(1), 1);
            }
            let mut dataset = gdal::Dataset::open_ex(file.path(), gdal::DatasetOptions { open_flags: gdal::GdalOpenFlags::GDAL_OF_UPDATE, ..Default::default() })?;
            dataset.rasterband(1)?.set_no_data_value(None)?;
            dataset.flush_cache()?;
            drop(dataset);
            db.pool.get().await?.batch_execute(&format!("DROP TABLE {sql}; CREATE TABLE {sql}(_id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,rast raster NOT NULL)")).await?;
            for expression in [
                "ST_SetSRID(NEW.rast,3857)",
                "ST_SetScale(NEW.rast,0.1000000000001,-0.1)",
                "ST_SetSkew(NEW.rast,0.0000000000001,0)",
                "ST_AddBand(ST_MakeEmptyRaster(NEW.rast),'32BF'::text,1.0,NULL::float8)",
                "ST_AddBand(NEW.rast,'64BF'::text,1.0,NULL::float8)",
            ] {
                // A hand-created raster table has no AddRasterConstraints safety net.
                PostGisRasterWriter::new(&db.client, table.clone()).mode(WriteMode::ReplaceRows).write(RasterSource::new(file.path()), &WriteControl::default()).await?;
                let before: String = db.pool.get().await?.query_one(&format!("SELECT rast::text FROM {sql}"), &[]).await?.get(0);
                db.pool.get().await?.batch_execute(&format!("CREATE OR REPLACE FUNCTION {}.change_header() RETURNS trigger LANGUAGE plpgsql AS $$BEGIN NEW.rast := {expression}; RETURN NEW; END$$; CREATE TRIGGER change_header BEFORE INSERT ON {sql} FOR EACH ROW EXECUTE FUNCTION {}.change_header()", db.schema, db.schema)).await?;
                let error = PostGisRasterWriter::new(&db.client, table.clone()).mode(WriteMode::ReplaceRows).write(RasterSource::new(file.path()), &WriteControl::default()).await.unwrap_err();
                assert!(matches!(error.primary(), PostGisError::SchemaMismatch(_)), "{expression}: {error:?}");
                let after: String = db.pool.get().await?.query_one(&format!("SELECT rast::text FROM {sql}"), &[]).await?.get(0);
                assert_eq!(before, after);
                db.pool.get().await?.batch_execute(&format!("DROP TRIGGER change_header ON {sql}")).await?;
            }
            Ok::<_, anyhow::Error>(())
        }.await;
        db.cleanup().await?;
        result
    }
}
