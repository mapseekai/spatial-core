use std::{fmt, path::PathBuf};

use ::duckdb::arrow::datatypes::SchemaRef;
use serde_json::Value as Json;

use super::{Control, Error, Result, Session, identifier, literal, path_text};

/// GeoParquet uses DuckDB's columnar reader.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reader {
    GeoParquet,
    /// Requires the signed Spatial patch that propagates GDAL stream/CPL errors.
    Gdal,
}

#[derive(Clone)]
enum Location {
    Local(PathBuf),
    Remote(String),
}

/// A single file or GDAL dataset. Construction performs no I/O.
/// Debug deliberately omits paths, URLs and driver options.
#[derive(Clone)]
pub struct VectorSource {
    location: Location,
    reader: Reader,
    layer: Option<String>,
    allowed_drivers: Vec<String>,
    open_options: Vec<String>,
    sibling_files: Vec<String>,
}

impl fmt::Debug for VectorSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("VectorSource")
            .field("reader", &self.reader)
            .finish_non_exhaustive()
    }
}

impl VectorSource {
    #[must_use]
    pub fn local(path: impl Into<PathBuf>, reader: Reader) -> Self {
        Self::new(Location::Local(path.into()), reader)
    }

    #[must_use]
    pub fn remote(uri: impl Into<String>, reader: Reader) -> Self {
        Self::new(Location::Remote(uri.into()), reader)
    }

    fn new(location: Location, reader: Reader) -> Self {
        Self {
            location,
            reader,
            layer: None,
            allowed_drivers: Vec::new(),
            open_options: Vec::new(),
            sibling_files: Vec::new(),
        }
    }

    #[must_use]
    pub fn layer(mut self, name: impl Into<String>) -> Self {
        self.layer = Some(name.into());
        self
    }

    #[must_use]
    pub fn allowed_drivers(mut self, drivers: Vec<String>) -> Self {
        self.allowed_drivers = drivers;
        self
    }

    #[must_use]
    pub fn open_options(mut self, options: Vec<String>) -> Self {
        self.open_options = options;
        self
    }

    #[must_use]
    pub fn sibling_files(mut self, files: Vec<String>) -> Self {
        self.sibling_files = files;
        self
    }

    fn location(&self) -> Result<String> {
        let value = match &self.location {
            Location::Local(path) => path_text(&path.canonicalize()?)?.to_owned(),
            Location::Remote(uri) => {
                if !["s3://", "https://", "http://"].iter().any(|scheme| {
                    uri.strip_prefix(scheme)
                        .is_some_and(|tail| !tail.is_empty())
                }) {
                    return Err(Error::InvalidInput(
                        "remote sources require an s3://, https:// or http:// URI",
                    ));
                }
                uri.clone()
            }
        };
        if !value.starts_with("http://")
            && !value.starts_with("https://")
            && value.contains(['*', '?', '[', ']'])
        {
            return Err(Error::Unsupported(
                "glob sources; register each concrete file explicitly",
            ));
        }
        if self.reader == Reader::GeoParquet
            && (self.layer.is_some()
                || !self.allowed_drivers.is_empty()
                || !self.open_options.is_empty()
                || !self.sibling_files.is_empty())
        {
            return Err(Error::InvalidInput(
                "GDAL reader options cannot be used with GeoParquet",
            ));
        }
        Ok(value)
    }

    fn read_sql(&self, location: &str, layer: Option<&str>) -> Result<String> {
        if self.reader == Reader::GeoParquet {
            return Ok(format!(
                "SELECT * FROM read_parquet({})",
                literal(location)?
            ));
        }
        let mut options = self.gdal_options(location)?;
        if let Some(layer) = layer {
            options.push(format!("layer = {}", literal(layer)?));
        }
        Ok(format!("SELECT * FROM ST_Read({})", options.join(", ")))
    }

    fn gdal_options(&self, location: &str) -> Result<Vec<String>> {
        let mut options = vec![literal(location)?];
        for (name, values) in [
            ("allowed_drivers", &self.allowed_drivers),
            ("open_options", &self.open_options),
            ("sibling_files", &self.sibling_files),
        ] {
            if !values.is_empty() {
                let values = values
                    .iter()
                    .map(|v| literal(v))
                    .collect::<Result<Vec<_>>>()?;
                options.push(format!("{name} = [{}]", values.join(", ")));
            }
        }
        Ok(options)
    }
}

/// CRS is full PROJJSON from the Arrow extension metadata; None means unknown.
#[derive(Debug, Clone, PartialEq)]
pub struct GeometryColumn {
    pub name: String,
    pub crs: Option<Json>,
}

#[derive(Debug, Clone)]
pub struct VectorMetadata {
    pub schema: SchemaRef,
    pub geometry_columns: Vec<GeometryColumn>,
    /// Available only for GDAL metadata, using the same open options as the scan.
    pub layers: Vec<String>,
    pub selected_layer: Option<String>,
    /// Original GeoParquet 1.x metadata, when using the native reader.
    pub geoparquet: Option<Json>,
}

impl Session {
    /// Bind metadata without scanning features for counts or extents.
    /// Multiple GDAL layers require an explicit selection.
    pub fn inspect(
        &mut self,
        source: &VectorSource,
        control: &Control<'_>,
    ) -> Result<VectorMetadata> {
        super::query::controlled(&self.connection, control, || {
            self.describe_source(source).map(|(_, metadata)| metadata)
        })
    }

    /// Register a lazy temporary view. Existing names are never replaced.
    /// Files must remain stable until all queries using the view have completed.
    pub fn register(
        &mut self,
        name: &str,
        source: &VectorSource,
        control: &Control<'_>,
    ) -> Result<VectorMetadata> {
        let name = identifier(name)?;
        control.check()?;
        let mut transaction = self
            .connection
            .unchecked_transaction()
            .map_err(|e| Error::engine("begin source registration", e))?;
        let result = super::query::controlled(&transaction, control, || {
            let (sql, metadata) = self.describe_source(source)?;
            transaction
                .execute_batch(&format!("CREATE TEMP VIEW {name} AS {sql}"))
                .map_err(|e| Error::engine("register source", e))?;
            Ok(metadata)
        })
        .and_then(|metadata| {
            // The cancellation monitor has joined; a committed view is reported as success.
            transaction
                .execute_batch("COMMIT")
                .map_err(|e| Error::engine("commit source registration", e))?;
            Ok(metadata)
        });
        match result {
            Ok(metadata) => {
                transaction.set_drop_behavior(::duckdb::DropBehavior::Ignore);
                Ok(metadata)
            }
            Err(primary) => match transaction.rollback() {
                Ok(()) => Err(primary),
                Err(rollback) => Err(Error::Rollback {
                    primary: Box::new(primary),
                    rollback: Box::new(Error::engine("rollback source registration", rollback)),
                }),
            },
        }
    }

    fn describe_source(&self, source: &VectorSource) -> Result<(String, VectorMetadata)> {
        if source.reader == Reader::Gdal && !self.gdal_read_supported {
            return Err(Error::Unsupported(
                "GDAL reads require the signed, patched Spatial extension; see docs/DUCKDB.md",
            ));
        }
        let location = source.location()?;
        let mut layers = Vec::new();
        let mut selected_layer = None;
        let mut geoparquet = None;
        match source.reader {
            Reader::GeoParquet => {
                geoparquet = Some(self.parquet_metadata(&location)?);
            }
            Reader::Gdal => {
                let json: String = self
                    .connection
                    .query_row(
                        &format!(
                            "SELECT to_json(layers) FROM ST_Read_Meta({})",
                            source.gdal_options(&location)?.join(", ")
                        ),
                        [],
                        |row| row.get(0),
                    )
                    .map_err(|e| Error::engine("GDAL metadata", e))?;
                let data: Json = serde_json::from_str(&json)
                    .map_err(|_| Error::Unsupported("invalid GDAL layer metadata"))?;
                for layer in data
                    .as_array()
                    .ok_or(Error::Unsupported("invalid GDAL layer metadata"))?
                {
                    layers.push(
                        layer["name"]
                            .as_str()
                            .ok_or(Error::Unsupported("GDAL layer has no name"))?
                            .to_owned(),
                    );
                }
                selected_layer = Some(match &source.layer {
                    Some(layer) if layers.contains(layer) => layer.clone(),
                    Some(_) => {
                        return Err(Error::InvalidInput("selected GDAL layer does not exist"));
                    }
                    None if layers.len() == 1 => layers[0].clone(),
                    None => {
                        return Err(Error::InvalidInput(
                            "select exactly one GDAL layer explicitly",
                        ));
                    }
                });
            }
        }
        let sql = source.read_sql(&location, selected_layer.as_deref())?;
        let schema = super::query::schema(&self.connection, &sql, &[])?;
        let geometry_columns = geometry_columns(&schema)?;
        if let Some(geo) = &geoparquet {
            for name in geo["columns"]
                .as_object()
                .ok_or(Error::Unsupported("missing GeoParquet columns"))?
                .keys()
            {
                if !geometry_columns.iter().any(|column| &column.name == name) {
                    return Err(Error::Unsupported(
                        "GeoParquet geometry was not decoded as GEOMETRY",
                    ));
                }
            }
        }
        Ok((
            sql,
            VectorMetadata {
                schema,
                geometry_columns,
                layers,
                selected_layer,
                geoparquet,
            },
        ))
    }

    pub(crate) fn parquet_metadata(&self, location: &str) -> Result<Json> {
        let json: String = self
            .connection
            .query_row(
                "SELECT decode(value) FROM parquet_kv_metadata(?) WHERE decode(key) = 'geo'",
                [location],
                |row| row.get(0),
            )
            .map_err(|e| Error::engine("GeoParquet metadata", e))?;
        let geo = serde_json::from_str(&json)
            .map_err(|_| Error::Unsupported("invalid GeoParquet JSON metadata"))?;
        validate_geoparquet(&geo)?;
        let native_geometry: bool = self.connection.query_row(
            "SELECT count(*) > 0 FROM parquet_schema(?) WHERE logical_type LIKE 'GeometryType%' OR logical_type LIKE 'GeographyType%'",
            [location], |row| row.get(0),
        ).map_err(|e| Error::engine("GeoParquet physical schema", e))?;
        // shortcut: reject competing CRS authorities until native annotations are supported.
        if native_geometry {
            return Err(Error::Unsupported(
                "native Parquet geometry annotations are not supported by the WKB GeoParquet reader",
            ));
        }
        Ok(geo)
    }
}

pub(crate) fn geometry_columns(schema: &SchemaRef) -> Result<Vec<GeometryColumn>> {
    schema
        .fields()
        .iter()
        .filter(|field| {
            field
                .metadata()
                .get("ARROW:extension:name")
                .is_some_and(|name| name == "geoarrow.wkb")
        })
        .map(|field| {
            let metadata: Json = match field.metadata().get("ARROW:extension:metadata") {
                Some(value) => serde_json::from_str(value)
                    .map_err(|_| Error::Unsupported("invalid geometry CRS metadata"))?,
                None => Json::Null,
            };
            Ok(GeometryColumn {
                name: field.name().clone(),
                crs: metadata.get("crs").filter(|crs| !crs.is_null()).cloned(),
            })
        })
        .collect()
}

fn validate_geoparquet(geo: &Json) -> Result<()> {
    if !geo["version"].as_str().is_some_and(|v| v.starts_with("1.")) {
        return Err(Error::Unsupported(
            "only GeoParquet 1.x metadata is supported",
        ));
    }
    let columns = geo["columns"]
        .as_object()
        .ok_or(Error::Unsupported("missing GeoParquet columns"))?;
    if !geo["primary_column"]
        .as_str()
        .is_some_and(|name| columns.contains_key(name))
    {
        return Err(Error::Unsupported("invalid primary geometry column"));
    }
    for column in columns.values() {
        if column["encoding"] != "WKB" {
            return Err(Error::Unsupported(
                "only WKB GeoParquet encoding is supported",
            ));
        }
        if column.get("edges").is_some_and(|edges| edges != "planar") {
            return Err(Error::Unsupported(
                "spherical GeoParquet edges require an explicit interpretation",
            ));
        }
        if column.get("epoch").is_some_and(|epoch| !epoch.is_null()) {
            return Err(Error::Unsupported(
                "dynamic CRS coordinate epochs are not supported",
            ));
        }
        if column["geometry_types"].as_array().is_some_and(|types| {
            types
                .iter()
                .any(|kind| kind.as_str().is_some_and(|kind| kind.ends_with('M')))
        }) {
            return Err(Error::Unsupported(
                "M coordinates are not supported by GeoParquet 1.x",
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metadata_rejects_unimplemented_semantics() {
        let mut geo = serde_json::json!({"version":"1.1.0", "primary_column":"geom", "columns":{"geom":{"encoding":"WKB", "geometry_types":["Point"]}}});
        assert!(validate_geoparquet(&geo).is_ok());
        geo["columns"]["geom"]["edges"] = "spherical".into();
        assert!(validate_geoparquet(&geo).is_err());
        geo["columns"]["geom"]["edges"] = "planar".into();
        geo["columns"]["geom"]["geometry_types"][0] = "Point ZM".into();
        assert!(validate_geoparquet(&geo).is_err());
    }

    #[test]
    fn sources_redact_urls_and_reject_implicit_globs() {
        let source = VectorSource::remote("https://host/file?token=secret", Reader::GeoParquet);
        assert_eq!(source.location().unwrap(), "https://host/file?token=secret");
        assert!(!format!("{source:?}").contains("token"));
        assert!(
            VectorSource::remote("s3://bucket/*.parquet", Reader::GeoParquet)
                .location()
                .is_err()
        );
        assert!(
            VectorSource::remote("/vsis3/bucket/file", Reader::Gdal)
                .location()
                .is_err()
        );
    }
}

#[cfg(test)]
#[path = "source_test.rs"]
mod integration_tests;
