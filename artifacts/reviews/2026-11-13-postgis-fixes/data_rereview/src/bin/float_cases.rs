use spatial_core::postgis::{
    PostGisClient, TableName, WriteControl, WriteMode,
    vector::{
        ColumnSchema, CoordinateDimension, GeometryColumn, GeometryKind, PostGisVectorWriter,
        PostgresType, VectorSchema, VectorSource,
    },
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
    pool.get()
        .await?
        .batch_execute("CREATE SCHEMA IF NOT EXISTS review_final_data")
        .await?;
    let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let fixture =
        std::path::PathBuf::from(std::env::var("SC_REVIEW_FIXTURES")?).join("float_case.geojson");
    let mut passed = 0;
    let mut failed = 0;
    for (index, line) in std::fs::read_to_string(root.join("float_cases.tsv"))?
        .lines()
        .enumerate()
    {
        let mut fields = line.splitn(3, '\t');
        let kind = fields.next().unwrap();
        let expected = fields.next().unwrap() == "1";
        let text = fields.next().unwrap();
        let datatype = if kind == "real" {
            PostgresType::Real
        } else {
            PostgresType::DoublePrecision
        };
        let schema = VectorSchema::new(
            vec![ColumnSchema::new("value", datatype)?],
            GeometryColumn::new("geom", GeometryKind::Point, CoordinateDimension::Xy, 4326)?,
        )?;
        std::fs::write(
            &fixture,
            format!(
                r#"{{"type":"FeatureCollection","features":[{{"type":"Feature","properties":{{"value":"{text}"}},"geometry":{{"type":"Point","coordinates":[1,2]}}}}]}}"#
            ),
        )?;
        let table = TableName::new("review_final_data", "float_probe")?;
        let result = PostGisVectorWriter::new(&db, table)
            .mode(WriteMode::ReplaceTable)
            .schema(schema)
            .write(VectorSource::new(&fixture), &WriteControl::default())
            .await;
        let matches = if result.is_ok() != expected {
            false
        } else if result.is_ok() {
            let actual = pool
                .get()
                .await?
                .query_one(
                    "SELECT value::float8 FROM review_final_data.float_probe",
                    &[],
                )
                .await?
                .get::<_, f64>(0);
            let parsed = text.parse::<f64>()?;
            let want = if kind == "real" {
                parsed as f32 as f64
            } else {
                parsed
            };
            actual.to_bits() == want.to_bits() || actual.is_nan() && want.is_nan()
        } else {
            true
        };
        if matches {
            passed += 1;
        } else {
            failed += 1;
            println!(
                "MISMATCH {index} type={kind} expected={expected} text={text} result={result:?}"
            );
        }
    }
    pool.get()
        .await?
        .batch_execute("DROP TABLE IF EXISTS review_final_data.float_probe")
        .await?;
    println!("float_public_api_cases: passed={passed} failed={failed}");
    anyhow::ensure!(failed == 0, "float conversion cases failed");
    Ok(())
}
