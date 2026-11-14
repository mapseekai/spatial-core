//! Owned utility options and synchronous GDAL calls. No subprocess is used.
// Options wrappers consolidate the inherited georust/gdal-style wrappers.
// Copyright (c) 2014 Alex Morega; notice: third_party/georust-gdal/LICENSE.txt.
use crate::gdal_ext::{ffi::last_null_pointer_err, progress::ProgressCallbackState};
use gdal::{
    Dataset,
    errors::{GdalError, Result},
};
use std::{
    ffi::{CStr, CString, c_void},
    ptr,
};

#[derive(Clone, Copy, Debug)]
pub(crate) enum Kind {
    Translate,
    Warp,
    Dem,
    Rasterize,
    BuildVrt,
}

macro_rules! options {
    ($name:ident, $raw:ident, $new:ident, $free:ident, $progress:ident) => {
        pub(crate) struct $name {
            pub(super) c_options: *mut gdal_sys::$raw,
        }
        impl $name {
            pub(crate) fn new<S: Into<Vec<u8>>, I: IntoIterator<Item = S>>(
                args: I,
            ) -> Result<Self> {
                let strings = args
                    .into_iter()
                    .map(CString::new)
                    .collect::<std::result::Result<Vec<_>, _>>()?;
                let mut pointers = strings
                    .iter()
                    .map(|s| s.as_ptr().cast_mut())
                    .chain(std::iter::once(ptr::null_mut()))
                    .collect::<Vec<_>>();
                // SAFETY: the parser copies the live null-terminated argument list.
                let raw = unsafe { gdal_sys::$new(pointers.as_mut_ptr(), ptr::null_mut()) };
                if raw.is_null() {
                    return Err(last_null_pointer_err(stringify!($new)));
                }
                Ok(Self { c_options: raw })
            }
            /// # Safety
            /// The callback state must outlive every native operation using these options.
            pub(crate) unsafe fn set_progress(
                &mut self,
                callback: gdal_sys::GDALProgressFunc,
                data: *mut c_void,
            ) {
                // SAFETY: options are live and the caller guarantees the callback's lifetime.
                unsafe { gdal_sys::$progress(self.c_options, callback, data) };
            }
        }
        impl Drop for $name {
            fn drop(&mut self) {
                // SAFETY: this wrapper uniquely owns a non-null native options allocation.
                unsafe { gdal_sys::$free(self.c_options) };
            }
        }
    };
}
options!(
    TranslateOptions,
    GDALTranslateOptions,
    GDALTranslateOptionsNew,
    GDALTranslateOptionsFree,
    GDALTranslateOptionsSetProgress
);
options!(
    WarpOptions,
    GDALWarpAppOptions,
    GDALWarpAppOptionsNew,
    GDALWarpAppOptionsFree,
    GDALWarpAppOptionsSetProgress
);
options!(
    DemOptions,
    GDALDEMProcessingOptions,
    GDALDEMProcessingOptionsNew,
    GDALDEMProcessingOptionsFree,
    GDALDEMProcessingOptionsSetProgress
);
options!(
    RasterizeOptions,
    GDALRasterizeOptions,
    GDALRasterizeOptionsNew,
    GDALRasterizeOptionsFree,
    GDALRasterizeOptionsSetProgress
);
options!(
    VrtOptions,
    GDALBuildVRTOptions,
    GDALBuildVRTOptionsNew,
    GDALBuildVRTOptionsFree,
    GDALBuildVRTOptionsSetProgress
);

pub(crate) enum Options {
    Translate(TranslateOptions),
    Warp(WarpOptions),
    Dem(DemOptions),
    Rasterize(RasterizeOptions),
    BuildVrt(VrtOptions),
}
impl Options {
    pub(crate) fn new(kind: Kind, args: &[String]) -> Result<Self> {
        let args = args.iter().map(String::as_str);
        Ok(match kind {
            Kind::Translate => Self::Translate(TranslateOptions::new(args)?),
            Kind::Warp => Self::Warp(WarpOptions::new(args)?),
            Kind::Dem => Self::Dem(DemOptions::new(args)?),
            Kind::Rasterize => Self::Rasterize(RasterizeOptions::new(args)?),
            Kind::BuildVrt => Self::BuildVrt(VrtOptions::new(args)?),
        })
    }

    pub(crate) fn execute(
        &mut self,
        inputs: &[Dataset],
        destination: &CStr,
        dem: Option<(&CStr, Option<&CStr>)>,
        progress: &mut ProgressCallbackState<'_>,
    ) -> Outcome {
        let mut sources = inputs.iter().map(Dataset::c_dataset).collect::<Vec<_>>();
        let mut usage_error = 0;
        // SAFETY: all datasets, strings, options and callback state outlive these
        // synchronous calls. Each utility opens its own output and borrows inputs.
        let raw = unsafe {
            match self {
                Self::Translate(options) => {
                    options.set_progress(progress.native_callback(), progress.user_data());
                    gdal_sys::GDALTranslate(
                        destination.as_ptr(),
                        sources[0],
                        options.c_options,
                        &mut usage_error,
                    )
                }
                Self::Warp(options) => {
                    options.set_progress(progress.native_callback(), progress.user_data());
                    gdal_sys::GDALWarp(
                        destination.as_ptr(),
                        ptr::null_mut(),
                        sources.len() as i32,
                        sources.as_mut_ptr(),
                        options.c_options,
                        &mut usage_error,
                    )
                }
                Self::Dem(options) => {
                    options.set_progress(progress.native_callback(), progress.user_data());
                    let (mode, colors) = dem.expect("DEM parameters validated");
                    gdal_sys::GDALDEMProcessing(
                        destination.as_ptr(),
                        sources[0],
                        mode.as_ptr(),
                        colors.map_or(ptr::null(), CStr::as_ptr),
                        options.c_options,
                        &mut usage_error,
                    )
                }
                Self::Rasterize(options) => {
                    options.set_progress(progress.native_callback(), progress.user_data());
                    gdal_sys::GDALRasterize(
                        destination.as_ptr(),
                        ptr::null_mut(),
                        sources[0],
                        options.c_options,
                        &mut usage_error,
                    )
                }
                Self::BuildVrt(options) => {
                    options.set_progress(progress.native_callback(), progress.user_data());
                    gdal_sys::GDALBuildVRT(
                        destination.as_ptr(),
                        sources.len() as i32,
                        sources.as_mut_ptr(),
                        ptr::null(),
                        options.c_options,
                        &mut usage_error,
                    )
                }
            }
        };
        let error = raw
            .is_null()
            .then(|| last_null_pointer_err("GDAL raster utility"));
        let output = if raw.is_null() {
            None
        } else {
            // SAFETY: the utility transferred one newly owned non-null output handle.
            Some(unsafe { Dataset::from_c_dataset(raw) })
        };
        Outcome {
            output,
            error,
            usage_error: usage_error != 0,
        }
    }
}

pub(crate) struct Outcome {
    pub output: Option<Dataset>,
    pub error: Option<GdalError>,
    pub usage_error: bool,
}
