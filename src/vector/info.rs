pub use crate::operation::ScanMode;
use crate::{
    config::gdal::GdalOptions,
    dataset::{Kind, Source},
    gdal_ext::{
        ffi::checked_gdal_call,
        query::{self, inspect_error},
    },
    operation::{Control, Error, Outcome, Result},
};
use gdal::{Metadata, vector::LayerAccess};
use std::collections::BTreeMap;

/// An attribute column as declared by its driver.
#[derive(Debug)]
pub struct FieldInfo {
    /// Original column name.
    pub name: String,
    /// OGR field type code.
    pub field_type: gdal_sys::OGRFieldType::Type,
    /// OGR subtype, such as Boolean, Int16, Float32 or JSON.
    pub field_subtype: gdal_sys::OGRFieldSubType::Type,
    /// Formatting width; zero means unspecified.
    pub width: i32,
    /// Decimal precision.
    pub precision: i32,
    /// Whether null values are permitted.
    pub nullable: bool,
    /// Whether values are declared unique.
    pub unique: bool,
    /// Driver-provided default expression.
    pub default_value: Option<String>,
}
/// A geometry column, including its own spatial reference.
#[derive(Debug)]
pub struct GeometryFieldInfo {
    /// Original geometry-column name.
    pub name: String,
    /// OGR geometry type including Z/M flags.
    pub geometry_type: gdal_sys::OGRwkbGeometryType::Type,
    /// Declared spatial reference WKT, when present.
    pub spatial_ref: Option<String>,
}
/// Inspection of one layer.
#[derive(Debug)]
pub struct LayerInfo {
    /// Source layer name.
    pub name: String,
    /// Attribute columns in source order.
    pub fields: Vec<FieldInfo>,
    /// All geometry columns, not only the first one.
    pub geometry_fields: Vec<GeometryFieldInfo>,
    /// Count, or None when the driver cannot answer without a forced scan.
    pub feature_count: Option<u64>,
    /// First geometry field extent as min-x/min-y/max-x/max-y.
    pub extent: Option<[f64; 4]>,
    /// Driver-declared support for fast feature counts.
    pub fast_feature_count: bool,
    /// Driver-declared support for fast extent queries.
    pub fast_extent: bool,
}
/// Owned vector metadata.
#[derive(Debug)]
pub struct VectorInfo {
    /// Canonical driver name.
    pub driver: String,
    /// Selected layers in order.
    pub layers: Vec<LayerInfo>,
    /// Explicitly selected metadata domains.
    pub metadata: BTreeMap<String, Vec<String>>,
}
/// Inspect vector schema, with inexpensive counts and extents by default.
#[derive(Debug, Clone)]
#[must_use]
pub struct Info {
    source: Source,
    layers: Option<Vec<String>>,
    count: ScanMode,
    extent: ScanMode,
    domains: Vec<String>,
    config: GdalOptions,
}
impl Info {
    /// Inspect all source layers; construction performs no I/O.
    pub fn new(source: impl Into<Source>) -> Self {
        Self {
            source: source.into(),
            layers: None,
            count: ScanMode::Fast,
            extent: ScanMode::Fast,
            domains: Vec::new(),
            config: GdalOptions::default(),
        }
    }
    /// Replace the set of inspected layer names.
    pub fn layers<I, S>(mut self, names: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.layers = Some(names.into_iter().map(Into::into).collect());
        self
    }
    /// Permit a forced feature count; native drivers may block between cancellation checks.
    pub fn feature_count(mut self, mode: ScanMode) -> Self {
        self.count = mode;
        self
    }
    /// Permit a forced extent scan.
    pub fn extent(mut self, mode: ScanMode) -> Self {
        self.extent = mode;
        self
    }
    /// Select a raw metadata domain.
    pub fn metadata_domain(mut self, domain: impl Into<String>) -> Self {
        self.domains.push(domain.into());
        self
    }
    /// Apply configuration until input close.
    pub fn gdal_options(mut self, config: GdalOptions) -> Self {
        self.config = config;
        self
    }
    /// Execute synchronously.
    pub fn run(self) -> Result<Outcome<VectorInfo>> {
        self.run_with(&mut Control::default())
    }
    /// Execute with borrowed control.
    pub fn run_with(self, control: &mut Control<'_>) -> Result<Outcome<VectorInfo>> {
        if self.domains.iter().any(|d| d.contains('\0')) {
            return Err(Error::invalid("metadata domain contains NUL"));
        }
        if let Some(names) = &self.layers {
            for name in names {
                crate::dataset::validate_text(name, "layer name")?;
            }
        }
        query::run(
            self.source,
            Kind::Vector,
            self.config,
            control,
            |input, progress| {
                let indices = self
                    .layers
                    .unwrap_or_else(|| input.layers().map(|layer| layer.name()).collect());
                let mut layers = Vec::with_capacity(indices.len());
                for (position, name) in indices.iter().enumerate() {
                    query::pulse(
                        progress,
                        position as f64 / indices.len().max(1) as f64,
                        c"inspecting vector layer",
                    )?;
                    let layer = input.layer_by_name(name).map_err(inspect_error)?;
                    let defn = layer.defn();
                    let fields = defn
                        .fields()
                        .enumerate()
                        .map(|(index, field)| FieldInfo {
                            name: field.name(),
                            field_type: field.field_type(),
                            field_subtype: super::execute::field_subtype(defn, index),
                            width: field.width(),
                            precision: field.precision(),
                            nullable: field.is_nullable(),
                            unique: field.is_unique(),
                            default_value: field.default_value(),
                        })
                        .collect();
                    let geometry_fields = defn
                        .geom_fields()
                        .map(|field| {
                            Ok(GeometryFieldInfo {
                                name: field.name(),
                                geometry_type: field.field_type(),
                                spatial_ref: field
                                    .spatial_ref()
                                    .ok()
                                    .map(|s| s.to_wkt())
                                    .transpose()
                                    .map_err(inspect_error)?,
                            })
                        })
                        .collect::<Result<Vec<_>>>()?;
                    let feature_count = checked_gdal_call(|| match self.count {
                        ScanMode::Fast => layer.try_feature_count(),
                        ScanMode::Exact => Some(layer.feature_count()),
                    })
                    .map_err(inspect_error)?;
                    let extent = if geometry_fields.is_empty() {
                        None
                    } else {
                        match self.extent {
                            ScanMode::Fast => checked_gdal_call(|| layer.try_get_extent())
                                .map_err(inspect_error)?
                                .map_err(inspect_error)?,
                            ScanMode::Exact if feature_count == Some(0) => None,
                            ScanMode::Exact => Some(
                                checked_gdal_call(|| layer.get_extent())
                                    .map_err(inspect_error)?
                                    .map_err(inspect_error)?,
                            ),
                        }
                        .map(|b| [b.MinX, b.MinY, b.MaxX, b.MaxY])
                    };
                    // SAFETY: this layer remains borrowed from the live input; capability strings are static.
                    let (fast_feature_count, fast_extent) = unsafe {
                        (
                            gdal_sys::OGR_L_TestCapability(
                                layer.c_layer(),
                                c"FastFeatureCount".as_ptr(),
                            ) != 0,
                            gdal_sys::OGR_L_TestCapability(
                                layer.c_layer(),
                                c"FastGetExtent".as_ptr(),
                            ) != 0,
                        )
                    };
                    layers.push(LayerInfo {
                        name: layer.name(),
                        fields,
                        geometry_fields,
                        feature_count,
                        extent,
                        fast_feature_count,
                        fast_extent,
                    });
                }
                let metadata = self
                    .domains
                    .into_iter()
                    .map(|domain| {
                        let values = input.metadata_domain(&domain).unwrap_or_default();
                        (domain, values)
                    })
                    .collect();
                query::pulse(progress, 1.0, c"vector inspection complete")?;
                Ok(VectorInfo {
                    driver: input.driver().short_name(),
                    layers,
                    metadata,
                })
            },
        )
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn schema_counts_and_extent_are_owned_and_cancellation_is_observed() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("points.geojson");
        std::fs::write(
            &path,
            r#"{"type":"FeatureCollection","features":[{"type":"Feature","properties":{"name":"first","n":7},"geometry":{"type":"Point","coordinates":[2,3]}},{"type":"Feature","properties":{"name":null,"n":8},"geometry":{"type":"Point","coordinates":[4,5]}}]}"#,
        )?;
        let result = Info::new(&path)
            .feature_count(ScanMode::Exact)
            .extent(ScanMode::Exact)
            .run()?;
        assert_eq!(result.value().layers[0].feature_count, Some(2));
        assert_eq!(result.value().layers[0].extent, Some([2., 3., 4., 5.]));
        assert_eq!(result.value().layers[0].fields.len(), 2);
        let cancellation = crate::control::Cancellation::default();
        cancellation.cancel();
        let error = Info::new(&path)
            .run_with(&mut Control::default().cancellation(&cancellation))
            .unwrap_err();
        assert_eq!(error.kind(), crate::operation::ErrorKind::Cancelled);
        Ok(())
    }
}
