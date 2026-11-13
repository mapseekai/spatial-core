//! GDAL vector file conversion. Database ingestion lives in `postgis::vector`.
use crate::gdal_ext::vector::vector_translate::vector_translate;
use anyhow::Result;
use gdal::Dataset;
use std::path::Path;

/// Convert a vector file to GeoParquet. This is a pure GDAL operation;
/// uploading the result to object storage is the caller's responsibility.
/// Preserves the existing export policy: EPSG:4326, 2D, makevalid and skipfailures.
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

/// Export a PostgreSQL SQL dump using GDAL.
/// Preserves the existing EPSG:4326/makevalid policy; does not execute the SQL.
pub fn to_pg_dump<P: AsRef<Path>>(url: P, out: &Path, schema: &str, table: &str) -> Result<()> {
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
