use spatial_core::postgis::{
    PostGisClient, TableName, WriteControl, WriteMode,
    raster::{PostGisRasterWriter, RasterSource, TileLayout},
};
fn fixture(path: &std::path::Path, size: usize) -> anyhow::Result<()> {
    let mut ds = gdal::DriverManager::get_driver_by_name("GTiff")?
        .create_with_band_type::<f64, _>(path, size, size, 1)?;
    ds.set_spatial_ref(&gdal::spatial_ref::SpatialRef::from_epsg(4326)?)?;
    ds.set_geo_transform(&[0., 0.1, 0., 2., 0., -0.1])?;
    ds.rasterband(1)?.write(
        (0, 0),
        (size, size),
        &mut gdal::raster::Buffer::new((size, size), vec![1.; size * size]),
    )?;
    ds.flush_cache()?;
    Ok(())
}
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let pool = deadpool_postgres::Pool::builder(deadpool_postgres::Manager::new(
        std::env::var("SC_REVIEW_DSN")?.parse()?,
        tokio_postgres::NoTls,
    ))
    .max_size(1)
    .build()?;
    let db = PostGisClient::from_pool(pool.clone(), tokio_postgres::NoTls);
    let root = std::path::PathBuf::from(std::env::var("SC_REVIEW_FIXTURES")?);
    let original = root.join("final_data_origin_old.tif");
    let source = root.join("final_data_origin_new.tif");
    fixture(&original, 3)?;
    fixture(&source, 2)?;
    pool.get()
        .await?
        .batch_execute("CREATE SCHEMA IF NOT EXISTS review_final_data")
        .await?;
    let table = TableName::new("review_final_data", "origin_trigger")?;
    PostGisRasterWriter::new(&db, table.clone())
        .mode(WriteMode::ReplaceTable)
        .tile_layout(TileLayout::tiled(2, 2)?)
        .write(RasterSource::new(&original), &WriteControl::default())
        .await?;
    pool.get().await?.batch_execute("CREATE OR REPLACE FUNCTION review_final_data.move_origin() RETURNS trigger LANGUAGE plpgsql AS $$BEGIN NEW.rast := ST_SetUpperLeft(NEW.rast,ST_UpperLeftX(NEW.rast)+0.1,ST_UpperLeftY(NEW.rast)); RETURN NEW; END$$; CREATE TRIGGER move_origin BEFORE INSERT ON review_final_data.origin_trigger FOR EACH ROW EXECUTE FUNCTION review_final_data.move_origin()").await?;
    let result = PostGisRasterWriter::new(&db, table)
        .mode(WriteMode::ReplaceRows)
        .tile_layout(TileLayout::Whole)
        .write(RasterSource::new(&source), &WriteControl::default())
        .await;
    println!("origin_result={result:?}");
    let row=pool.get().await?.query_one("SELECT ST_UpperLeftX(rast),ST_UpperLeftY(rast),ST_Width(rast),ST_Height(rast) FROM review_final_data.origin_trigger LIMIT 1",&[]).await?;
    println!(
        "actual_origin={},{}; dimensions={},{}",
        row.get::<_, f64>(0),
        row.get::<_, f64>(1),
        row.get::<_, i32>(2),
        row.get::<_, i32>(3)
    );
    pool.get().await?.batch_execute("DROP TABLE review_final_data.origin_trigger; DROP FUNCTION review_final_data.move_origin()").await?;
    Ok(())
}
