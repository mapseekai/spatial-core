use crate::control::Cancellation;
use crate::pg::{acquire_connection, finish_database_result, quote_ident, run_with_cancellation};
use anyhow::{Context, Result, anyhow};
use deadpool_postgres::Pool;
use serde::Serialize;
use std::collections::HashMap;

#[derive(Debug, Clone, Serialize)]
pub struct VectorMetadata {
    pub srid: i64,
    pub bound: Option<[f64; 4]>,
    pub has_z: bool,
    pub attributes_schema: HashMap<String, String>,
    pub geometry_type: String,
    pub feature_count: i64,
}

/// Compute metadata for a vector dataset that has already been ingested into PostGIS.
pub async fn compute_vector_metadata(
    pool: &Pool,
    schema: &str,
    table: &str,
    cancel: &Cancellation,
) -> Result<VectorMetadata> {
    cancel.check()?;
    let client = acquire_connection(pool, cancel).await?;

    let operation = async {
        let qualified = format!("{}.{}", quote_ident(schema), quote_ident(table));

        let row = client
            .query_one(
                &format!(
                    r#"SELECT COUNT(*)::bigint, ST_Extent(geom)::text FROM {}"#,
                    qualified
                ),
                &[],
            )
            .await
            .context("Failed to query feature count/extent")?;
        let feature_count: i64 = row.get(0);
        let extent_box: Option<String> = row.get(1);
        let bound = if feature_count == 0 {
            None
        } else {
            Some(parse_extent_box(&extent_box.ok_or_else(|| {
                anyhow!("No extent returned (empty geometry?)")
            })?)?)
        };
        let geometry = client.query_opt(
            &format!("SELECT ST_SRID(geom)::int, GeometryType(geom)::text, ST_Zmflag(geom)::int FROM {qualified} WHERE geom IS NOT NULL LIMIT 1"), &[],
        ).await.context("Failed to query SRID/geometry type")?;
        let (srid, geometry_type, has_z) = if let Some(row) = geometry {
            (
                i64::from(row.get::<_, i32>(0)),
                row.get::<_, String>(1),
                matches!(row.get::<_, i32>(2), 2 | 3),
            )
        } else if feature_count == 0 {
            let row = client.query_opt(
                "SELECT postgis_typmod_srid(a.atttypmod)::int, postgis_typmod_type(a.atttypmod)::text FROM pg_attribute a JOIN pg_class c ON c.oid=a.attrelid JOIN pg_namespace n ON n.oid=c.relnamespace WHERE n.nspname=$1 AND c.relname=$2 AND a.attname='geom' AND NOT a.attisdropped",
                &[&schema,&table],
            ).await?.ok_or_else(||anyhow!("Empty vector table lacks a geometry contract"))?;
            let srid = row.get::<_, i32>(0);
            let kind = row.get::<_, String>(1).to_ascii_uppercase();
            let has_z = kind.ends_with('Z') || kind.ends_with("ZM");
            let geometry_type = kind.trim_end_matches('M').trim_end_matches('Z').to_owned();
            if srid <= 0
                || !matches!(
                    geometry_type.as_str(),
                    "POINT"
                        | "MULTIPOINT"
                        | "LINESTRING"
                        | "MULTILINESTRING"
                        | "POLYGON"
                        | "MULTIPOLYGON"
                )
            {
                return Err(anyhow!(
                    "Empty vector table lacks a supported geometry contract"
                ));
            }
            (i64::from(srid), geometry_type, has_z)
        } else {
            return Err(anyhow!("No valid geometry found"));
        };

        let rows = client
            .query(
                r#"
SELECT column_name, data_type, udt_name
FROM information_schema.columns
WHERE table_schema = $1 AND table_name = $2
ORDER BY ordinal_position
"#,
                &[&schema, &table],
            )
            .await
            .context("Failed to query column info")?;

        let mut attributes_schema = HashMap::new();
        for r in rows {
            let column_name: String = r.get(0);
            // Hide our auto-generated PK column and the geometry column
            // from the user-facing attribute schema.
            if column_name == "_id" || column_name == "geom" {
                continue;
            }
            let data_type: String = r.get(1);
            let udt_name: String = r.get(2);
            attributes_schema.insert(column_name, map_attribute_type(&data_type, &udt_name));
        }

        Ok(VectorMetadata {
            srid,
            bound,
            has_z,
            attributes_schema,
            geometry_type,
            feature_count,
        })
    };
    let result = run_with_cancellation(cancel, client.cancel_token(), operation).await;
    finish_database_result(&client, result).await
}

fn map_attribute_type(data_type: &str, udt_name: &str) -> String {
    match data_type.to_lowercase().as_str() {
        "array" if udt_name == "_int4" => "integer[]".to_string(),
        "integer" | "smallint" | "bigint" => "integer".to_string(),
        "real" | "double precision" | "numeric" | "decimal" => "float".to_string(),
        "boolean" => "boolean".to_string(),
        "date" | "timestamp without time zone" | "timestamp with time zone" => {
            "datetime".to_string()
        }
        _ => "string".to_string(),
    }
}

fn parse_extent_box(s: &str) -> Result<[f64; 4]> {
    let t = s.trim();
    let inner = t
        .strip_prefix("BOX(")
        .and_then(|x| x.strip_suffix(')'))
        .or_else(|| t.strip_prefix("BOX3D(").and_then(|x| x.strip_suffix(')')))
        .ok_or_else(|| anyhow!("Cannot parse extent: {}", s))?;

    let mut parts = inner.split(',');
    let p1 = parts
        .next()
        .ok_or_else(|| anyhow!("Cannot parse extent: {}", s))?;
    let p2 = parts
        .next()
        .ok_or_else(|| anyhow!("Cannot parse extent: {}", s))?;

    fn parse_xy(p: &str) -> Result<(f64, f64)> {
        let mut it = p.split_whitespace();
        let x = it
            .next()
            .ok_or_else(|| anyhow!("Cannot parse extent point: {}", p))?
            .parse::<f64>()
            .map_err(|e| anyhow!("Cannot parse extent value: {} ({:?})", p, e))?;
        let y = it
            .next()
            .ok_or_else(|| anyhow!("Cannot parse extent point: {}", p))?
            .parse::<f64>()
            .map_err(|e| anyhow!("Cannot parse extent value: {} ({:?})", p, e))?;
        Ok((x, y))
    }

    let (minx, miny) = parse_xy(p1)?;
    let (maxx, maxy) = parse_xy(p2)?;
    Ok([minx, miny, maxx, maxy])
}

#[cfg(test)]
mod tests {
    use super::{map_attribute_type, parse_extent_box};

    #[test]
    fn parse_extent_box_supports_box_and_box3d() {
        assert_eq!(
            parse_extent_box("BOX(1 2,3 4)").expect("BOX should parse"),
            [1.0, 2.0, 3.0, 4.0]
        );
        assert_eq!(
            parse_extent_box("BOX3D(-1 -2,3 4)").expect("BOX3D should parse"),
            [-1.0, -2.0, 3.0, 4.0]
        );
    }

    #[test]
    fn map_attribute_type_maps_common_postgres_types() {
        assert_eq!(map_attribute_type("integer", "int4"), "integer");
        assert_eq!(map_attribute_type("double precision", "float8"), "float");
        assert_eq!(map_attribute_type("boolean", "bool"), "boolean");
        assert_eq!(
            map_attribute_type("timestamp with time zone", "timestamptz"),
            "datetime"
        );
        assert_eq!(map_attribute_type("text", "text"), "string");
        assert_eq!(map_attribute_type("ARRAY", "_int4"), "integer[]");
        assert_eq!(map_attribute_type("ARRAY", "_text"), "string");
    }
}
