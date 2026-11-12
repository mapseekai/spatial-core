//! Preserve the explicit working schema across Dataset copy interchange formats.
use super::{
    PgCopy, acquire_connection, finish_database_result, quote_ident, run_with_cancellation,
};
use crate::config::gdal::{GdalS3Config, apply_thread_local_gdal_s3_config};
use crate::control::{Cancellation, InvalidInput};
use crate::gdal_ext::ffi::checked_gdal_call;
use anyhow::Result;
use deadpool_postgres::Pool;
use gdal::{Dataset, spatial_ref::SpatialRef, vector::LayerAccess};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::Path};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VectorCopyContract {
    pub attributes_schema: BTreeMap<String, String>,
    pub geometry_type: String,
    pub crs: String,
    pub has_z: bool,
    pub feature_count: i64,
    #[serde(default)]
    pub data_revision: String,
}

fn invalid(message: &str) -> anyhow::Error {
    InvalidInput(message.into()).into()
}

fn field_type(raw: &str) -> Result<String> {
    let ty = raw
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase();
    let simple = match ty.as_str() {
        "string" => "text",
        "float" | "number" => "double precision",
        "datetime" => "timestamp with time zone",
        "bool" => "boolean",
        "int2" => "smallint",
        "int4" => "integer",
        "int[]" | "int4[]" | "integer[]" => "integer[]",
        "int8" => "bigint",
        "float4" => "real",
        "float8" => "double precision",
        "text"
        | "varchar"
        | "character varying"
        | "char"
        | "character"
        | "smallint"
        | "integer"
        | "bigint"
        | "real"
        | "double precision"
        | "numeric"
        | "decimal"
        | "boolean"
        | "date"
        | "timestamp"
        | "timestamptz"
        | "timestamp with time zone"
        | "timestamp without time zone"
        | "json"
        | "jsonb" => ty.as_str(),
        _ => "",
    };
    if !simple.is_empty() {
        return Ok(simple.to_owned());
    }
    if let Some((base, rest)) = ty.split_once('(')
        && let Some((args, suffix)) = rest.split_once(')')
    {
        let suffix = suffix.trim();
        let values = args
            .split(',')
            .map(|v| v.trim().parse::<i32>())
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|_| invalid("unsupported vector copy field type"))?;
        let valid = match base.trim() {
            "varchar" | "character varying" | "char" | "character" => {
                suffix.is_empty() && values.len() == 1 && (1..=10_485_760).contains(&values[0])
            }
            "numeric" | "decimal" => {
                suffix.is_empty()
                    && matches!(values.len(), 1 | 2)
                    && (1..=1000).contains(&values[0])
                    && (values.len() == 1 || (-1000..=1000).contains(&values[1]))
            }
            "timestamp" | "timestamptz" => {
                values.len() == 1
                    && (0..=6).contains(&values[0])
                    && matches!(suffix, "" | "with time zone" | "without time zone")
            }
            _ => false,
        };
        if valid {
            return Ok(format!(
                "{}({}){}",
                base.trim(),
                values
                    .iter()
                    .map(i32::to_string)
                    .collect::<Vec<_>>()
                    .join(","),
                if suffix.is_empty() {
                    String::new()
                } else {
                    format!(" {suffix}")
                }
            ));
        }
    }
    Err(invalid("unsupported vector copy field type"))
}

fn copy_field_type(name: &str, ty: &str) -> Result<String> {
    if name.is_empty()
        || name.len() > 63
        || name.contains('\0')
        || matches!(name, "_id" | "geom" | "__mapseek_feature_id")
    {
        return Err(invalid("invalid vector copy field name"));
    }
    field_type(ty)
}

/// GeoJSON carries exact decimal values as strings; restore types before B can become READY.
pub async fn restore_vector_copy_fields(
    pool: &Pool,
    schema: &str,
    table: &str,
    contract: &VectorCopyContract,
    cancel: &Cancellation,
) -> Result<()> {
    if contract.feature_count < 1 || contract.attributes_schema.len() > 1024 {
        return Err(invalid("invalid nonempty vector copy contract"));
    }
    let fields = contract
        .attributes_schema
        .iter()
        .map(|(name, ty)| Ok((name, copy_field_type(name, ty)?)))
        .collect::<Result<BTreeMap<_, _>>>()?;
    let mut client = acquire_connection(pool, cancel).await?;
    let query_cancel = client.cancel_token();
    let result = {
        let write = async {
            let transaction = client.transaction().await?;
            let current=transaction.query("SELECT a.attname,format_type(a.atttypid,a.atttypmod) FROM pg_attribute a JOIN pg_class c ON c.oid=a.attrelid JOIN pg_namespace n ON n.oid=c.relnamespace WHERE n.nspname=$1 AND c.relname=$2 AND a.attnum>0 AND NOT a.attisdropped AND a.attname NOT IN ('_id','geom')",&[&schema,&table]).await?.into_iter().map(|r|(r.get::<_,String>(0),r.get::<_,String>(1))).collect::<BTreeMap<_,_>>();
            if current
                .keys()
                .any(|name| !contract.attributes_schema.contains_key(name))
            {
                return Err(invalid("uploaded fields disagree with vector copy schema"));
            }
            let mut changes = Vec::new();
            for (name, ty) in fields {
                let existing = current.get(name.as_str());
                let name = quote_ident(name);
                match existing {
                    Some(old) if field_type(old)? == ty => {}
                    Some(_) => changes.push(format!(
                        "ALTER COLUMN {name} TYPE {ty} USING {name}::text::{ty}"
                    )),
                    None => changes.push(format!("ADD COLUMN {name} {ty}")),
                }
            }
            if !changes.is_empty() {
                transaction
                    .batch_execute(&format!(
                        "ALTER TABLE {}.{} {}",
                        quote_ident(schema),
                        quote_ident(table),
                        changes.join(",")
                    ))
                    .await?;
            }
            cancel.check()?;
            anyhow::Ok(transaction)
        };
        match run_with_cancellation(cancel, query_cancel, write).await {
            Ok(transaction) => {
                if let Err(error) = cancel.check() {
                    drop(transaction);
                    Err(error)
                } else {
                    transaction.commit().await.map_err(Into::into)
                }
            }
            Err(error) => Err(error),
        }
    };
    finish_database_result(&client, result).await
}

impl VectorCopyContract {
    fn definitions(&self, target_srid: u32) -> Result<Vec<String>> {
        if self.feature_count != 0
            || self.attributes_schema.len() > 1024
            || !matches!(target_srid, 4326 | 3857)
        {
            return Err(invalid("invalid empty vector copy contract"));
        }
        SpatialRef::from_definition(&self.crs).map_err(|_| invalid("invalid vector copy CRS"))?;
        let kind = self.geometry_type.to_ascii_uppercase();
        if !matches!(
            kind.as_str(),
            "POINT" | "MULTIPOINT" | "LINESTRING" | "MULTILINESTRING" | "POLYGON" | "MULTIPOLYGON"
        ) {
            return Err(invalid("unsupported vector copy geometry type"));
        }
        let mut fields = vec!["\"_id\" BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY".to_owned()];
        for (name, ty) in &self.attributes_schema {
            fields.push(format!(
                "{} {}",
                quote_ident(name),
                copy_field_type(name, ty)?
            ));
        }
        fields.push(format!(
            "geom geometry({kind}{},{target_srid})",
            if self.has_z { "Z" } else { "" }
        ));
        Ok(fields)
    }
}

#[allow(clippy::too_many_arguments)] // Explicit input contract and cancellation belong to this COPY boundary.
pub async fn copy_empty_vector(
    pool: &Pool,
    path: &Path,
    schema: &str,
    table: &str,
    target_srid: u32,
    gdal_s3: Option<GdalS3Config>,
    contract: &VectorCopyContract,
    cancel: &Cancellation,
) -> Result<()> {
    let definitions = contract.definitions(target_srid)?;
    cancel.check()?;
    let path = path.to_owned();
    let token = cancel.clone();
    tokio::task::spawn_blocking(move || -> Result<()> {
        token.check()?;
        apply_thread_local_gdal_s3_config(gdal_s3.as_ref())?;
        let dataset = Dataset::open(&path)?;
        if dataset.layer_count() != 1 {
            return Err(invalid("empty vector copy requires one layer"));
        }
        let mut layer = checked_gdal_call(|| dataset.layer(0))??;
        let mut features = checked_gdal_call(|| layer.features())?;
        if checked_gdal_call(|| features.next())?.is_some() {
            return Err(invalid(
                "empty vector contract does not match uploaded features",
            ));
        }
        token.check()
    })
    .await??;
    let mut client = acquire_connection(pool, cancel).await?;
    let query_cancel = client.cancel_token();
    let result = {
        let write = async {
            let transaction = client.transaction().await?;
            transaction.batch_execute(&format!("CREATE SCHEMA IF NOT EXISTS {}; DROP TABLE IF EXISTS {}.{}; CREATE TABLE {}.{} ({})",quote_ident(schema),quote_ident(schema),quote_ident(table),quote_ident(schema),quote_ident(table),definitions.join(","))).await?;
            PgCopy::create_geom_index(&transaction, schema, table, None).await?;
            PgCopy::create_cursor_index(&transaction, schema, table, None).await?;
            cancel.check()?;
            anyhow::Ok(transaction)
        };
        let result = run_with_cancellation(cancel, query_cancel, write).await;
        match result {
            Ok(transaction) => {
                if let Err(error) = cancel.check() {
                    drop(transaction);
                    Err(error)
                } else {
                    transaction.commit().await.map_err(Into::into)
                }
            }
            Err(error) => Err(error),
        }
    };
    finish_database_result(&client, result).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    #[ignore = "requires SPATIAL_CORE_TEST_POSTGRES_DSN with PostGIS"]
    async fn native_read_failure_is_not_empty_and_preserves_committed_table() -> Result<()> {
        use deadpool_postgres::{Manager, Pool};
        let (directory, path) = crate::pg::tests::failing_vector_fixture(0)?;
        let dsn = std::env::var("SPATIAL_CORE_TEST_POSTGRES_DSN")?;
        let pool = Pool::builder(Manager::new(dsn.parse()?, tokio_postgres::NoTls))
            .max_size(2)
            .build()?;
        let client = pool.get().await?;
        let schema = format!("empty_read_failure_{}", uuid::Uuid::new_v4().simple());
        let qualified = format!("{}.features", quote_ident(&schema));
        client.batch_execute(&format!("CREATE SCHEMA {}; CREATE TABLE {qualified}(sentinel text); INSERT INTO {qualified} VALUES('keep me')", quote_ident(&schema))).await?;
        let contract = VectorCopyContract {
            attributes_schema: BTreeMap::from([("value".into(), "integer".into())]),
            geometry_type: "POINT".into(),
            crs: "EPSG:4326".into(),
            has_z: false,
            feature_count: 0,
            data_revision: String::new(),
        };
        let cancel = Cancellation::default();
        let result: Result<()> = async {
            for _ in 0..2 {
                let result = copy_empty_vector(
                    &pool, &path, &schema, "features", 4326, None, &contract, &cancel,
                )
                .await;
                anyhow::ensure!(
                    result.is_err(),
                    "native read failure was accepted as empty input"
                );
                let rows = client
                    .query(&format!("SELECT sentinel FROM {qualified}"), &[])
                    .await?;
                anyhow::ensure!(
                    rows.len() == 1 && rows[0].get::<_, String>(0) == "keep me",
                    "failed empty read replaced the committed table"
                );
            }
            let empty = directory.path().join("empty.geojson");
            std::fs::write(&empty, r#"{"type":"FeatureCollection","features":[]}"#)?;
            copy_empty_vector(
                &pool, &empty, &schema, "features", 4326, None, &contract, &cancel,
            )
            .await?;
            let count: i64 = client
                .query_one(&format!("SELECT COUNT(*) FROM {qualified}"), &[])
                .await?
                .get(0);
            anyhow::ensure!(count == 0, "valid empty input must still be accepted");
            Ok(())
        }
        .await;
        client
            .batch_execute(&format!("DROP SCHEMA {} CASCADE", quote_ident(&schema)))
            .await?;
        result
    }

    fn contract() -> VectorCopyContract {
        VectorCopyContract {
            attributes_schema: BTreeMap::from([
                ("label".into(), "text".into()),
                ("value".into(), "numeric(30,12)".into()),
            ]),
            geometry_type: "MULTIPOLYGON".into(),
            crs: "EPSG:3857".into(),
            has_z: true,
            feature_count: 0,
            data_revision: String::new(),
        }
    }
    #[test]
    fn empty_schema_accepts_only_complete_safe_types() {
        for ty in [
            "text",
            "varchar(23)",
            "character varying(20)",
            "smallint",
            "integer[]",
            "int4[]",
            "bigint",
            "double precision",
            "numeric(30,12)",
            "numeric(20,-2)",
            "boolean",
            "date",
            "timestamp(3) with time zone",
            "jsonb",
        ] {
            assert!(field_type(ty).is_ok(), "{ty}");
        }
        for ty in [
            "text; DROP TABLE x",
            "varchar(0)",
            "numeric(1001,0)",
            "timestamp(7)",
            "integer[][]",
            "integer[]; DROP TABLE x",
            "varchar(2) CHECK (true)",
        ] {
            assert!(field_type(ty).is_err(), "{ty}");
        }
        let mut value = contract();
        value
            .attributes_schema
            .insert("gis_part_path".into(), "integer[]".into());
        assert!(
            value
                .definitions(3857)
                .unwrap()
                .iter()
                .any(|definition| definition == "\"gis_part_path\" integer[]")
        );
        assert!(
            value
                .definitions(3857)
                .unwrap()
                .last()
                .unwrap()
                .contains("MULTIPOLYGONZ")
        );
        value.attributes_schema.insert("_id".into(), "text".into());
        assert!(value.definitions(3857).is_err());
        value.attributes_schema.remove("_id");
        value.feature_count = 1;
        assert!(value.definitions(3857).is_err());
        value.feature_count = 0;
        value.attributes_schema = (0..1024)
            .map(|i| (format!("field_{i}"), "text".into()))
            .collect();
        assert!(value.definitions(3857).is_ok());
        value
            .attributes_schema
            .insert("overflow".into(), "text".into());
        assert!(value.definitions(3857).is_err());
    }
    #[tokio::test]
    #[ignore = "requires SPATIAL_CORE_TEST_POSTGRES_DSN with PostGIS"]
    async fn edited_copy_preserves_integer_array_paths_empty_and_null() -> Result<()> {
        let dsn = std::env::var("SPATIAL_CORE_TEST_POSTGRES_DSN")?;
        let pool = Pool::builder(deadpool_postgres::Manager::new(
            dsn.parse()?,
            tokio_postgres::NoTls,
        ))
        .max_size(2)
        .build()?;
        let schema = format!("array_copy_{}", uuid::Uuid::new_v4().simple());
        let file = tempfile::NamedTempFile::with_suffix(".geojson")?;
        std::fs::write(
            file.path(),
            r#"{"type":"FeatureCollection","features":[
            {"type":"Feature","properties":{"gis_part_path":[2,1,3]},"geometry":{"type":"Point","coordinates":[1,2]}},
            {"type":"Feature","properties":{"gis_part_path":[]},"geometry":{"type":"Point","coordinates":[2,3]}},
            {"type":"Feature","properties":{"gis_part_path":null},"geometry":{"type":"Point","coordinates":[3,4]}}
        ]}"#,
        )?;
        let outcome: Result<()> = async {
            let mut client = pool.get().await?;
            PgCopy::copy_in(&mut client, &schema, "features", file.path(), 4326).await?;
            let value = VectorCopyContract {
                attributes_schema: BTreeMap::from([("gis_part_path".into(), "integer[]".into())]),
                geometry_type: "POINT".into(),
                crs: "EPSG:4326".into(),
                has_z: false,
                feature_count: 3,
                data_revision: String::new(),
            };
            restore_vector_copy_fields(
                &pool,
                &schema,
                "features",
                &value,
                &Cancellation::default(),
            )
            .await?;
            let rows = client
                .query(
                    &format!(
                        "SELECT gis_part_path FROM {}.features ORDER BY _id",
                        quote_ident(&schema)
                    ),
                    &[],
                )
                .await?;
            for (row, expected) in rows.iter().zip([Some(vec![2, 1, 3]), Some(vec![]), None]) {
                assert_eq!(row.get::<_, Option<Vec<i32>>>(0), expected);
            }
            assert_eq!(rows.len(), 3);
            Ok(())
        }
        .await;
        pool.get()
            .await?
            .batch_execute(&format!(
                "DROP SCHEMA IF EXISTS {} CASCADE",
                quote_ident(&schema)
            ))
            .await?;
        outcome
    }

    #[tokio::test]
    #[ignore = "requires SPATIAL_CORE_TEST_POSTGRES_DSN with PostGIS"]
    async fn edited_copy_restores_exact_field_types_without_losing_precision() -> Result<()> {
        let dsn = std::env::var("SPATIAL_CORE_TEST_POSTGRES_DSN")?;
        let pool = Pool::builder(deadpool_postgres::Manager::new(
            dsn.parse()?,
            tokio_postgres::NoTls,
        ))
        .max_size(2)
        .build()?;
        let schema = format!("edited_copy_{}", uuid::Uuid::new_v4().simple());
        let file = tempfile::NamedTempFile::with_suffix(".geojson")?;
        std::fs::write(
            file.path(),
            r#"{"type":"FeatureCollection","features":[{"type":"Feature","properties":{"large_id":"9007199254740993","value":"123456789012345678.123456789012","enabled":"true","new_label":"working"},"geometry":{"type":"Point","coordinates":[116,39,17]}}]}"#,
        )?;
        let outcome=async {
            let mut client=pool.get().await?;
            PgCopy::copy_in(&mut client,&schema,"features",file.path(),4326).await?;
            drop(client);
            let mut value=contract(); value.feature_count=1; value.geometry_type="POINT".into(); value.crs="EPSG:4326".into();
            value.attributes_schema=BTreeMap::from([("large_id".into(),"bigint".into()),("value".into(),"numeric(30,12)".into()),("enabled".into(),"boolean".into()),("new_label".into(),"text".into())]);
            restore_vector_copy_fields(&pool,&schema,"features",&value,&Cancellation::default()).await?;
            let row=pool.get().await?.query_one(&format!("SELECT pg_typeof(large_id)::text,large_id::text,pg_typeof(value)::text,value::text,ST_Z(geom),enabled::text FROM {}.features",quote_ident(&schema)),&[]).await?;
            anyhow::ensure!(row.get::<_,String>(0)=="bigint","edited bigint became text");
            anyhow::ensure!(row.get::<_,String>(1)=="9007199254740993" && row.get::<_,String>(2)=="numeric" && row.get::<_,String>(3)=="123456789012345678.123456789012" && row.get::<_,f64>(4)==17.0 && row.get::<_,String>(5)=="true");
            let client=pool.get().await?;
            client.batch_execute(&format!("ALTER TABLE {}.features ADD COLUMN broken text; UPDATE {}.features SET broken='not-a-number'",quote_ident(&schema),quote_ident(&schema))).await?;
            value.attributes_schema.insert("large_id".into(),"text".into());
            value.attributes_schema.insert("broken".into(),"bigint".into());
            anyhow::ensure!(restore_vector_copy_fields(&pool,&schema,"features",&value,&Cancellation::default()).await.is_err());
            let ty:String=client.query_one(&format!("SELECT pg_typeof(large_id)::text FROM {}.features",quote_ident(&schema)),&[]).await?.get(0);
            anyhow::ensure!(ty=="bigint","failed type conversion did not roll back");
            client.batch_execute(&format!("UPDATE {}.features SET broken=NULL; ALTER TABLE {}.features ALTER COLUMN broken TYPE numeric USING broken::numeric; UPDATE {}.features SET broken=1.5",quote_ident(&schema),quote_ident(&schema),quote_ident(&schema))).await?;
            anyhow::ensure!(restore_vector_copy_fields(&pool,&schema,"features",&value,&Cancellation::default()).await.is_err(),"fractional values must not silently round into integer columns");
            anyhow::Ok(())
        }.await;
        pool.get()
            .await?
            .batch_execute(&format!(
                "DROP SCHEMA IF EXISTS {} CASCADE",
                quote_ident(&schema)
            ))
            .await?;
        outcome
    }

    #[tokio::test]
    #[ignore = "requires SPATIAL_CORE_TEST_POSTGRES_DSN with PostGIS"]
    async fn empty_copy_preserves_schema_z_and_absent_extent_and_rejects_data_loss() -> Result<()> {
        let dsn = std::env::var("SPATIAL_CORE_TEST_POSTGRES_DSN")?;
        let pool = Pool::builder(deadpool_postgres::Manager::new(
            dsn.parse()?,
            tokio_postgres::NoTls,
        ))
        .max_size(2)
        .build()?;
        let schema = format!("empty_copy_{}", uuid::Uuid::new_v4().simple());
        let file = tempfile::NamedTempFile::with_suffix(".geojson")?;
        std::fs::write(file.path(), r#"{"type":"FeatureCollection","features":[]}"#)?;
        let outcome=async {
            let mut value=contract();
            value.attributes_schema.insert("large_id".into(),"bigint".into());
            value.attributes_schema.insert("enabled".into(),"boolean".into());
            value.attributes_schema.insert("captured".into(),"timestamp(3) with time zone".into());
            value.attributes_schema.insert("gis_part_path".into(),"integer[]".into());
            copy_empty_vector(&pool,file.path(),&schema,"features",3857,None,&value,&Cancellation::default()).await?;
            let metadata=crate::vector::metadata::compute_vector_metadata(&pool,&schema,"features",&Cancellation::default()).await?;
            anyhow::ensure!(metadata.feature_count==0 && metadata.bound.is_none() && metadata.has_z && metadata.srid==3857 && metadata.geometry_type=="MULTIPOLYGON");
            anyhow::ensure!(metadata.attributes_schema["gis_part_path"]=="integer[]");
            let client=pool.get().await?;
            let types=client.query("SELECT a.attname,format_type(a.atttypid,a.atttypmod) FROM pg_attribute a JOIN pg_class c ON c.oid=a.attrelid JOIN pg_namespace n ON n.oid=c.relnamespace WHERE n.nspname=$1 AND c.relname='features' AND a.attnum>0 AND NOT a.attisdropped",&[&schema]).await?.iter().map(|r|(r.get::<_,String>(0),r.get::<_,String>(1))).collect::<BTreeMap<_,_>>();
            anyhow::ensure!(types["large_id"]=="bigint" && types["value"]=="numeric(30,12)" && types["enabled"]=="boolean" && types["geom"]=="geometry(MultiPolygonZ,3857)");
            anyhow::ensure!(types["gis_part_path"]=="integer[]");
            client.batch_execute(&format!("INSERT INTO {}.features(label) VALUES ('must stay')",quote_ident(&schema))).await?;
            std::fs::write(file.path(),r#"{"type":"FeatureCollection","features":[{"type":"Feature","properties":{},"geometry":{"type":"Point","coordinates":[1,2]}}]}"#)?;
            let error=copy_empty_vector(&pool,file.path(),&schema,"features",3857,None,&value,&Cancellation::default()).await.unwrap_err();
            anyhow::ensure!(error.is::<InvalidInput>());
            let count:i64=client.query_one(&format!("SELECT count(*) FROM {}.features",quote_ident(&schema)),&[]).await?.get(0);
            anyhow::ensure!(count==1,"mismatched empty contract must not replace existing data");
            let token=Cancellation::default();token.cancel();
            anyhow::ensure!(copy_empty_vector(&pool,file.path(),&schema,"features",3857,None,&value,&token).await.unwrap_err().is::<crate::control::Cancelled>());
            anyhow::Ok(())
        }.await;
        pool.get()
            .await?
            .batch_execute(&format!(
                "DROP SCHEMA IF EXISTS {} CASCADE",
                quote_ident(&schema)
            ))
            .await?;
        outcome
    }
}
