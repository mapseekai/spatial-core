use super::LayerSource;
use crate::{
    config::gdal::GdalOptions,
    dataset::{self, Options},
    gdal_ext::{
        algorithm::{Algorithm, Argument},
        diagnostics::Diagnostics,
        execution::{close, supported_version},
        output,
        progress::ProgressCallbackState,
        query,
    },
    operation::{Control, Error, ErrorKind, Outcome, OutputState, Result, Stage},
};
use gdal::{Dataset, DriverManager};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub(super) struct Output {
    pub path: PathBuf,
    pub driver: String,
    pub layer: String,
    pub creation: Options,
    pub layer_options: Options,
}
impl Output {
    pub fn new(path: impl AsRef<Path>, layer: &str) -> Self {
        Self {
            path: path.as_ref().to_owned(),
            driver: "GPKG".into(),
            layer: layer.into(),
            creation: Options::default(),
            layer_options: Options::default(),
        }
    }
    fn validate(&self) -> Result<output::AliasCheck> {
        self.creation.validate()?;
        self.layer_options.validate()?;
        super::options::validate_layer_name(&self.layer)?;
        let driver = DriverManager::get_driver_by_name(&self.driver)
            .map_err(|e| Error::unsupported("output driver is unavailable").with_source(e))?;
        if matches!(driver.short_name().as_str(), "GDALG" | "OGR_VRT" | "VRT") {
            return Err(Error::unsupported(
                "analysis output must materialize data rather than reference temporary inputs",
            ));
        }
        output::check_vector_creation(&output::absolute_output(&self.path)?, &driver, 1)?;
        let mut aliases = output::AliasCheck::new(&self.path)?;
        aliases.add_driver(Some(&driver.short_name()))?;
        Ok(aliases)
    }
    pub fn set_algorithm(&self, algorithm: &mut Algorithm<'_>) -> Result<()> {
        algorithm.set(
            "output",
            Argument::String(
                self.path
                    .to_str()
                    .ok_or_else(|| Error::unsupported("algorithm output path must be UTF-8"))?,
            ),
        )?;
        algorithm.set("output-format", Argument::String(&self.driver))?;
        algorithm.set("output-layer", Argument::String(&self.layer))?;
        for (name, options) in [
            ("creation-option", &self.creation),
            ("layer-creation-option", &self.layer_options),
        ] {
            let values = options.strings();
            if !values.is_empty() {
                algorithm.set(name, Argument::Strings(&values))?;
            }
        }
        Ok(())
    }
}
pub(super) fn field_subtype(
    defn: &gdal::vector::Defn,
    index: usize,
) -> gdal_sys::OGRFieldSubType::Type {
    // SAFETY: callers only pass indices from this live definition's field iterator.
    unsafe {
        gdal_sys::OGR_Fld_GetSubType(gdal_sys::OGR_FD_GetFieldDefn(defn.c_defn(), index as i32))
    }
}

pub(super) fn field_schema(
    layer: &impl gdal::vector::LayerAccess,
) -> Vec<(
    String,
    gdal_sys::OGRFieldType::Type,
    gdal_sys::OGRFieldSubType::Type,
)> {
    layer
        .defn()
        .fields()
        .enumerate()
        .map(|(index, field)| {
            (
                field.name(),
                field.field_type(),
                field_subtype(layer.defn(), index),
            )
        })
        .collect()
}
pub(super) struct Destination<'a> {
    pub spec: Option<&'a Output>,
    pub dataset: Option<Dataset>,
    pub state: OutputState,
}
impl Destination<'_> {
    pub fn create(&mut self) -> Result<&mut Dataset> {
        let spec = self.spec.expect("writing operation");
        let driver =
            DriverManager::get_driver_by_name(&spec.driver).map_err(query::inspect_error)?;
        let name = dataset::local_name(&spec.path)?;
        let mut list = gdal::cpl::CslStringList::new();
        for value in spec.creation.strings() {
            list.add_string(&value).map_err(query::inspect_error)?;
        }
        self.state = OutputState::MayHaveChanged;
        // SAFETY: live driver/options/path; successful zero-band GDALCreate transfers ownership.
        let raw = crate::gdal_ext::ffi::checked_gdal_call(|| unsafe {
            gdal_sys::GDALCreate(
                driver.c_driver(),
                name.as_ptr(),
                0,
                0,
                0,
                gdal_sys::GDALDataType::GDT_Unknown,
                list.as_ptr(),
            )
        })
        .map_err(query::inspect_error)?;
        if raw.is_null() {
            return Err(Error::native(
                Stage::OpenOutput,
                crate::gdal_ext::ffi::last_null_pointer_err("GDALCreate"),
            ));
        }
        // SAFETY: the non-null newly created handle is uniquely owned by this destination.
        self.dataset = Some(unsafe { Dataset::from_c_dataset(raw) });
        Ok(self.dataset.as_mut().expect("created output"))
    }
}
pub(super) fn run<T>(
    sources: &[LayerSource],
    config: GdalOptions,
    output: Option<&Output>,
    control: &mut Control<'_>,
    execute: impl FnOnce(&[Dataset], &mut Destination<'_>, &mut ProgressCallbackState<'_>) -> Result<T>,
) -> Result<Outcome<T>> {
    supported_version()?;
    control.check()?;
    for source in sources {
        source.validate()?;
    }
    let diagnostics = Diagnostics::enter();
    let scope = config.enter();
    let mut inputs = Vec::new();
    let mut destination = Destination {
        spec: output,
        dataset: None,
        state: OutputState::NotModified,
    };
    let result = (|| {
        let mut aliases = output.map(Output::validate).transpose()?;
        for source in sources {
            control.check()?;
            if let Some(aliases) = &mut aliases {
                aliases.check_source(&source.source)?;
            }
            inputs.push(source.source.open()?);
            if let Some(aliases) = &mut aliases {
                aliases.check_dataset(inputs.last().expect("opened input"))?;
            }
        }
        query::invoke(control, |progress| {
            execute(&inputs, &mut destination, progress)
        })
    })();
    let (value, mut failure) = match result {
        Ok(value) => (Some(value), None),
        Err(error) => (None, Some(error)),
    };
    close(destination.dataset.take(), Stage::CloseOutput, &mut failure);
    for input in inputs {
        close(Some(input), Stage::CloseSource, &mut failure);
    }
    if failure.is_none() && diagnostics.failures() != 0 {
        failure = Some(Error::new(
            ErrorKind::Processing,
            Stage::Execute,
            "GDAL reported a vector analysis failure",
        ));
    }
    drop(scope);
    let report = diagnostics.take_report();
    if let Some(mut error) = failure {
        error.report = report;
        error.output_state = destination.state;
        Err(error)
    } else {
        Ok(Outcome::new(value.expect("successful operation"), report))
    }
}
pub(super) fn geos(minor: u32) -> Result<()> {
    let (mut major, mut actual_minor, mut patch) = (0, 0, 0);
    // SAFETY: GDAL writes three stack-owned version integers.
    let available =
        unsafe { gdal_sys::OGRGetGEOSVersion(&mut major, &mut actual_minor, &mut patch) };
    if !available || major < 3 || major == 3 && actual_minor < minor as i32 {
        Err(Error::unsupported(format!(
            "this operation requires GDAL linked with GEOS >= 3.{minor}"
        )))
    } else {
        Ok(())
    }
}
pub(super) fn linear_xy(geometry: &gdal::vector::Geometry) -> Result<()> {
    let kind = geometry.geometry_type();
    // SAFETY: these functions inspect only the geometry type code.
    if unsafe {
        gdal_sys::OGR_GT_HasZ(kind) != 0
            || gdal_sys::OGR_GT_HasM(kind) != 0
            || gdal_sys::OGR_GT_IsNonLinear(kind) != 0
    } {
        Err(Error::unsupported(
            "analysis requires linear XY geometries; normalize explicitly with Translate",
        ))
    } else {
        Ok(())
    }
}
pub(super) fn valid_geometry(geometry: &gdal::vector::Geometry) -> Result<bool> {
    crate::gdal_ext::ffi::checked_gdal_call(|| geometry.is_valid()).map_err(query::inspect_error)
}
pub(super) fn geometry<'a>(
    feature: &'a gdal::vector::Feature<'_>,
) -> Option<&'a gdal::vector::Geometry> {
    // gdal 0.19 can return Some(Geometry) wrapping a NULL handle; check before using it.
    // SAFETY: the feature is live and the returned handle remains borrowed from it.
    if unsafe { gdal_sys::OGR_F_GetGeometryRef(feature.c_feature()) }.is_null() {
        None
    } else {
        feature.geometry()
    }
}
