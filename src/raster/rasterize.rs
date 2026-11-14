use super::{
    DataType,
    execute::Job,
    options::{self, common, debug_builder, numbers, pair},
};
use crate::{
    dataset::{Kind, Source, validate_text},
    gdal_ext::raster::utility,
    operation::{Control, Error, Report, Result},
    vector::SqlDialect,
};
use std::path::Path;

/// Burn vector features into a new raster (GTiff). Requires size or resolution.
#[derive(Clone)]
#[must_use]
pub struct Rasterize {
    job: Job,
    layers: Vec<String>,
    sql: Option<(String, SqlDialect)>,
    filter: Option<String>,
    burn: Option<Vec<f64>>,
    attribute: Option<String>,
    size: Option<(u32, u32)>,
    resolution: Option<(f64, f64)>,
    bounds: Option<[f64; 4]>,
    srs: Option<String>,
    data_type: Option<DataType>,
    nodata: Option<f64>,
    initial: Option<f64>,
    all_touched: bool,
    add: bool,
}
debug_builder!(Rasterize);
impl Rasterize {
    /// Describe raster creation. Burn defaults to the constant 1.
    pub fn new(source: impl Into<Source>, output: impl AsRef<Path>) -> Self {
        Self {
            job: Job::new(source, output, "GTiff"),
            layers: Vec::new(),
            sql: None,
            filter: None,
            burn: None,
            attribute: None,
            size: None,
            resolution: None,
            bounds: None,
            srs: None,
            data_type: None,
            nodata: None,
            initial: None,
            all_touched: false,
            add: false,
        }
    }
    common!();
    /// Select source layers explicitly.
    pub fn layers<I, S>(mut self, values: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.layers = values.into_iter().map(Into::into).collect();
        self
    }
    /// Select a SQL result. Conflicts with layers and where_clause.
    pub fn sql(mut self, query: impl Into<String>, dialect: SqlDialect) -> Self {
        self.sql = Some((query.into(), dialect));
        self
    }
    /// Set a source attribute filter.
    pub fn where_clause(mut self, expression: impl Into<String>) -> Self {
        self.filter = Some(expression.into());
        self
    }
    /// Burn one constant per output band. Conflicts with attribute.
    pub fn burn(mut self, values: impl IntoIterator<Item = f64>) -> Self {
        self.burn = Some(values.into_iter().collect());
        self
    }
    /// Burn values from this source field. Conflicts with burn.
    pub fn attribute(mut self, name: impl Into<String>) -> Self {
        self.attribute = Some(name.into());
        self
    }
    /// Set pixel dimensions. Conflicts with resolution.
    pub fn size(mut self, width: u32, height: u32) -> Self {
        self.size = Some((width, height));
        self
    }
    /// Set positive output pixel spacing. Conflicts with size.
    pub fn resolution(mut self, x: f64, y: f64) -> Self {
        self.resolution = Some((x, y));
        self
    }
    /// Set xmin,ymin,xmax,ymax in output CRS units.
    pub fn bounds(mut self, bounds: [f64; 4]) -> Self {
        self.bounds = Some(bounds);
        self
    }
    /// Assign an output CRS label without reprojecting source geometries.
    pub fn assign_srs(mut self, srs: impl Into<String>) -> Self {
        self.srs = Some(srs.into());
        self
    }
    /// Set output pixel type (GDAL defaults to Float64 or appropriate Int64).
    pub fn data_type(mut self, value: DataType) -> Self {
        self.data_type = Some(value);
        self
    }
    /// Assign NoData; this does not initialize pixels.
    pub fn nodata(mut self, value: f64) -> Self {
        self.nodata = Some(value);
        self
    }
    /// Initialize pixels before burning features.
    pub fn initial_value(mut self, value: f64) -> Self {
        self.initial = Some(value);
        self
    }
    /// Include every touched pixel, rather than the default center rule.
    pub fn all_touched(mut self, enabled: bool) -> Self {
        self.all_touched = enabled;
        self
    }
    /// Add values to existing pixel values within the newly created output.
    pub fn add_values(mut self, enabled: bool) -> Self {
        self.add = enabled;
        self
    }
    /// Execute synchronously and close datasets.
    pub fn run(self) -> Result<Report> {
        self.run_with(&mut Control::default())
    }
    /// Execute with borrowed progress and cancellation.
    pub fn run_with(self, control: &mut Control<'_>) -> Result<Report> {
        if self.size.is_some() == self.resolution.is_some() {
            return Err(Error::invalid(
                "rasterize requires exactly one of size or resolution",
            ));
        }
        if self.burn.is_some() && self.attribute.is_some() {
            return Err(Error::invalid("burn and attribute are mutually exclusive"));
        }
        if self.sql.is_some() && (!self.layers.is_empty() || self.filter.is_some()) {
            return Err(Error::invalid("SQL conflicts with layers and where_clause"));
        }
        let mut args = vec!["-of".into(), self.job.driver.clone()];
        for layer in self.layers {
            validate_text(&layer, "layer")?;
            if layer.starts_with('-') {
                return Err(Error::invalid("layer names beginning with '-' require SQL"));
            }
            pair(&mut args, "-l", layer);
        }
        if let Some((query, dialect)) = self.sql {
            expression(&query)?;
            pair(&mut args, "-sql", query);
            match dialect {
                SqlDialect::Native => {}
                SqlDialect::Ogr => pair(&mut args, "-dialect", "OGRSQL"),
                SqlDialect::Sqlite => pair(&mut args, "-dialect", "SQLITE"),
            }
        }
        if let Some(filter) = self.filter {
            expression(&filter)?;
            pair(&mut args, "-where", filter);
        }
        if let Some(attribute) = self.attribute {
            validate_text(&attribute, "attribute")?;
            pair(&mut args, "-a", attribute);
        } else {
            let values = self.burn.unwrap_or_else(|| vec![1.]);
            if values.is_empty() {
                return Err(Error::invalid("burn values must not be empty"));
            }
            for value in values {
                options::finite(value, "burn value")?;
                pair(&mut args, "-burn", value);
            }
        }
        if let Some((w, h)) = self.size {
            options::size(w, h)?;
            args.extend(["-ts".into(), w.to_string(), h.to_string()]);
        }
        if let Some((x, y)) = self.resolution {
            options::positive(x, "x resolution")?;
            options::positive(y, "y resolution")?;
            numbers(&mut args, "-tr", [x, y]);
        }
        if let Some(bounds) = self.bounds {
            options::bounds(bounds)?;
            numbers(&mut args, "-te", bounds);
        }
        if let Some(srs) = &self.srs {
            crate::dataset::validate_text(srs, "spatial reference")?;
            pair(&mut args, "-a_srs", srs);
        }
        if let Some(data_type) = self.data_type {
            pair(&mut args, "-ot", data_type.name());
        }
        for (flag, value) in [("-a_nodata", self.nodata), ("-init", self.initial)] {
            if let Some(value) = value {
                if value.is_infinite() {
                    return Err(Error::invalid("pixel value must not be infinite"));
                }
                pair(&mut args, flag, value);
            }
        }
        if self.all_touched {
            args.push("-at".into());
        }
        if self.add {
            args.push("-add".into());
        }
        self.job.creation.append_args("-co", &mut args);
        self.job
            .run(Kind::Vector, false, control, move |prepared, progress| {
                if let Some(srs) = &self.srs {
                    options::srs(srs)?;
                }
                prepared.utility(utility::Kind::Rasterize, &args, None, progress)
            })
    }
}
fn expression(value: &str) -> Result<()> {
    validate_text(value, "expression")?;
    if value.starts_with('@') {
        Err(Error::invalid("typed expressions cannot reference @files"))
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::raster::test_support::pixels;
    #[test]
    fn burns_attribute_values_and_preserves_nodata_initialization() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let src = dir.path().join("area.geojson");
        std::fs::write(
            &src,
            r#"{"type":"FeatureCollection","features":[{"type":"Feature","properties":{"value":7},"geometry":{"type":"Polygon","coordinates":[[[0,0],[1,0],[1,2],[0,2],[0,0]]]}}]}"#,
        )?;
        let out = dir.path().join("burn.tif");
        let _ = Rasterize::new(&src, &out)
            .attribute("value")
            .bounds([0., 0., 2., 2.])
            .size(2, 2)
            .initial_value(-9999.)
            .nodata(-9999.)
            .run()?;
        assert_eq!(pixels(&out)?, [7., -9999., 7., -9999.]);
        assert!(
            Rasterize::new(&src, dir.path().join("bad.tif"))
                .size(2, 2)
                .burn([1.])
                .attribute("value")
                .run()
                .is_err()
        );
        Ok(())
    }
}
