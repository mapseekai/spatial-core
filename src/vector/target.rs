use super::options::{Dimensions, TranslateOptions, WriteMode, validate_layer_name};
use crate::dataset::{Source, local_name};
use crate::gdal_ext::output::{absolute_output, check_vector_creation};
pub(crate) use crate::gdal_ext::output::{reject_alias, reject_dataset_alias};
use crate::operation::{Error, ErrorKind, Result, Stage};
use gdal::spatial_ref::{AxisMappingStrategy, SpatialRef};
use gdal::vector::{LayerAccess, OGRwkbGeometryType};
use gdal::{Dataset, Driver, DriverManager, DriverType};
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
    for clip in [&options.clip_source, &options.clip_destination]
        .into_iter()
        .flatten()
    {
        clip.validate_geometry()?;
    }
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
        check_vector_creation(&path, driver.as_ref().expect("creation driver"), count)?;
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
            check_vector_creation(
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

fn io_error(error: std::io::Error) -> Error {
    Error::invalid("cannot inspect the local dataset path").with_source(error)
}
