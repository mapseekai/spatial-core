use anyhow::{Result, ensure};
use gdal::{
    Dataset,
    vector::{FieldValue, LayerAccess},
};
use spatial_core::postgis::{
    PostGisClient, TableName, WriteControl, WriteMode,
    raster::{PostGisRasterWriter, RasterSource, RasterStorage, RasterWriteOptions, TileLayout},
    vector::{PostGisVectorWriter, VectorSource, VectorWriteOptions},
};
use std::{env, time::Instant};

fn quote(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}
fn flags<'a>(args: &'a [String], key: &str) -> Option<&'a str> {
    args.iter().find_map(|s| s.strip_prefix(&format!("{key}=")))
}
fn eq_float(a: f64, b: f64) -> bool {
    a == b || a.is_nan() && b.is_nan()
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = env::args().skip(1).collect::<Vec<_>>();
    ensure!(args.len() >= 3, "kind id path [options]");
    let (kind, id, path) = (&args[0], &args[1], &args[2]);
    let pool = deadpool_postgres::Pool::builder(deadpool_postgres::Manager::new(
        env::var("SC_REVIEW_DSN")?.parse()?,
        tokio_postgres::NoTls,
    ))
    .max_size(1)
    .build()?;
    let client = PostGisClient::from_pool(pool.clone(), tokio_postgres::NoTls);
    let table = TableName::new("review_main", id)?;
    let sql_table = format!("review_main.{}", quote(id));
    let start = Instant::now();
    let target = flags(&args, "target").map(str::parse::<i32>).transpose()?;
    if kind == "vector" {
        let dataset = Dataset::open(flags(&args, "oracle").unwrap_or(path))?;
        let driver = dataset.driver().short_name();
        let mut layer = dataset.layer(0)?;
        let source_count = layer.try_feature_count();
        let source_srid = layer.spatial_ref().and_then(|s| s.auth_code().ok());
        let names = layer.defn().fields().map(|f| f.name()).collect::<Vec<_>>();
        let mut samples = Vec::new();
        for f in layer.features().take(5) {
            let wkb = f
                .geometry()
                .filter(|g| g.has_gdal_ptr() && !unsafe { g.c_geometry() }.is_null())
                .map(|g| g.wkb())
                .transpose()?;
            let fields = (0..names.len())
                .map(|i| {
                    if unsafe { gdal_sys::OGR_F_IsFieldSetAndNotNull(f.c_feature(), i as i32) } == 0
                    {
                        Ok(None)
                    } else {
                        f.field(i)
                    }
                })
                .collect::<std::result::Result<Vec<_>, _>>()?;
            samples.push((wkb, fields));
        }
        drop(layer);
        drop(dataset);
        let mut options = VectorWriteOptions::default().mode(WriteMode::ReplaceTable);
        if let Some(srid) = target {
            options = options.target_srid(srid);
        }
        if flags(&args, "rename_id") == Some("true") {
            options = options.rename_field("_id", "source_id");
        }
        match PostGisVectorWriter::new(&client, table)
            .options(options)
            .write(VectorSource::new(path), &WriteControl::default())
            .await
        {
            Err(error) => {
                let exists = pool
                    .get()
                    .await?
                    .query_one("SELECT to_regclass($1) IS NOT NULL", &[&sql_table])
                    .await?
                    .get::<_, bool>(0);
                println!(
                    "WRITE_ERROR|{id}|{error:?}|target_exists={exists}|elapsed_ms={}",
                    start.elapsed().as_millis()
                );
            }
            Ok(report) => {
                let db = pool.get().await?;
                let count = db
                    .query_one(&format!("SELECT count(*) FROM {sql_table}"), &[])
                    .await?
                    .get::<_, i64>(0) as u64;
                ensure!(count == report.feature_count, "row count mismatch");
                if let Some(expected) = source_count {
                    ensure!(count == expected, "source count mismatch");
                }
                let selects = names
                    .iter()
                    .map(|n| {
                        format!(
                            "{}::text",
                            quote(if flags(&args, "rename_id") == Some("true") && n == "_id" {
                                "source_id"
                            } else {
                                n
                            })
                        )
                    })
                    .collect::<Vec<_>>();
                let attr = if selects.is_empty() {
                    Vec::new()
                } else {
                    db.query(
                        &format!(
                            "SELECT {} FROM {sql_table} ORDER BY _id LIMIT 5",
                            selects.join(",")
                        ),
                        &[],
                    )
                    .await?
                };
                let mut checked_attrs = 0usize;
                let mut geom_checks = 0;
                let mut max_reprojection_delta = 0.0f64;
                for (idx, (wkb, fields)) in samples.iter().enumerate() {
                    if let (Some(wkb), Some(srid)) = (wkb, source_srid) {
                        let rownum = (idx + 1) as i64;
                        let mut expected = "ST_GeomFromWKB($1,$2)".to_string();
                        if driver == "ESRI Shapefile" {
                            expected = format!("ST_Multi({expected})");
                        }
                        if report.schema.geometry.srid != srid {
                            expected =
                                format!("ST_Transform({expected},{})", report.schema.geometry.srid);
                        }
                        let row=db.query_one(&format!("SELECT ST_AsEWKB(geom,'NDR')=ST_AsEWKB({expected},'NDR'),ST_NDims(geom)=ST_NDims({expected}),ST_SRID(geom),ST_HausdorffDistance(geom,{expected}) FROM {sql_table} WHERE _id=$3"),&[wkb,&srid,&rownum]).await?;
                        if report.schema.geometry.srid == srid {
                            ensure!(
                                row.get::<_, bool>(0),
                                "sample EWKB coordinates differ at {idx}"
                            );
                        } else {
                            let delta = row.get::<_, f64>(3);
                            max_reprojection_delta = max_reprojection_delta.max(delta);
                            ensure!(
                                delta <= 1e-8,
                                "independent projection delta exceeds 1e-8 coordinate units at {idx}: {delta}"
                            );
                        }
                        ensure!(row.get::<_, bool>(1), "sample dimension differs");
                        ensure!(
                            row.get::<_, i32>(2) == report.schema.geometry.srid,
                            "sample SRID differs"
                        );
                        geom_checks += 1;
                    }
                    for (col, value) in fields.iter().enumerate() {
                        let actual: Option<String> = attr[idx].get(col);
                        let valid = match (value, actual.as_deref()) {
                            (None, None) => true,
                            (Some(FieldValue::IntegerValue(v)), Some(a)) => a.parse::<i32>()? == *v,
                            (Some(FieldValue::Integer64Value(v)), Some(a)) => {
                                a.parse::<i64>()? == *v
                            }
                            (Some(FieldValue::RealValue(v)), Some(a)) => {
                                eq_float(a.parse::<f64>()?, *v)
                            }
                            (Some(FieldValue::StringValue(v)), Some(a)) => a == v,
                            (Some(FieldValue::DateValue(v)), Some(a)) => {
                                a == v.format("%Y-%m-%d").to_string()
                            }
                            (Some(FieldValue::IntegerListValue(v)), Some(a)) => {
                                a == format!(
                                    "{{{}}}",
                                    v.iter()
                                        .map(ToString::to_string)
                                        .collect::<Vec<_>>()
                                        .join(",")
                                )
                            }
                            _ => false,
                        };
                        ensure!(
                            valid,
                            "attribute mismatch at sample {idx} column {}",
                            names[col]
                        );
                        checked_attrs += 1;
                    }
                }
                println!(
                    "PASS|{id}|rows={count}|fields={}|geometry_samples={geom_checks}|attribute_values={checked_attrs}|max_reprojection_delta={max_reprojection_delta}|srid={}|elapsed_ms={}",
                    names.len(),
                    report.schema.geometry.srid,
                    start.elapsed().as_millis()
                );
            }
        }
    } else {
        let dataset = Dataset::open(flags(&args, "oracle").unwrap_or(path))?;
        let (width, height) = dataset.raster_size();
        let transform = dataset.geo_transform()?;
        let bands = dataset.raster_count();
        let whole = flags(&args, "whole") == Some("true");
        let tw = flags(&args, "width").unwrap_or("256").parse::<u32>()?;
        let th = flags(&args, "height").unwrap_or("256").parse::<u32>()?;
        let overlap = flags(&args, "overlap").unwrap_or("0").parse::<u32>()?;
        let layout = if whole {
            TileLayout::Whole
        } else {
            TileLayout::tiled(tw, th)?.with_overlap(overlap)?
        };
        let storage = if let Some(server) = flags(&args, "outdb") {
            pool.get().await?.batch_execute("SET postgis.enable_outdb_rasters=true; SET postgis.gdal_enabled_drivers='GTiff'").await?;
            RasterStorage::OutDb {
                server_path: server.into(),
            }
        } else {
            RasterStorage::InDb
        };
        let mut options = RasterWriteOptions::default()
            .mode(WriteMode::ReplaceTable)
            .tile_layout(layout)
            .storage(storage);
        if let Some(srid) = target {
            options = options.target_srid(srid);
        }
        match PostGisRasterWriter::new(&client, table)
            .options(options)
            .write(RasterSource::new(path), &WriteControl::default())
            .await
        {
            Err(error) => {
                let exists = pool
                    .get()
                    .await?
                    .query_one("SELECT to_regclass($1) IS NOT NULL", &[&sql_table])
                    .await?
                    .get::<_, bool>(0);
                println!(
                    "WRITE_ERROR|{id}|{error:?}|target_exists={exists}|size={width}x{height}|bands={bands}|elapsed_ms={}",
                    start.elapsed().as_millis()
                );
            }
            Ok(report) => {
                let db = pool.get().await?;
                let count = db
                    .query_one(&format!("SELECT count(*) FROM {sql_table}"), &[])
                    .await?
                    .get::<_, i64>(0) as u64;
                ensure!(count == report.tile_count, "tile count mismatch");
                if target.is_none() || flags(&args, "oracle").is_some() {
                    let sx = if whole {
                        width
                    } else {
                        (tw - overlap) as usize
                    };
                    let sy = if whole {
                        height
                    } else {
                        (th - overlap) as usize
                    };
                    let nx = width.div_ceil(sx);
                    ensure!(
                        count == (nx * height.div_ceil(sy)) as u64,
                        "tile plan mismatch"
                    );
                    let sampled=db.query(&format!("SELECT _id,ST_Width(rast),ST_Height(rast),ST_UpperLeftX(rast),ST_UpperLeftY(rast),ST_ScaleX(rast),ST_ScaleY(rast),ST_SkewX(rast),ST_SkewY(rast),ST_SRID(rast),ST_NumBands(rast) FROM {sql_table} ORDER BY _id"),&[]).await?;
                    let mut checked_pixels = 0;
                    for row in sampled {
                        let rowid = row.get::<_, i64>(0);
                        let index = rowid as usize - 1;
                        let x = (index % nx) * sx;
                        let y = (index / nx) * sy;
                        let w = row.get::<_, i32>(1) as usize;
                        let h = row.get::<_, i32>(2) as usize;
                        ensure!(
                            w == (width - x).min(if whole { width } else { tw as usize })
                                && h == (height - y).min(if whole { height } else { th as usize }),
                            "tile dimensions differ"
                        );
                        ensure!(
                            (row.get::<_, f64>(3)
                                - (transform[0]
                                    + transform[1] * x as f64
                                    + transform[2] * y as f64))
                                .abs()
                                < 1e-8,
                            "tile origin X differs"
                        );
                        ensure!(
                            (row.get::<_, f64>(4)
                                - (transform[3]
                                    + transform[4] * x as f64
                                    + transform[5] * y as f64))
                                .abs()
                                < 1e-8,
                            "tile origin Y differs"
                        );
                        for (column, expected) in [
                            (5, transform[1]),
                            (6, transform[5]),
                            (7, transform[2]),
                            (8, transform[4]),
                        ] {
                            ensure!(
                                row.get::<_, f64>(column) == expected,
                                "raster grid coefficient differs at tile {rowid}"
                            );
                        }
                        ensure!(
                            row.get::<_, i32>(9) == dataset.spatial_ref()?.auth_code()?,
                            "raster SRID differs"
                        );
                        ensure!(
                            row.get::<_, i32>(10) as usize == bands,
                            "raster band count differs"
                        );
                        if index < 5 || index + 1 == count as usize {
                            for band in 1..=bands {
                                for (px, py) in [(0, 0), (w / 2, h / 2), (w - 1, h - 1)] {
                                    let expected = dataset
                                        .rasterband(band)?
                                        .read_as::<f64>(
                                            ((x + px) as isize, (y + py) as isize),
                                            (1, 1),
                                            (1, 1),
                                            None,
                                        )?
                                        .data()[0];
                                    let b = band as i32;
                                    let xx = (px + 1) as i32;
                                    let yy = (py + 1) as i32;
                                    let actual=db.query_one(&format!("SELECT ST_Value(rast,$1,$2,$3,false) FROM {sql_table} WHERE _id=$4"),&[&b,&xx,&yy,&rowid]).await?.get::<_,f64>(0);
                                    ensure!(
                                        eq_float(expected, actual),
                                        "pixel differs at tile {rowid} band {band} local {px},{py}"
                                    );
                                    checked_pixels += 1;
                                }
                            }
                        }
                    }
                    println!(
                        "PASS|{id}|tiles={count}|size={width}x{height}|bands={bands}|pixel_samples={checked_pixels}|srid={}|elapsed_ms={}",
                        report.schema.srid,
                        start.elapsed().as_millis()
                    );
                } else {
                    println!(
                        "PASS|{id}|tiles={count}|output={:?}|srid={}|elapsed_ms={}",
                        report.output_size,
                        report.schema.srid,
                        start.elapsed().as_millis()
                    );
                }
            }
        }
    }
    Ok(())
}
