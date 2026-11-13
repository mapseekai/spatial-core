use spatial_core::postgis::{
    PostGisClient, TableName, WriteControl, WriteMode,
    raster::{PostGisRasterWriter, RasterSource, TileLayout},
};
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let pool = deadpool_postgres::Pool::builder(deadpool_postgres::Manager::new(
        std::env::var("SC_REVIEW_DSN")?.parse()?,
        tokio_postgres::NoTls,
    ))
    .max_size(1)
    .build()?;
    let db = PostGisClient::from_pool(pool.clone(), tokio_postgres::NoTls);
    let fixture = std::path::PathBuf::from(std::env::var("SC_REVIEW_FIXTURES")?)
        .join("final_data_nodata.tif");
    let mut ds = gdal::DriverManager::get_driver_by_name("GTiff")?
        .create_with_band_type::<f64, _>(&fixture, 2, 2, 1)?;
    ds.set_spatial_ref(&gdal::spatial_ref::SpatialRef::from_epsg(4326)?)?;
    ds.set_geo_transform(&[0., 0.1, 0., 2., 0., -0.1])?;
    ds.rasterband(1)?.set_no_data_value(Some(0.1234567890123))?;
    ds.rasterband(1)?.write(
        (0, 0),
        (2, 2),
        &mut gdal::raster::Buffer::new((2, 2), vec![1., 2., 3., 4.]),
    )?;
    ds.flush_cache()?;
    drop(ds);
    pool.get()
        .await?
        .batch_execute("CREATE SCHEMA IF NOT EXISTS review_final_data")
        .await?;
    let table = TableName::new("review_final_data", "nodata_trigger")?;
    let first = PostGisRasterWriter::new(&db, table.clone())
        .mode(WriteMode::ReplaceTable)
        .tile_layout(TileLayout::Whole)
        .write(RasterSource::new(&fixture), &WriteControl::default())
        .await?;
    println!("control reported_nodata={:?}", first.schema.bands[0].nodata);
    pool.get().await?.batch_execute("CREATE OR REPLACE FUNCTION review_final_data.change_nodata() RETURNS trigger LANGUAGE plpgsql AS $$BEGIN NEW.rast := ST_SetBandNoDataValue(NEW.rast,1,0.1234567890124::float8); RETURN NEW; END$$; CREATE TRIGGER change_nodata BEFORE INSERT ON review_final_data.nodata_trigger FOR EACH ROW EXECUTE FUNCTION review_final_data.change_nodata()").await?;
    let result = PostGisRasterWriter::new(&db, table)
        .mode(WriteMode::ReplaceRows)
        .tile_layout(TileLayout::Whole)
        .write(RasterSource::new(&fixture), &WriteControl::default())
        .await;
    println!("replace_result={result:?}");
    let row=pool.get().await?.query_one("SELECT ST_BandNoDataValue(rast,1),nodata_values::text FROM review_final_data.nodata_trigger CROSS JOIN raster_columns WHERE r_table_schema='review_final_data' AND r_table_name='nodata_trigger'",&[]).await?;
    println!(
        "actual_nodata={:?}, catalog_nodata={}",
        row.get::<_, Option<f64>>(0),
        row.get::<_, String>(1)
    );
    pool.get().await?.batch_execute("DROP TABLE review_final_data.nodata_trigger; DROP FUNCTION review_final_data.change_nodata()").await?;
    Ok(())
}
