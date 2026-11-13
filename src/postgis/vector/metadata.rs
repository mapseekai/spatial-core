use crate::postgis::{
    ColumnName, PostGisClient, PostGisError, Result, TableName, WriteControl, WriteStage,
    connection::Capabilities,
};

/// A database column's exact PostgreSQL type, without display-category folding.
#[derive(Debug, Clone)]
pub struct DatabaseColumn {
    /// Original column name.
    pub name: String,
    /// PostgreSQL format_type output, including modifiers and array types.
    pub data_type: String,
    /// Whether NULL is allowed.
    pub nullable: bool,
    /// Identity generation: empty, `a` (always), or `d` (by default).
    pub identity: String,
}

/// Metadata from one consistent database snapshot, not just the first feature.
#[derive(Debug, Clone)]
pub struct VectorMetadata {
    /// Ordered columns including identity and geometry.
    pub columns: Vec<DatabaseColumn>,
    /// Selected geometry column.
    pub geometry: ColumnName,
    /// Declared typmod family, or None for an unconstrained geometry column.
    pub geometry_type: Option<String>,
    /// Declared SRID, or None when the column is unconstrained.
    pub srid: Option<i32>,
    /// Declared coordinate dimension, or None for an unconstrained column.
    pub coordinate_dimension: Option<i32>,
    /// Exact row count.
    pub feature_count: u64,
    /// Bounds, absent when no geometry contributes coordinates.
    pub extent: Option<[f64; 4]>,
}

pub(crate) fn extent_sql(table: &TableName, geometry: &ColumnName, caps: &Capabilities) -> String {
    let expression = geometry.sql();
    let function = |name| caps.geometry_function(name);
    format!(
        "SELECT {}(e)::float8,{}(e)::float8,{}(e)::float8,{}(e)::float8 FROM (SELECT {}({expression}) AS e FROM {}) bounds",
        function("st_xmin"),
        function("st_ymin"),
        function("st_xmax"),
        function("st_ymax"),
        function("st_extent"),
        table.sql()
    )
}

/// Inspect columns, declared geometry type, count, and extent in one snapshot.
/// # Errors
/// Returns an error for an absent/non-geometry column, unavailable extension,
/// cancellation, or PostgreSQL failure. Never infers a table contract from row 1.
pub async fn read_metadata(
    db: &PostGisClient,
    table: &TableName,
    geometry: &ColumnName,
    control: &WriteControl<'_>,
) -> Result<VectorMetadata> {
    let mut lease = db.acquire(&control.cancel).await?;
    let client = lease.client();
    let result = db.cancellable(&control.cancel, client.cancel_token(), async {
        client.batch_execute("BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY").await.map_err(database)?;
        client.batch_execute(&format!("LOCK TABLE {} IN ACCESS SHARE MODE", table.sql())).await.map_err(database)?;
        let caps = Capabilities::read(client, false).await?;
        caps.configure_transaction(client).await?;
        let columns = client.query("SELECT a.attname,pg_catalog.format_type(a.atttypid,a.atttypmod),NOT a.attnotnull,a.attidentity::text FROM pg_catalog.pg_attribute a WHERE a.attrelid=pg_catalog.to_regclass($1) AND a.attnum>0 AND NOT a.attisdropped ORDER BY a.attnum", &[&table.sql()]).await.map_err(database)?
            .into_iter().map(|r| DatabaseColumn { name: r.get(0), data_type: r.get(1), nullable: r.get(2), identity: r.get(3) }).collect();
        let row = client.query_opt(&format!("SELECT a.atttypmod,{}(a.atttypmod),{}(a.atttypmod),{}(a.atttypmod) FROM pg_catalog.pg_attribute a WHERE a.attrelid=pg_catalog.to_regclass($1) AND a.attname=$2 AND NOT a.attisdropped AND a.atttypid=pg_catalog.to_regtype($3)", caps.geometry_function("postgis_typmod_type"), caps.geometry_function("postgis_typmod_srid"), caps.geometry_function("postgis_typmod_dims")), &[&table.sql(), &geometry.as_str(), &caps.geometry_function("geometry")]).await.map_err(database)?
            .ok_or_else(|| PostGisError::SchemaMismatch("geometry column was not found".into()))?;
        let constrained = row.get::<_, i32>(0) >= 0;
        let count = client.query_one(&format!("SELECT count(*) FROM {}", table.sql()), &[]).await.map_err(database)?.get::<_, i64>(0);
        let extent = client.query_one(&extent_sql(table, geometry, &caps), &[]).await.map_err(database)?;
        let extent = extent.get::<_, Option<f64>>(0).map(|x| [x, extent.get(1), extent.get(2), extent.get(3)]);
        client.batch_execute("COMMIT").await.map_err(database)?;
        Ok(VectorMetadata { columns, geometry: geometry.clone(), geometry_type: constrained.then(|| row.get(1)), srid: constrained.then(|| row.get(2)), coordinate_dimension: constrained.then(|| row.get(3)), feature_count: count as u64, extent })
    }).await;
    if result.is_ok() {
        lease.reusable();
    }
    result
}

fn database(error: tokio_postgres::Error) -> PostGisError {
    PostGisError::database(WriteStage::Finalizing, error)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::postgis::test_support::{TestDb, TestProxy};
    use std::time::Duration;

    #[tokio::test]
    #[ignore = "requires dedicated PostGIS test database"]
    async fn metadata_blocks_replacement_until_its_snapshot_is_complete() -> anyhow::Result<()> {
        let db = TestDb::new().await?;
        let result = async {
            let table = db.table("metadata")?;
            let sql = table.sql();
            db.pool.get().await?.batch_execute(&format!("CREATE TABLE {sql}(old_col integer,geom geometry(Point,4326)); INSERT INTO {sql} VALUES(1,ST_SetSRID(ST_Point(12,55),4326))")).await?;
            let mut proxy = TestProxy::new(Some(format!("SELECT count(*) FROM {sql}")), false).await?;
            let client = PostGisClient::from_pool(proxy.pool.clone(), tokio_postgres::NoTls);
            let target = table.clone();
            let read = tokio::spawn(async move { read_metadata(&client, &target, &ColumnName::new("geom")?, &WriteControl::default()).await });
            tokio::time::timeout(Duration::from_secs(10), &mut proxy.reached).await??;
            let (ddl, driver) = tokio_postgres::connect(&std::env::var("SPATIAL_CORE_TEST_POSTGRES_DSN")?, tokio_postgres::NoTls).await?;
            let driver = tokio::spawn(driver);
            let pid = ddl.query_one("SELECT pg_backend_pid()", &[]).await?.get::<_, i32>(0);
            let replacement = format!("BEGIN; DROP TABLE {sql}; CREATE TABLE {sql}(new_col text,geom geometry(Point,3857)); INSERT INTO {sql} VALUES('new',ST_SetSRID(ST_Point(1000,2000),3857)); COMMIT");
            let replacing = tokio::spawn(async move { ddl.batch_execute(&replacement).await });
            let observer = db.pool.get().await?;
            let blocked = tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    anyhow::ensure!(!replacing.is_finished(), "replacement ran between metadata queries");
                    if observer.query_one("SELECT EXISTS(SELECT 1 FROM pg_locks WHERE pid=$1 AND mode='AccessExclusiveLock' AND NOT granted)", &[&pid]).await?.get::<_, bool>(0) { return Ok::<_, anyhow::Error>(()); }
                    tokio::task::yield_now().await;
                }
            }).await;
            proxy.resume();
            let old = tokio::time::timeout(Duration::from_secs(5), read).await???;
            tokio::time::timeout(Duration::from_secs(5), replacing).await???;
            driver.abort();
            blocked??;
            assert_eq!(old.columns[0].name, "old_col");
            assert_eq!(old.srid, Some(4326));
            assert_eq!(old.feature_count, 1);
            assert_eq!(old.extent, Some([12., 55., 12., 55.]));
            drop(observer);
            let new = read_metadata(&db.client, &table, &ColumnName::new("geom")?, &WriteControl::default()).await?;
            assert_eq!(new.columns[0].name, "new_col");
            assert_eq!(new.srid, Some(3857));
            assert_eq!(new.feature_count, 1);
            assert_eq!(new.extent, Some([1000., 2000., 1000., 2000.]));
            Ok::<_, anyhow::Error>(())
        }.await;
        db.cleanup().await?;
        result
    }
}
