use super::{
    PostGisClient, PostGisError, Result, TableName, WriteControl, WriteMode, WriteStage,
    connection::{CLEANUP_TIMEOUT, Capabilities},
    options::TargetOptions,
    sql::quote,
    stream::SourceStream,
};
use deadpool_postgres::Object;
use futures::SinkExt;
use std::pin::pin;

pub(crate) struct TableColumn {
    pub name: String,
    pub sql_type: String,
    pub nullable: bool,
    pub identity: bool,
}

pub(crate) struct TablePlan {
    pub columns: Vec<TableColumn>,
    pub srid: i32,
    pub allow_empty: bool,
    pub total: Option<u64>,
    pub finalize: Vec<FinalSql>,
    pub verify: Option<FinalSql>,
    pub extent_sql: String,
}

pub(crate) enum FinalSql {
    Execute(String),
    Check {
        sql: String,
        parameters: Vec<String>,
    },
}

impl FinalSql {
    async fn execute(&self, client: &Object) -> Result<()> {
        match self {
            Self::Execute(sql) => client
                .batch_execute(sql)
                .await
                .map_err(|e| PostGisError::database(WriteStage::Finalizing, e)),
            Self::Check { sql, parameters } => {
                let parameters = parameters
                    .iter()
                    .map(|value| value as &(dyn tokio_postgres::types::ToSql + Sync))
                    .collect::<Vec<_>>();
                let row = client
                    .query_one(sql, &parameters)
                    .await
                    .map_err(|e| PostGisError::database(WriteStage::Finalizing, e))?;
                if !row.get::<_, bool>(0) {
                    return Err(PostGisError::SchemaMismatch(
                        "database finalization check failed".into(),
                    ));
                }
                Ok(())
            }
        }
    }
}

impl TablePlan {
    fn create_sql(&self, table: &TableName) -> String {
        let columns = self
            .columns
            .iter()
            .map(|column| {
                format!(
                    "{} {}{}{}",
                    quote(&column.name),
                    column.sql_type,
                    if column.identity {
                        " GENERATED ALWAYS AS IDENTITY PRIMARY KEY"
                    } else {
                        ""
                    },
                    if !column.nullable && !column.identity {
                        " NOT NULL"
                    } else {
                        ""
                    }
                )
            })
            .collect::<Vec<_>>()
            .join(",");
        format!("CREATE TABLE {} ({columns})", table.sql())
    }
    fn copy_sql(&self, table: &TableName) -> String {
        let columns = self
            .columns
            .iter()
            .filter(|c| !c.identity)
            .map(|c| quote(&c.name))
            .collect::<Vec<_>>()
            .join(",");
        format!(
            "COPY {} ({columns}) FROM STDIN WITH (FORMAT text)",
            table.sql()
        )
    }
}

fn preparing(error: tokio_postgres::Error) -> PostGisError {
    PostGisError::database(WriteStage::Preparing, error)
}

async fn target_oid(client: &Object, table: &TableName) -> Result<Option<u32>> {
    Ok(client
        .query_one("SELECT pg_catalog.to_regclass($1)::oid", &[&table.sql()])
        .await
        .map_err(preparing)?
        .get(0))
}

async fn validate_target(client: &Object, oid: u32) -> Result<()> {
    let row = client.query_one("SELECT relkind::text,relispartition,relrowsecurity,EXISTS(SELECT 1 FROM pg_catalog.pg_inherits WHERE inhrelid=c.oid OR inhparent=c.oid) FROM pg_catalog.pg_class c WHERE c.oid=$1", &[&oid]).await.map_err(preparing)?;
    if row.get::<_, &str>(0) != "r" || row.get::<_, bool>(1) || row.get::<_, bool>(3) {
        return Err(PostGisError::Unsupported(
            "target must be an ordinary table without partitions or inheritance".into(),
        ));
    }
    if row.get::<_, bool>(2) {
        return Err(PostGisError::Unsupported(
            "RLS targets are not supported".into(),
        ));
    }
    Ok(())
}

async fn prepare(
    client: &Object,
    table: &TableName,
    options: &TargetOptions,
    plan: &TablePlan,
) -> Result<bool> {
    let timeout = options.lock_timeout.as_millis();
    if timeout == 0 || timeout > i32::MAX as u128 {
        return Err(PostGisError::InvalidInput(
            "lock timeout must be 1..=2147483647 milliseconds".into(),
        ));
    }
    client.query_one("SELECT pg_catalog.set_config('lock_timeout',$1,true),pg_catalog.set_config('DateStyle','ISO, YMD',true),pg_catalog.set_config('TimeZone','UTC',true)", &[&format!("{timeout}ms")]).await.map_err(preparing)?;
    client
        .query_one(
            "SELECT pg_catalog.pg_advisory_xact_lock($1)",
            &[&table.lock_key()],
        )
        .await
        .map_err(preparing)?;
    if options.create_schema {
        client
            .batch_execute(&format!(
                "CREATE SCHEMA IF NOT EXISTS {}",
                quote(table.schema())
            ))
            .await
            .map_err(preparing)?;
    }
    let exists = target_oid(client, table).await?;
    if exists.is_some() && options.mode == WriteMode::Create {
        return Err(PostGisError::SchemaMismatch("target already exists".into()));
    }
    if exists.is_none() && options.mode == WriteMode::ReplaceRows {
        return Err(PostGisError::SchemaMismatch(
            "ReplaceRows requires an existing target".into(),
        ));
    }
    if let Some(oid) = exists {
        validate_target(client, oid).await?;
        // Lock first, then recheck: advisory locks cannot exclude external DDL.
        client
            .batch_execute(&format!(
                "LOCK TABLE ONLY {} IN ACCESS EXCLUSIVE MODE",
                table.sql()
            ))
            .await
            .map_err(preparing)?;
        if target_oid(client, table).await? != Some(oid) {
            return Err(PostGisError::SchemaMismatch(
                "target changed during lock acquisition".into(),
            ));
        }
        validate_target(client, oid).await?;
        if options.mode == WriteMode::ReplaceRows {
            validate_columns(client, oid, &plan.columns).await?;
            client
                .batch_execute(&format!(
                    "TRUNCATE TABLE ONLY {} CONTINUE IDENTITY RESTRICT",
                    table.sql()
                ))
                .await
                .map_err(preparing)?;
            return Ok(false);
        }
        client
            .batch_execute(&format!("DROP TABLE {} RESTRICT", table.sql()))
            .await
            .map_err(preparing)?;
    }
    client
        .batch_execute(&plan.create_sql(table))
        .await
        .map_err(preparing)?;
    Ok(true)
}

async fn validate_columns(client: &Object, oid: u32, expected: &[TableColumn]) -> Result<()> {
    let rows = client.query("SELECT a.attname,pg_catalog.format_type(a.atttypid,a.atttypmod),a.attnotnull,a.attidentity::text,a.attgenerated::text,EXISTS(SELECT 1 FROM pg_catalog.pg_index i WHERE i.indrelid=a.attrelid AND i.indisprimary AND i.indisvalid AND i.indnkeyatts=1 AND i.indkey[0]=a.attnum) FROM pg_catalog.pg_attribute a WHERE a.attrelid=$1 AND a.attnum>0 AND NOT a.attisdropped ORDER BY a.attnum", &[&oid]).await.map_err(preparing)?;
    if rows.len() != expected.len() {
        return Err(PostGisError::SchemaMismatch("column count differs".into()));
    }
    for (row, column) in rows.iter().zip(expected) {
        // Resolve qualification through PostgreSQL itself, including extension types.
        let type_row = client
            .query_one(
                &format!(
                    "SELECT pg_catalog.format_type(pg_catalog.pg_typeof(NULL::{})::oid, NULL)",
                    column.sql_type
                ),
                &[],
            )
            .await
            .map_err(preparing)?;
        let base: String = type_row.get(0);
        let suffix = column
            .sql_type
            .find('(')
            .map(|i| &column.sql_type[i..])
            .unwrap_or("");
        let canonical = if suffix.is_empty() {
            base
        } else {
            // timestamp's precision precedes its timezone suffix.
            if column.sql_type.starts_with("timestamp") {
                column.sql_type.clone()
            } else {
                format!("{base}{suffix}")
            }
        };
        if row.get::<_, &str>(0) != column.name
            || row.get::<_, &str>(1) != canonical
            || row.get::<_, bool>(2) == column.nullable
            || row.get::<_, &str>(3) != if column.identity { "a" } else { "" }
            || !row.get::<_, &str>(4).is_empty()
            || (column.identity && !row.get::<_, bool>(5))
        {
            return Err(PostGisError::SchemaMismatch(format!(
                "column {} does not match the write schema",
                column.name
            )));
        }
    }
    Ok(())
}

/// Shared transaction protocol; schemas and encoders remain in their domain modules.
pub(crate) async fn write<M: Send + 'static>(
    db: &PostGisClient,
    table: &TableName,
    options: &TargetOptions,
    start: impl FnOnce() -> SourceStream<M>,
    raster: bool,
    control: &WriteControl<'_>,
    plan: impl FnOnce(&M, &Capabilities) -> Result<TablePlan>,
) -> Result<(M, u64, Option<[f64; 4]>)> {
    control.emit(WriteStage::Preparing, 0, None);
    // Cold pool DNS/TLS may need a blocking thread; connect before occupying one with the source.
    let mut lease = db.acquire(&control.cancel).await?;
    let mut stream = start();
    let metadata = tokio::select! {
        biased;
        _ = control.cancel.cancelled() => Err(PostGisError::Cancelled),
        metadata = stream.metadata() => metadata,
    };
    let metadata = match metadata {
        Ok(metadata) => metadata,
        Err(error) => {
            lease.reusable(); // No SQL or cancellation packet has been sent on this connection.
            return Err(match stream.stop().await {
                Ok(()) => error,
                Err(cleanup) => error.with_cleanup(cleanup),
            });
        }
    };
    let client = lease.client();
    let operation = async {
        let capabilities = Capabilities::read(client, raster).await?;
        let plan = plan(&metadata, &capabilities)?;
        stream
            .validate_srid(capabilities.srid_definition(client, plan.srid).await?)
            .await?;
        client.batch_execute("BEGIN").await.map_err(preparing)?;
        capabilities.configure_transaction(client).await?;
        let created = prepare(client, table, options, &plan).await?;
        let sink = client
            .copy_in(&plan.copy_sql(table))
            .await
            .map_err(|e| PostGisError::database(WriteStage::Copying, e))?;
        let mut sink = pin!(sink);
        let mut sent = 0u64;
        while let Some(row) = stream.rows.recv().await {
            sink.as_mut()
                .send(row.bytes.clone())
                .await
                .map_err(|e| PostGisError::database(WriteStage::Copying, e))?;
            sent += 1;
            if sent == 1 || sent.is_multiple_of(256) {
                control.emit(WriteStage::Copying, sent, plan.total);
            }
        }
        let produced = stream.finish().await?;
        if produced != sent {
            return Err(PostGisError::InvalidInput(
                "source row count disagrees with transfer count".into(),
            ));
        }
        let copied = sink
            .finish()
            .await
            .map_err(|e| PostGisError::database(WriteStage::Copying, e))?;
        if copied != sent {
            return Err(PostGisError::SchemaMismatch(
                "COPY row count changed (possibly a filtering trigger)".into(),
            ));
        }
        if sent == 0 && !plan.allow_empty {
            return Err(PostGisError::InvalidInput(
                "empty input is not allowed".into(),
            ));
        }
        control.emit(WriteStage::Finalizing, sent, plan.total);
        // Deferred triggers must run before the final count and bounds become a success report.
        client
            .batch_execute("SET CONSTRAINTS ALL IMMEDIATE")
            .await
            .map_err(|e| PostGisError::database(WriteStage::Finalizing, e))?;
        if created {
            for step in &plan.finalize {
                step.execute(client).await?;
            }
        }
        // DDL event triggers can also mutate rows or enqueue deferred effects.
        client
            .batch_execute("SET CONSTRAINTS ALL IMMEDIATE")
            .await
            .map_err(|e| PostGisError::database(WriteStage::Finalizing, e))?;
        if let Some(verify) = &plan.verify {
            verify.execute(client).await?;
        }
        client
            .batch_execute(&format!("ANALYZE {}", table.sql()))
            .await
            .map_err(|e| PostGisError::database(WriteStage::Finalizing, e))?;
        let stored = client
            .query_one(&format!("SELECT count(*) FROM {}", table.sql()), &[])
            .await
            .map_err(|e| PostGisError::database(WriteStage::Finalizing, e))?
            .get::<_, i64>(0);
        if stored as u64 != sent {
            return Err(PostGisError::SchemaMismatch(
                "final row count differs from the imported count after triggers".into(),
            ));
        }
        let extent = client
            .query_one(&plan.extent_sql, &[])
            .await
            .map_err(|e| PostGisError::database(WriteStage::Finalizing, e))?;
        let extent = extent
            .get::<_, Option<f64>>(0)
            .map(|x| [x, extent.get(1), extent.get(2), extent.get(3)]);
        if control.cancel.is_cancelled() {
            return Err(PostGisError::Cancelled);
        }
        Ok((sent, plan.total, extent))
    };
    let result = db
        .cancellable(&control.cancel, client.cancel_token(), operation)
        .await;
    match result {
        Ok((records, total, extent)) => {
            // Never race COMMIT against cancellation or return an unknown connection.
            control.emit(WriteStage::Committing, records, total);
            match client.batch_execute("COMMIT").await {
                Ok(()) => {
                    lease.reusable();
                    control.emit(WriteStage::Completed, records, total);
                    Ok((metadata, records, extent))
                }
                Err(error) if error.as_db_error().is_some_and(|e| e.severity() == "ERROR") => {
                    Err(PostGisError::database(WriteStage::Committing, error))
                }
                Err(error) => Err(PostGisError::CommitOutcomeUnknown(error)),
            }
        }
        Err(mut error) => {
            if matches!(error, PostGisError::Cleanup { .. })
                && matches!(error.primary(), PostGisError::Cancelled)
            {
                // A failed CancelRequest leaves SQL running; queuing ROLLBACK could wait forever.
                drop(lease);
                return Err(match stream.stop().await {
                    Ok(()) => error,
                    Err(cleanup) => error.with_cleanup(cleanup),
                });
            }
            if let Err(cleanup) = stream.stop().await {
                error = error.with_cleanup(cleanup);
            }
            match tokio::time::timeout(CLEANUP_TIMEOUT, client.batch_execute("ROLLBACK")).await {
                Ok(Ok(())) => {
                    // A cancel packet is not acknowledged; never risk a late packet on a reused backend.
                    if !matches!(error.primary(), PostGisError::Cancelled) {
                        lease.reusable();
                    }
                }
                Ok(Err(cleanup)) => {
                    error =
                        error.with_cleanup(PostGisError::database(WriteStage::Finalizing, cleanup))
                }
                Err(_) => error = error.with_cleanup(PostGisError::CleanupTimeout),
            }
            Err(error)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::postgis::{
        test_support::{TestDb, points},
        vector::{PostGisVectorWriter, VectorSource, VectorWriteOptions},
    };
    use std::time::Duration;

    #[tokio::test]
    #[ignore = "requires dedicated PostGIS test database"]
    async fn concurrent_writers_serialize_per_target_without_blocking_other_tables()
    -> anyhow::Result<()> {
        let db = TestDb::new().await?;
        let dsn = std::env::var("SPATIAL_CORE_TEST_POSTGRES_DSN")?;
        let (observer, connection) = tokio_postgres::connect(&dsn, tokio_postgres::NoTls).await?;
        let driver = tokio::spawn(connection);
        let mut config: tokio_postgres::Config = dsn.parse()?;
        config.application_name(&db.schema);
        let pool = deadpool_postgres::Pool::builder(deadpool_postgres::Manager::new(
            config,
            tokio_postgres::NoTls,
        ))
        .max_size(3)
        .build()?;
        let client = PostGisClient::from_pool(pool.clone(), tokio_postgres::NoTls);
        let result = async {
            let table = db.table("same_target")?;
            let key = table.lock_key();
            observer.query_one("SELECT pg_advisory_lock($1)", &[&key]).await?;
            let spawn_write = || {
                let client = client.clone(); let table = table.clone();
                tokio::spawn(async move { PostGisVectorWriter::new(&client, table).mode(WriteMode::ReplaceTable).write(VectorSource::new(points()), &WriteControl::default()).await })
            };
            let first = spawn_write(); let second = spawn_write();
            tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    let count = observer.query_one("SELECT count(*) FROM pg_locks l JOIN pg_stat_activity a ON a.pid=l.pid WHERE a.application_name=$1 AND l.locktype='advisory' AND NOT l.granted", &[&db.schema]).await?.get::<_, i64>(0);
                    if count == 2 { return Ok::<_, tokio_postgres::Error>(()); }
                    tokio::task::yield_now().await;
                }
            }).await??;
            let other = tokio::time::timeout(Duration::from_secs(5), PostGisVectorWriter::new(&client, db.table("other_target")?).write(VectorSource::new(points()), &WriteControl::default())).await??;
            assert_eq!(other.feature_count, 3);
            observer.query_one("SELECT pg_advisory_unlock($1)", &[&key]).await?;
            assert_eq!(tokio::time::timeout(Duration::from_secs(5), first).await???.feature_count, 3);
            assert_eq!(tokio::time::timeout(Duration::from_secs(5), second).await???.feature_count, 3);
            assert_eq!(observer.query_one(&format!("SELECT count(*) FROM {}", table.sql()), &[]).await?.get::<_, i64>(0), 3);
            Ok::<_, anyhow::Error>(())
        }.await;
        drop(observer);
        driver.abort();
        drop(pool);
        db.cleanup().await?;
        result
    }

    #[tokio::test]
    #[ignore = "requires dedicated PostGIS test database"]
    async fn lock_timeout_rolls_back_and_the_pool_remains_usable() -> anyhow::Result<()> {
        let db = TestDb::new().await?;
        let (blocker, connection) = tokio_postgres::connect(
            &std::env::var("SPATIAL_CORE_TEST_POSTGRES_DSN")?,
            tokio_postgres::NoTls,
        )
        .await?;
        let driver = tokio::spawn(connection);
        let result = async {
            db.vector("points")?
                .write(VectorSource::new(points()), &WriteControl::default())
                .await?;
            blocker
                .batch_execute(&format!(
                    "BEGIN; LOCK TABLE {}.points IN ACCESS EXCLUSIVE MODE",
                    db.schema
                ))
                .await?;
            let options = VectorWriteOptions::default()
                .mode(WriteMode::ReplaceRows)
                .lock_timeout(Duration::from_millis(30));
            let error = db
                .vector("points")?
                .options(options)
                .write(VectorSource::new(points()), &WriteControl::default())
                .await
                .unwrap_err();
            match error.primary() {
                PostGisError::Database { source, .. } => {
                    assert_eq!(source.code().map(|c| c.code()), Some("55P03"))
                }
                error => panic!("unexpected lock error: {error:?}"),
            }
            blocker.batch_execute("ROLLBACK").await?;
            db.vector("points")?
                .options(TestDb::replace())
                .write(VectorSource::new(points()), &WriteControl::default())
                .await?;
            Ok::<_, anyhow::Error>(())
        }
        .await;
        drop(blocker);
        driver.abort();
        db.cleanup().await?;
        result
    }

    #[tokio::test]
    #[ignore = "requires dedicated PostGIS test database"]
    async fn cancellation_at_commit_does_not_override_a_confirmed_commit() -> anyhow::Result<()> {
        let db = TestDb::new().await?;
        let result=async {
            let cancel=crate::control::Cancellation::default();
            let callback=|progress:super::super::WriteProgress| { if progress.stage == WriteStage::Committing { cancel.cancel(); } };
            let report=db.vector("points")?.write(VectorSource::new(points()),&WriteControl::new(cancel.clone()).on_progress(&callback)).await?;
            assert!(cancel.is_cancelled()); assert_eq!(report.feature_count,3);
            db.pool.get().await?.batch_execute(&format!("CREATE FUNCTION {s}.reject_commit() RETURNS trigger LANGUAGE plpgsql AS $$BEGIN RAISE EXCEPTION 'deferred rejection'; END$$; CREATE CONSTRAINT TRIGGER reject_commit AFTER INSERT ON {s}.points DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION {s}.reject_commit()",s=db.schema)).await?;
            let error=db.vector("points")?.mode(WriteMode::ReplaceRows).write(VectorSource::new(points()),&WriteControl::default()).await.unwrap_err();
            assert!(matches!(error.primary(),PostGisError::Database{stage:WriteStage::Finalizing,..}));
            assert_eq!(db.pool.get().await?.query_one(&format!("SELECT count(*) FROM {}.points",db.schema),&[]).await?.get::<_,i64>(0),3);
            Ok::<_,anyhow::Error>(())
        }.await;
        db.cleanup().await?;
        result
    }

    #[tokio::test]
    #[ignore = "requires dedicated PostGIS database with backend termination permission"]
    async fn disconnect_during_commit_returns_unknown_and_discards_the_connection()
    -> anyhow::Result<()> {
        use crate::postgis::test_support::TestProxy;
        let db = TestDb::new().await?;
        let result = async {
            db.vector("points")?
                .write(VectorSource::new(points()), &WriteControl::default())
                .await?;
            let mut proxy = TestProxy::new(Some("COMMIT".into()), false).await?;
            let pid = proxy
                .pool
                .get()
                .await?
                .query_one("SELECT pg_backend_pid()", &[])
                .await?
                .get::<_, i32>(0);
            let client = PostGisClient::from_pool(proxy.pool.clone(), tokio_postgres::NoTls);
            let table = db.table("points")?;
            let write = tokio::spawn(async move {
                PostGisVectorWriter::new(&client, table)
                    .mode(WriteMode::ReplaceRows)
                    .write(VectorSource::new(points()), &WriteControl::default())
                    .await
            });
            tokio::time::timeout(Duration::from_secs(10), &mut proxy.reached).await??;
            // COMMIT has left the client, but its response is lost when the backend dies.
            assert!(
                db.pool
                    .get()
                    .await?
                    .query_one("SELECT pg_terminate_backend($1)", &[&pid])
                    .await?
                    .get::<_, bool>(0)
            );
            proxy.resume();
            let error = tokio::time::timeout(Duration::from_secs(5), write)
                .await??
                .unwrap_err();
            assert!(
                matches!(error.primary(), PostGisError::CommitOutcomeUnknown(_)),
                "{error:?}"
            );
            let new_pid = proxy
                .pool
                .get()
                .await?
                .query_one("SELECT pg_backend_pid()", &[])
                .await?
                .get::<_, i32>(0);
            assert_ne!(pid, new_pid);
            Ok::<_, anyhow::Error>(())
        }
        .await;
        db.cleanup().await?;
        result
    }

    #[test]
    #[ignore = "requires dedicated PostGIS Raster database"]
    fn both_writers_finish_with_one_blocking_thread() -> anyhow::Result<()> {
        use crate::postgis::{
            raster::{PostGisRasterWriter, RasterSource, TileLayout},
            test_support::{TestProxy, json_file},
        };
        tokio::runtime::Builder::new_current_thread().max_blocking_threads(1).enable_all().build()?.block_on(async {
            let db = TestDb::new().await?;
            let result = tokio::time::timeout(Duration::from_secs(10), async {
                let features = (0..100).map(|i| format!(r#"{{"type":"Feature","properties":{{"value":{i}}},"geometry":{{"type":"Point","coordinates":[12,55]}}}}"#)).collect::<Vec<_>>().join(",");
                let file = json_file(&format!(r#"{{"type":"FeatureCollection","features":[{features}]}}"#))?;
                let vector_proxy = TestProxy::new(None, false).await?;
                let vector_client = PostGisClient::from_pool(vector_proxy.pool.clone(), tokio_postgres::NoTls);
                let report = PostGisVectorWriter::new(&vector_client, db.table("points")?).write(VectorSource::new(file.path()), &WriteControl::default()).await?;
                assert_eq!(report.feature_count, 100);
                let raster = tempfile::NamedTempFile::with_suffix(".tif")?;
                let mut dataset = gdal::DriverManager::get_driver_by_name("GTiff")?.create_with_band_type::<u8, _>(raster.path(), 8, 8, 1)?;
                dataset.set_spatial_ref(&gdal::spatial_ref::SpatialRef::from_epsg(4326)?)?;
                dataset.set_geo_transform(&[0., 0.1, 0., 2., 0., -0.1])?;
                drop(dataset);
                let raster_proxy = TestProxy::new(None, false).await?;
                let raster_client = PostGisClient::from_pool(raster_proxy.pool.clone(), tokio_postgres::NoTls);
                let report = PostGisRasterWriter::new(&raster_client, db.table("tiles")?).tile_layout(TileLayout::tiled(1, 1)?).write(RasterSource::new(raster.path()), &WriteControl::default()).await?;
                assert_eq!(report.tile_count, 64);
                let rows = db.pool.get().await?.query_one(&format!("SELECT (SELECT count(*) FROM {}.points),(SELECT count(*) FROM {}.tiles)", db.schema, db.schema), &[]).await?;
                assert_eq!(rows.get::<_, i64>(0), 100);
                assert_eq!(rows.get::<_, i64>(1), 64);
                Ok::<_, anyhow::Error>(())
            }).await;
            db.cleanup().await?;
            result?
        })
    }

    #[tokio::test]
    #[ignore = "requires dedicated PostGIS test database"]
    async fn after_and_deferred_triggers_are_checked_before_reporting_success() -> anyhow::Result<()>
    {
        use crate::postgis::{ColumnName, vector::read_metadata};
        let db = TestDb::new().await?;
        let result = async {
            let table = db.table("points")?;
            let sql = table.sql();
            for deferred in [false, true] {
                for delete in [true, false] {
                    let initial = db.vector("points")?.mode(WriteMode::ReplaceTable).write(VectorSource::new(points()), &WriteControl::default()).await?;
                    let before: Vec<String> = db.pool.get().await?.query_one(&format!("SELECT array_agg(row_to_json(t)::text ORDER BY _id) FROM {sql} t"), &[]).await?.get(0);
                    let action = if delete { format!("DELETE FROM {sql} WHERE _id=NEW._id") } else { format!("UPDATE {sql} SET geom=ST_Translate(geom,1,0) WHERE _id=NEW._id") };
                    let constraint = if deferred { "CONSTRAINT " } else { "" };
                    let deferrable = if deferred { "DEFERRABLE INITIALLY DEFERRED " } else { "" };
                    db.pool.get().await?.batch_execute(&format!("CREATE OR REPLACE FUNCTION {}.change_rows() RETURNS trigger LANGUAGE plpgsql AS $$BEGIN {action}; RETURN NEW; END$$; CREATE {constraint}TRIGGER change_rows AFTER INSERT ON {sql} {deferrable}FOR EACH ROW EXECUTE FUNCTION {}.change_rows()", db.schema, db.schema)).await?;
                    let result = db.vector("points")?.mode(WriteMode::ReplaceRows).write(VectorSource::new(points()), &WriteControl::default()).await;
                    if delete {
                        assert!(matches!(result.unwrap_err().primary(), PostGisError::SchemaMismatch(_)));
                        let after: Vec<String> = db.pool.get().await?.query_one(&format!("SELECT array_agg(row_to_json(t)::text ORDER BY _id) FROM {sql} t"), &[]).await?.get(0);
                        assert_eq!(after, before);
                    } else {
                        let report = result?;
                        let actual = read_metadata(&db.client, &table, &ColumnName::new("geom")?, &WriteControl::default()).await?;
                        let mut expected = initial.extent.unwrap(); expected[0] += 1.; expected[2] += 1.;
                        assert_eq!(report.feature_count, 3);
                        assert_eq!(report.feature_count, actual.feature_count);
                        assert_eq!(report.extent, actual.extent);
                        assert_eq!(report.extent, Some(expected));
                    }
                }
            }
            Ok::<_, anyhow::Error>(())
        }.await;
        db.cleanup().await?;
        result
    }
    #[tokio::test]
    #[ignore = "requires dedicated PostGIS database with backend termination permission"]
    async fn failed_cancel_connection_discards_without_waiting_for_running_sql()
    -> anyhow::Result<()> {
        use crate::{control::Cancellation, postgis::test_support::TestProxy};
        let db = TestDb::new().await?;
        let result = async {
            db.vector("points")?.write(VectorSource::new(points()), &WriteControl::default()).await?;
            let sql = db.table("points")?.sql();
            db.pool.get().await?.batch_execute(&format!("CREATE FUNCTION {}.slow_copy() RETURNS trigger LANGUAGE plpgsql AS $$BEGIN PERFORM pg_sleep(30); RETURN NEW; END$$; CREATE TRIGGER slow_copy BEFORE INSERT ON {sql} FOR EACH ROW EXECUTE FUNCTION {}.slow_copy()", db.schema, db.schema)).await?;
            let proxy = TestProxy::new(None, true).await?;
            let pid = proxy.pool.get().await?.query_one("SELECT pg_backend_pid()", &[]).await?.get::<_, i32>(0);
            let client = PostGisClient::from_pool(proxy.pool.clone(), tokio_postgres::NoTls);
            let table = db.table("points")?;
            let cancel = Cancellation::default();
            let control = WriteControl::new(cancel.clone());
            let write = tokio::spawn(async move { PostGisVectorWriter::new(&client, table).mode(WriteMode::ReplaceRows).write(VectorSource::new(points()), &control).await });
            let observer = db.pool.get().await?;
            tokio::time::timeout(Duration::from_secs(10), async {
                loop {
                    if observer.query_one("SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE pid=$1 AND wait_event='PgSleep')", &[&pid]).await?.get::<_, bool>(0) { return Ok::<_, tokio_postgres::Error>(()); }
                    tokio::task::yield_now().await;
                }
            }).await??;
            cancel.cancel();
            let result = tokio::time::timeout(Duration::from_secs(2), write).await;
            // A closed socket does not promise immediate server interruption; stop only our test backend.
            observer.query_one("SELECT pg_terminate_backend($1)", &[&pid]).await?;
            let error = result??.unwrap_err();
            assert!(matches!(error.primary(), PostGisError::Cancelled));
            assert!(matches!(&error, PostGisError::Cleanup { .. }), "{error:?}");
            assert_eq!(proxy.pool.status().size, 0);
            assert_eq!(observer.query_one(&format!("SELECT count(*) FROM {sql}"), &[]).await?.get::<_, i64>(0), 3);
            observer.batch_execute(&format!("DROP TRIGGER slow_copy ON {sql}")).await?;
            drop(observer);
            db.vector("points")?.mode(WriteMode::ReplaceRows).write(VectorSource::new(points()), &WriteControl::default()).await?;
            Ok::<_, anyhow::Error>(())
        }.await;
        db.cleanup().await?;
        result
    }
    #[tokio::test]
    #[ignore = "requires dedicated PostGIS database with event trigger privilege"]
    async fn ddl_trigger_row_changes_are_checked_after_finalization() -> anyhow::Result<()> {
        let db = TestDb::new().await?;
        let result = async {
            let table = db.table("points")?;
            let sql = table.sql();
            db.vector("points")?.write(VectorSource::new(points()), &WriteControl::default()).await?;
            let before: Vec<String> = db.pool.get().await?.query_one(&format!("SELECT array_agg(row_to_json(t)::text ORDER BY _id) FROM {sql} t"), &[]).await?.get(0);
            let trigger = format!("{}_index", db.schema);
            db.pool.get().await?.batch_execute(&format!(r#"
                CREATE FUNCTION {schema}.on_index() RETURNS event_trigger LANGUAGE plpgsql AS $$
                BEGIN
                    IF EXISTS(SELECT 1 FROM pg_event_trigger_ddl_commands() d
                        JOIN pg_catalog.pg_index i ON i.indexrelid=d.objid
                        WHERE d.classid='pg_catalog.pg_class'::regclass AND i.indrelid=to_regclass('{sql}')) THEN
                        INSERT INTO {sql}(name,value,geom) VALUES('ddl',999,ST_SetSRID(ST_Point(100,30),4326));
                    END IF;
                END$$;
                CREATE EVENT TRIGGER {trigger} ON ddl_command_end WHEN TAG IN ('CREATE INDEX')
                    EXECUTE FUNCTION {schema}.on_index();
            "#, schema=db.schema)).await?;
            let result = db.vector("points")?.mode(WriteMode::ReplaceTable).write(VectorSource::new(points()), &WriteControl::default()).await;
            db.pool.get().await?.batch_execute(&format!("DROP EVENT TRIGGER {trigger}")).await?;
            assert!(matches!(result.unwrap_err().primary(), PostGisError::SchemaMismatch(_)));
            let after: Vec<String> = db.pool.get().await?.query_one(&format!("SELECT array_agg(row_to_json(t)::text ORDER BY _id) FROM {sql} t"), &[]).await?.get(0);
            assert_eq!(before, after);
            Ok::<_, anyhow::Error>(())
        }.await;
        db.cleanup().await?;
        result
    }
}
