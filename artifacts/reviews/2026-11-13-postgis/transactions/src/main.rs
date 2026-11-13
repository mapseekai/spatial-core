use deadpool_postgres::{Manager, Pool};
use spatial_core::{
    control::Cancellation,
    postgis::{
        ColumnName, PostGisClient, PostGisError, TableName, WriteControl, WriteMode,
        vector::{PostGisVectorWriter, VectorSource, read_metadata},
    },
};
use std::{
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::{Notify, oneshot},
};
const SCHEMA: &str = "review_transactions";
const SOURCE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../../../testdata/points.geojson"
);
fn config() -> anyhow::Result<tokio_postgres::Config> {
    Ok(std::env::var("SC_REVIEW_DSN")?.parse()?)
}
fn pool(config: tokio_postgres::Config) -> anyhow::Result<Pool> {
    Ok(Pool::builder(Manager::new(config, tokio_postgres::NoTls))
        .max_size(1)
        .build()?)
}
fn proxied_config(port: u16) -> anyhow::Result<tokio_postgres::Config> {
    let original = config()?;
    let mut c = tokio_postgres::Config::new();
    c.host("127.0.0.1")
        .port(port)
        .user(original.get_user().unwrap())
        .password(original.get_password().unwrap())
        .dbname(original.get_dbname().unwrap())
        .ssl_mode(tokio_postgres::config::SslMode::Disable);
    Ok(c)
}
async fn observer() -> anyhow::Result<tokio_postgres::Client> {
    let (c, driver) = config()?.connect(tokio_postgres::NoTls).await?;
    tokio::spawn(async move {
        let _ = driver.await;
    });
    Ok(c)
}
async fn initial(db: &PostGisClient, name: &str) -> anyhow::Result<()> {
    PostGisVectorWriter::new(db, TableName::new(SCHEMA, name)?)
        .mode(WriteMode::ReplaceTable)
        .write(VectorSource::new(SOURCE), &WriteControl::default())
        .await?;
    Ok(())
}
fn main() -> anyhow::Result<()> {
    let case = std::env::args().nth(1).unwrap_or_default();
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .max_blocking_threads(if case == "starvation" {
            1
        } else if case == "starvation_control" {
            2
        } else {
            512
        })
        .enable_all()
        .build()?
        .block_on(async {
            match case.as_str() {
                "triggers" => triggers().await,
                "cancel" => cancel_failure().await,
                "metadata" => metadata_race().await,
                "starvation" | "starvation_control" => starvation().await,
                _ => anyhow::bail!("select triggers, cancel, metadata or starvation"),
            }
        })
}
async fn triggers() -> anyhow::Result<()> {
    let pool = pool(config()?)?;
    let db = PostGisClient::from_pool(pool.clone(), tokio_postgres::NoTls);
    let observer = observer().await?;
    for deferred in [false, true] {
        initial(&db, "trigger_target").await?;
        observer.batch_execute("CREATE OR REPLACE FUNCTION review_transactions.delete_written_row() RETURNS trigger LANGUAGE plpgsql AS $$BEGIN DELETE FROM review_transactions.trigger_target WHERE _id=NEW._id; RETURN NEW; END$$").await?;
        let trigger = if deferred {
            "CREATE CONSTRAINT TRIGGER erase_new AFTER INSERT ON review_transactions.trigger_target DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION review_transactions.delete_written_row()"
        } else {
            "CREATE TRIGGER erase_new AFTER INSERT ON review_transactions.trigger_target FOR EACH ROW EXECUTE FUNCTION review_transactions.delete_written_row()"
        };
        observer.batch_execute(trigger).await?;
        let report = PostGisVectorWriter::new(&db, TableName::new(SCHEMA, "trigger_target")?)
            .mode(WriteMode::ReplaceRows)
            .write(VectorSource::new(SOURCE), &WriteControl::default())
            .await?;
        let actual = observer
            .query_one(
                "SELECT count(*),ST_Extent(geom)::text FROM review_transactions.trigger_target",
                &[],
            )
            .await?;
        println!(
            "deferred={deferred}: successful report count={}, extent={:?}; actual rows={}, extent={:?}",
            report.feature_count,
            report.extent,
            actual.get::<_, i64>(0),
            actual.get::<_, Option<String>>(1)
        );
    }
    observer.batch_execute("DROP TABLE review_transactions.trigger_target; DROP FUNCTION review_transactions.delete_written_row()").await?;
    Ok(())
}
async fn cancel_failure() -> anyhow::Result<()> {
    let observer = observer().await?;
    let direct_pool = pool(config()?)?;
    let direct = PostGisClient::from_pool(direct_pool, tokio_postgres::NoTls);
    initial(&direct, "cancel_target").await?;
    observer.batch_execute("CREATE FUNCTION review_transactions.delay_copy() RETURNS trigger LANGUAGE plpgsql AS $$BEGIN PERFORM pg_sleep(2); RETURN NEW; END$$; CREATE TRIGGER delay_copy BEFORE INSERT ON review_transactions.cancel_target FOR EACH ROW EXECUTE FUNCTION review_transactions.delay_copy()").await?;
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    let backend_port = config()?.get_ports()[0];
    let proxy = tokio::spawn(async move {
        let (mut client, _) = listener.accept().await?;
        drop(listener);
        let mut server = TcpStream::connect(("127.0.0.1", backend_port)).await?;
        tokio::io::copy_bidirectional(&mut client, &mut server).await?;
        Ok::<_, std::io::Error>(())
    });
    let pool = pool(proxied_config(port)?)?;
    let db = PostGisClient::from_pool(pool.clone(), tokio_postgres::NoTls);
    let pid = pool
        .get()
        .await?
        .query_one("SELECT pg_backend_pid()", &[])
        .await?
        .get::<_, i32>(0);
    let cancel = Cancellation::default();
    let control = WriteControl::new(cancel.clone());
    let mut write = tokio::spawn(async move {
        PostGisVectorWriter::new(&db, TableName::new(SCHEMA, "cancel_target").unwrap())
            .mode(WriteMode::ReplaceRows)
            .write(VectorSource::new(SOURCE), &control)
            .await
    });
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let row = observer
                .query_one(
                    "SELECT wait_event FROM pg_stat_activity WHERE pid=$1",
                    &[&pid],
                )
                .await?;
            if row.get::<_, Option<&str>>(0) == Some("PgSleep") {
                return Ok::<_, tokio_postgres::Error>(());
            }
            tokio::task::yield_now().await;
        }
    })
    .await??;
    let start = Instant::now();
    cancel.cancel();
    let blocked = tokio::time::timeout(Duration::from_secs(1), &mut write)
        .await
        .is_err();
    println!(
        "Cancellation is still waiting after 1 second despite cancel connection being refused: {blocked}"
    );
    if !blocked {
        anyhow::bail!("unexpected fast completion; cannot await twice");
    }
    let error = tokio::time::timeout(Duration::from_secs(15), write)
        .await??
        .unwrap_err();
    println!(
        "Final cancellation elapsed={} ms, primary cancelled={}, full error={error:?}",
        start.elapsed().as_millis(),
        matches!(error.primary(), PostGisError::Cancelled)
    );
    println!(
        "Old rows after eventual rollback={}",
        observer
            .query_one(
                "SELECT count(*) FROM review_transactions.cancel_target",
                &[]
            )
            .await?
            .get::<_, i64>(0)
    );
    observer.batch_execute("DROP TABLE review_transactions.cancel_target; DROP FUNCTION review_transactions.delay_copy()").await?;
    drop(pool);
    proxy.abort();
    Ok(())
}
async fn metadata_race() -> anyhow::Result<()> {
    let observer = observer().await?;
    observer.batch_execute("CREATE TABLE review_transactions.meta_target(old_col integer,geom geometry(Point,4326)); INSERT INTO review_transactions.meta_target VALUES(1,ST_SetSRID(ST_Point(12,55),4326))").await?;
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    let backend_port = config()?.get_ports()[0];
    let (reached, at_count) = oneshot::channel();
    let gate = Arc::new(Notify::new());
    let release = gate.clone();
    let proxy = tokio::spawn(async move {
        let (client, _) = listener.accept().await?;
        drop(listener);
        let server = TcpStream::connect(("127.0.0.1", backend_port)).await?;
        let (mut cr, mut cw) = client.into_split();
        let (mut sr, mut sw) = server.into_split();
        let responses = tokio::spawn(async move { tokio::io::copy(&mut sr, &mut cw).await });
        let len = cr.read_u32().await?;
        let mut startup = vec![0u8; len as usize - 4];
        cr.read_exact(&mut startup).await?;
        sw.write_u32(len).await?;
        sw.write_all(&startup).await?;
        let mut reached = Some(reached);
        loop {
            let kind = match cr.read_u8().await {
                Ok(k) => k,
                Err(_) => break,
            };
            let len = cr.read_u32().await?;
            let mut body = vec![0u8; len as usize - 4];
            cr.read_exact(&mut body).await?;
            let needle = b"SELECT count(*) FROM \"review_transactions\".\"meta_target\"";
            if kind == b'P' && body.windows(needle.len()).any(|w| w == needle) && reached.is_some()
            {
                let _ = reached.take().unwrap().send(());
                gate.notified().await;
            }
            sw.write_u8(kind).await?;
            sw.write_u32(len).await?;
            sw.write_all(&body).await?;
        }
        responses.abort();
        Ok::<_, std::io::Error>(())
    });
    let pool = pool(proxied_config(port)?)?;
    let db = PostGisClient::from_pool(pool.clone(), tokio_postgres::NoTls);
    let read = tokio::spawn(async move {
        read_metadata(
            &db,
            &TableName::new(SCHEMA, "meta_target").unwrap(),
            &ColumnName::new("geom").unwrap(),
            &WriteControl::default(),
        )
        .await
    });
    tokio::time::timeout(Duration::from_secs(10), at_count).await??;
    observer.batch_execute("BEGIN; DROP TABLE review_transactions.meta_target; CREATE TABLE review_transactions.meta_target(new_col text,geom geometry(Point,3857)); INSERT INTO review_transactions.meta_target VALUES('new',ST_SetSRID(ST_Point(1000,2000),3857)); COMMIT").await?;
    release.notify_one();
    let result = tokio::time::timeout(Duration::from_secs(10), read).await??;
    println!("Metadata result across concurrent replacement: {result:?}");
    let row=observer.query_one("SELECT count(*),ST_SRID(geom) FROM review_transactions.meta_target GROUP BY ST_SRID(geom)",&[]).await?;
    println!(
        "Actual replacement table: rows={}, srid={}, attribute=new_col:text",
        row.get::<_, i64>(0),
        row.get::<_, i32>(1)
    );
    observer
        .batch_execute("DROP TABLE review_transactions.meta_target")
        .await?;
    drop(pool);
    proxy.abort();
    Ok(())
}

async fn starvation() -> anyhow::Result<()> {
    let features=(0..100).map(|i|format!(r#"{{"type":"Feature","properties":{{"value":{i}}},"geometry":{{"type":"Point","coordinates":[12,55]}}}}"#)).collect::<Vec<_>>().join(",");
    let path =
        std::path::PathBuf::from(std::env::var("SC_REVIEW_FIXTURES")?).join("hundred.geojson");
    std::fs::write(
        &path,
        format!(r#"{{"type":"FeatureCollection","features":[{features}]}}"#),
    )?;
    let pool = pool(config()?)?;
    let db = PostGisClient::from_pool(pool, tokio_postgres::NoTls);
    let cancel = Cancellation::default();
    let control = WriteControl::new(cancel.clone());
    let mut write = tokio::spawn(async move {
        PostGisVectorWriter::new(&db, TableName::new(SCHEMA, "starvation_target").unwrap())
            .mode(WriteMode::ReplaceTable)
            .write(VectorSource::new(path), &control)
            .await
    });
    let start = Instant::now();
    let first = tokio::time::timeout(Duration::from_secs(3), &mut write).await;
    let blocked = first.is_err();
    println!("100-feature import still pending after 3s: {blocked}");
    if let Ok(result) = first {
        println!(
            "Completed in {} ms: {result:?}",
            start.elapsed().as_millis()
        );
    }
    if blocked {
        cancel.cancel();
        let result = tokio::time::timeout(Duration::from_secs(5), write).await??;
        println!(
            "After explicit cancellation ({} ms): {result:?}",
            start.elapsed().as_millis()
        );
    }
    let observer = observer().await?;
    observer
        .batch_execute("DROP TABLE IF EXISTS review_transactions.starvation_target")
        .await?;
    Ok(())
}
