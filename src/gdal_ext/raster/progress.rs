use crate::raster::convert::GdalOperationProgress;
use gdal_sys::GDALProgressFunc;
use libc::{c_char, c_int};
use std::any::Any;
use std::ffi::{CStr, c_void};
use std::panic::{AssertUnwindSafe, catch_unwind};

pub(crate) struct ProgressCallbackState<'a> {
    callback: &'a mut dyn FnMut(GdalOperationProgress) -> bool,
    aborted: bool,
}

impl<'a> ProgressCallbackState<'a> {
    pub(crate) fn new(callback: &'a mut dyn FnMut(GdalOperationProgress) -> bool) -> Self {
        Self {
            callback,
            aborted: false,
        }
    }

    pub(crate) fn user_data(&mut self) -> *mut c_void {
        self as *mut Self as *mut c_void
    }

    pub(crate) fn check(&self) -> gdal::errors::Result<()> {
        if self.aborted {
            return Err(gdal::errors::GdalError::CplError {
                class: gdal_sys::CPLErr::CE_Failure,
                number: 9, // CPLE_UserInterrupt
                msg: "GDAL operation cancelled by progress callback".into(),
            });
        }
        Ok(())
    }
}

pub(crate) const GDAL_PROGRESS_TRAMPOLINE: GDALProgressFunc = Some(gdal_progress_trampoline);

unsafe extern "C" fn gdal_progress_trampoline(
    complete: f64,
    message: *const c_char,
    user_data: *mut c_void,
) -> c_int {
    if user_data.is_null() {
        return 1;
    }

    // SAFETY: the synchronous GDAL caller exclusively borrows this live state until the operation returns.
    let callback = unsafe { &mut *(user_data as *mut ProgressCallbackState<'_>) };
    if callback.aborted {
        return 0;
    }

    let progress = GdalOperationProgress {
        fraction: normalize_progress_fraction(complete),
        message: if message.is_null() {
            String::new()
        } else {
            // SAFETY: GDAL's non-null message is NUL-terminated and remains live during this callback.
            unsafe { CStr::from_ptr(message) }
                .to_string_lossy()
                .into_owned()
        },
    };

    // Some native operations ignore a callback rejection or call it again before returning.
    callback.aborted = true;
    match catch_unwind(AssertUnwindSafe(|| (callback.callback)(progress))) {
        Ok(true) => {
            callback.aborted = false;
            1
        }
        Ok(false) => 0,
        Err(payload) => {
            tracing::error!(
                panic = panic_payload_message(payload.as_ref()),
                "GDAL progress callback panicked; aborting GDAL operation"
            );
            0
        }
    }
}

fn normalize_progress_fraction(complete: f64) -> f64 {
    if complete.is_nan() {
        0.0
    } else {
        complete.clamp(0.0, 1.0)
    }
}

fn panic_payload_message(payload: &(dyn Any + Send)) -> &str {
    if let Some(message) = payload.downcast_ref::<&'static str>() {
        message
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.as_str()
    } else {
        "non-string panic payload"
    }
}

#[cfg(test)]
mod tests {
    use super::{GDAL_PROGRESS_TRAMPOLINE, ProgressCallbackState, gdal_progress_trampoline};
    use crate::raster::convert::GdalOperationProgress;
    use std::ffi::CString;
    use std::panic::{self, AssertUnwindSafe};
    use std::sync::{Mutex, OnceLock};

    fn with_suppressed_panic_hook<T>(f: impl FnOnce() -> T) -> T {
        static PANIC_HOOK_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

        let _hook_lock = PANIC_HOOK_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .expect("panic hook lock should not be poisoned");
        let original_hook = panic::take_hook();
        panic::set_hook(Box::new(|_| {}));

        let result = panic::catch_unwind(AssertUnwindSafe(f));
        panic::set_hook(original_hook);

        match result {
            Ok(value) => value,
            Err(payload) => panic::resume_unwind(payload),
        }
    }

    #[test]
    fn gdal_progress_trampoline_clamps_and_converts_message() {
        let message = CString::new("hello").expect("message should be valid");
        let mut observed = Vec::new();
        let mut callback = |progress: GdalOperationProgress| {
            observed.push((progress.fraction, progress.message));
            true
        };
        let mut state = ProgressCallbackState::new(&mut callback);

        // SAFETY: the message and exclusive callback state remain alive for this direct invocation.
        let result = unsafe { gdal_progress_trampoline(1.5, message.as_ptr(), state.user_data()) };

        assert_eq!(result, 1);
        assert_eq!(observed, vec![(1.0, String::from("hello"))]);
        assert!(GDAL_PROGRESS_TRAMPOLINE.is_some());
    }

    #[test]
    fn gdal_progress_trampoline_treats_null_message_as_empty() {
        let mut observed = Vec::new();
        let mut callback = |progress: GdalOperationProgress| {
            observed.push(progress.message);
            true
        };
        let mut state = ProgressCallbackState::new(&mut callback);

        // SAFETY: null messages are supported and the exclusive callback state is live.
        let result = unsafe { gdal_progress_trampoline(-0.5, std::ptr::null(), state.user_data()) };

        assert_eq!(result, 1);
        assert_eq!(observed, vec![String::new()]);
    }

    #[test]
    fn gdal_progress_trampoline_normalizes_non_finite_fraction() {
        let mut observed = Vec::new();
        let mut callback = |progress: GdalOperationProgress| {
            observed.push(progress.fraction);
            true
        };
        let mut state = ProgressCallbackState::new(&mut callback);

        // SAFETY: null messages are supported and the exclusive callback state is live.
        let nan_result =
            unsafe { gdal_progress_trampoline(f64::NAN, std::ptr::null(), state.user_data()) };
        // SAFETY: null messages are supported and the exclusive callback state is live.
        let infinity_result =
            unsafe { gdal_progress_trampoline(f64::INFINITY, std::ptr::null(), state.user_data()) };
        // SAFETY: null messages are supported and the exclusive callback state is live.
        let neg_infinity_result = unsafe {
            gdal_progress_trampoline(f64::NEG_INFINITY, std::ptr::null(), state.user_data())
        };

        assert_eq!(nan_result, 1);
        assert_eq!(infinity_result, 1);
        assert_eq!(neg_infinity_result, 1);
        assert_eq!(observed, vec![0.0, 1.0, 0.0]);
    }

    #[test]
    fn gdal_progress_trampoline_returns_zero_for_false_or_panic() {
        let mut callback_false = |_progress: GdalOperationProgress| false;
        let mut state_false = ProgressCallbackState::new(&mut callback_false);
        // SAFETY: null messages are supported and state_false exclusively borrows the live callback.
        let false_result =
            unsafe { gdal_progress_trampoline(0.25, std::ptr::null(), state_false.user_data()) };

        let mut callback_panic = |_progress: GdalOperationProgress| -> bool {
            panic!("boom");
        };
        let mut state_panic = ProgressCallbackState::new(&mut callback_panic);
        // SAFETY: state_panic remains alive and the trampoline catches panics before crossing the C ABI.
        let panic_result = with_suppressed_panic_hook(|| unsafe {
            gdal_progress_trampoline(0.25, std::ptr::null(), state_panic.user_data())
        });

        // SAFETY: null user_data explicitly skips dereferencing either pointer.
        let null_user_data_result =
            unsafe { gdal_progress_trampoline(0.25, std::ptr::null(), std::ptr::null_mut()) };

        assert_eq!(false_result, 0);
        assert_eq!(panic_result, 0);
        assert_eq!(null_user_data_result, 1);
    }

    #[test]
    fn progress_rejection_is_sticky_even_if_gdal_calls_again() {
        for panics in [false, true] {
            let mut calls = 0;
            let mut callback = |_progress: GdalOperationProgress| {
                calls += 1;
                if calls > 1 {
                    return true;
                }
                assert!(!panics, "callback failed");
                false
            };
            let mut state = ProgressCallbackState::new(&mut callback);
            with_suppressed_panic_hook(|| {
                for fraction in [0.25, 1.0] {
                    // SAFETY: the callback state stays live through both synchronous calls.
                    let result = unsafe {
                        gdal_progress_trampoline(fraction, std::ptr::null(), state.user_data())
                    };
                    assert_eq!(result, 0);
                }
            });
            assert!(state.check().is_err());
            assert_eq!(calls, 1);
        }
    }
}
