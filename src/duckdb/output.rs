use std::path::Path;

use ::duckdb::arrow::datatypes::{DataType, Field};
use ::duckdb::params_from_iter;

use super::{
    Control, Error, Query, Reader, Result, VectorMetadata, VectorSource, identifier, literal,
    path_text,
    query::{controlled, schema},
    source::geometry_columns,
};

#[derive(Debug, Clone)]
pub struct FileReport {
    pub rows: u64,
    pub bytes: u64,
    pub metadata: VectorMetadata,
}

impl Query<'_> {
    /// Write WKB GeoParquet 1.1 into an exclusive temporary directory beside the
    /// destination, validate it, then publish without replacing any existing path.
    /// File-byte limits are checked before publication, not as a disk quota.
    /// The first result geometry is primary. Types are recorded as unknown ([])
    /// and bbox is omitted, avoiding another full scan of the source.
    pub fn write_geoparquet(
        self,
        path: impl AsRef<Path>,
        control: &Control<'_>,
    ) -> Result<FileReport> {
        let path = path.as_ref();
        control.check()?;
        if path.symlink_metadata().is_ok() {
            return Err(Error::InvalidInput(
                "output already exists; overwrites are not supported",
            ));
        }
        let parent = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let parent = parent.canonicalize()?;
        let name = path
            .file_name()
            .ok_or(Error::InvalidInput("output requires a file name"))?;
        let destination = parent.join(name);
        path_text(&destination)?;
        let directory = tempfile::Builder::new()
            .prefix(".spatial-duckdb-output-")
            .tempdir_in(parent)?;
        let temporary = directory.path().join("result.parquet");
        let result = controlled(&self.session.connection, control, || {
            let input_schema = schema(&self.session.connection, &self.sql, &self.parameters)?;
            for field in input_schema.fields() {
                validate_attribute_type(field.data_type())?;
            }
            let geometries = geometry_columns(&input_schema)?;
            if geometries.is_empty() {
                return Err(Error::InvalidInput(
                    "GeoParquet output requires a GEOMETRY column",
                ));
            }
            let guarded = geometries
                .iter()
                .map(|column| {
                    let name = identifier(&column.name)?;
                    Ok(format!("coalesce(ST_HasM({name}), false)"))
                })
                .collect::<Result<Vec<_>>>()?;
            let wkb = geometries
                .iter()
                .map(|column| {
                    let name = identifier(&column.name)?;
                    Ok(format!("ST_AsWKB({name}) AS {name}"))
                })
                .collect::<Result<Vec<_>>>()?;
            // DuckDB 2.0's automatic V1 metadata omits all-NULL columns and
            // unknown CRS. Supply complete metadata to prevent silent relabeling.
            let columns: serde_json::Map<String, serde_json::Value> = geometries.iter().map(|column| {
                (column.name.clone(), serde_json::json!({"encoding": "WKB", "geometry_types": [], "crs": column.crs}))
            }).collect();
            let geo = serde_json::json!({"version": "1.1.0", "primary_column": geometries[0].name, "columns": columns});
            let mut sql = format!(
                "SELECT * REPLACE ({}) FROM ({}) AS spatial_core_output WHERE CASE WHEN {} THEN error('GeoParquet V1 cannot preserve M coordinates') ELSE true END",
                wkb.join(", "),
                self.sql,
                guarded.join(" OR ")
            );
            if let Some(limit) = control.max_rows.and_then(|limit| limit.checked_add(1)) {
                sql.push_str(&format!(" LIMIT {limit}"));
            }
            let copy = format!(
                "COPY ({sql}) TO {} (FORMAT PARQUET, GEOPARQUET_VERSION 'NONE', KV_METADATA {{geo: {}}})",
                literal(path_text(&temporary)?)?,
                literal(&geo.to_string())?
            );
            let rows: u64 = self
                .session
                .connection
                .query_row(&copy, params_from_iter(self.parameters.iter()), |row| {
                    row.get(0)
                })
                .map_err(|e| Error::engine("GeoParquet export", e))?;
            if control.max_rows.is_some_and(|limit| rows > limit) {
                return Err(Error::LimitExceeded("row budget"));
            }
            let bytes = temporary.metadata()?.len();
            if control.max_bytes.is_some_and(|limit| bytes > limit) {
                return Err(Error::LimitExceeded("file byte budget"));
            }
            Ok((rows, bytes, input_schema, geometries))
        });
        let result = result.and_then(|(rows, bytes, input_schema, geometries)| {
            let metadata = self.session.inspect(
                &VectorSource::local(&temporary, Reader::GeoParquet),
                control,
            )?;
            if geometries != metadata.geometry_columns
                || input_schema
                    .fields()
                    .iter()
                    .zip(metadata.schema.fields())
                    .any(|(before, after)| {
                        before.name() != after.name() || before.data_type() != after.data_type()
                    })
                || input_schema.fields().len() != metadata.schema.fields().len()
            {
                return Err(Error::Unsupported(
                    "GeoParquet roundtrip changed the field types or geometry CRS",
                ));
            }
            control.check()?;
            // hard_link provides atomic no-replace publication on this filesystem.
            // Both names are in the same parent filesystem; cleanup removes only ours.
            std::fs::hard_link(&temporary, &destination)?;
            Ok(FileReport {
                rows,
                bytes,
                metadata,
            })
        });
        match (result, directory.close()) {
            (result, Ok(())) => result,
            (Ok(_), Err(cleanup)) => Err(Error::PublishedCleanup(cleanup)),
            (Err(primary), Err(cleanup)) => Err(Error::Cleanup {
                primary: Box::new(primary),
                cleanup,
            }),
        }
    }
}

fn validate_nested_field(field: &Field) -> Result<()> {
    if field
        .metadata()
        .get("ARROW:extension:name")
        .is_some_and(|name| name == "geoarrow.wkb")
    {
        return Err(Error::Unsupported(
            "GeoParquet geometry columns must be at the schema root",
        ));
    }
    validate_attribute_type(field.data_type())
}

fn validate_attribute_type(data_type: &DataType) -> Result<()> {
    match data_type {
        DataType::Interval(_) => Err(Error::Unsupported(
            "Parquet INTERVAL loses microseconds; cast it explicitly before exporting",
        )),
        DataType::List(field)
        | DataType::LargeList(field)
        | DataType::ListView(field)
        | DataType::LargeListView(field)
        | DataType::FixedSizeList(field, _)
        | DataType::Map(field, _) => validate_nested_field(field),
        DataType::Struct(fields) => fields
            .iter()
            .try_for_each(|field| validate_nested_field(field)),
        DataType::Union(fields, _) => fields
            .iter()
            .try_for_each(|(_, field)| validate_nested_field(field)),
        DataType::Dictionary(key, value) => {
            validate_attribute_type(key)?;
            validate_attribute_type(value)
        }
        DataType::RunEndEncoded(ends, values) => {
            validate_nested_field(ends)?;
            validate_nested_field(values)
        }
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::duckdb::{
        Value,
        arrow::array::{Decimal128Array, Int64Array, TimestampNanosecondArray},
        test_session,
    };

    fn export(
        session: &mut crate::duckdb::Session,
        path: &Path,
        control: &Control<'_>,
    ) -> Result<FileReport> {
        session.query("SELECT 9007199254740993::BIGINT AS id, '12345678901234567890.123456789012345678'::DECIMAL(38,18) AS exact, '2026-11-17 01:02:03.123456789'::TIMESTAMP_NS AS time, 'POINT(120 30)'::GEOMETRY('OGC:CRS84') AS geom", []).write_geoparquet(path, control)
    }

    #[test]
    #[ignore = "requires version-matched Spatial/httpfs extensions"]
    fn geoparquet_roundtrip_preserves_precision_crs_and_no_replace() {
        let directory = tempfile::tempdir().unwrap();
        let mut session = test_session(directory.path());
        let path = directory.path().join("引用'中文.parquet");
        let report = export(&mut session, &path, &Control::default())
            .unwrap_or_else(|e| panic!("{e}: {:?}", e.sensitive_diagnostic()));
        assert_eq!(report.rows, 1);
        assert_eq!(report.metadata.geometry_columns.len(), 1);
        assert_eq!(
            report.metadata.geometry_columns[0].crs.as_ref().unwrap()["id"]["code"],
            "CRS84"
        );
        let before = std::fs::read(&path).unwrap();
        assert!(export(&mut session, &path, &Control::default()).is_err());
        assert_eq!(before, std::fs::read(&path).unwrap());
        session
            .register(
                "data\"中文",
                &VectorSource::local(&path, Reader::GeoParquet),
                &Control::default(),
            )
            .unwrap();
        session
            .query(
                "SELECT id, exact, time FROM \"data\"\"中文\" WHERE id = ?",
                [Value::BigInt(9007199254740993)],
            )
            .for_each_batch(&Control::default(), |batch| {
                assert_eq!(
                    batch
                        .column(0)
                        .as_any()
                        .downcast_ref::<Int64Array>()
                        .unwrap()
                        .value(0),
                    9007199254740993
                );
                assert_eq!(
                    batch
                        .column(1)
                        .as_any()
                        .downcast_ref::<Decimal128Array>()
                        .unwrap()
                        .value(0),
                    12345678901234567890123456789012345678_i128
                );
                let time = batch
                    .column(2)
                    .as_any()
                    .downcast_ref::<TimestampNanosecondArray>()
                    .unwrap()
                    .value(0);
                assert_eq!(time % 1_000_000_000, 123456789);
                Ok(())
            })
            .unwrap();
        session.close().unwrap();
    }

    #[test]
    #[ignore = "requires version-matched Spatial/httpfs extensions"]
    fn failed_export_removes_temporary_files_and_never_publishes() {
        let directory = tempfile::tempdir().unwrap();
        let mut session = test_session(directory.path());
        let path = directory.path().join("failed.parquet");
        for control in [
            Control {
                max_rows: Some(0),
                ..Control::default()
            },
            Control {
                max_bytes: Some(1),
                ..Control::default()
            },
        ] {
            assert!(matches!(
                export(&mut session, &path, &control),
                Err(Error::LimitExceeded(_))
            ));
            assert!(!path.exists());
        }
        for sql in [
            "SELECT 'POINT M(1 2 3)'::GEOMETRY AS geom",
            "SELECT 'POINT(1 2)'::GEOMETRY AS geom, ['POINT M(3 4 5)'::GEOMETRY] AS nested",
            "SELECT 'POINT(1 2)'::GEOMETRY AS geom, {'shape': 'POINT(3 4)'::GEOMETRY} AS nested",
            "SELECT 'POINT(1 2)'::GEOMETRY AS geom, INTERVAL '1 month 2 days 03:04:05.123456' AS duration",
            "SELECT 'POINT(1 2)'::GEOMETRY AS geom, {'parts': [INTERVAL '1 microsecond', INTERVAL '1001 microsecond']} AS attributes",
            "SELECT 'POINT(1 2)'::GEOMETRY AS geom, '12:34:56.123456+05:30'::TIMETZ AS time",
            "SELECT 'POINT(1 2)'::GEOMETRY AS geom, {'times': ['12:34:56.123456+05:30'::TIMETZ]} AS attributes",
            "SELECT CASE WHEN i < 10000 THEN 'POINT(1 2)'::GEOMETRY ELSE error('late failure') END AS geom FROM range(20000) t(i)",
        ] {
            let result = session
                .query(sql, [])
                .write_geoparquet(&path, &Control::default());
            assert!(result.is_err(), "{result:?}");
            assert!(!path.exists());
        }
        assert!(std::fs::read_dir(directory.path()).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".spatial-duckdb-output-")
        }));
        session.close().unwrap();
    }

    #[test]
    #[ignore = "requires version-matched Spatial/httpfs extensions"]
    fn ordinary_nested_attributes_remain_supported() {
        let directory = tempfile::tempdir().unwrap();
        let mut session = test_session(directory.path());
        let report = session.query("SELECT 'POINT(1 2)'::GEOMETRY AS geom, {'values': [1, 2], 'label': '中文'} AS attributes", [])
            .write_geoparquet(directory.path().join("nested.parquet"), &Control::default())
            .unwrap();
        assert_eq!(report.rows, 1);
        session.close().unwrap();
    }

    #[test]
    #[ignore = "requires version-matched Spatial/httpfs extensions"]
    fn empty_and_multiple_geometry_outputs_preserve_unknown_crs() {
        let directory = tempfile::tempdir().unwrap();
        let mut session = test_session(directory.path());
        for (index, suffix) in ["", " WHERE false"].into_iter().enumerate() {
            let path = directory.path().join(format!("geometry-{index}.parquet"));
            let report = session.query(format!("SELECT 'POINT EMPTY'::GEOMETRY AS unknown, NULL::GEOMETRY('EPSG:3857') AS known{suffix}"), []).write_geoparquet(&path, &Control::default()).unwrap_or_else(|e| panic!("{e}: {:?}", e.sensitive_diagnostic()));
            assert_eq!(report.metadata.geometry_columns.len(), 2);
            assert!(report.metadata.geometry_columns[0].crs.is_none());
            assert_eq!(
                report.metadata.geometry_columns[1].crs.as_ref().unwrap()["id"]["code"],
                3857
            );
            assert_eq!(report.rows, if index == 0 { 1 } else { 0 });
        }
        session.close().unwrap();
    }
}
