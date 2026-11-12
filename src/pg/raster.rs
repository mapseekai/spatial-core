use super::{
    Cancellation, GenericClient, Object, PgCopy, RasterCopyBatchStats, RasterCopyRow, Result,
    finish_database_result, quote_ident, run_with_cancellation,
};
use anyhow::Context;
use bytes::Bytes;
use futures::SinkExt;
use std::pin::pin;
use tokio::{sync::mpsc, task::JoinHandle};

impl PgCopy {
    pub async fn ensure_raster_table<C>(client: &C, schema: &str, table: &str) -> Result<()>
    where
        C: GenericClient + Sync,
    {
        let schema_sql = format!("CREATE SCHEMA IF NOT EXISTS {}", quote_ident(schema));
        client.execute(&schema_sql, &[]).await?;

        let table_sql = format!(
            "CREATE TABLE IF NOT EXISTS {}.{} (_id BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY, rast raster NOT NULL)",
            quote_ident(schema),
            quote_ident(table)
        );
        client.execute(&table_sql, &[]).await?;
        Ok(())
    }

    /// Copy already materialized rows through the same transactional path.
    pub async fn copy_raster_rows(
        client: &mut Object,
        schema: &str,
        table: &str,
        rows: &[RasterCopyRow],
        batch_size: usize,
    ) -> Result<RasterCopyBatchStats> {
        let (row_tx, row_rx) = mpsc::channel(rows.len().max(1));
        for row in rows {
            row_tx.send(Ok(row.clone())).await?;
        }
        drop(row_tx);
        let producer = tokio::spawn(async { Ok(()) });
        Self::copy_raster_rows_from_receiver(
            client,
            schema,
            table,
            row_rx,
            batch_size,
            producer,
            &Cancellation::default(),
        )
        .await
    }

    /// A closed row channel is insufficient proof of successful reading: the
    /// producer must complete successfully before the transaction can commit.
    pub async fn copy_raster_rows_from_receiver(
        client: &mut Object,
        schema: &str,
        table: &str,
        mut rows: mpsc::Receiver<Result<RasterCopyRow>>,
        batch_size: usize,
        producer: JoinHandle<Result<()>>,
        cancel: &Cancellation,
    ) -> Result<RasterCopyBatchStats> {
        let cancel_query = client.cancel_token();
        let mut producer = Some(producer);
        let result = {
            let write = async {
                let tx = client.transaction().await?;
                Self::ensure_raster_table(&tx, schema, table).await?;
                tx.execute(
                    &format!(
                        "TRUNCATE TABLE {}.{}",
                        quote_ident(schema),
                        quote_ident(table)
                    ),
                    &[],
                )
                .await?;
                let copy_sql = format!(
                    "COPY {}.{} (rast) FROM STDIN",
                    quote_ident(schema),
                    quote_ident(table)
                );
                let safe_batch_size = batch_size.max(1);
                let mut stats = RasterCopyBatchStats {
                    committed_rows: 0,
                    committed_batches: 0,
                };
                {
                    let sink = tx.copy_in(&copy_sql).await?;
                    let mut sink = pin!(sink);
                    while let Some(row) = rows.recv().await {
                        cancel.check()?;
                        let mut line = row?.rast_hex;
                        line.push('\n');
                        SinkExt::feed(&mut sink.as_mut(), Bytes::from(line)).await?;
                        stats.committed_rows += 1;
                        if stats.committed_rows.is_multiple_of(safe_batch_size) {
                            stats.committed_batches += 1;
                            sink.as_mut().flush().await?;
                        }
                    }
                    // Retain ownership of the join handle until await completes, so
                    // cancellation cannot detach a still-running blocking reader.
                    let reader_result = producer.as_mut().expect("reader is present").await;
                    producer.take();
                    reader_result.context("row stream task failed")??;
                    cancel.check()?;
                    sink.as_mut().finish().await?;
                }
                if !stats.committed_rows.is_multiple_of(safe_batch_size) {
                    stats.committed_batches += 1;
                }
                cancel.check()?;
                Ok((tx, stats))
            };
            let result = run_with_cancellation(cancel, cancel_query, write).await;
            // Wake blocking_send on every failure, including DDL and channel errors.
            drop(rows);
            if let Some(producer) = producer {
                // The successful path already joined this task before returning a
                // transaction. On failure preserve the original error while still
                // waiting for any remaining reader to stop.
                let _ = producer.await;
            }
            match result {
                Ok((tx, stats)) => {
                    if let Err(error) = cancel.check() {
                        drop(tx);
                        Err(error)
                    } else {
                        // COMMIT is an atomic decision; always await completion.
                        tx.commit().await.map(|()| stats).map_err(Into::into)
                    }
                }
                Err(error) => Err(error),
            }
        };
        finish_database_result(client, result).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    #[ignore = "requires SPATIAL_CORE_TEST_POSTGRES_DSN with PostGIS raster"]
    async fn producer_failure_and_cancellation_preserve_committed_table() -> Result<()> {
        use deadpool_postgres::{Manager, Pool};
        let dsn = std::env::var("SPATIAL_CORE_TEST_POSTGRES_DSN")?;
        let pool = Pool::builder(Manager::new(dsn.parse()?, tokio_postgres::NoTls))
            .max_size(1)
            .build()?;
        let mut client = pool.get().await?;
        let table = format!("raster_rollback_{}", uuid::Uuid::new_v4().simple());
        let qualified = format!("public.{}", quote_ident(&table));
        client.batch_execute(&format!("CREATE TABLE {qualified} (_id BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY, rast raster NOT NULL); INSERT INTO {qualified}(rast) VALUES(ST_MakeEmptyRaster(1,1,0,0,1,-1,0,0,4326))")).await?;
        let result: Result<()> =
            async {
                let new_raster: String = client.query_one(
                "SELECT encode(ST_AsBinary(ST_MakeEmptyRaster(2,2,0,0,1,-1,0,0,4326)), 'hex')", &[],
            ).await?.get(0);
                for panic in [false, true] {
                    let (sender, receiver) = mpsc::channel::<Result<RasterCopyRow>>(1);
                    let rast_hex = new_raster.clone();
                    let reader = tokio::task::spawn_blocking(move || -> Result<()> {
                        sender.blocking_send(Ok(RasterCopyRow { rast_hex }))?;
                        drop(sender);
                        if panic {
                            panic!("injected reader panic");
                        }
                        Err(anyhow::anyhow!("injected reader open failure"))
                    });
                    let error = PgCopy::copy_raster_rows_from_receiver(
                        &mut client,
                        "public",
                        &table,
                        receiver,
                        1,
                        reader,
                        &Cancellation::default(),
                    )
                    .await;
                    assert!(error.is_err());
                    let count: i64 = client
                        .query_one(&format!("SELECT COUNT(*) FROM {qualified}"), &[])
                        .await?
                        .get(0);
                    assert_eq!(count, 1, "failed producer committed TRUNCATE");
                    let width: i32 = client
                        .query_one(&format!("SELECT ST_Width(rast) FROM {qualified}"), &[])
                        .await?
                        .get(0);
                    assert_eq!(
                        width, 1,
                        "failed producer replaced old raster with the partially copied row"
                    );
                }
                // Keep COPY open after sending a valid row, then cancel while the
                // writer is waiting for more input. The reader must finish first.
                let running_cancel = Cancellation::default();
                let reader_cancel = running_cancel.clone();
                let (sender, receiver) = mpsc::channel::<Result<RasterCopyRow>>(1);
                let reader = tokio::task::spawn_blocking(move || -> Result<()> {
                    sender.blocking_send(Ok(RasterCopyRow {
                        rast_hex: new_raster,
                    }))?;
                    while !reader_cancel.is_cancelled() {
                        std::thread::sleep(std::time::Duration::from_millis(10));
                    }
                    reader_cancel.check()
                });
                let trigger_cancel = running_cancel.clone();
                let trigger = tokio::task::spawn_blocking(move || {
                    std::thread::sleep(std::time::Duration::from_millis(100));
                    trigger_cancel.cancel();
                });
                let error = PgCopy::copy_raster_rows_from_receiver(
                    &mut client,
                    "public",
                    &table,
                    receiver,
                    1,
                    reader,
                    &running_cancel,
                )
                .await
                .unwrap_err();
                trigger.await?;
                assert!(error.downcast_ref::<crate::control::Cancelled>().is_some());
                let width: i32 = client
                    .query_one(&format!("SELECT ST_Width(rast) FROM {qualified}"), &[])
                    .await?
                    .get(0);
                assert_eq!(
                    width, 1,
                    "cancelled COPY must roll back the partially copied row"
                );
                let cancel = Cancellation::default();
                let (sender, receiver2) = mpsc::channel::<Result<RasterCopyRow>>(1);
                let reader = tokio::task::spawn_blocking(move || -> Result<()> {
                    while sender
                        .blocking_send(Ok(RasterCopyRow {
                            rast_hex: "invalid".into(),
                        }))
                        .is_ok()
                    {}
                    Ok(())
                });
                cancel.cancel();
                let error = PgCopy::copy_raster_rows_from_receiver(
                    &mut client,
                    "public",
                    &table,
                    receiver2,
                    1,
                    reader,
                    &cancel,
                )
                .await
                .unwrap_err();
                assert!(error.downcast_ref::<crate::control::Cancelled>().is_some());
                let count: i64 = client
                    .query_one(&format!("SELECT COUNT(*) FROM {qualified}"), &[])
                    .await?
                    .get(0);
                assert_eq!(count, 1);
                Ok(())
            }
            .await;
        client
            .batch_execute(&format!("DROP TABLE {qualified}"))
            .await?;
        result
    }
}
