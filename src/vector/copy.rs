use crate::config::gdal::{GdalS3Config, apply_gdal_config};
use crate::control::Cancellation;
use crate::gdal_ext::destination::DatasetDestination;
use crate::gdal_ext::vector::vector_translate::{VectorTranslateOptions, vector_translate};
pub use crate::pg::VectorIngestError;
use crate::pg::{PgCopy, acquire_connection};
use anyhow::{Context, Result, anyhow};
use deadpool_postgres::Pool;
use gdal::Dataset;
use std::{ffi::CString, path::Path};
use tokio::task;

/// Copy a vector file (via GDAL path, e.g. `/vsis3/…`) into PostGIS using
/// binary COPY.
///
/// Pass `gdal_s3` when the source file is on S3-compatible storage; `None`
/// for local or HTTP paths where GDAL handles auth separately.
/// `lowercase_fields` keeps ordinary upload normalization; a typed Dataset
/// copy disables it to retain its authoritative working-schema names.
#[allow(clippy::too_many_arguments)] // Preserve the established caller-facing COPY context.
pub async fn copy_to_postgis<P: AsRef<Path>>(
    pool: &Pool,
    gdal_s3: Option<&GdalS3Config>,
    path: P,
    schema: &str,
    table: &str,
    target_srid: u32,
    lowercase_fields: bool,
    progress: Option<&(dyn Fn(u64, Option<u64>) + Send + Sync)>,
    cancel: &Cancellation,
) -> Result<()> {
    cancel.check()?;
    apply_gdal_config(gdal_s3).await?;
    let mut client = acquire_connection(pool, cancel).await?;
    PgCopy::copy_in_with_gdal_config(
        &mut client,
        schema,
        table,
        path,
        gdal_s3.cloned(),
        target_srid,
        lowercase_fields,
        progress,
        cancel,
    )
    .await?;
    tracing::info!(schema, table, "Vector data copied to PostGIS");
    Ok(())
}

/// Execute raw SQL against PostGIS.
pub async fn store_postgis(pool: &Pool, sql: &str) -> Result<()> {
    let client = pool
        .get()
        .await
        .map_err(|e| anyhow!("Failed to get database connection: {:?}", e))?;
    client
        .batch_execute(sql)
        .await
        .map_err(|e| anyhow!("SQL execution failed: {:?}", e))?;
    Ok(())
}

// ── GDAL conversion helpers (synchronous, for use inside spawn_blocking) ──

/// Convert a vector file to GeoParquet. This is a pure GDAL operation;
/// uploading the result to object storage is the caller's responsibility.
pub fn to_geoparquet<P: AsRef<Path>>(url: P, out: &Path) -> Result<()> {
    let src = Dataset::open(url)?;
    let opts = Some(
        vec![
            "-f",
            "Parquet",
            "-t_srs",
            "EPSG:4326",
            "-dim",
            "2",
            "-lco",
            "FID=_id",
            "-makevalid",
            "-skipfailures",
        ]
        .try_into()?,
    );
    vector_translate(&src, out.try_into()?, opts)?;
    Ok(())
}

pub fn zipshp2sql<P: AsRef<Path>>(url: P, out: &Path, schema: &str, table: &str) -> Result<()> {
    let schema_opt = format!("SCHEMA={}", schema);
    let src = Dataset::open(url)?;
    let opts = Some(
        vec![
            "-f",
            "PGDump",
            "-t_srs",
            "EPSG:4326",
            "-nln",
            table,
            "-lco",
            "GEOMETRY_NAME=geom",
            "-lco",
            "FID=_id",
            "-lco",
            schema_opt.as_str(),
            "-lco",
            "CREATE_SCHEMA=OFF",
            "-lco",
            "GEOM_COLUMN_POSITION=END",
            "-lco",
            "LAUNDER=NO",
            "-makevalid",
        ]
        .try_into()?,
    );
    vector_translate(&src, out.try_into()?, opts)?;
    Ok(())
}

pub async fn vector_to_pg(pg_string: &str, url: &str, schema: &str, table: &str) -> Result<()> {
    let schema_opt = format!("SCHEMA={}", schema);
    let dst_connection =
        CString::new(pg_string).map_err(|_| anyhow!("PG connection string contains null byte"))?;
    let url = url.to_string();
    let table = table.to_string();
    task::spawn_blocking(move || {
        let src = Dataset::open(url)?;
        let opts: Option<VectorTranslateOptions> = Some(
            vec![
                "-t_srs",
                "EPSG:4326",
                "-nln",
                table.as_str(),
                "-lco",
                "OVERWRITE=NO",
                "-lco",
                "GEOMETRY_NAME=geom",
                "-lco",
                "FID=_id",
                "-lco",
                schema_opt.as_str(),
                "-lco",
                "LAUNDER=NO",
                "-makevalid",
            ]
            .try_into()?,
        );
        vector_translate(&src, DatasetDestination::Path(dst_connection), opts)?;
        Ok::<(), anyhow::Error>(())
    })
    .await
    .context("GDAL vector import failed")??;
    Ok(())
}

pub fn get_file_name(path: impl AsRef<Path>, remove_ext: Option<&str>) -> Result<String> {
    let file_name = path
        .as_ref()
        .file_name()
        .ok_or_else(|| anyhow!("path has no file name: {}", path.as_ref().display()))?
        .to_string_lossy()
        .to_string();
    Ok(match remove_ext {
        Some(ext) => file_name
            .strip_suffix(&format!(".{}", ext))
            .unwrap_or(&file_name)
            .to_string(),
        None => file_name.split('.').next().unwrap_or("").to_string(),
    })
}

pub fn add_prefix_from_ext(path: &str, bucket_name: &str, ext: Option<&str>) -> String {
    let ext = ext
        .map(|e| e.to_lowercase())
        .or_else(|| path.split('.').next_back().map(|s| s.to_lowercase()));
    let prefix = match ext.as_deref() {
        Some("zip") => "/vsizip//vsis3/",
        _ => "/vsis3/",
    };
    format!("{}{}{}", prefix, bucket_name, path)
}
