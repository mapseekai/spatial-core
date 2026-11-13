use super::options::{Dimensions, TranslateOptions, WriteMode, validate_layer_name};
use crate::dataset::{Source, local_name};
use crate::operation::{Error, ErrorKind, Result, Stage};
use gdal::spatial_ref::{AxisMappingStrategy, SpatialRef};
use gdal::vector::{LayerAccess, OGRwkbGeometryType};
use gdal::{Dataset, Driver, DriverManager, DriverType, Metadata};
use std::ffi::CString;
use std::path::{Path, PathBuf};

pub(super) struct Target {
    path: PathBuf,
    pub name: CString,
    pub driver: String,
    pub layers: Vec<String>,
}

pub(super) fn prepare(
    source: &Source,
    input: &Dataset,
    output: &Path,
    options: &TranslateOptions,
) -> Result<Target> {
    let path = absolute_output(output)?;
    reject_alias(source, &path)?;
    let driver = if let Some(name) = &options.driver {
        Some(
            DriverManager::get_driver_by_name(name)
                .map_err(|e| Error::unsupported("output driver is not installed").with_source(e))?,
        )
    } else if options.write_mode == WriteMode::CreateDataset {
        let drivers: Vec<_> =
            DriverManager::get_output_drivers_for_dataset_name(&path, DriverType::Vector).collect();
        if drivers.len() != 1 {
            return Err(Error::unsupported(
                "output format is missing or ambiguous; specify driver explicitly",
            ));
        }
        drivers.into_iter().next()
    } else {
        None
    };
    let driver_name = driver.as_ref().map_or_else(String::new, Driver::short_name);
    let layers = selected_layers(input, options)?;
    let count = if options.sql.is_some() {
        1
    } else {
        layers.len()
    };
    if count > 1 && (options.layer_name.is_some() || options.write_mode != WriteMode::CreateDataset)
    {
        return Err(Error::invalid(
            "a target layer name or update mode requires exactly one source layer",
        ));
    }
    if options.write_mode == WriteMode::CreateDataset {
        check_creation(&path, driver.as_ref().expect("creation driver"), count)?;
    } else if !path.try_exists().map_err(io_error)? {
        return Err(Error::new(
            ErrorKind::Open,
            Stage::OpenOutput,
            "update mode requires an existing output dataset",
        ));
    }
    for definition in [
        &options.source_srs,
        &options.reproject_to,
        &options.assign_srs,
    ]
    .into_iter()
    .flatten()
    {
        parse_srs(definition)?;
    }
    Ok(Target {
        name: local_name(&path)?,
        path,
        driver: driver_name,
        layers,
    })
}

fn absolute_output(path: &Path) -> Result<PathBuf> {
    local_name(path)?;
    if path
        .to_str()
        .is_some_and(|v| v.contains("://") || v.starts_with("/vsi") || v.starts_with("PG:"))
    {
        return Err(Error::invalid(
            "typed output must be a local path; use NativeTranslate for GDAL names",
        ));
    }
    let filename = path
        .file_name()
        .ok_or_else(|| Error::invalid("output must name a file or new dataset directory"))?;
    let parent = path
        .parent()
        .filter(|v| !v.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    Ok(parent.canonicalize().map_err(io_error)?.join(filename))
}

pub(super) fn reject_alias(source: &Source, output: &Path) -> Result<()> {
    let Some(input) = source.local_path() else {
        return Ok(());
    };
    reject_file_alias(input, output)
}

fn reject_file_alias(input: &Path, output: &Path) -> Result<()> {
    let input = input.canonicalize().map_err(io_error)?;
    let resolved = output
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
        .canonicalize()
        .ok()
        .and_then(|parent| output.file_name().map(|name| parent.join(name)));
    let output = resolved.as_deref().unwrap_or(output);
    if let Ok(target) = output.canonicalize() {
        if target == input
            || input.is_dir() && target.starts_with(&input)
            || target.is_dir() && input.starts_with(&target)
        {
            return Err(Error::invalid("input and output refer to the same dataset"));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let a = input.metadata().map_err(io_error)?;
            let b = target.metadata().map_err(io_error)?;
            if a.dev() == b.dev() && a.ino() == b.ino() {
                return Err(Error::invalid(
                    "input and output are hard links to the same dataset",
                ));
            }
        }
    } else if input.is_dir() && output.starts_with(&input) {
        return Err(Error::invalid(
            "output must not be inside the source dataset directory",
        ));
    }
    Ok(())
}

pub(super) fn reject_dataset_alias(input: &Dataset, output: &Path) -> Result<()> {
    let mut targets = vec![output.to_owned()];
    if output.extension().is_some_and(|extension| {
        ["shp", "dbf", "shx"]
            .iter()
            .any(|known| extension.eq_ignore_ascii_case(known))
    }) {
        targets.extend(shapefile_companions(output)?);
    } else if output
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("gpkg"))
    {
        for suffix in ["-wal", "-shm", "-journal"] {
            let mut path = output.as_os_str().to_owned();
            path.push(suffix);
            targets.push(path.into());
        }
    } else if output.is_dir() {
        for entry in std::fs::read_dir(output).map_err(io_error)? {
            targets.push(entry.map_err(io_error)?.path());
        }
    }
    struct Files(*mut *mut std::ffi::c_char);
    impl Drop for Files {
        fn drop(&mut self) {
            // SAFETY: GDALGetFileList transfers this list to the caller; null is accepted.
            unsafe { gdal_sys::CSLDestroy(self.0) };
        }
    }
    // SAFETY: input is live; the returned list is independently owned.
    let files = Files(unsafe { gdal_sys::GDALGetFileList(input.c_dataset()) });
    if files.0.is_null() {
        return Ok(());
    }
    let mut entry = files.0;
    // SAFETY: GDAL returns a null-terminated array of NUL-terminated filenames.
    while !unsafe { *entry }.is_null() {
        // SAFETY: the current non-null entry lives until Files is dropped.
        let name = unsafe { std::ffi::CStr::from_ptr(*entry) };
        #[cfg(unix)]
        let path = {
            use std::os::unix::ffi::OsStrExt;
            Path::new(std::ffi::OsStr::from_bytes(name.to_bytes()))
        };
        #[cfg(not(unix))]
        let path =
            Path::new(name.to_str().map_err(|e| {
                Error::unsupported("cannot inspect dataset filenames").with_source(e)
            })?);
        if path.exists() {
            for target in &targets {
                reject_file_alias(path, target)?;
            }
        }
        // SAFETY: advance within this null-terminated array; the next iteration checks its terminator.
        entry = unsafe { entry.add(1) };
    }
    Ok(())
}

fn check_creation(path: &Path, driver: &Driver, layer_count: usize) -> Result<()> {
    if std::fs::symlink_metadata(path).is_ok() {
        return Err(output_exists());
    }
    if driver.metadata_item("DCAP_VECTOR", "").as_deref() != Some("YES")
        || !(driver.metadata_item("DCAP_CREATE", "").as_deref() == Some("YES")
            || driver
                .metadata_item("DCAP_VECTOR_TRANSLATE_FROM", "")
                .as_deref()
                == Some("YES"))
    {
        return Err(Error::unsupported(
            "selected driver cannot create vector output",
        ));
    }
    if layer_count > 1
        && driver
            .metadata_item("DCAP_MULTIPLE_VECTOR_LAYERS", "")
            .as_deref()
            != Some("YES")
    {
        return Err(Error::unsupported(
            "selected driver does not support multiple vector layers",
        ));
    }
    match driver.short_name().as_str() {
        "GPKG" => {
            for suffix in ["-wal", "-shm", "-journal"] {
                let mut companion = path.as_os_str().to_owned();
                companion.push(suffix);
                if std::fs::symlink_metadata(Path::new(&companion)).is_ok() {
                    return Err(output_exists());
                }
            }
        }
        "ESRI Shapefile" => {
            if path
                .extension()
                .is_some_and(|v| v.eq_ignore_ascii_case("shp") || v.eq_ignore_ascii_case("dbf"))
                && !shapefile_companions(path)?.is_empty()
            {
                return Err(output_exists());
            }
        }
        "GeoJSON" | "FlatGeobuf" | "Parquet" | "PGDUMP" => {}
        _ => {
            if std::fs::read_dir(path.parent().expect("absolute path"))
                .map_err(io_error)?
                .next()
                .is_some()
            {
                return Err(Error::unsupported(
                    "unverified output driver requires a caller-owned empty output directory",
                ));
            }
        }
    }
    Ok(())
}

fn shapefile_companions(path: &Path) -> Result<Vec<PathBuf>> {
    let Some(stem) = path.file_stem() else {
        return Ok(Vec::new());
    };
    let stem = stem.as_encoded_bytes();
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    if !parent.is_dir() {
        return Ok(Vec::new());
    }
    let mut files = Vec::new();
    for entry in std::fs::read_dir(parent).map_err(io_error)? {
        let entry = entry.map_err(io_error)?;
        let name = entry.file_name();
        let bytes = name.as_encoded_bytes();
        if bytes.len() <= stem.len()
            || bytes[stem.len()] != b'.'
            || !bytes[..stem.len()].eq_ignore_ascii_case(stem)
        {
            continue;
        }
        let suffix = &bytes[stem.len() + 1..];
        if [
            "shp", "shx", "dbf", "prj", "cpg", "qix", "sbn", "sbx", "fbn", "fbx", "ain", "aih",
            "ixs", "mxs", "idm", "ind", "qpj", "shp.xml", "dbf.xml",
        ]
        .iter()
        .any(|known| suffix.eq_ignore_ascii_case(known.as_bytes()))
            || suffix.ends_with(b".atx")
        {
            files.push(entry.path());
        }
    }
    Ok(files)
}

fn selected_layers(input: &Dataset, options: &TranslateOptions) -> Result<Vec<String>> {
    if options.sql.is_some() {
        return Ok(Vec::new());
    }
    let names = options
        .layers
        .clone()
        .unwrap_or_else(|| input.layers().map(|layer| layer.name()).collect());
    if names.is_empty() {
        return Err(Error::new(
            ErrorKind::LayerNotFound,
            Stage::Inspect,
            "input has no vector layers",
        ));
    }
    let mut resolved = Vec::with_capacity(names.len());
    for name in &names {
        validate_layer_name(name)?;
        let mut layer = input.layer_by_name(name).map_err(|e| {
            Error::new(
                ErrorKind::LayerNotFound,
                Stage::Inspect,
                "source layer does not exist",
            )
            .with_source(e)
        })?;
        let resolved_name = layer.name();
        if resolved
            .iter()
            .any(|name: &String| name.eq_ignore_ascii_case(&resolved_name))
        {
            return Err(Error::invalid("source layer names collide in the output"));
        }
        resolved.push(resolved_name);
        if let Some(fields) = &options.fields {
            let available: Vec<_> = layer
                .defn()
                .fields()
                .map(|f| f.name())
                .chain(layer.defn().geom_fields().map(|f| f.name()))
                .collect();
            if fields.iter().any(|field| !available.contains(field)) {
                return Err(Error::invalid(
                    "selected field does not exist in every selected layer",
                ));
            }
        }
        if let Some(expression) = &options.where_clause {
            layer
                .set_attribute_filter(expression)
                .map_err(|e| Error::native(Stage::Inspect, e))?;
            layer.clear_attribute_filter();
        }
    }
    Ok(resolved)
}

pub(super) fn check_update(
    input: &Dataset,
    output: &Dataset,
    target: &Target,
    options: &TranslateOptions,
) -> Result<()> {
    if !target.driver.is_empty() && output.driver().short_name() != target.driver {
        return Err(Error::invalid(
            "specified driver does not match the existing output",
        ));
    }
    let name = options
        .layer_name
        .as_deref()
        .expect("validated update layer");
    if output.driver().short_name() == "ESRI Shapefile" {
        if !name
            .chars()
            .all(|c| c.is_alphanumeric() || c == '_' || c == '-')
        {
            return Err(Error::unsupported(
                "typed Shapefile updates require an unambiguous alphanumeric, '_' or '-' layer name; use NativeTranslate for other names",
            ));
        }
        if options.write_mode == WriteMode::CreateLayer {
            if !target.path.is_dir() {
                return Err(Error::unsupported(
                    "CreateLayer for Shapefile requires a directory dataset",
                ));
            }
            check_creation(
                &target.path.join(format!("{name}.shp")),
                &output.driver(),
                1,
            )?;
        }
    }
    // Enumerating avoids failure diagnostics from an expected missing layer.
    let existing = output
        .layers()
        .find(|layer| layer.name().eq_ignore_ascii_case(name));
    match (options.write_mode, existing) {
        (WriteMode::CreateLayer, Some(_)) => Err(Error::new(
            ErrorKind::LayerExists,
            Stage::Inspect,
            "target layer already exists",
        )),
        (WriteMode::ReplaceLayer | WriteMode::AppendFeatures, None) => Err(Error::new(
            ErrorKind::LayerNotFound,
            Stage::Inspect,
            "target layer does not exist",
        )),
        (WriteMode::AppendFeatures, Some(layer)) => {
            let source = input
                .layer_by_name(&target.layers[0])
                .map_err(|e| Error::native(Stage::Inspect, e))?;
            check_append(&source, &layer, options)
        }
        _ => Ok(()),
    }
}

fn check_append(
    source: &impl LayerAccess,
    target: &impl LayerAccess,
    options: &TranslateOptions,
) -> Result<()> {
    let source_fields: Vec<_> = source
        .defn()
        .fields()
        .map(|f| (f.name(), f.field_type()))
        .collect();
    let target_fields: Vec<_> = target
        .defn()
        .fields()
        .map(|f| (f.name(), f.field_type()))
        .collect();
    if source_fields.len() != target_fields.len()
        || source_fields
            .iter()
            .any(|field| !target_fields.contains(field))
    {
        return Err(Error::invalid(
            "append requires the same field names and compatible field types",
        ));
    }
    let source_geometry: Vec<_> = source.defn().geom_fields().collect();
    let target_geometry: Vec<_> = target.defn().geom_fields().collect();
    if source_geometry.len() != target_geometry.len() {
        return Err(Error::invalid("append geometry field counts differ"));
    }
    for (source, target) in source_geometry.iter().zip(&target_geometry) {
        if source_geometry.len() > 1 {
            let source_name = source.name();
            if source_name.is_empty()
                || source_geometry
                    .iter()
                    .filter(|field| field.name().eq_ignore_ascii_case(&source_name))
                    .count()
                    != 1
            {
                return Err(Error::invalid("append geometry field mapping is ambiguous"));
            }
            if source_name != target.name()
                || target_geometry
                    .iter()
                    .filter(|field| field.name().eq_ignore_ascii_case(&source_name))
                    .count()
                    != 1
            {
                return Err(Error::invalid(
                    "append requires geometry field names in the same order",
                ));
            }
        }
        let mut kind = source.field_type();
        if let Some(dim) = options.dimensions {
            let z = matches!(dim, Dimensions::Xyz | Dimensions::Xyzm);
            let m = matches!(dim, Dimensions::Xym | Dimensions::Xyzm);
            // SAFETY: pure geometry type enum conversion, no pointers.
            kind = unsafe { gdal_sys::OGR_GT_SetModifier(kind, i32::from(z), i32::from(m)) };
        }
        if kind != target.field_type() || kind == OGRwkbGeometryType::wkbUnknown {
            return Err(Error::invalid(
                "append requires matching known geometry types and dimensions",
            ));
        }
        let effective_srs = if let Some(definition) = options
            .reproject_to
            .as_ref()
            .or(options.assign_srs.as_ref())
        {
            parse_srs(definition)?
        } else {
            source
                .spatial_ref()
                .map_err(|e| Error::invalid("append source CRS is unknown").with_source(e))?
        };
        let target_srs = target
            .spatial_ref()
            .map_err(|e| Error::invalid("append target CRS is unknown").with_source(e))?;
        if effective_srs != target_srs || axis_mapping(&effective_srs) != axis_mapping(&target_srs)
        {
            return Err(Error::invalid(
                "append CRS differs from target; explicitly reproject or assign a matching CRS",
            ));
        }
    }
    Ok(())
}

fn parse_srs(definition: &str) -> Result<SpatialRef> {
    let mut srs =
        SpatialRef::from_definition(definition).map_err(|e| Error::native(Stage::Prepare, e))?;
    srs.set_axis_mapping_strategy(AxisMappingStrategy::TraditionalGisOrder);
    Ok(srs)
}

fn axis_mapping(srs: &SpatialRef) -> Vec<i32> {
    let mut count = 0;
    // SAFETY: the borrowed mapping belongs to this live spatial reference.
    let mapping = unsafe { gdal_sys::OSRGetDataAxisToSRSAxisMapping(srs.to_c_hsrs(), &mut count) };
    if mapping.is_null() || count <= 0 {
        return Vec::new();
    }
    // SAFETY: GDAL returned count entries valid for the lifetime of srs.
    unsafe { std::slice::from_raw_parts(mapping, count as usize) }.to_vec()
}

fn output_exists() -> Error {
    Error::new(
        ErrorKind::OutputExists,
        Stage::Prepare,
        "output dataset or companion file already exists",
    )
}
fn io_error(error: std::io::Error) -> Error {
    Error::invalid("cannot inspect the local dataset path").with_source(error)
}
