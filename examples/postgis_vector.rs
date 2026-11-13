//! Run only with SPATIAL_CORE_TEST_POSTGRES_DSN pointing at a disposable database.
use deadpool_postgres::{Manager, Pool};
use spatial_core::postgis::{
    PostGisClient, TableName, WriteControl,
    vector::{PostGisVectorWriter, VectorSource, VectorWriteOptions, read_metadata},
};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let dsn = std::env::var("SPATIAL_CORE_TEST_POSTGRES_DSN")?;
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
        let table = TableName::new(&schema, "points")?;
        let report = PostGisVectorWriter::new(&db, table.clone())
            .options(VectorWriteOptions::default().target_srid(3857))
            .write(
                VectorSource::new(concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/testdata/points.geojson"
                )),
                &WriteControl::default(),
            )
            .await?;
        let metadata = read_metadata(
            &db,
            &table,
            &report.schema.geometry.name,
            &WriteControl::default(),
        )
        .await?;
        assert_eq!(report.feature_count, 3);
        assert_eq!(metadata.feature_count, report.feature_count);
        assert_eq!(metadata.srid, Some(3857));
        println!(
            "Committed {} features; bounds {:?}",
            report.feature_count, report.extent
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
