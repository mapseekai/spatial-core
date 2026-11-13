use spatial_core::postgis::{
    ColumnName, PostGisClient, TableName, WriteControl, WriteMode,
    raster::{PostGisRasterWriter, RasterSource, RasterWriteOptions, TileLayout},
    vector::{PostGisVectorWriter, VectorSource},
};
use std::path::Path;
fn fixture(path: &Path, scale: f64, nodata: Option<f64>) -> anyhow::Result<()> {
    let mut dataset = gdal::DriverManager::get_driver_by_name("GTiff")?
        .create_with_band_type::<f64, _>(path, 2, 2, 1)?;
    dataset.set_spatial_ref(&gdal::spatial_ref::SpatialRef::from_epsg(4326)?)?;
    dataset.set_geo_transform(&[0., scale, 0., 2., 0., -scale])?;
    dataset.rasterband(1)?.write(
        (0, 0),
        (2, 2),
        &mut gdal::raster::Buffer::new((2, 2), vec![1., 2., 3., 4.]),
    )?;
    dataset.rasterband(1)?.set_no_data_value(nodata)?;
    dataset.flush_cache()?;
    Ok(())
}
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let dsn = std::env::var("SC_REVIEW_DSN")?;
    let fixtures = std::path::PathBuf::from(std::env::var("SC_REVIEW_FIXTURES")?);
    let pool = deadpool_postgres::Pool::builder(deadpool_postgres::Manager::new(
        dsn.parse()?,
        tokio_postgres::NoTls,
    ))
    .max_size(1)
    .build()?;
    let db = PostGisClient::from_pool(pool.clone(), tokio_postgres::NoTls);
    pool.get()
        .await?
        .batch_execute("CREATE SCHEMA IF NOT EXISTS review_data")
        .await?;
    for (name, scale, nodata) in [
        ("dr_control", 0.1, None),
        ("dr_scale", 0.0000235176759123, None),
        ("dr_nodata", 0.1, Some(0.1234567890123)),
    ] {
        let path = fixtures.join(format!("{name}.tif"));
        fixture(&path, scale, nodata)?;
        let result = PostGisRasterWriter::new(&db, TableName::new("review_data", name)?)
            .mode(WriteMode::ReplaceTable)
            .tile_layout(TileLayout::Whole)
            .write(RasterSource::new(&path), &WriteControl::default())
            .await;
        println!("{name}: {result:?}");
    }
    let result = PostGisRasterWriter::new(&db, TableName::new("review_data", "dr_cloud")?)
        .mode(WriteMode::ReplaceTable)
        .tile_layout(TileLayout::Whole)
        .write(
            RasterSource::new(fixtures.join("review_data_cloud64.tif")),
            &WriteControl::default(),
        )
        .await;
    println!("dr_cloud_real_64x64: {result:?}");
    let vsizip = format!(
        "/vsizip/{}/points.geojson",
        fixtures.join("review_data_points.zip").display()
    );
    println!(
        "direct_gdal_zip_open: {:?}",
        gdal::Dataset::open(&vsizip).map(|d| d.layer_count())
    );
    let result = PostGisVectorWriter::new(&db, TableName::new("review_data", "dr_zip")?)
        .mode(WriteMode::ReplaceTable)
        .write(VectorSource::new(vsizip), &WriteControl::default())
        .await;
    println!("library_zip_open: {result:?}");

    let shp_zip = format!(
        "/vsizip/{}/points.shp",
        fixtures.join("review_data_shp.zip").display()
    );
    println!(
        "direct_gdal_shp_zip_member: {:?}",
        gdal::Dataset::open(&shp_zip).map(|d| d.layer_count())
    );
    let result = PostGisVectorWriter::new(&db, TableName::new("review_data", "dr_shp_zip_member")?)
        .mode(WriteMode::ReplaceTable)
        .write(VectorSource::new(shp_zip), &WriteControl::default())
        .await;
    println!("library_shp_zip_member: {result:?}");
    let shp_zip_root = format!("/vsizip/{}", fixtures.join("review_data_shp.zip").display());
    let result = PostGisVectorWriter::new(&db, TableName::new("review_data", "dr_shp_zip_root")?)
        .mode(WriteMode::ReplaceTable)
        .write(VectorSource::new(shp_zip_root), &WriteControl::default())
        .await;
    println!("library_shp_zip_root_control: {result:?}");
    pool.get().await?.batch_execute("DROP TABLE IF EXISTS review_data.dr_catalog;CREATE TABLE review_data.dr_catalog AS SELECT ST_AddBand(ST_MakeEmptyRaster(2,2,0,2,0.0000235176759123,-0.0000235176759123,0,0,4326),'64BF'::text,1::float8,0.1234567890123::float8) rast;SELECT AddRasterConstraints('review_data'::name,'dr_catalog'::name,'rast'::name,VARIADIC ARRAY['scale_x','scale_y','nodata_values']);").await?;
    let rows=pool.get().await?.query("SELECT ST_ScaleX(rast)::text,scale_x::text,ST_BandNoDataValue(rast,1)::text,nodata_values::text FROM review_data.dr_catalog CROSS JOIN raster_columns WHERE r_table_schema='review_data' AND r_table_name='dr_catalog'",&[]).await?;
    for row in rows {
        println!(
            "catalog_values: actual_scale={}, catalog_scale={}, actual_nodata={}, catalog_nodata={}",
            row.get::<_, String>(0),
            row.get::<_, String>(1),
            row.get::<_, String>(2),
            row.get::<_, String>(3)
        );
    }
    pool.get().await?.batch_execute("DROP TABLE IF EXISTS review_data.dr_mixed;CREATE TABLE review_data.dr_mixed(rast raster);INSERT INTO review_data.dr_mixed VALUES(ST_MakeEmptyRaster(2,2,100,30,0.01,-0.01,0,0,4326)),(ST_MakeEmptyRaster(2,2,11131949,3503549,10,-10,0,0,3857));").await?;
    let metadata = spatial_core::postgis::raster::read_metadata(
        &db,
        &TableName::new("review_data", "dr_mixed")?,
        &ColumnName::new("rast")?,
        &WriteControl::default(),
    )
    .await;
    println!("mixed_srid_metadata: {metadata:?}");

    pool.get()
        .await?
        .batch_execute(
            "SET postgis.enable_outdb_rasters=on; SET postgis.gdal_enabled_drivers='GTiff'",
        )
        .await?;
    for (name, reference) in [
        (
            "dr_outdb_good",
            fixtures.join("dr_control.tif").display().to_string(),
        ),
        (
            "dr_outdb_missing",
            fixtures.join("does-not-exist.tif").display().to_string(),
        ),
    ] {
        let options = RasterWriteOptions::default()
            .mode(WriteMode::ReplaceTable)
            .tile_layout(TileLayout::Whole)
            .storage(spatial_core::postgis::raster::RasterStorage::OutDb {
                server_path: reference,
            });
        let result = PostGisRasterWriter::new(&db, TableName::new("review_data", name)?)
            .options(options)
            .write(
                RasterSource::new(fixtures.join("dr_control.tif")),
                &WriteControl::default(),
            )
            .await;
        println!("{name}: {result:?}");
        if result.is_ok() {
            let result = pool
                .get()
                .await?
                .query_one(
                    &format!("SELECT ST_Value(rast,1,1,1) FROM review_data.{name}"),
                    &[],
                )
                .await
                .map(|row| row.get::<_, Option<f64>>(0));
            println!("{name}_read_pixel: {result:?}");
        }
    }

    {
        use spatial_core::postgis::vector::{
            ColumnSchema, CoordinateDimension, GeometryColumn, GeometryKind, PostgresType,
            VectorSchema,
        };
        let schema = VectorSchema::new(
            vec![ColumnSchema::new("value", PostgresType::DoublePrecision)?],
            GeometryColumn::new("geom", GeometryKind::Point, CoordinateDimension::Xy, 4326)?,
        )?;
        let result = PostGisVectorWriter::new(&db, TableName::new("review_data", "dr_text_float")?)
            .mode(WriteMode::ReplaceTable)
            .schema(schema)
            .write(
                VectorSource::new(fixtures.join("review_data_float_text.geojson")),
                &WriteControl::default(),
            )
            .await;
        println!("double_precision_text_input: {result:?}");
        if result.is_ok() {
            let rows = pool
                .get()
                .await?
                .query(
                    "SELECT value::text FROM review_data.dr_text_float ORDER BY _id",
                    &[],
                )
                .await?;
            println!(
                "double_precision_text_readback: {:?}",
                rows.iter()
                    .map(|r| r.get::<_, String>(0))
                    .collect::<Vec<_>>()
            );
        }
    }
    // Explicit reprojection of a valid small source uses the same catalog contract.
    let result = PostGisRasterWriter::new(&db, TableName::new("review_data", "dr_warp")?)
        .options(
            RasterWriteOptions::default()
                .mode(WriteMode::ReplaceTable)
                .target_srid(3857),
        )
        .write(
            RasterSource::new(fixtures.join("dr_control.tif")),
            &WriteControl::default(),
        )
        .await;
    println!("warp_3857: {result:?}");
    Ok(())
}
