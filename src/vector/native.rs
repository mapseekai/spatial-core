use super::translate::{Request, execute};
use crate::{
    config::gdal::GdalOptions,
    dataset::Source,
    operation::{Control, Error, Report, Result},
};
use std::{
    fmt,
    path::{Path, PathBuf},
};

/// GDAL utility arguments with native write semantics.
///
/// Arguments contain options and optional layer names, never an executable,
/// input or output filename. Existing outputs may be modified. Input open
/// options belong on [`Source`]; configuration belongs on [`GdalOptions`].
#[derive(Clone)]
#[must_use]
pub struct NativeTranslate {
    source: Source,
    output: PathBuf,
    args: Vec<String>,
    config: GdalOptions,
}

impl fmt::Debug for NativeTranslate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NativeTranslate")
            .field("source", &self.source)
            .field("argument_count", &self.args.len())
            .finish_non_exhaustive()
    }
}

impl NativeTranslate {
    /// Describe one conversion without opening datasets or parsing native options.
    pub fn new<I, S>(source: impl Into<Source>, output: impl AsRef<Path>, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            source: source.into(),
            output: output.as_ref().to_owned(),
            args: args.into_iter().map(Into::into).collect(),
            config: GdalOptions::default(),
        }
    }

    /// Apply thread-local configuration through open, execution and close.
    pub fn gdal_options(mut self, options: GdalOptions) -> Self {
        self.config = options;
        self
    }

    /// Execute synchronously; success includes explicit dataset closure.
    pub fn run(self) -> Result<Report> {
        self.run_with(&mut Control::default())
    }

    /// Execute with borrowed progress and cancellation control.
    pub fn run_with(self, control: &mut Control<'_>) -> Result<Report> {
        execute(
            self.source,
            self.output,
            Request::Native(self.args),
            self.config,
            control,
        )
    }
}

pub(super) fn validate_args(args: &[String]) -> Result<()> {
    for token in args {
        // Empty tokens are valid values (for example -select "").
        if token.contains('\0') {
            return Err(Error::invalid("native argument contains NUL"));
        }
        let flag = token
            .split_once('=')
            .map_or(token.as_str(), |(flag, _)| flag);
        if [
            "-oo",
            "-if",
            "--config",
            "--thread-local-config",
            "--config-file",
        ]
        .iter()
        .any(|reserved| flag.eq_ignore_ascii_case(reserved))
        {
            return Err(Error::invalid(
                "input/configuration flags are reserved; use Source and GdalOptions",
            ));
        }
    }
    Ok(())
}
