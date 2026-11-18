use spatial_core::duckdb::{Control, Reader, Session, SessionConfig, Value, VectorSource};

fn main() -> anyhow::Result<()> {
    let extensions = std::env::var("SPATIAL_CORE_DUCKDB_EXTENSION_DIR")?;
    let directory = tempfile::tempdir()?;
    let mut config = SessionConfig::new(extensions, directory.path());
    config.spatial_public_key = Some(std::fs::read_to_string(std::env::var(
        "SPATIAL_CORE_DUCKDB_SPATIAL_PUBLIC_KEY",
    )?)?);
    let mut session = Session::open(config)?;
    let capabilities = session.capabilities()?;
    println!(
        "DuckDB {} / system GDAL {} / {} Spatial drivers",
        capabilities.engine_version,
        gdal::version_info("RELEASE_NAME"),
        capabilities.drivers.len()
    );
    let control = Control {
        max_rows: Some(1000),
        max_bytes: Some(8 * 1024 * 1024),
        ..Control::default()
    };
    let source = directory.path().join("points.geojson");
    std::fs::write(
        &source,
        r#"{"type":"FeatureCollection","features":[{"type":"Feature","properties":{"id":1},"geometry":{"type":"Point","coordinates":[120,30]}}]}"#,
    )?;
    session.register(
        "points",
        &VectorSource::local(source, Reader::Gdal),
        &control,
    )?;
    let output = directory.path().join("buffers.parquet");
    let report = session.query(
        "SELECT id, ST_Buffer(ST_Transform(geom, 'EPSG:3857'), ?) AS geom FROM points WHERE id >= ?",
        [Value::Double(100.0), Value::Int(1)],
    ).write_geoparquet(&output, &control)?;
    anyhow::ensure!(report.rows == 1, "unexpected feature count");
    anyhow::ensure!(
        report.metadata.geometry_columns[0].crs.is_some(),
        "CRS was lost"
    );
    session.register(
        "buffers",
        &VectorSource::local(output, Reader::GeoParquet),
        &control,
    )?;
    let streamed = session
        .query("SELECT id, ST_Area(geom) AS area_m2 FROM buffers", [])
        .for_each_batch(&control, |batch| {
            println!("{} rows, {} fields", batch.num_rows(), batch.num_columns());
            Ok(())
        })?;
    anyhow::ensure!(streamed.rows == 1, "stream did not complete");
    session.close()?;
    directory.close()?;
    Ok(())
}
