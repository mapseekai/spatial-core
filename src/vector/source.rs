use crate::{
    dataset::Source,
    gdal_ext::{ffi::checked_gdal_call, query::inspect_error},
    operation::{Error, ErrorKind, Result, Stage},
};
use gdal::{
    Dataset,
    vector::{Layer, LayerAccess},
};

/// A single layer and optional filters, opened only during execution.
#[derive(Clone)]
#[must_use]
pub struct LayerSource {
    pub(crate) source: Source,
    pub(crate) layer: Option<String>,
    pub(crate) filter: Option<String>,
    pub(crate) bounds: Option<[f64; 4]>,
}
impl std::fmt::Debug for LayerSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LayerSource")
            .field("source", &self.source)
            .field("explicit_layer", &self.layer.is_some())
            .field("attribute_filter", &self.filter.is_some())
            .field("spatial_filter", &self.bounds.is_some())
            .finish()
    }
}
impl<S: Into<Source>> From<S> for LayerSource {
    fn from(source: S) -> Self {
        Self::new(source)
    }
}
impl LayerSource {
    pub(super) fn scope(&self, layer: &impl LayerAccess) -> super::CheckScope {
        super::CheckScope {
            layer: layer.name(),
            attribute_filter: self.filter.is_some(),
            spatial_filter: self.bounds,
        }
    }
    /// Select the only layer automatically; multiple layers require an explicit name.
    pub fn new(source: impl Into<Source>) -> Self {
        Self {
            source: source.into(),
            layer: None,
            filter: None,
            bounds: None,
        }
    }
    /// Select a layer by its exact name.
    pub fn layer(mut self, name: impl Into<String>) -> Self {
        self.layer = Some(name.into());
        self
    }
    /// Filter features using the source driver's attribute expression.
    pub fn where_clause(mut self, expression: impl Into<String>) -> Self {
        self.filter = Some(expression.into());
        self
    }
    /// Filter by a bounding box in the source CRS; this does not clip geometry.
    pub fn spatial_filter(mut self, bounds: [f64; 4]) -> Self {
        self.bounds = Some(bounds);
        self
    }
    pub(crate) fn validate(&self) -> Result<()> {
        self.source.validate()?;
        if let Some(name) = &self.layer {
            crate::dataset::validate_text(name, "layer name")?;
        }
        if let Some(filter) = &self.filter {
            crate::dataset::validate_text(filter, "attribute filter")?;
        }
        if let Some(b) = self.bounds
            && (!b.into_iter().all(f64::is_finite) || b[0] >= b[2] || b[1] >= b[3])
        {
            return Err(Error::invalid(
                "spatial filter must have finite coordinates and positive area",
            ));
        }
        Ok(())
    }
    pub(crate) fn select<'a>(&self, dataset: &'a Dataset) -> Result<Layer<'a>> {
        let mut layer = if let Some(name) = &self.layer {
            dataset.layer_by_name(name).map_err(|e| {
                Error::new(
                    ErrorKind::LayerNotFound,
                    Stage::Inspect,
                    "source layer does not exist",
                )
                .with_source(e)
            })?
        } else {
            let count = checked_gdal_call(|| dataset.layer_count()).map_err(inspect_error)?;
            if count != 1 {
                return Err(Error::invalid(
                    "select an explicit layer when the dataset does not contain exactly one layer",
                ));
            }
            dataset.layer(0).map_err(inspect_error)?
        };
        if let Some(filter) = &self.filter {
            checked_gdal_call(|| layer.set_attribute_filter(filter))
                .map_err(inspect_error)?
                .map_err(inspect_error)?;
        }
        if let Some([min_x, min_y, max_x, max_y]) = self.bounds {
            checked_gdal_call(|| layer.set_spatial_filter_rect(min_x, min_y, max_x, max_y))
                .map_err(inspect_error)?;
        }
        Ok(layer)
    }
}
