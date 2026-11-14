use super::ffi::checked_gdal_call;
use crate::operation::{CleanupError, Error, ErrorKind, Result, Stage};
use gdal::Dataset;

pub(crate) fn supported_version() -> Result<u32> {
    let build = env!("SPATIAL_CORE_GDAL_BUILD_VERSION")
        .parse::<u32>()
        .unwrap_or(0);
    let runtime = gdal::version::version_info("VERSION_NUM")
        .parse::<u32>()
        .unwrap_or(0);
    if build < 3_070_000 || runtime < 3_070_000 {
        return Err(Error::unsupported(
            "checked dataset close requires build-time and runtime GDAL >= 3.7",
        ));
    }
    Ok(build.min(runtime))
}

pub(crate) fn close(dataset: Option<Dataset>, stage: Stage, primary: &mut Option<Error>) {
    if let Some(dataset) = dataset {
        // Some drivers report deferred write failures only through CPL, even on CE_None.
        let result = checked_gdal_call(|| dataset.close()).and_then(|result| result);
        record_close(result, stage, primary);
    }
}

pub(crate) fn record_close(
    result: gdal::errors::Result<()>,
    stage: Stage,
    primary: &mut Option<Error>,
) {
    if let Err(source) = result {
        let error = primary.get_or_insert_with(|| {
            Error::new(
                ErrorKind::Close,
                stage,
                "dataset close failed; inspect cleanup errors",
            )
        });
        error.cleanup.push(CleanupError { stage, source });
    }
}

pub(crate) fn validate_native_args(args: &[String]) -> Result<()> {
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
