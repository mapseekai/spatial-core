//! Read-only execution and shared, borrowed progress control.
use crate::{
    config::gdal::GdalOptions,
    dataset::{Kind, Source},
    gdal_ext::{
        diagnostics::Diagnostics,
        execution::{close, supported_version},
        progress::ProgressCallbackState,
    },
    operation::{Control, Error, ErrorKind, Outcome, Result, Stage},
};
use gdal::Dataset;
use std::{ffi::CStr, ops::ControlFlow};

pub(crate) fn run<T>(
    source: Source,
    kind: Kind,
    config: GdalOptions,
    control: &mut Control<'_>,
    inspect: impl FnOnce(&Dataset, &mut ProgressCallbackState<'_>) -> Result<T>,
) -> Result<Outcome<T>> {
    supported_version()?;
    source.validate()?;
    control.check()?;
    let diagnostics = Diagnostics::enter();
    let scope = config.enter();
    let mut input = None;
    let result = (|| {
        input = Some(source.open_kind(kind)?);
        invoke(control, |progress| {
            inspect(input.as_ref().expect("opened input"), progress)
        })
    })();
    let (value, mut failure) = match result {
        Ok(value) => (Some(value), None),
        Err(error) => (None, Some(error)),
    };
    close(input, Stage::CloseSource, &mut failure);
    drop(scope);
    let native_failed = diagnostics.failures() != 0;
    let report = diagnostics.take_report();
    if failure.is_none() && native_failed {
        failure = Some(Error::new(
            ErrorKind::Processing,
            Stage::Inspect,
            "GDAL reported a query failure",
        ));
    }
    if let Some(mut error) = failure {
        error.report = report;
        Err(error)
    } else {
        Ok(Outcome::new(value.expect("successful query"), report))
    }
}

pub(crate) fn invoke<T>(
    control: &mut Control<'_>,
    execute: impl FnOnce(&mut ProgressCallbackState<'_>) -> Result<T>,
) -> Result<T> {
    control.check()?;
    let active = control.is_active();
    let cancelled = || {
        control
            .cancellation
            .is_some_and(crate::control::Cancellation::is_cancelled)
    };
    let mut callback = |progress| {
        if cancelled() {
            return false;
        }
        control
            .callback
            .as_mut()
            .is_none_or(|callback| matches!(callback(&progress), ControlFlow::Continue(())))
    };
    let mut progress =
        ProgressCallbackState::optional(if active { Some(&mut callback) } else { None });
    let result = execute(&mut progress);
    check_progress(&progress)?;
    let value = result?;
    if cancelled() {
        return Err(Error::new(
            ErrorKind::Cancelled,
            Stage::Execute,
            "operation cancelled",
        ));
    }
    Ok(value)
}

pub(crate) fn check_progress(progress: &ProgressCallbackState<'_>) -> Result<()> {
    if let Some(kind) = progress.interruption() {
        Err(Error::new(
            kind,
            Stage::Execute,
            "operation interrupted by progress control",
        ))
    } else {
        Ok(())
    }
}

pub(crate) fn pulse(
    progress: &mut ProgressCallbackState<'_>,
    fraction: f64,
    message: &CStr,
) -> Result<()> {
    if let Some(callback) = progress.native_callback() {
        // SAFETY: callback state and the static message remain live during this synchronous call.
        unsafe { callback(fraction, message.as_ptr(), progress.user_data()) };
    }
    check_progress(progress)
}

pub(crate) fn inspect_error(error: gdal::errors::GdalError) -> Error {
    Error::new(
        ErrorKind::Processing,
        Stage::Inspect,
        "cannot inspect dataset",
    )
    .with_source(error)
}
