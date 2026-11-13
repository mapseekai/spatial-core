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
    pub error_policy: ErrorPolicy,
    pub write_mode: WriteMode,
    pub dataset_options: Options,
    pub layer_options: Options,
    pub output_open_options: Options,
}

impl TranslateOptions {
    pub fn validate(&self, version: u32) -> Result<()> {
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
