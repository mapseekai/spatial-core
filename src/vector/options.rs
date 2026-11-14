use crate::dataset::{Options, validate_text};
use crate::operation::{Error, Result};

/// What may be created or changed by a typed conversion.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum WriteMode {
    /// Create an absent local dataset; never replace existing files.
    #[default]
    CreateDataset,
    /// Create one absent layer in an existing dataset.
    CreateLayer,
    /// Replace one existing layer, preserving other layers.
    ReplaceLayer,
    /// Append to one existing layer with a compatible schema and CRS.
    AppendFeatures,
}

/// Explicit output coordinate dimensions; omission preserves native defaults.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Dimensions {
    /// X and Y.
    Xy,
    /// X, Y and Z.
    Xyz,
    /// X, Y and measure.
    Xym,
    /// X, Y, Z and measure.
    Xyzm,
}

/// Handling of failures reported by GDAL during conversion.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ErrorPolicy {
    /// Fail on native failure diagnostics as well as a failed return value.
    #[default]
    Abort,
    /// Let GDAL skip failures it supports skipping; inspect report diagnostics.
    SkipFailures,
}

/// SQL dialect used by the input driver.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum SqlDialect {
    /// Let the driver choose its native dialect.
    Native,
    /// GDAL's OGR SQL dialect.
    Ogr,
    /// SQLite dialect, including GDAL's virtual tables.
    Sqlite,
}

/// Clip geometries in an explicitly selected source or destination CRS.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum Clip {
    /// xmin, ymin, xmax, ymax with positive area.
    Bounds([f64; 4]),
    /// A POLYGON or MULTIPOLYGON WKT geometry, not a filename.
    Wkt(String),
}
impl Clip {
    fn validate(&self) -> Result<()> {
        match self {
            Self::Bounds([a, b, c, d])
                if [a, b, c, d].iter().all(|v| v.is_finite()) && a < c && b < d =>
            {
                Ok(())
            }
            Self::Bounds(_) => Err(Error::invalid(
                "clip bounds must be finite with positive area",
            )),
            Self::Wkt(wkt) => validate_text(wkt, "clip WKT"),
        }
    }
    pub(super) fn validate_geometry(&self) -> Result<()> {
        if let Self::Wkt(wkt) = self {
            let geometry = gdal::vector::Geometry::from_wkt(wkt)
                .map_err(|e| Error::invalid("invalid clipping WKT").with_source(e))?;
            // SAFETY: OGR_GT_Flatten accepts any geometry type value.
            let ty = unsafe { gdal_sys::OGR_GT_Flatten(geometry.geometry_type()) };
            if ![
                gdal_sys::OGRwkbGeometryType::wkbPolygon,
                gdal_sys::OGRwkbGeometryType::wkbMultiPolygon,
            ]
            .contains(&ty)
                || geometry.is_empty()
            {
                return Err(Error::invalid(
                    "clipping WKT must be a nonempty polygon or multipolygon",
                ));
            }
        }
        Ok(())
    }
    fn append(&self, flag: &str, args: &mut Vec<String>) {
        args.push(flag.into());
        match self {
            Self::Bounds(values) => args.extend(values.map(|v| v.to_string())),
            Self::Wkt(wkt) => args.push(wkt.clone()),
        }
    }
}

/// Explicit output geometry conversion. Concrete types require explicit dimensions;
/// semantic conversions preserve dimensions unless separately overridden.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum GeometryType {
    /// Point.
    Point,
    /// Line string.
    LineString,
    /// Polygon.
    Polygon,
    /// Multiple points.
    MultiPoint,
    /// Multiple line strings.
    MultiLineString,
    /// Multiple polygons.
    MultiPolygon,
    /// General geometry collection.
    GeometryCollection,
    /// Promote single geometries into corresponding multi types.
    PromoteToMulti,
    /// Linearize curved geometries.
    ConvertToLinear,
    /// Promote linear geometries to compatible curve types.
    ConvertToCurve,
}
impl GeometryType {
    fn is_concrete(self) -> bool {
        !matches!(
            self,
            Self::PromoteToMulti | Self::ConvertToLinear | Self::ConvertToCurve
        )
    }
    fn name(self) -> &'static str {
        match self {
            Self::Point => "POINT",
            Self::LineString => "LINESTRING",
            Self::Polygon => "POLYGON",
            Self::MultiPoint => "MULTIPOINT",
            Self::MultiLineString => "MULTILINESTRING",
            Self::MultiPolygon => "MULTIPOLYGON",
            Self::GeometryCollection => "GEOMETRYCOLLECTION",
            Self::PromoteToMulti => "PROMOTE_TO_MULTI",
            Self::ConvertToLinear => "CONVERT_TO_LINEAR",
            Self::ConvertToCurve => "CONVERT_TO_CURVE",
        }
    }
}

#[derive(Clone, Default)]
pub(crate) struct TranslateOptions {
    pub driver: Option<String>,
    pub layers: Option<Vec<String>>,
    pub sql: Option<(String, SqlDialect)>,
    pub where_clause: Option<String>,
    pub fields: Option<Vec<String>>,
    pub spatial_filter: Option<[f64; 4]>,
    pub layer_name: Option<String>,
    pub source_srs: Option<String>,
    pub reproject_to: Option<String>,
    pub assign_srs: Option<String>,
    pub dimensions: Option<Dimensions>,
    pub make_valid: bool,
    pub clip_source: Option<Clip>,
    pub clip_destination: Option<Clip>,
    pub simplify: Option<f64>,
    pub segmentize: Option<f64>,
    pub explode_collections: bool,
    pub geometry_type: Option<GeometryType>,
    pub error_policy: ErrorPolicy,
    pub write_mode: WriteMode,
    pub dataset_options: Options,
    pub layer_options: Options,
    pub output_open_options: Options,
}

impl TranslateOptions {
    pub fn validate(&self, version: u32) -> Result<()> {
        if self.geometry_type.is_some_and(GeometryType::is_concrete) && self.dimensions.is_none() {
            return Err(Error::invalid(
                "a concrete geometry type requires explicit dimensions",
            ));
        }
        for clip in [&self.clip_source, &self.clip_destination]
            .into_iter()
            .flatten()
        {
            clip.validate()?;
        }
        for value in [self.simplify, self.segmentize].into_iter().flatten() {
            if !value.is_finite() || value <= 0. {
                return Err(Error::invalid(
                    "geometry tolerance must be positive and finite",
                ));
            }
        }
        if self.simplify.is_some() && self.segmentize.is_some() {
            return Err(Error::invalid(
                "simplify and segmentize require separate operations",
            ));
        }
        if self.write_mode == WriteMode::AppendFeatures
            && (self.clip_source.is_some()
                || self.clip_destination.is_some()
                || self.simplify.is_some()
                || self.segmentize.is_some()
                || self.explode_collections
                || self.geometry_type.is_some())
        {
            return Err(Error::unsupported(
                "geometry processing with append requires NativeTranslate or an intermediate dataset",
            ));
        }
        for options in [
            &self.dataset_options,
            &self.layer_options,
            &self.output_open_options,
        ] {
            options.validate()?;
            if options.get("OVERWRITE").is_some() {
                return Err(Error::invalid(
                    "OVERWRITE is reserved; choose an explicit WriteMode",
                ));
            }
            if options.get("LAUNDER").is_some_and(|v| {
                !["NO", "FALSE", "OFF", "0"]
                    .iter()
                    .any(|off| v.eq_ignore_ascii_case(off))
            }) {
                return Err(Error::unsupported(
                    "name laundering requires NativeTranslate",
                ));
            }
        }
        if self.output_open_options.get("PRELUDE_STATEMENTS").is_some() {
            return Err(Error::invalid(
                "output PRELUDE_STATEMENTS requires NativeTranslate",
            ));
        }
        for text in [
            &self.driver,
            &self.source_srs,
            &self.reproject_to,
            &self.assign_srs,
        ]
        .into_iter()
        .flatten()
        {
            validate_text(text, "option value")?;
        }
        if let Some(names) = &self.layers {
            if names.is_empty() {
                return Err(Error::invalid("layers must not be empty"));
            }
            for (i, name) in names.iter().enumerate() {
                validate_layer_name(name)?;
                if names[..i].contains(name) {
                    return Err(Error::invalid("duplicate source layer"));
                }
            }
        }
        if let Some(name) = &self.layer_name {
            validate_layer_name(name)?;
        }
        if let Some(fields) = &self.fields {
            for field in fields {
                validate_text(field, "field name")?;
                if version < 3_090_000
                    && field
                        .chars()
                        .any(|c| c.is_whitespace() || ",\"\\".contains(c))
                {
                    return Err(Error::unsupported(
                        "quoted field selection requires GDAL 3.9 or later",
                    ));
                }
            }
        }
        if let Some((sql, _)) = &self.sql {
            validate_expression(sql)?;
            if self.layers.is_some() || self.where_clause.is_some() {
                return Err(Error::invalid(
                    "SQL cannot be combined with layers or where_clause",
                ));
            }
        }
        if let Some(expression) = &self.where_clause {
            validate_expression(expression)?;
        }
        if let Some([xmin, ymin, xmax, ymax]) = self.spatial_filter
            && (![xmin, ymin, xmax, ymax].into_iter().all(f64::is_finite)
                || xmin > xmax
                || ymin > ymax)
        {
            return Err(Error::invalid(
                "spatial filter must contain ordered finite bounds",
            ));
        }
        if self.source_srs.is_some() && self.reproject_to.is_none() {
            return Err(Error::invalid("source_srs requires reproject_to"));
        }
        if self.assign_srs.is_some() && self.reproject_to.is_some() {
            return Err(Error::invalid(
                "assign_srs and reproject_to are mutually exclusive",
            ));
        }
        if self.write_mode == WriteMode::CreateDataset {
            if !self.output_open_options.is_empty() {
                return Err(Error::invalid(
                    "output open options require an existing dataset",
                ));
            }
        } else {
            if self.layer_name.is_none() {
                return Err(Error::invalid(
                    "updating a dataset requires an explicit layer_name",
                ));
            }
            if !self.dataset_options.is_empty() {
                return Err(Error::invalid(
                    "dataset creation options require CreateDataset",
                ));
            }
        }
        if self.write_mode == WriteMode::AppendFeatures
            && (self.fields.is_some() || self.sql.is_some() || !self.layer_options.is_empty())
        {
            return Err(Error::invalid(
                "typed append does not accept SQL, fields, or layer creation options",
            ));
        }
        Ok(())
    }

    pub fn args(
        &self,
        driver: &str,
        layers: &[String],
        progress: bool,
        version: u32,
    ) -> Vec<String> {
        let mut args = Vec::new();
        if self.write_mode == WriteMode::CreateDataset {
            pair(&mut args, "-f", driver);
        }
        match self.write_mode {
            WriteMode::CreateDataset => {}
            WriteMode::CreateLayer => args.push("-update".into()),
            WriteMode::ReplaceLayer => args.push("-overwrite".into()),
            WriteMode::AppendFeatures => args.push("-append".into()),
        }
        if let Some((query, dialect)) = &self.sql {
            pair(&mut args, "-sql", query);
            match dialect {
                SqlDialect::Native => {}
                SqlDialect::Ogr => pair(&mut args, "-dialect", "OGRSQL"),
                SqlDialect::Sqlite => pair(&mut args, "-dialect", "SQLITE"),
            }
        }
        for (flag, value) in [
            ("-where", &self.where_clause),
            ("-nln", &self.layer_name),
            ("-s_srs", &self.source_srs),
            ("-t_srs", &self.reproject_to),
            ("-a_srs", &self.assign_srs),
        ] {
            if let Some(value) = value {
                pair(&mut args, flag, value);
            }
        }
        if let Some(fields) = &self.fields {
            let fields = fields
                .iter()
                .map(|name| {
                    if version < 3_090_000 {
                        name.clone()
                    } else {
                        format!("\"{}\"", name.replace('\\', "\\\\").replace('"', "\\\""))
                    }
                })
                .collect::<Vec<_>>()
                .join(",");
            pair(&mut args, "-select", &fields);
        }
        if let Some(bounds) = self.spatial_filter {
            args.push("-spat".into());
            args.extend(bounds.into_iter().map(|v| v.to_string()));
        }
        if let Some(dim) = self.dimensions {
            pair(
                &mut args,
                "-dim",
                match dim {
                    Dimensions::Xy => "XY",
                    Dimensions::Xyz => "XYZ",
                    Dimensions::Xym => "XYM",
                    Dimensions::Xyzm => "XYZM",
                },
            );
        }
        for (flag, clip) in [
            ("-clipsrc", &self.clip_source),
            ("-clipdst", &self.clip_destination),
        ] {
            if let Some(clip) = clip {
                clip.append(flag, &mut args);
            }
        }
        for (flag, value) in [
            ("-simplify", self.simplify),
            ("-segmentize", self.segmentize),
        ] {
            if let Some(value) = value {
                pair(&mut args, flag, &value.to_string());
            }
        }
        if self.explode_collections {
            args.push("-explodecollections".into());
        }
        if let Some(ty) = self.geometry_type {
            let suffix = if ty.is_concrete() {
                match self.dimensions.expect("validated dimensions") {
                    Dimensions::Xy => "",
                    Dimensions::Xyz => "Z",
                    Dimensions::Xym => "M",
                    Dimensions::Xyzm => "ZM",
                }
            } else {
                ""
            };
            pair(&mut args, "-nlt", &format!("{}{suffix}", ty.name()));
        }
        if self.make_valid {
            args.push("-makevalid".into());
        }
        if self.error_policy == ErrorPolicy::SkipFailures {
            args.push("-skipfailures".into());
        }
        if progress {
            args.push("-progress".into());
        }
        self.dataset_options.append_args("-dsco", &mut args);
        self.layer_options.append_args("-lco", &mut args);
        args.extend_from_slice(layers);
        args
    }
}

pub(crate) fn validate_layer_name(name: &str) -> Result<()> {
    validate_text(name, "layer name")?;
    if name.starts_with('-') {
        return Err(Error::invalid(
            "layer names beginning with '-' require a quoted SQL selection or NativeTranslate",
        ));
    }
    Ok(())
}

fn validate_expression(expression: &str) -> Result<()> {
    validate_text(expression, "expression")?;
    if expression.starts_with('@') {
        return Err(Error::invalid(
            "typed SQL and where_clause accept expressions; use NativeTranslate for @files",
        ));
    }
    Ok(())
}

fn pair(args: &mut Vec<String>, flag: &str, value: &str) {
    args.extend([flag.to_owned(), value.to_owned()]);
}
