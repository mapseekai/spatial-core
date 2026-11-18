use std::{
    collections::HashMap,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};

use super::*;
use crate::duckdb::{HttpSecret, S3Secret, Value, test_session};

fn fixtures(session: &mut Session, directory: &std::path::Path) {
    session.connection.execute_batch("CREATE TEMP TABLE fixture AS SELECT 1 AS id, 'POINT(120 30)'::GEOMETRY('EPSG:4326') AS geom").unwrap();
    for (name, driver) in [
        ("points.geojson", "GeoJSON"),
        ("points.fgb", "FlatGeobuf"),
        ("points.gpkg", "GPKG"),
        ("points.shp", "ESRI Shapefile"),
        ("points.kml", "KML"),
        ("points.gml", "GML"),
    ] {
        session
            .connection
            .execute_batch(&format!(
                "COPY fixture TO {} (FORMAT GDAL, DRIVER {}, SRS 'EPSG:4326')",
                literal(path_text(&directory.join(name)).unwrap()).unwrap(),
                literal(driver).unwrap(),
            ))
            .unwrap();
    }
    session
        .query("SELECT * FROM fixture", [])
        .write_geoparquet(directory.join("points.parquet"), &Control::default())
        .unwrap();
}

fn one_point(session: &mut Session, name: &str, source: &VectorSource) -> VectorMetadata {
    let metadata = session
        .register(name, source, &Control::default())
        .unwrap_or_else(|e| panic!("{source:?}: {e}: {:?}", e.sensitive_diagnostic()));
    assert_eq!(metadata.geometry_columns.len(), 1);
    let geometry = identifier(&metadata.geometry_columns[0].name).unwrap();
    let name = identifier(name).unwrap();
    let (count, x, y): (u64, f64, f64) = session
        .connection
        .query_row(
            &format!("SELECT count(*), min(ST_X({geometry})), min(ST_Y({geometry})) FROM {name}"),
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!((count, x, y), (1, 120.0, 30.0));
    metadata
}

#[test]
#[ignore = "requires version-matched Spatial/httpfs extensions"]
fn local_vector_formats_and_system_gdal_coexist() {
    let directory = tempfile::tempdir().unwrap();
    let mut session = test_session(directory.path());
    fixtures(&mut session, directory.path());
    for (i, (name, reader)) in [
        ("points.parquet", Reader::GeoParquet),
        ("points.geojson", Reader::Gdal),
        ("points.fgb", Reader::Gdal),
        ("points.gpkg", Reader::Gdal),
        ("points.shp", Reader::Gdal),
        ("points.kml", Reader::Gdal),
        ("points.gml", Reader::Gdal),
    ]
    .into_iter()
    .enumerate()
    {
        let path = directory.path().join(name);
        let metadata = one_point(
            &mut session,
            &format!("data{i}"),
            &VectorSource::local(&path, reader),
        );
        assert!(
            metadata.geometry_columns[0].crs.is_some(),
            "{name} lost CRS"
        );
        if reader == Reader::Gdal {
            use gdal::vector::LayerAccess;
            let dataset = gdal::Dataset::open(path).unwrap();
            assert_eq!(dataset.layer(0).unwrap().feature_count(), 1);
        }
    }
    assert!(
        session
            .register(
                "data0",
                &VectorSource::local(directory.path().join("points.parquet"), Reader::GeoParquet),
                &Control::default()
            )
            .is_err()
    );
    let csv = directory.path().join("points.csv");
    std::fs::write(&csv, "id,WKT\n1,POINT(120 30)\n").unwrap();
    one_point(
        &mut session,
        "csv",
        &VectorSource::local(&csv, Reader::Gdal)
            .allowed_drivers(vec!["CSV".into()])
            .open_options(vec![
                "GEOM_POSSIBLE_NAMES=WKT".into(),
                "KEEP_GEOM_COLUMNS=NO".into(),
            ]),
    );
    // Capability gating rejects old extensions before touching the source.
    session.gdal_read_supported = false;
    assert!(matches!(
        session.inspect(
            &VectorSource::local("missing.fgb", Reader::Gdal),
            &Control::default()
        ),
        Err(Error::Unsupported(_))
    ));
    session.close().unwrap();
}

#[test]
#[ignore = "requires the patched Spatial extension"]
fn gdal_metadata_and_scan_share_driver_options_and_layers() {
    use gdal::vector::{LayerAccess, sql::Dialect};
    let directory = tempfile::tempdir().unwrap();
    let mut session = test_session(directory.path());
    fixtures(&mut session, directory.path());
    let path = directory.path().join("points.gpkg");
    let dataset = gdal::Dataset::open_ex(
        &path,
        gdal::DatasetOptions {
            open_flags: gdal::GdalOpenFlags::GDAL_OF_UPDATE | gdal::GdalOpenFlags::GDAL_OF_VECTOR,
            ..Default::default()
        },
    )
    .unwrap();
    dataset
        .execute_sql("CREATE TABLE hidden (id INTEGER)", None, Dialect::DEFAULT)
        .unwrap();
    drop(dataset);
    let source = VectorSource::local(&path, Reader::Gdal)
        .allowed_drivers(vec!["GPKG".into()])
        .open_options(vec!["LIST_ALL_TABLES=NO".into()]);
    let metadata = session.inspect(&source, &Control::default()).unwrap();
    assert_eq!(metadata.layers.len(), 1);
    assert_eq!(metadata.selected_layer.as_ref(), metadata.layers.first());
    one_point(&mut session, "visible", &source);
    let all = VectorSource::local(&path, Reader::Gdal)
        .layer(metadata.layers[0].clone())
        .open_options(vec!["LIST_ALL_TABLES=YES".into()]);
    let all_metadata = session.inspect(&all, &Control::default()).unwrap();
    assert!(all_metadata.layers.iter().any(|name| name == "hidden"));
    assert!(
        session
            .inspect(
                &VectorSource::local(&path, Reader::Gdal)
                    .open_options(vec!["LIST_ALL_TABLES=YES".into()]),
                &Control::default()
            )
            .is_err()
    );
    assert!(
        session
            .inspect(&source.clone().layer("hidden"), &Control::default())
            .is_err()
    );
    assert!(
        session
            .inspect(
                &source.allowed_drivers(vec!["FlatGeobuf".into()]),
                &Control::default()
            )
            .is_err()
    );

    let shape = directory.path().join("points.shp");
    let dataset = gdal::Dataset::open(&shape).unwrap();
    let layer_name = dataset.layer(0).unwrap().name();
    drop(dataset);
    let metadata = one_point(
        &mut session,
        "shape_siblings",
        &VectorSource::local(shape, Reader::Gdal)
            .layer(layer_name)
            .sibling_files(
                ["shp", "shx", "dbf", "prj"]
                    .map(|ext| {
                        directory
                            .path()
                            .join(format!("points.{ext}"))
                            .to_str()
                            .unwrap()
                            .to_owned()
                    })
                    .to_vec(),
            ),
    );
    assert!(
        metadata.geometry_columns[0].crs.is_some(),
        "sibling .prj was ignored"
    );
    session.close().unwrap();
}

#[test]
#[ignore = "requires the patched Spatial extension"]
fn gdal_geometry_names_do_not_shadow_attributes() {
    let directory = tempfile::tempdir().unwrap();
    let mut session = test_session(directory.path());
    let cases = [
        (
            "geom",
            vec![("geom", "ordinary geometry name")],
            "geom_1",
            "wkb_geometry",
        ),
        (
            "case",
            vec![("Geom", "mixed case")],
            "geom_1",
            "wkb_geometry",
        ),
        (
            "wkb",
            vec![("wkb_geometry", "ordinary WKB name")],
            "geom",
            "wkb_geometry_1",
        ),
        (
            "wkb_case",
            vec![("WKB_GEOMETRY", "uppercase WKB")],
            "geom",
            "wkb_geometry_1",
        ),
        (
            "suffix_control",
            vec![("geom_1", "reserved suffix")],
            "geom",
            "wkb_geometry",
        ),
        (
            "suffixes",
            vec![
                ("geom", "first"),
                ("geom_1", "second"),
                ("GEOM_2", "third"),
                ("wkb_geometry", "fourth"),
                ("wkb_geometry_1", "fifth"),
                ("WKB_GEOMETRY_2", "sixth"),
                ("label\"中文", "quoted 属性"),
            ],
            "geom_3",
            "wkb_geometry_3",
        ),
        (
            "csv",
            vec![("shape", "POINT(120 30)"), ("shape_1", "reserved")],
            "shape_2",
            "shape_2",
        ),
    ];
    for (name, attributes, geometry_name, wkb_name) in cases {
        let path = directory.path().join(format!(
            "{name}.{}",
            if name == "csv" { "csv" } else { "geojson" }
        ));
        let source = if name == "csv" {
            std::fs::write(&path, "id,shape,shape_1\n1,POINT(120 30),reserved\n").unwrap();
            VectorSource::local(&path, Reader::Gdal)
                .open_options(vec!["GEOM_POSSIBLE_NAMES=shape".into()])
        } else {
            let mut properties = serde_json::Map::new();
            properties.insert("id".into(), Json::from(1));
            for (attribute, value) in &attributes {
                properties.insert((*attribute).into(), Json::from(*value));
            }
            std::fs::write(
                &path,
                serde_json::json!({
                    "type": "FeatureCollection",
                    "features": [{"type": "Feature", "properties": properties,
                        "geometry": {"type": "Point", "coordinates": [120, 30]}}]
                })
                .to_string(),
            )
            .unwrap();
            VectorSource::local(&path, Reader::Gdal)
        };
        let inspected = session
            .inspect(&source, &Control::default())
            .unwrap_or_else(|e| panic!("{name}: {e}: {:?}", e.sensitive_diagnostic()));
        let view = format!("names_{name}");
        let registered = session
            .register(&view, &source, &Control::default())
            .unwrap();
        assert_eq!(inspected.schema, registered.schema);
        assert_eq!(registered.geometry_columns.len(), 1);
        assert_eq!(registered.geometry_columns[0].name, geometry_name);
        let view_sql = identifier(&view).unwrap();
        let output = directory.path().join(format!("{name}.parquet"));
        session
            .query(format!("SELECT * FROM {view_sql}"), [])
            .write_geoparquet(&output, &Control::default())
            .unwrap();
        let roundtrip = format!("{view}_roundtrip");
        let metadata = session
            .register(
                &roundtrip,
                &VectorSource::local(output, Reader::GeoParquet),
                &Control::default(),
            )
            .unwrap();
        assert_eq!(metadata.geometry_columns, registered.geometry_columns);
        let roundtrip_sql = identifier(&roundtrip).unwrap();
        let raw_sql = format!(
            "SELECT * FROM ST_Read({}, keep_wkb=true{})",
            literal(path_text(&path).unwrap()).unwrap(),
            if name == "csv" {
                ", open_options=['GEOM_POSSIBLE_NAMES=shape']"
            } else {
                ""
            }
        );
        for relation in [&view_sql, &roundtrip_sql, &format!("({raw_sql})")] {
            for (attribute, expected) in &attributes {
                let actual: String = session
                    .connection
                    .query_row(
                        &format!(
                            "SELECT {}::VARCHAR FROM {relation}",
                            identifier(attribute).unwrap()
                        ),
                        [],
                        |row| row.get(0),
                    )
                    .unwrap();
                assert_eq!(&actual, expected, "{name}: {attribute} in {relation}");
            }
        }
        let geometry = identifier(geometry_name).unwrap();
        for relation in [&view_sql, &roundtrip_sql] {
            let (id, x, y): (i64, f64, f64) = session
                .connection
                .query_row(
                    &format!(
                        "SELECT id::BIGINT, ST_X({geometry}), ST_Y({geometry}) FROM {relation}"
                    ),
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .unwrap();
            assert_eq!((id, x, y), (1, 120.0, 30.0));
        }
        let raw_geometry = identifier(wkb_name).unwrap();
        let (wkb, x, y): (Vec<u8>, f64, f64) = session.connection.query_row(
            &format!("SELECT {raw_geometry}, ST_X(ST_GeomFromWKB({raw_geometry})), ST_Y(ST_GeomFromWKB({raw_geometry})) FROM ({raw_sql})"),
            [], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?))).unwrap();
        assert_eq!((wkb.len(), x, y), (21, 120.0, 30.0));
    }
    let plain = directory.path().join("nonspatial.csv");
    std::fs::write(&plain, "id,wkb_geometry\n1,ordinary attribute\n").unwrap();
    let metadata = session
        .register(
            "nonspatial",
            &VectorSource::local(plain, Reader::Gdal),
            &Control::default(),
        )
        .unwrap();
    assert!(metadata.geometry_columns.is_empty());
    let value: String = session
        .connection
        .query_row("SELECT wkb_geometry FROM nonspatial", [], |row| row.get(0))
        .unwrap();
    assert_eq!(value, "ordinary attribute");
    session.close().unwrap();
}

#[test]
#[ignore = "requires the patched Spatial extension"]
fn gdal_empty_attribute_names_remain_unique_after_binding() {
    let directory = tempfile::tempdir().unwrap();
    let mut session = test_session(directory.path());
    let cases = [
        (
            "empty_control",
            r#"{"type":"Feature","properties":{"row_key":"first","":"unnamed"},"geometry":{"type":"Point","coordinates":[120,30]}}"#,
            vec![("C2", "unnamed")],
        ),
        (
            "empty_reserved",
            r#"{"type":"Feature","properties":{"row_key":"first","":"unnamed","C2":"second","C2_1":"third","c2_2":"fourth"},"geometry":{"type":"Point","coordinates":[120,30]}}"#,
            vec![
                ("C2", "unnamed"),
                ("C2_1", "second"),
                ("C2_1_1", "third"),
                ("c2_2", "fourth"),
            ],
        ),
    ];
    for (name, data, expected_attributes) in cases {
        let path = directory.path().join(format!("{name}.geojson"));
        std::fs::write(&path, data).unwrap();
        let source = VectorSource::local(&path, Reader::Gdal);
        let inspected = session
            .inspect(&source, &Control::default())
            .unwrap_or_else(|e| panic!("{name}: {e}: {:?}", e.sensitive_diagnostic()));
        let metadata = session
            .register(name, &source, &Control::default())
            .unwrap();
        assert_eq!(metadata.schema, inspected.schema);
        assert_eq!(
            metadata.schema.fields().len(),
            expected_attributes.len() + 3
        );
        assert_eq!(metadata.schema.field(2).name(), "C2");
        let view = identifier(name).unwrap();
        let output = directory.path().join(format!("{name}.parquet"));
        session
            .query(format!("SELECT * FROM {view}"), [])
            .write_geoparquet(&output, &Control::default())
            .unwrap();
        let roundtrip = format!("{name}_roundtrip");
        let readback = session
            .register(
                &roundtrip,
                &VectorSource::local(output, Reader::GeoParquet),
                &Control::default(),
            )
            .unwrap();
        assert_eq!(metadata.geometry_columns, readback.geometry_columns);
        let roundtrip_view = identifier(&roundtrip).unwrap();
        let raw = format!(
            "ST_Read({}, keep_wkb=true)",
            literal(path_text(&path).unwrap()).unwrap()
        );
        for relation in [&view, &roundtrip_view, &raw] {
            for (attribute, value) in &expected_attributes {
                let actual: String = session
                    .connection
                    .query_row(
                        &format!(
                            "SELECT {} FROM {relation} WHERE row_key='first'",
                            identifier(attribute).unwrap()
                        ),
                        [],
                        |row| row.get(0),
                    )
                    .unwrap();
                assert_eq!(actual.as_str(), *value, "{name}: {attribute}");
            }
        }
        let geometry = identifier(&metadata.geometry_columns[0].name).unwrap();
        for relation in [&view, &roundtrip_view] {
            let (x, y): (f64, f64) = session
                .connection
                .query_row(
                    &format!("SELECT ST_X({geometry}), ST_Y({geometry}) FROM {relation}"),
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .unwrap();
            assert_eq!((x, y), (120.0, 30.0));
        }
    }
    session.close().unwrap();
}

fn gdal_failure_is_not_success(
    session: &mut Session,
    path: &std::path::Path,
    output: &std::path::Path,
) -> &'static str {
    let source = VectorSource::local(path, Reader::Gdal);
    let sql = source.read_sql(path_text(path).unwrap(), None).unwrap();
    let result = session
        .query(&sql, [])
        .for_each_batch(&Control::default(), |_| Ok(()));
    let stage = match result {
        Err(Error::Engine { stage, .. }) => stage,
        other => panic!("{}: {other:?}", path.display()),
    };
    let result = session
        .query(&sql, [])
        .write_geoparquet(output, &Control::default());
    assert!(
        matches!(result, Err(Error::Engine { .. })),
        "{}: {result:?}",
        path.display()
    );
    assert!(!output.exists(), "corrupt input was published");
    let recovery = output.with_extension("geojson");
    std::fs::write(&recovery, r#"{"type":"FeatureCollection","features":[{"type":"Feature","geometry":{"type":"Point","coordinates":[1,2]},"properties":{}}]}"#).unwrap();
    let recovery_sql = VectorSource::local(&recovery, Reader::Gdal)
        .read_sql(path_text(&recovery).unwrap(), None)
        .unwrap();
    assert_eq!(
        session
            .query(recovery_sql, [])
            .for_each_batch(&Control::default(), |_| Ok(()))
            .unwrap()
            .rows,
        1
    );
    stage
}

#[test]
#[ignore = "requires the patched Spatial extension"]
fn truncated_gdal_datasets_fail_instead_of_publishing_empty_or_null_rows() {
    use gdal::vector::{LayerAccess, sql::Dialect};
    let directory = tempfile::tempdir().unwrap();
    let mut session = test_session(directory.path());
    for (name, driver) in [
        ("points.fgb", "FlatGeobuf"),
        ("points.shp", "ESRI Shapefile"),
        ("points.gpkg", "GPKG"),
    ] {
        let path = directory.path().join(name);
        let options = if driver == "ESRI Shapefile" {
            ""
        } else {
            ", LAYER_CREATION_OPTIONS 'SPATIAL_INDEX=NO'"
        };
        session.connection.execute_batch(&format!("COPY (SELECT i AS id, ST_Point(i, i) AS geom FROM range(100) t(i)) TO {} (FORMAT GDAL, DRIVER {}{options})", literal(path_text(&path).unwrap()).unwrap(), literal(driver).unwrap())).unwrap();
        let mut bytes = std::fs::read(&path).unwrap();
        match driver {
            "FlatGeobuf" => {
                let header = u32::from_le_bytes(bytes[8..12].try_into().unwrap()) as usize;
                let first = 12 + header;
                let first_size =
                    u32::from_le_bytes(bytes[first..first + 4].try_into().unwrap()) as usize;
                bytes.truncate(first + 4 + first_size + 7);
            }
            "ESRI Shapefile" => bytes.truncate(143),
            "GPKG" => {
                let dataset = gdal::Dataset::open(&path).unwrap();
                let mut result = dataset
                    .execute_sql(
                        "SELECT rootpage FROM sqlite_master WHERE name='points'",
                        None,
                        Dialect::SQLITE,
                    )
                    .unwrap()
                    .unwrap();
                let root = result
                    .features()
                    .next()
                    .unwrap()
                    .field_as_integer(0)
                    .unwrap()
                    .unwrap() as usize;
                let page_size = u16::from_be_bytes(bytes[16..18].try_into().unwrap()) as usize;
                let mut offset = (root - 1) * page_size;
                // Follow the rightmost child of interior B-tree pages to damage a late leaf.
                while bytes[offset] == 5 {
                    let child =
                        u32::from_be_bytes(bytes[offset + 8..offset + 12].try_into().unwrap())
                            as usize;
                    offset = (child - 1) * page_size;
                }
                assert_eq!(bytes[offset], 13, "expected SQLite table leaf");
                bytes[offset] = 255;
            }
            _ => unreachable!(),
        }
        std::fs::write(&path, bytes).unwrap();
        gdal_failure_is_not_success(
            &mut session,
            &path,
            &directory.path().join(format!("{name}.parquet")),
        );
    }
    session.close().unwrap();
}

#[test]
#[ignore = "requires the patched Spatial extension"]
fn flatgeobuf_preserves_null_and_empty_geometries_across_batches() {
    use gdal::vector::LayerAccess;
    let directory = tempfile::tempdir().unwrap();
    let mut session = test_session(directory.path());
    for (name, geometry, nulls, empty) in [
        ("all_null", "NULL::GEOMETRY('EPSG:4326')", 4097, 0),
        (
            "mixed",
            "(CASE i % 4 WHEN 0 THEN NULL WHEN 1 THEN ST_Point(5,6) WHEN 2 THEN ST_Point(7,8) ELSE ST_Point(20,30) END)::GEOMETRY('EPSG:4326')",
            1025,
            1024,
        ),
    ] {
        let path = directory.path().join(format!("{name}.fgb"));
        session.connection.execute_batch(&format!(
            "COPY (SELECT i AS id, {geometry} AS geom FROM range(4097) t(i)) TO {} (FORMAT GDAL, DRIVER 'FlatGeobuf', LAYER_CREATION_OPTIONS 'SPATIAL_INDEX=NO')",
            literal(path_text(&path).unwrap()).unwrap(),
        )).unwrap();
        if name == "mixed" {
            // GDAL's FGB writer maps EMPTY to NULL; encode NaN coordinates in the existing Point buffers.
            let marker = [7_f64.to_le_bytes(), 8_f64.to_le_bytes()].concat();
            let coordinates = [f64::NAN.to_le_bytes(); 2].concat();
            let mut bytes = std::fs::read(&path).unwrap();
            let mut offset = 12 + u32::from_le_bytes(bytes[8..12].try_into().unwrap()) as usize;
            let mut replaced = 0;
            while offset < bytes.len() {
                let end = offset
                    + 4
                    + u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap()) as usize;
                if let Some(position) = bytes[offset + 4..end]
                    .windows(16)
                    .position(|value| value == marker)
                {
                    let start = offset + 4 + position;
                    bytes[start..start + 16].copy_from_slice(&coordinates);
                    replaced += 1;
                }
                offset = end;
            }
            assert_eq!(replaced, empty);
            std::fs::write(&path, bytes).unwrap();
        }
        {
            let dataset = gdal::Dataset::open(&path).unwrap();
            let mut layer = dataset.layer(0).unwrap();
            let mut verified = (0_u64, 0_u64, 0_u64);
            for feature in layer.features() {
                verified.0 += 1;
                match feature.geometry_by_index(0) {
                    Err(gdal::errors::GdalError::NullPointer { .. }) => verified.1 += 1,
                    Ok(geometry) if geometry.is_empty() => verified.2 += 1,
                    Ok(_) => (),
                    Err(error) => panic!("fixture geometry: {error}"),
                }
            }
            assert_eq!(verified, (4097, nulls, empty));
        }
        let metadata = session
            .register(
                name,
                &VectorSource::local(&path, Reader::Gdal),
                &Control::default(),
            )
            .unwrap();
        let geom = identifier(&metadata.geometry_columns[0].name).unwrap();
        let counts = format!(
            "count(*), count(*) FILTER (WHERE {geom} IS NULL), count(*) FILTER (WHERE ST_IsEmpty({geom}))"
        );
        let expected = (4097_u64, nulls, empty);
        let actual: (u64, u64, u64) = session
            .connection
            .query_row(
                &format!("SELECT {counts} FROM {}", identifier(name).unwrap()),
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(actual, expected);
        let wkb: (u64, u64) = session.connection.query_row(
            "SELECT count(*) FILTER (WHERE wkb_geometry IS NULL), count(*) FILTER (WHERE octet_length(wkb_geometry) = 0) FROM ST_Read(?, keep_wkb=true)",
            [path_text(&path).unwrap()], |row| Ok((row.get(0)?, row.get(1)?)),
        ).unwrap();
        assert_eq!(wkb, (nulls, 0));
        let matched: u64 = session
            .connection
            .query_row(
                &format!(
                    "SELECT count(*) FROM {} WHERE ST_Intersects({geom}, ST_MakeEnvelope(4,5,6,7))",
                    identifier(name).unwrap()
                ),
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(matched, if name == "mixed" { 1024 } else { 0 });
        let output = directory.path().join(format!("{name}.parquet"));
        session
            .query(format!("SELECT * FROM {}", identifier(name).unwrap()), [])
            .write_geoparquet(&output, &Control::default())
            .unwrap();
        let restored = format!("{name}_restored");
        session
            .register(
                &restored,
                &VectorSource::local(output, Reader::GeoParquet),
                &Control::default(),
            )
            .unwrap();
        let roundtrip: (u64, u64, u64) = session
            .connection
            .query_row(
                &format!("SELECT {counts} FROM {}", identifier(&restored).unwrap()),
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(roundtrip, expected);
    }
    session.close().unwrap();
}

#[test]
#[ignore = "requires the patched Spatial extension"]
fn gdal_sql_errors_before_and_after_valid_rows_are_not_eof() {
    let directory = tempfile::tempdir().unwrap();
    let mut session = test_session(directory.path());
    let path = directory.path().join("points.gpkg");
    session.connection.execute_batch(&format!("COPY (SELECT i AS id, ST_Point(i, i) AS geom FROM range(5000) t(i)) TO {} (FORMAT GDAL, DRIVER 'GPKG', LAYER_CREATION_OPTIONS 'SPATIAL_INDEX=NO')", literal(path_text(&path).unwrap()).unwrap())).unwrap();
    for valid_rows in [0, 1, 2047, 2048, 2049, 4097] {
        let vrt = directory.path().join(format!("broken{valid_rows}.vrt"));
        std::fs::write(&vrt, format!(r#"<OGRVRTDataSource><OGRVRTLayer name="broken"><SrcDataSource relativeToVRT="1">points.gpkg</SrcDataSource><SrcSQL dialect="SQLITE">SELECT geom, json_extract(CASE WHEN fid &gt; {valid_rows} THEN 'invalid-json' ELSE '{{"a":1}}' END, '$.a') AS value FROM points</SrcSQL><GeometryType>wkbPoint</GeometryType><LayerSRS>EPSG:4326</LayerSRS><Field name="value" type="Integer"/><FeatureCount>5000</FeatureCount><ExtentXMin>0</ExtentXMin><ExtentYMin>0</ExtentYMin><ExtentXMax>4999</ExtentXMax><ExtentYMax>4999</ExtentYMax></OGRVRTLayer></OGRVRTDataSource>"#)).unwrap();
        if valid_rows >= 2047 {
            session
                .query(
                    "SELECT * FROM ST_Read(?)",
                    [Value::Text(path_text(&vrt).unwrap().into())],
                )
                .schema(&Control::default())
                .unwrap();
        }
        let stage = gdal_failure_is_not_success(
            &mut session,
            &vrt,
            &directory.path().join(format!("broken{valid_rows}.parquet")),
        );
        if valid_rows >= 2047 {
            assert_eq!(stage, "fetch Arrow batch");
        }
    }
    // A real empty layer remains a successful zero-row read/export.
    let empty = directory.path().join("empty.geojson");
    std::fs::write(&empty, r#"{"type":"FeatureCollection","features":[]}"#).unwrap();
    session
        .register(
            "empty",
            &VectorSource::local(empty, Reader::Gdal),
            &Control::default(),
        )
        .unwrap();
    assert_eq!(
        session
            .query("SELECT * FROM empty", [])
            .write_geoparquet(directory.path().join("empty.parquet"), &Control::default())
            .unwrap()
            .rows,
        0
    );
    session.close().unwrap();
}

struct Server {
    address: String,
    stop: Arc<AtomicBool>,
    requests: Arc<Mutex<Vec<String>>>,
    worker: Option<thread::JoinHandle<()>>,
}

impl Server {
    fn start(files: HashMap<String, Vec<u8>>) -> Self {
        Self::with_redirect(files, None)
    }

    fn with_redirect(files: HashMap<String, Vec<u8>>, redirect: Option<String>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap().to_string();
        listener.set_nonblocking(true).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let stopped = stop.clone();
        let captured = requests.clone();
        let worker = thread::spawn(move || {
            while !stopped.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((stream, _)) => serve(stream, &files, &captured, redirect.as_deref()),
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::park_timeout(Duration::from_millis(2))
                    }
                    Err(error) => panic!("fixture listener: {error}"),
                }
            }
        });
        Self {
            address,
            stop,
            requests,
            worker: Some(worker),
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            worker.thread().unpark();
            worker.join().unwrap();
        }
    }
}

fn serve(
    mut stream: TcpStream,
    files: &HashMap<String, Vec<u8>>,
    captured: &Mutex<Vec<String>>,
    redirect: Option<&str>,
) {
    // Accepted sockets inherit nonblocking mode on macOS.
    stream.set_nonblocking(false).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    stream
        .set_write_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let mut request = Vec::new();
    let mut buffer = [0; 4096];
    while request.len() < 65536 && !request.windows(4).any(|part| part == b"\r\n\r\n") {
        let Ok(size) = stream.read(&mut buffer) else {
            return;
        };
        if size == 0 {
            return;
        }
        request.extend_from_slice(&buffer[..size]);
    }
    let request = String::from_utf8(request).unwrap();
    captured.lock().unwrap().push(request.clone());
    let mut parts = request.lines().next().unwrap().split_whitespace();
    let method = parts.next().unwrap();
    let uri = parts.next().unwrap();
    let path = uri.split('?').next().unwrap();
    if let Some(base) = redirect {
        let name = path.rsplit('/').next().unwrap();
        let _ = stream.write_all(format!("HTTP/1.1 302 Found\r\nLocation: {base}/public/{name}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").as_bytes());
        return;
    }
    let lower = request.to_ascii_lowercase();
    let denied =
        path.starts_with("/private/") && !lower.contains("authorization: bearer local-token");
    let signed = !path.starts_with("/bucket/") || request.contains("Credential=fixture-key/");
    let Some(file) = files.get(path).filter(|_| !denied && signed) else {
        let _ = stream
            .write_all(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        return;
    };
    let range = lower
        .lines()
        .find_map(|line| line.strip_prefix("range: bytes="));
    let (start, end, partial) =
        if let Some(range) = range.filter(|_| !path.starts_with("/no-range/")) {
            let (start, end) = range.split_once('-').unwrap();
            let start: usize = start.parse().unwrap();
            let end = if end.is_empty() {
                file.len() - 1
            } else {
                end.parse::<usize>().unwrap().min(file.len() - 1)
            };
            (start, end, true)
        } else {
            (0, file.len() - 1, false)
        };
    let Some(body) = file.get(start..=end) else {
        let _ = stream.write_all(format!("HTTP/1.1 416 Range Not Satisfiable\r\nContent-Range: bytes */{}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n", file.len()).as_bytes());
        return;
    };
    let status = if partial {
        "206 Partial Content"
    } else {
        "200 OK"
    };
    let range = if partial {
        format!("Content-Range: bytes {start}-{end}/{}\r\n", file.len())
    } else {
        String::new()
    };
    let headers = format!(
        "HTTP/1.1 {status}\r\nContent-Length: {}\r\nAccept-Ranges: bytes\r\n{range}Connection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(headers.as_bytes());
    if method != "HEAD" {
        let body = if path.starts_with("/truncated/") {
            &body[..body.len() / 2]
        } else {
            body
        };
        let _ = stream.write_all(body);
    }
}

#[test]
#[ignore = "requires extensions and local TCP sockets; fixture checks signing headers, not AWS signature validity"]
fn http_and_s3_readers_range_headers_and_failures() {
    let directory = tempfile::tempdir().unwrap();
    let mut session = test_session(directory.path());
    fixtures(&mut session, directory.path());
    let mut files = HashMap::new();
    for name in [
        "points.parquet",
        "points.geojson",
        "points.fgb",
        "points.gpkg",
        "points.shp",
        "points.shx",
        "points.dbf",
        "points.prj",
        "points.kml",
        "points.gml",
        "points.xsd",
    ] {
        let bytes = std::fs::read(directory.path().join(name)).unwrap();
        for prefix in [
            "/public/",
            "/private/",
            "/bucket/",
            "/truncated/",
            "/no-range/",
        ] {
            files.insert(format!("{prefix}{name}"), bytes.clone());
        }
    }
    let server = Server::start(files);
    let base = format!("http://{}", server.address);
    session
        .add_http_secret(HttpSecret {
            scope: format!("{base}/private/"),
            headers: vec![("Authorization".into(), "Bearer local-token".into())],
        })
        .unwrap();
    session
        .add_s3_secret(S3Secret {
            scope: "s3://bucket/".into(),
            key_id: "fixture-key".into(),
            secret: "fixture-secret".into(),
            session_token: None,
            region: "us-east-1".into(),
            endpoint: Some(server.address.clone()),
            path_style: true,
            use_ssl: false,
        })
        .unwrap();
    for (i, (name, reader)) in [
        ("points.parquet", Reader::GeoParquet),
        ("points.geojson", Reader::Gdal),
        ("points.fgb", Reader::Gdal),
        ("points.gpkg", Reader::Gdal),
        ("points.shp", Reader::Gdal),
        ("points.kml", Reader::Gdal),
        ("points.gml", Reader::Gdal),
    ]
    .into_iter()
    .enumerate()
    {
        for (j, prefix) in [
            format!("{base}/public/"),
            format!("{base}/private/"),
            "s3://bucket/".into(),
        ]
        .into_iter()
        .enumerate()
        {
            one_point(
                &mut session,
                &format!("remote{i}_{j}"),
                &VectorSource::remote(format!("{prefix}{name}"), reader),
            );
        }
    }
    for (name, reader) in [
        ("points.parquet", Reader::GeoParquet),
        ("points.geojson", Reader::Gdal),
        ("points.fgb", Reader::Gdal),
    ] {
        one_point(
            &mut session,
            &format!("signed_{name}"),
            &VectorSource::remote(format!("{base}/public/{name}?signature=keepme"), reader),
        );
    }
    one_point(
        &mut session,
        "no_range",
        &VectorSource::remote(
            format!("{base}/no-range/points.parquet"),
            Reader::GeoParquet,
        ),
    );
    for (name, reader) in [
        ("points.parquet", Reader::GeoParquet),
        ("points.geojson", Reader::Gdal),
        ("points.fgb", Reader::Gdal),
    ] {
        for prefix in ["missing", "truncated"] {
            let source = VectorSource::remote(format!("{base}/{prefix}/{name}"), reader);
            // Some drivers defer I/O until the first scan.
            if session
                .register("failed_remote", &source, &Control::default())
                .is_ok()
            {
                let result = session
                    .query("SELECT * FROM failed_remote", [])
                    .for_each_batch(&Control::default(), |_| Ok(()));
                assert!(result.is_err(), "{prefix}/{name} unexpectedly succeeded");
                session
                    .connection
                    .execute_batch("DROP VIEW failed_remote")
                    .unwrap();
            }
        }
    }
    let requests = server.requests.lock().unwrap();
    assert!(requests.iter().any(|r| r.contains("?signature=keepme")));
    assert!(
        requests
            .iter()
            .any(|r| r.to_ascii_lowercase().contains("range: bytes="))
    );
    assert!(
        requests
            .iter()
            .any(|r| r.contains("Credential=fixture-key/"))
    );
    drop(requests);
    // A separate instance must not inherit the first session's HTTP credentials.
    let mut other = test_session(directory.path());
    assert!(
        other
            .inspect(
                &VectorSource::remote(format!("{base}/private/points.parquet"), Reader::GeoParquet),
                &Control::default()
            )
            .is_err()
    );
    other.close().unwrap();
    session.close().unwrap();
}

#[test]
#[ignore = "requires version-matched Spatial/httpfs extensions"]
fn conflicting_native_parquet_geometry_is_rejected() {
    let directory = tempfile::tempdir().unwrap();
    let mut session = test_session(directory.path());
    let path = directory.path().join("conflict.parquet");
    // Omitted GeoParquet CRS means CRS84, but the physical annotation says 3857.
    let geo = serde_json::json!({"version":"1.1.0","primary_column":"geom","columns":{"geom":{"encoding":"WKB","geometry_types":["Point"]}}});
    session.connection.execute_batch(&format!(
        "COPY (SELECT 'POINT(1 2)'::GEOMETRY('EPSG:3857') AS geom) TO {} (FORMAT PARQUET, GEOPARQUET_VERSION 'NONE', KV_METADATA {{geo:{}}})",
        literal(path_text(&path).unwrap()).unwrap(), literal(&geo.to_string()).unwrap(),
    )).unwrap();
    assert!(matches!(
        session.inspect(
            &VectorSource::local(&path, Reader::GeoParquet),
            &Control::default()
        ),
        Err(Error::Unsupported(_))
    ));
    session.close().unwrap();
}

#[test]
#[ignore = "requires extensions and local TCP sockets"]
fn credentials_are_not_forwarded_to_a_different_http_origin() {
    let directory = tempfile::tempdir().unwrap();
    let mut session = test_session(directory.path());
    fixtures(&mut session, directory.path());
    let files = ["points.parquet", "points.geojson", "points.fgb"]
        .into_iter()
        .map(|name| {
            (
                format!("/public/{name}"),
                std::fs::read(directory.path().join(name)).unwrap(),
            )
        })
        .collect();
    let target = Server::start(files);
    for (index, address) in [
        target.address.clone(),
        target.address.replace("127.0.0.1", "localhost"),
    ]
    .into_iter()
    .enumerate()
    {
        let origin = Server::with_redirect(HashMap::new(), Some(format!("http://{address}")));
        session
            .add_http_secret(HttpSecret {
                scope: format!("http://{}/", origin.address),
                headers: vec![("aUtHoRiZaTiOn".into(), "Bearer redirect-fixture".into())],
            })
            .unwrap();
        session
            .add_s3_secret(S3Secret {
                scope: format!("s3://bucket{index}/"),
                key_id: "fixture-key".into(),
                secret: "fixture-secret".into(),
                session_token: None,
                region: "us-east-1".into(),
                endpoint: Some(origin.address.clone()),
                path_style: true,
                use_ssl: false,
            })
            .unwrap();
        for (file, reader) in [
            ("points.parquet", Reader::GeoParquet),
            ("points.geojson", Reader::Gdal),
            ("points.fgb", Reader::Gdal),
        ] {
            for (protocol, uri) in [
                ("http", format!("http://{}/public/{file}", origin.address)),
                ("s3", format!("s3://bucket{index}/{file}")),
            ] {
                one_point(
                    &mut session,
                    &format!("redirect_{index}_{protocol}_{file}"),
                    &VectorSource::remote(uri, reader),
                );
            }
        }
        assert!(
            origin
                .requests
                .lock()
                .unwrap()
                .iter()
                .any(|request| request.contains("Bearer redirect-fixture"))
        );
    }
    let requests = target.requests.lock().unwrap();
    assert!(!requests.is_empty());
    assert!(requests.iter().all(|request| {
        let request = request.to_ascii_lowercase();
        !request.contains("authorization:") && !request.contains("x-amz-security-token:")
    }));
    drop(requests);
    session.close().unwrap();
}

#[test]
#[ignore = "requires extensions and local TCP sockets"]
fn http_credentials_do_not_match_a_port_prefix() {
    let directory = tempfile::tempdir().unwrap();
    let mut session = test_session(directory.path());
    fixtures(&mut session, directory.path());
    let server = Server::start(HashMap::from([(
        "/public/points.parquet".into(),
        std::fs::read(directory.path().join("points.parquet")).unwrap(),
    )]));
    let scope = format!("http://{}", &server.address[..server.address.len() - 1]);
    session
        .add_http_secret(HttpSecret {
            scope,
            headers: vec![("Authorization".into(), "Bearer fixture".into())],
        })
        .unwrap();
    one_point(
        &mut session,
        "public_port",
        &VectorSource::remote(
            format!("http://{}/public/points.parquet", server.address),
            Reader::GeoParquet,
        ),
    );
    let requests = server.requests.lock().unwrap();
    assert!(!requests.is_empty());
    assert!(
        requests
            .iter()
            .all(|request| !request.to_ascii_lowercase().contains("authorization:"))
    );
    drop(requests);
    session.close().unwrap();
}

#[test]
#[ignore = "requires version-matched Spatial/httpfs extensions"]
fn cancelled_registration_never_leaves_a_view_or_prevents_retry() {
    let directory = tempfile::tempdir().unwrap();
    let mut session = test_session(directory.path());
    let path = directory.path().join("points.parquet");
    session
        .query("SELECT 1 AS id, 'POINT(1 2)'::GEOMETRY AS geom", [])
        .write_geoparquet(&path, &Control::default())
        .unwrap();
    let source = VectorSource::local(&path, Reader::GeoParquet);
    let mut cancelled = 0;
    for attempt in 0..48 {
        let token = crate::control::Cancellation::default();
        let name = format!("registration_{attempt}");
        let result = thread::scope(|scope| {
            scope.spawn(|| {
                thread::sleep(Duration::from_micros(
                    [0, 10, 50, 100, 500, 1000][attempt % 6],
                ));
                token.cancel();
            });
            session.register(
                &name,
                &source,
                &Control {
                    cancellation: Some(&token),
                    ..Control::default()
                },
            )
        });
        match result {
            Err(Error::Cancelled) => {
                cancelled += 1;
                let count: u64 = session
                    .connection
                    .query_row(
                        "SELECT count(*) FROM duckdb_views() WHERE view_name = ?",
                        [&name],
                        |row| row.get(0),
                    )
                    .unwrap();
                assert_eq!(count, 0, "cancelled registration left {name}");
                session
                    .register(&name, &source, &Control::default())
                    .unwrap();
            }
            Ok(_) => {} // A commit that wins the race must remain successful.
            Err(error) => panic!("{error}: {:?}", error.sensitive_diagnostic()),
        }
        assert!(
            session
                .register(&name, &source, &Control::default())
                .is_err()
        );
        let count: u64 = session
            .connection
            .query_row(
                &format!("SELECT count(*) FROM {}", identifier(&name).unwrap()),
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            count, 1,
            "failed duplicate registration removed the existing view"
        );
    }
    assert!(cancelled > 0, "test did not exercise cancellation");
    session.close().unwrap();
}
