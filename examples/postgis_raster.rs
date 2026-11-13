//! Run only with SPATIAL_CORE_TEST_POSTGRES_DSN pointing at a disposable database.
use deadpool_postgres::{Manager, Pool};
use spatial_core::postgis::{
    PostGisClient, TableName, WriteControl,
    raster::{PostGisRasterWriter, RasterSource, TileLayout},
};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let dsn = std::env::var("SPATIAL_CORE_TEST_POSTGRES_DSN")?;
    let file = tempfile::NamedTempFile::with_suffix(".tif")?;
    let mut dataset = gdal::DriverManager::get_driver_by_name("GTiff")?
        .create_with_band_type::<u16, _>(file.path(), 2, 2, 1)?;
    dataset.set_spatial_ref(&gdal::spatial_ref::SpatialRef::from_epsg(4326)?)?;
    dataset.set_geo_transform(&[100., 0.1, 0., 30., 0., -0.1])?;
    dataset.rasterband(1)?.write(
        (0, 0),
        (2, 2),
        &mut gdal::raster::Buffer::new((2, 2), vec![10, 20, 30, 40]),
    )?;
    dataset.flush_cache()?;
    drop(dataset);

    let pool = Pool::builder(Manager::new(dsn.parse()?, tokio_postgres::NoTls))
        .max_size(1)
        .build()?;
    let db = PostGisClient::from_pool(pool.clone(), tokio_postgres::NoTls);
    let schema = format!("example_{}", uuid::Uuid::new_v4().simple());
    pool.get()
        .await?
        .batch_execute(&format!("CREATE SCHEMA {schema}"))
        .await?;
    let result = async {
        let report = PostGisRasterWriter::new(&db, TableName::new(&schema, "tiles")?)
            .tile_layout(TileLayout::tiled(1, 1)?)
            .write(RasterSource::new(file.path()), &WriteControl::default())
            .await?;
        assert_eq!(report.tile_count, 4);
        let sum = pool
            .get()
            .await?
            .query_one(
                &format!("SELECT sum(ST_Value(rast,1,1,1)) FROM {schema}.tiles"),
                &[],
            )
            .await?
            .get::<_, f64>(0);
        assert_eq!(sum, 100.);
        println!(
            "Committed {} tiles; bounds {:?}",
            report.tile_count, report.extent
        );
        Ok::<_, anyhow::Error>(())
    }
    .await;
    pool.get()
        .await?
        .batch_execute(&format!("DROP SCHEMA {schema} CASCADE"))
        .await?;
    result
}
