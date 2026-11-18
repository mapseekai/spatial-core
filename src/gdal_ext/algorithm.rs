//! The small C algorithm binding missing from gdal-sys 0.12.
// Signatures follow GDAL 3.12's gdalalgorithm_c.h; no C++ ABI is used.
#[cfg(gdal_3_12)]
use crate::gdal_ext::query::check_progress;
use crate::{
    gdal_ext::{execution::supported_version, progress::ProgressCallbackState},
    operation::{Error, Result},
};
use gdal::Dataset;
use std::marker::PhantomData;

#[cfg(gdal_3_12)]
mod ffi {
    use std::ffi::{c_char, c_void};
    unsafe extern "C" {
        pub(super) fn GDALGetGlobalAlgorithmRegistry() -> *mut c_void;
        pub(super) fn GDALAlgorithmRegistryRelease(registry: *mut c_void);
        pub(super) fn GDALAlgorithmRegistryInstantiateAlgFromPath(
            registry: *mut c_void,
            path: *const *const c_char,
        ) -> *mut c_void;
        pub(super) fn GDALAlgorithmRelease(algorithm: *mut c_void);
        pub(super) fn GDALAlgorithmGetArgNonConst(
            algorithm: *mut c_void,
            name: *const c_char,
        ) -> *mut c_void;
        pub(super) fn GDALAlgorithmArgRelease(argument: *mut c_void);
        pub(super) fn GDALAlgorithmArgGetAsDatasetValue(argument: *mut c_void) -> *mut c_void;
        pub(super) fn GDALArgDatasetValueGetDatasetIncreaseRefCount(
            value: *mut c_void,
        ) -> gdal_sys::GDALDatasetH;
        pub(super) fn GDALArgDatasetValueRelease(value: *mut c_void);
        pub(super) fn GDALAlgorithmArgGetType(argument: *mut c_void) -> i32;
        pub(super) fn GDALAlgorithmArgSetAsString(
            argument: *mut c_void,
            value: *const c_char,
        ) -> bool;
        pub(super) fn GDALAlgorithmArgSetAsStringList(
            argument: *mut c_void,
            values: *const *const c_char,
        ) -> bool;
        pub(super) fn GDALAlgorithmArgSetAsBoolean(argument: *mut c_void, value: bool) -> bool;
        pub(super) fn GDALAlgorithmArgSetAsDouble(argument: *mut c_void, value: f64) -> bool;
        pub(super) fn GDALAlgorithmArgSetAsDoubleList(
            argument: *mut c_void,
            count: usize,
            values: *const f64,
        ) -> bool;
        pub(super) fn GDALAlgorithmArgSetDataset(
            argument: *mut c_void,
            dataset: gdal_sys::GDALDatasetH,
        ) -> bool;
        pub(super) fn GDALAlgorithmArgSetDatasets(
            argument: *mut c_void,
            count: usize,
            datasets: *mut gdal_sys::GDALDatasetH,
        ) -> bool;
        pub(super) fn GDALAlgorithmArgSetDatasetNames(
            argument: *mut c_void,
            names: *const *const c_char,
        ) -> bool;
        pub(super) fn GDALAlgorithmRun(
            algorithm: *mut c_void,
            callback: gdal_sys::GDALProgressFunc,
            data: *mut c_void,
        ) -> bool;
        pub(super) fn GDALAlgorithmFinalize(algorithm: *mut c_void) -> bool;
    }
}
// Compatibility builds keep the public builders, but cannot read native argument values.
#[cfg_attr(not(gdal_3_12), allow(dead_code))]
pub(crate) enum Argument<'a> {
    String(&'a str),
    Strings(&'a [String]),
    DatasetNames(&'a [String]),
    Boolean(bool),
    Double(f64),
    Doubles(&'a [f64]),
}
pub(crate) struct Algorithm<'a> {
    #[cfg(gdal_3_12)]
    raw: std::ptr::NonNull<std::ffi::c_void>,
    finished: bool,
    inputs: PhantomData<&'a Dataset>,
}
impl<'a> Algorithm<'a> {
    pub(crate) fn new(path: &[&str]) -> Result<Self> {
        if supported_version()? < 3_120_000 {
            return Err(Error::unsupported(
                "this algorithm requires GDAL 3.12 or later",
            ));
        }
        #[cfg(gdal_3_12)]
        {
            gdal::DriverManager::count();
            let path = Strings::new(path.iter().copied())?;
            // SAFETY: the registry is owned, and the path list is NUL-terminated and live.
            let raw = unsafe {
                let registry = ffi::GDALGetGlobalAlgorithmRegistry();
                if registry.is_null() {
                    return Err(Error::unsupported("GDAL algorithm registry is unavailable"));
                }
                let result = ffi::GDALAlgorithmRegistryInstantiateAlgFromPath(
                    registry,
                    path.pointers().as_ptr(),
                );
                ffi::GDALAlgorithmRegistryRelease(registry);
                result
            };
            let raw = std::ptr::NonNull::new(raw)
                .ok_or_else(|| Error::unsupported("requested GDAL algorithm is unavailable"))?;
            Ok(Self {
                raw,
                finished: false,
                inputs: PhantomData,
            })
        }
        #[cfg(not(gdal_3_12))]
        {
            let _ = path;
            Err(Error::unsupported(
                "this build does not contain the GDAL 3.12 algorithm API",
            ))
        }
    }
    pub(crate) fn set(&mut self, name: &str, value: Argument<'_>) -> Result<()> {
        #[cfg(gdal_3_12)]
        {
            self.with_arg(name, |arg| match value {
                Argument::String(value) => {
                    let value = string(value)?;
                    // SAFETY: the argument belongs to this algorithm and GDAL copies the live string.
                    status(crate::operation::Stage::Prepare, || unsafe {
                        ffi::GDALAlgorithmArgSetAsString(arg, value.as_ptr())
                    })
                }
                Argument::Strings(values) => {
                    let values = Strings::new(values.iter().map(String::as_str))?;
                    // SAFETY: GDAL copies the NUL-terminated list while its strings are live.
                    status(crate::operation::Stage::Prepare, || unsafe {
                        ffi::GDALAlgorithmArgSetAsStringList(arg, values.pointers().as_ptr())
                    })
                }
                Argument::DatasetNames(values) => {
                    let values = Strings::new(values.iter().map(String::as_str))?;
                    // SAFETY: GDAL copies the live, NUL-terminated names list.
                    status(crate::operation::Stage::Prepare, || unsafe {
                        ffi::GDALAlgorithmArgSetDatasetNames(arg, values.pointers().as_ptr())
                    })
                }
                // SAFETY: the live argument is validated by GDAL against the setter's type.
                Argument::Boolean(value) => status(crate::operation::Stage::Prepare, || unsafe {
                    ffi::GDALAlgorithmArgSetAsBoolean(arg, value)
                }),
                // SAFETY: the live argument is validated by GDAL against the setter's type.
                Argument::Double(value) => status(crate::operation::Stage::Prepare, || unsafe {
                    ffi::GDALAlgorithmArgSetAsDouble(arg, value)
                }),
                // SAFETY: GDAL copies the live typed arrays of exactly the supplied lengths.
                Argument::Doubles(values) => status(crate::operation::Stage::Prepare, || unsafe {
                    ffi::GDALAlgorithmArgSetAsDoubleList(arg, values.len(), values.as_ptr())
                }),
            })
        }
        #[cfg(not(gdal_3_12))]
        {
            let _ = (name, value);
            Err(Error::unsupported("GDAL algorithm API is unavailable"))
        }
    }
    pub(crate) fn bind(&mut self, name: &str, dataset: &'a Dataset) -> Result<()> {
        #[cfg(gdal_3_12)]
        {
            self.with_arg(name, |arg| {
                // SAFETY: GDAL retains a reference; the lifetime also prevents dropping the caller's owner.
                status(crate::operation::Stage::Prepare, || unsafe {
                    if ffi::GDALAlgorithmArgGetType(arg) == 8
                    /* GAAT_DATASET_LIST */
                    {
                        let mut handle = dataset.c_dataset();
                        ffi::GDALAlgorithmArgSetDatasets(arg, 1, &mut handle)
                    } else {
                        ffi::GDALAlgorithmArgSetDataset(arg, dataset.c_dataset())
                    }
                })
            })
        }
        #[cfg(not(gdal_3_12))]
        {
            let _ = (name, dataset);
            Err(Error::unsupported("GDAL algorithm API is unavailable"))
        }
    }
    pub(crate) fn execute(mut self, progress: &mut ProgressCallbackState<'_>) -> Result<()> {
        #[cfg(gdal_3_12)]
        let result = {
            // SAFETY: inputs, algorithm and the borrowed callback state outlive the synchronous call.
            let result = status(crate::operation::Stage::Execute, || unsafe {
                ffi::GDALAlgorithmRun(
                    self.raw.as_ptr(),
                    progress.native_callback(),
                    progress.user_data(),
                )
            });
            check_progress(progress).and(result)
        };
        #[cfg(not(gdal_3_12))]
        let result: Result<()> = {
            let _ = progress;
            Err(Error::unsupported("GDAL algorithm API is unavailable"))
        };
        let mut failure = result.err();
        self.finish(&mut failure);
        failure.map_or(Ok(()), Err)
    }
    pub(crate) fn execute_dataset(
        self,
        progress: &mut ProgressCallbackState<'_>,
    ) -> Result<Dataset> {
        #[cfg(gdal_3_12)]
        {
            let mut this = self;
            // SAFETY: input and callback owners outlive this synchronous native invocation.
            let result = status(crate::operation::Stage::Execute, || unsafe {
                ffi::GDALAlgorithmRun(
                    this.raw.as_ptr(),
                    progress.native_callback(),
                    progress.user_data(),
                )
            });
            let mut output = None;
            let result = check_progress(progress).and(result).and_then(|()| {
                this.with_arg("output", |arg| {
                    // SAFETY: the dataset-value wrapper is released; its additional dataset reference is retained.
                    let raw = unsafe {
                        let value = ffi::GDALAlgorithmArgGetAsDatasetValue(arg);
                        if value.is_null() {
                            return Err(Error::unsupported("algorithm returned no output dataset"));
                        }
                        let raw = ffi::GDALArgDatasetValueGetDatasetIncreaseRefCount(value);
                        ffi::GDALArgDatasetValueRelease(value);
                        raw
                    };
                    if raw.is_null() {
                        return Err(Error::unsupported("algorithm returned no output dataset"));
                    }
                    // SAFETY: this owner has an independent native reference, retained beyond Finalize.
                    output = Some(unsafe { Dataset::from_c_dataset(raw) });
                    Ok(())
                })
            });
            let mut failure = result.err();
            this.finish(&mut failure);
            if failure.is_some() {
                crate::gdal_ext::execution::close(
                    output.take(),
                    crate::operation::Stage::CloseOutput,
                    &mut failure,
                );
            }
            if let Some(error) = failure {
                Err(error)
            } else {
                Ok(output.expect("algorithm output"))
            }
        }
        #[cfg(not(gdal_3_12))]
        {
            let _ = progress;
            Err(Error::unsupported("GDAL algorithm API is unavailable"))
        }
    }
    fn finish(&mut self, failure: &mut Option<Error>) {
        if self.finished {
            return;
        }
        self.finished = true;
        #[cfg(gdal_3_12)]
        {
            use crate::{
                gdal_ext::{
                    execution::record_close,
                    ffi::{checked_gdal_call, last_null_pointer_err},
                },
                operation::Stage,
            };
            // SAFETY: finalization runs exactly once before this algorithm is released.
            let result =
                checked_gdal_call(|| unsafe { ffi::GDALAlgorithmFinalize(self.raw.as_ptr()) })
                    .and_then(|ok| {
                        if ok {
                            Ok(())
                        } else {
                            Err(last_null_pointer_err("GDALAlgorithmFinalize"))
                        }
                    });
            record_close(result, Stage::CloseOutput, failure);
        }
        #[cfg(not(gdal_3_12))]
        let _ = failure;
    }
    #[cfg(gdal_3_12)]
    fn with_arg<T>(
        &self,
        name: &str,
        set: impl FnOnce(*mut std::ffi::c_void) -> Result<T>,
    ) -> Result<T> {
        let name = string(name)?;
        // SAFETY: algorithm and argument name are live; GDAL returns a separately released wrapper.
        let raw = unsafe { ffi::GDALAlgorithmGetArgNonConst(self.raw.as_ptr(), name.as_ptr()) };
        if raw.is_null() {
            return Err(Error::unsupported(
                "this GDAL algorithm does not support the requested parameter",
            ));
        }
        struct Arg(*mut std::ffi::c_void);
        impl Drop for Arg {
            fn drop(&mut self) {
                // SAFETY: this wrapper uniquely owns the argument handle, not its parent algorithm.
                unsafe { ffi::GDALAlgorithmArgRelease(self.0) };
            }
        }
        let arg = Arg(raw);
        set(arg.0)
    }
}
impl Drop for Algorithm<'_> {
    fn drop(&mut self) {
        self.finish(&mut None);
        #[cfg(gdal_3_12)]
        // SAFETY: this unique algorithm handle has been finalized and is released once.
        unsafe {
            ffi::GDALAlgorithmRelease(self.raw.as_ptr())
        };
    }
}
#[cfg(gdal_3_12)]
fn string(value: &str) -> Result<std::ffi::CString> {
    std::ffi::CString::new(value).map_err(|_| Error::invalid("algorithm argument contains NUL"))
}
#[cfg(gdal_3_12)]
struct Strings(Vec<std::ffi::CString>);
#[cfg(gdal_3_12)]
impl Strings {
    fn new<'a>(values: impl IntoIterator<Item = &'a str>) -> Result<Self> {
        values
            .into_iter()
            .map(string)
            .collect::<Result<Vec<_>>>()
            .map(Self)
    }
    fn pointers(&self) -> Vec<*const std::ffi::c_char> {
        self.0
            .iter()
            .map(|s| s.as_ptr())
            .chain(std::iter::once(std::ptr::null()))
            .collect()
    }
}
#[cfg(gdal_3_12)]
fn status(stage: crate::operation::Stage, call: impl FnOnce() -> bool) -> Result<()> {
    use crate::gdal_ext::ffi::{checked_gdal_call, last_null_pointer_err};
    if checked_gdal_call(call).map_err(|e| Error::native(stage, e))? {
        Ok(())
    } else {
        Err(Error::native(
            stage,
            last_null_pointer_err("GDAL algorithm"),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(not(gdal_3_12))]
    #[test]
    fn older_gdal_rejects_algorithms_before_execution() {
        use crate::operation::{ErrorKind, OutputState, Stage};

        assert!(matches!(Algorithm::new(&["raster", "calc"]), Err(error)
            if error.kind() == ErrorKind::Unsupported
                && error.stage() == Stage::Prepare
                && error.output_state() == OutputState::NotModified));
    }

    #[cfg(gdal_3_12)]
    #[test]
    fn bound_dataset_survives_algorithm_finalization() -> anyhow::Result<()> {
        use gdal::{DriverManager, raster::Buffer};

        let mut input = DriverManager::get_driver_by_name("MEM")?
            .create_with_band_type::<u8, _>("", 2, 1, 1)?;
        input.set_geo_transform(&[0., 1., 0., 1., 0., -1.])?;
        input
            .rasterband(1)?
            .write((0, 0), (2, 1), &mut Buffer::new((2, 1), vec![1, 2]))?;
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("mapped.tif");
        let mut algorithm = Algorithm::new(&["raster", "reclassify"])?;
        algorithm.bind("input", &input)?;
        algorithm.set("output", Argument::String(path.to_str().unwrap()))?;
        algorithm.set("output-format", Argument::String("GTiff"))?;
        algorithm.set("mapping", Argument::String("1=10; 2=20"))?;
        algorithm.execute(&mut ProgressCallbackState::optional(None))?;
        assert_eq!(input.rasterband(1)?.read_band_as::<u8>()?.data(), &[1, 2]);
        let output = Dataset::open(&path)?;
        assert_eq!(
            output.rasterband(1)?.read_band_as::<u8>()?.data(),
            &[10, 20]
        );
        output.close()?;
        input.close()?;
        Ok(())
    }
}
