use super::{
    Cancellation, Object, PgCopy, PgField, PgValue, Result, ToSql, Type, VectorIngestError, anyhow,
    finish_database_result, quote_ident, run_with_cancellation, should_report_vector_progress,
};
use crate::config::gdal::{GdalS3Config, apply_thread_local_gdal_s3_config};
use crate::gdal_ext::ffi::checked_gdal_call;
use anyhow::Context;
use gdal::{Dataset, vector::LayerAccess};
use std::{
    path::{Path, PathBuf},
    pin::pin,
};
use tokio::{
    sync::{mpsc, oneshot},
    task,
};
use tokio_postgres::binary_copy::BinaryCopyInWriter;

impl PgCopy {
    pub async fn copy_in<P: AsRef<Path>>(
        client: &mut Object,
        schema: &str,
        table: &str,
        file_path: P,
        target_srid: u32,
    ) -> Result<()> {
        Self::copy_in_with_gdal_config(
            client,
            schema,
            table,
            file_path,
            None,
            target_srid,
            true,
            None,
            &Cancellation::default(),
        )
        .await
    }

    #[allow(clippy::too_many_arguments)] // The existing COPY API carries independent request context.
    pub async fn copy_in_with_gdal_config<P: AsRef<Path>>(
        client: &mut Object,
        schema: &str,
        table: &str,
        file_path: P,
        gdal_s3: Option<GdalS3Config>,
        target_srid: u32,
        lowercase_fields: bool,
        progress: Option<&(dyn Fn(u64, Option<u64>) + Send + Sync)>,
        cancel: &Cancellation,
    ) -> Result<()> {
        cancel.check()?;
        let query_cancel = client.cancel_token();
        let reader_cancel = cancel.clone();
        let file_path = file_path.as_ref().to_path_buf();
        let (fields_tx, fields_rx) = oneshot::channel::<Result<Vec<PgField>>>();
        // Total feature count is computed in the reader (force=false) and may be
        // absent for streaming drivers; consumers degrade to indeterminate.
        let (count_tx, count_rx) = oneshot::channel::<Option<u64>>();
        let (row_tx, mut row_rx) = mpsc::channel::<(Vec<PgValue>, bool)>(64);
        let mut read_handle = Some(task::spawn_blocking(move || {
            reader_cancel.check()?;
            apply_thread_local_gdal_s3_config(gdal_s3.as_ref())?;
            let dataset = match open_vector_dataset(&file_path) {
                Ok(dataset) => dataset,
                Err(err) => {
                    let _ = fields_tx.send(Err(err));
                    return Err(anyhow!("Failed to open GDAL dataset"));
                }
            };
            let fields = match Self::read_fields(&dataset, Some(0), lowercase_fields, target_srid) {
                Ok(fields) => fields,
                Err(err) => {
                    let _ = fields_tx.send(Err(err));
                    return Err(anyhow!("Failed to read field definitions"));
                }
            };
            let mut layer = checked_gdal_call(|| dataset.layer(0))??;
            // Shapefile declares Polygon/LineString even when records contain multipart geometries.
            let multi_type = (dataset.driver().short_name() == "ESRI Shapefile")
                .then(|| Self::shapefile_geometry_type(layer.defn().geometry_type()));
            let source_srs = match Self::source_spatial_ref(&layer) {
                Ok(source_srs) => source_srs,
                Err(err) => {
                    let _ = fields_tx.send(Err(err));
                    return Err(anyhow!("Failed to read source spatial reference"));
                }
            };
            let (transform, source_crs) = match Self::coordinate_transform(source_srs, target_srid)
            {
                Ok(prepared) => prepared,
                Err(err) => {
                    let _ = fields_tx.send(Err(err));
                    return Err(anyhow!("Failed to prepare coordinate transformation"));
                }
            };
            tracing::info!(
                source_crs,
                target_srid,
                "prepared vector coordinate transformation"
            );
            if fields_tx.send(Ok(fields.clone())).is_err() {
                return Err(anyhow!("Field receiver closed"));
            }

            // force=false: cheap when the driver knows the count, None otherwise.
            let total = checked_gdal_call(|| layer.try_feature_count())?;
            let _ = count_tx.send(total);
            // Creating the iterator also asks the driver for a count. Both that
            // initialization and each read must distinguish failure from EOF.
            let mut features = checked_gdal_call(|| layer.features())?;
            loop {
                reader_cancel.check()?;
                let Some(feature) = checked_gdal_call(|| features.next())? else {
                    break;
                };
                let geometry = Self::require_geometry(feature.geometry())?;
                // GDAL cannot transform POINT EMPTY. It has no coordinates to
                // transform; preserve the empty geometry and still reject an
                // entirely empty dataset before COPY can commit.
                let transformed = if geometry.is_empty() {
                    geometry.clone()
                } else {
                    geometry
                        .transform(&transform)
                        .map_err(|err| VectorIngestError::GeometryTransform(err.to_string()))?
                };
                let transformed = match multi_type {
                    Some(kind) if transformed.geometry_type() != kind => {
                        let mut multi = gdal::vector::Geometry::empty(kind)?;
                        multi.add_geometry(transformed)?;
                        multi
                    }
                    _ => transformed,
                };
                let has_usable_geometry = !transformed.is_empty();
                let wkb = transformed.wkb()?;
                let geom_data = Self::wkb_to_ewkb(&wkb, target_srid);

                let mut row_values: Vec<PgValue> = fields
                    .iter()
                    .enumerate()
                    .filter(|(_, field)| field.field_name != "geom")
                    .map(|(index, field)| {
                        // GDAL's named lookup folds case even when fields are distinct.
                        let idx = if lowercase_fields {
                            feature.field_index(&field.field_name)?
                        } else {
                            index
                        };
                        Self::read_field_value(&feature, idx, &field.field_type)
                    })
                    .collect::<Result<Vec<_>>>()?;

                row_values.push(PgValue::Geometry(geom_data));
                if row_tx
                    .blocking_send((row_values, has_usable_geometry))
                    .is_err()
                {
                    return Err(anyhow!("Row receiver closed"));
                }
            }

            Ok::<_, anyhow::Error>(())
        }));

        let result = {
            let write = async {
                let mut fields = fields_rx
                    .await
                    .map_err(|_| anyhow!("Field channel closed"))??;

                // A workflow result can carry its source dataset's ID. Preserve
                // those values as attributes; this dataset owns a fresh primary key.
                // Rename only the destination field: reader indexes and COPY order stay intact.
                if let Some(index) = fields.iter().position(|f| f.field_name == "_id") {
                    let mut name = "source_id".to_string();
                    let mut suffix = 2;
                    while fields.iter().any(|f| f.field_name == name) {
                        name = format!("source_id_{suffix}");
                        suffix += 1;
                    }
                    fields[index].field_name = name;
                }

                let transaction = client.transaction().await?;
                transaction
                    .execute(
                        &format!("CREATE SCHEMA IF NOT EXISTS {}", quote_ident(schema)),
                        &[],
                    )
                    .await?;

                // A prior attempt may have committed the COPY before metadata or a
                // downstream activity failed. Replace that table transactionally so a
                // retry cannot append duplicate features. If this attempt fails, the
                // DROP rolls back together with the new table and the last committed
                // table remains available until a successful replacement commits.
                let drop_sql = format!(
                    "DROP TABLE IF EXISTS {}.{}",
                    quote_ident(schema),
                    quote_ident(table)
                );
                transaction.execute(&drop_sql, &[]).await.map_err(|err| {
                    anyhow!(
                        "Failed to replace table {}.{} before vector import: {:?}",
                        schema,
                        table,
                        err
                    )
                })?;
                Self::create_table(&transaction, schema, table, &fields, None).await?;

                let copy_sql = format!(
                    "COPY {}.{} ({}) FROM STDIN BINARY",
                    quote_ident(schema),
                    quote_ident(table),
                    fields
                        .iter()
                        .map(|field| quote_ident(&field.field_name))
                        .collect::<Vec<_>>()
                        .join(", ")
                );

                let field_types: Vec<Type> = fields
                    .iter()
                    .map(|f| Self::pg_type_to_postgres_type(&f.field_type))
                    .collect();

                let sink = transaction.copy_in(&copy_sql).await?;
                let mut writer = pin!(BinaryCopyInWriter::new(sink, &field_types));

                // Total may have been sent before the first row; absent → None.
                let total = count_rx.await.unwrap_or(None);
                let mut written: u64 = 0;
                let mut usable_geometry_count: u64 = 0;
                let mut write_result: Result<()> = Ok(());
                while let Some((row_values, has_usable_geometry)) = row_rx.recv().await {
                    cancel.check()?;
                    let value_refs: Vec<&(dyn ToSql + Sync)> = row_values
                        .iter()
                        .map(|v| v as &(dyn ToSql + Sync))
                        .collect();
                    if let Err(err) = writer.as_mut().write(value_refs.as_slice()).await {
                        write_result = Err(err.into());
                        break;
                    }
                    written += 1;
                    usable_geometry_count += u64::from(has_usable_geometry);
                    if let Some(cb) = progress
                        && should_report_vector_progress(written, total, usable_geometry_count)
                    {
                        cb(written, total);
                    }
                }

                write_result?;
                let reader_result = read_handle.as_mut().expect("reader is present").await;
                read_handle.take();
                reader_result.context("row stream task failed")??;
                cancel.check()?;
                Self::ensure_usable_geometry_count(usable_geometry_count)?;
                writer.finish().await?;
                Self::create_geom_index(&transaction, schema, table, None).await?;
                Self::create_cursor_index(&transaction, schema, table, None).await?;
                transaction
                    .batch_execute(&format!(
                        "ANALYZE {}.{}",
                        quote_ident(schema),
                        quote_ident(table)
                    ))
                    .await?;
                cancel.check()?;
                Ok(transaction)
            };
            let result = run_with_cancellation(cancel, query_cancel, write).await;
            drop(row_rx);
            if let Some(reader) = read_handle {
                // The successful path already joined this task before returning a
                // transaction. On failure preserve the original error while still
                // waiting for any remaining reader to stop.
                let _ = reader.await;
            }
            match result {
                Ok(transaction) => {
                    if let Err(error) = cancel.check() {
                        drop(transaction);
                        Err(error)
                    } else {
                        // Once COMMIT is sent, await its definitive outcome. A
                        // cancellation select must never drop this future.
                        transaction.commit().await.map_err(Into::into)
                    }
                }
                Err(error) => Err(error),
            }
        };
        finish_database_result(client, result).await
    }
}

fn open_vector_dataset(path: &Path) -> Result<Dataset> {
    if !path.to_string_lossy().starts_with("/vsizip/") {
        return Ok(Dataset::open(path)?);
    }
    let entries = gdal::vsi::read_dir(path, true)?;
    Ok(Dataset::open(shapefile_in_archive(path, &entries)?)?)
}

fn shapefile_in_archive(root: &Path, entries: &[PathBuf]) -> Result<PathBuf> {
    let mut candidates = entries.iter().filter(|path| {
        path.extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("shp"))
            && !path.components().any(|part| part.as_os_str() == "__MACOSX")
            && !path
                .file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with("._"))
    });
    let first = candidates
        .next()
        .context("ZIP archive contains no Shapefile")?;
    anyhow::ensure!(
        candidates.next().is_none(),
        "ZIP archive must contain exactly one Shapefile"
    );
    Ok(root.join(first))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    #[ignore = "requires SPATIAL_CORE_TEST_POSTGRES_DSN with PostGIS"]
    async fn native_read_failure_rolls_back_partial_copy_and_allows_retry() -> Result<()> {
        use deadpool_postgres::{Manager, Pool};
        let (_directory, path) = crate::pg::tests::failing_vector_fixture(1)?;
        let dsn = std::env::var("SPATIAL_CORE_TEST_POSTGRES_DSN")?;
        let pool = Pool::builder(Manager::new(dsn.parse()?, tokio_postgres::NoTls))
            .max_size(1)
            .build()?;
        let mut client = pool.get().await?;
        let schema = format!("read_failure_{}", uuid::Uuid::new_v4().simple());
        let qualified = format!("{}.features", quote_ident(&schema));
        client.batch_execute(&format!("CREATE SCHEMA {}; CREATE TABLE {qualified}(sentinel text); INSERT INTO {qualified} VALUES('keep me')", quote_ident(&schema))).await?;
        let result: Result<()> = async {
            for _ in 0..2 {
                let result = PgCopy::copy_in(&mut client, &schema, "features", &path, 4326).await;
                anyhow::ensure!(result.is_err(), "native read failure was accepted as EOF");
                let rows = client
                    .query(&format!("SELECT sentinel FROM {qualified}"), &[])
                    .await?;
                anyhow::ensure!(
                    rows.len() == 1 && rows[0].get::<_, String>(0) == "keep me",
                    "read failure replaced the committed table"
                );
            }
            let valid = Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata/points.geojson");
            PgCopy::copy_in(&mut client, &schema, "features", valid, 4326).await?;
            let count: i64 = client
                .query_one(&format!("SELECT COUNT(*) FROM {qualified}"), &[])
                .await?
                .get(0);
            anyhow::ensure!(count == 3, "healthy retry did not replace all rows");
            Ok(())
        }
        .await;
        client
            .batch_execute(&format!("DROP SCHEMA {} CASCADE", quote_ident(&schema)))
            .await?;
        result
    }

    #[test]
    fn selects_nested_shapefile_and_ignores_macos_metadata() {
        let root = Path::new("/vsizip//vsis3/data/city.zip");
        let entries = [
            PathBuf::from("City/city.SHP"),
            PathBuf::from("__MACOSX/._city.shp"),
        ];
        assert_eq!(
            shapefile_in_archive(root, &entries).unwrap(),
            root.join("City/city.SHP")
        );
        assert!(shapefile_in_archive(root, &[]).is_err());
        assert!(shapefile_in_archive(root, &["a.shp".into(), "b.shp".into()]).is_err());
    }

    #[test]
    fn shapefile_parts_share_a_multi_geometry_column() {
        use gdal_sys::OGRwkbGeometryType::*;
        assert_eq!(PgCopy::shapefile_geometry_type(wkbPolygon), wkbMultiPolygon);
        assert_eq!(
            PgCopy::shapefile_geometry_type(wkbLineString),
            wkbMultiLineString
        );
        assert_eq!(
            PgCopy::shapefile_geometry_type(wkbPolygonZM),
            wkbMultiPolygonZM
        );
        assert_eq!(PgCopy::shapefile_geometry_type(wkbPoint), wkbPoint);
    }
}
