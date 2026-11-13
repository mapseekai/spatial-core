use crate::postgis::{
    ColumnName, PostGisClient, PostGisError, Result, TableName, WriteControl, WriteStage,
    connection::Capabilities,
};

/// Table-wide raster metadata. Heterogeneity is explicit instead of sampling row 1.
#[derive(Debug, Clone)]
pub struct RasterMetadata {
    /// Total rows, including any NULL raster rows.
    pub tile_count: u64,
    /// Distinct non-NULL SRIDs.
    pub srids: Vec<i32>,
    /// Distinct band counts.
    pub band_counts: Vec<i32>,
    /// Whether dimensions, resolution, alignment or band metadata differ.
    pub heterogeneous: bool,
    /// Footprint bounds in the sole SRID, including NoData pixels.
    /// None for empty/all-NULL data or multiple SRIDs; unlike dimensions, CRS cannot be mixed.
    pub extent: Option<[f64; 4]>,
}

pub(crate) fn extent_sql(
    table: &TableName,
    raster: &ColumnName,
    caps: &Capabilities,
) -> Result<String> {
    let hull = format!(
        "{}({})",
        caps.raster_function("st_convexhull")?,
        raster.sql()
    );
    let extent = caps.geometry_function("st_extent");
    Ok(format!(
        "SELECT {}(e)::float8,{}(e)::float8,{}(e)::float8,{}(e)::float8 FROM (SELECT {extent}({hull}) AS e FROM {}) bounds",
        caps.geometry_function("st_xmin"),
        caps.geometry_function("st_ymin"),
        caps.geometry_function("st_xmax"),
        caps.geometry_function("st_ymax"),
        table.sql()
    ))
}

/// Inspect all raster rows in one snapshot, explicitly reporting heterogeneous data.
/// # Errors
/// Returns extension, type, permission, cancellation and database errors.
pub async fn read_metadata(
    db: &PostGisClient,
    table: &TableName,
    raster: &ColumnName,
    control: &WriteControl<'_>,
) -> Result<RasterMetadata> {
    let mut lease = db.acquire(&control.cancel).await?;
    let client = lease.client();
    let result = db.cancellable(&control.cancel, client.cancel_token(), async {
        client.batch_execute("BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY").await.map_err(database)?;
        client.batch_execute(&format!("LOCK TABLE {} IN ACCESS SHARE MODE", table.sql())).await.map_err(database)?;
        let caps = Capabilities::read(client, true).await?;
        caps.configure_transaction(client).await?;
        let f = |name| caps.raster_function(name);
        let r = raster.sql();
        let sql = format!("SELECT count(*),array_agg(DISTINCT {}({r})) FILTER(WHERE {r} IS NOT NULL),array_agg(DISTINCT {}({r})) FILTER(WHERE {r} IS NOT NULL),count(DISTINCT ROW({}({r}),{}({r}),{}({r}),{}({r}),{}({r}),{}({r}))) > 1 OR NOT COALESCE({}({r}),true) FROM {}", f("st_srid")?, f("st_numbands")?, f("st_width")?, f("st_height")?, f("st_scalex")?, f("st_scaley")?, f("st_skewx")?, f("st_skewy")?, f("st_samealignment")?, table.sql());
        let row = client.query_one(&sql, &[]).await.map_err(database)?;
        let srids: Vec<i32> = row.get::<_, Option<Vec<i32>>>(1).unwrap_or_default();
        let band_counts: Vec<i32> = row.get::<_, Option<Vec<i32>>>(2).unwrap_or_default();
        // Compare every band's type and NoData, without collecting tiles in memory.
        let bands = format!("SELECT EXISTS(SELECT 1 FROM (SELECT b,count(DISTINCT ROW({}({r},b),{}({r},b),{}({r},b) IS NOT NULL)) AS n FROM {} CROSS JOIN LATERAL generate_series(1,{}({r})) b GROUP BY b) x WHERE n>1)", f("st_bandpixeltype")?, f("st_bandnodatavalue")?, f("st_bandpath")?, table.sql(), f("st_numbands")?);
        let heterogeneous = row.get::<_, bool>(3) || srids.len() > 1 || band_counts.len() > 1 || client.query_one(&bands, &[]).await.map_err(database)?.get::<_, bool>(0);
        let extent = if srids.len() == 1 {
            let extent = client.query_one(&extent_sql(table, raster, &caps)?, &[]).await.map_err(database)?;
            extent.get::<_, Option<f64>>(0).map(|x| [x, extent.get(1), extent.get(2), extent.get(3)])
        } else {
            None
        };
        client.batch_execute("COMMIT").await.map_err(database)?;
        Ok(RasterMetadata { tile_count: row.get::<_, i64>(0) as u64, srids, band_counts, heterogeneous, extent })
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
    use crate::postgis::test_support::TestDb;

    #[tokio::test]
    #[ignore = "requires dedicated PostGIS Raster database"]
    async fn mixed_srids_have_no_extent_but_edge_tiles_do() -> anyhow::Result<()> {
        let db = TestDb::new().await?;
        let result = async {
            let table = db.table("tiles")?;
            let column = ColumnName::new("rast")?;
            let sql = table.sql();
            db.pool.get().await?.batch_execute(&format!("CREATE TABLE {sql}(rast raster); INSERT INTO {sql} VALUES (ST_MakeEmptyRaster(2,2,100,30,0.01,-0.01,0,0,4326)),(ST_MakeEmptyRaster(2,2,11131949,3503549,10,-10,0,0,3857)),(NULL)")).await?;
            let mixed = read_metadata(&db.client, &table, &column, &WriteControl::default()).await?;
            assert_eq!(mixed.srids, [3857, 4326]);
            assert_eq!(mixed.tile_count, 3);
            assert!(mixed.heterogeneous);
            assert_eq!(mixed.extent, None);
            db.pool.get().await?.batch_execute(&format!("TRUNCATE {sql}; INSERT INTO {sql} VALUES (ST_MakeEmptyRaster(2,2,100,30,0.01,-0.01,0,0,4326)),(ST_MakeEmptyRaster(1,2,100.02,30,0.01,-0.01,0,0,4326))")).await?;
            let edge = read_metadata(&db.client, &table, &column, &WriteControl::default()).await?;
            assert_eq!(edge.srids, [4326]);
            assert!(edge.heterogeneous);
            assert_eq!(edge.extent, Some([100., 29.98, 100.03, 30.]));
            db.pool.get().await?.batch_execute(&format!("TRUNCATE {sql}; INSERT INTO {sql} VALUES(NULL)")).await?;
            let null = read_metadata(&db.client, &table, &column, &WriteControl::default()).await?;
            assert_eq!(null.extent, None);
            assert_eq!(null.tile_count, 1);
            Ok::<_, anyhow::Error>(())
        }.await;
        db.cleanup().await?;
        result
    }
}
