use super::diagnostics::UserCallbackScope;
use crate::operation::{ErrorKind, Progress};
use std::{
    ffi::{CStr, c_char, c_int, c_void},
    panic::{AssertUnwindSafe, catch_unwind},
    sync::Mutex,
};

struct Callback<'a> {
    callback: &'a mut (dyn FnMut(Progress) -> bool + Send),
    interruption: Option<ErrorKind>,
}

/// GDAL may call from native workers. A shared pointer and this mutex protect
/// the borrowed FnMut; the synchronous caller must keep the state alive.
pub(crate) struct ProgressCallbackState<'a> {
    inner: Option<Mutex<Callback<'a>>>,
}

impl<'a> ProgressCallbackState<'a> {
    pub(crate) fn new(callback: &'a mut (dyn FnMut(Progress) -> bool + Send)) -> Self {
        Self::optional(Some(callback))
    }

    pub(crate) fn optional(callback: Option<&'a mut (dyn FnMut(Progress) -> bool + Send)>) -> Self {
        Self {
            inner: callback.map(|callback| {
                Mutex::new(Callback {
                    callback,
                    interruption: None,
                })
            }),
        }
    }

    pub(crate) fn native_callback(&self) -> gdal_sys::GDALProgressFunc {
        if self.inner.is_some() {
            GDAL_PROGRESS_TRAMPOLINE
        } else {
            None
        }
    }

    pub(crate) fn user_data(&mut self) -> *mut c_void {
        if self.inner.is_some() {
            (self as *mut Self).cast()
        } else {
            std::ptr::null_mut()
        }
    }

    pub(crate) fn interruption(&self) -> Option<ErrorKind> {
        self.inner
            .as_ref()?
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .interruption
    }

    pub(crate) fn check(&self) -> gdal::errors::Result<()> {
        if let Some(reason) = self.interruption() {
            Err(gdal::errors::GdalError::CplError {
                class: gdal_sys::CPLErr::CE_Failure,
                number: 9,
                msg: match reason {
                    ErrorKind::CallbackPanicked => "GDAL progress callback panicked",
                    _ => "GDAL operation cancelled by progress callback",
                }
                .into(),
            })
        } else {
            Ok(())
        }
    }
}

pub(crate) const GDAL_PROGRESS_TRAMPOLINE: gdal_sys::GDALProgressFunc = Some(trampoline);

unsafe extern "C" fn trampoline(fraction: f64, message: *const c_char, data: *mut c_void) -> c_int {
    if data.is_null() {
        return 1;
    }
    // SAFETY: the owner keeps the state at this address until all synchronous
    // native callbacks finish. Only the mutex grants mutable access.
    let state = unsafe { &*data.cast::<ProgressCallbackState<'_>>() };
    let Some(inner) = &state.inner else {
        return 1;
    };
    let mut state = inner.lock().unwrap_or_else(|e| e.into_inner());
    if state.interruption.is_some() {
        return 0;
    }
    state.interruption = Some(ErrorKind::CallbackPanicked);
    let result = catch_unwind(AssertUnwindSafe(|| {
        let _scope = UserCallbackScope::enter();
        let progress = Progress {
            fraction: if fraction.is_nan() {
                0.0
            } else {
                fraction.clamp(0.0, 1.0)
            },
            // SAFETY: GDAL's non-null message lives for this callback and is NUL-terminated.
            message: if message.is_null() {
                String::new()
            } else {
                // SAFETY: GDAL's message lives for this callback and is NUL-terminated.
                unsafe { CStr::from_ptr(message) }
                    .to_string_lossy()
                    .into_owned()
            },
        };
        (state.callback)(progress)
    }));
    match result {
        Ok(true) => {
            state.interruption = None;
            1
        }
        Ok(false) => {
            state.interruption = Some(ErrorKind::Cancelled);
            0
        }
        Err(payload) => {
            // A custom panic payload may panic on Drop; it must not unwind across C.
            std::mem::forget(payload);
            0
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absent_control_does_not_install_a_native_callback() {
        let mut state = ProgressCallbackState::optional(None);
        assert!(state.native_callback().is_none());
        assert!(state.user_data().is_null());
        assert!(state.interruption().is_none());
        assert!(state.check().is_ok());
    }

    #[test]
    fn fractions_and_messages_are_normalized_and_null_state_is_safe() {
        let mut observed = Vec::new();
        let mut callback = |progress: Progress| {
            observed.push(progress);
            true
        };
        let mut state = ProgressCallbackState::new(&mut callback);
        for fraction in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, 1.5, -0.5] {
            // SAFETY: state and static message remain valid through each direct callback.
            assert_eq!(
                // SAFETY: state and the static message outlive this call.
                unsafe { trampoline(fraction, c"hello".as_ptr(), state.user_data()) },
                1
            );
        }
        // SAFETY: null message and null user data are explicitly supported.
        unsafe {
            assert_eq!(trampoline(0.5, std::ptr::null(), state.user_data()), 1);
            assert_eq!(trampoline(0.5, std::ptr::null(), std::ptr::null_mut()), 1);
        }
        drop(state);
        assert_eq!(
            observed.iter().map(|p| p.fraction).collect::<Vec<_>>(),
            [0.0, 1.0, 0.0, 1.0, 0.0, 0.5]
        );
        assert!(observed[..5].iter().all(|p| p.message == "hello"));
        assert!(observed[5].message.is_empty());
    }

    #[test]
    fn callbacks_are_serialized_across_native_threads() {
        let mut calls = 0;
        let mut callback = |progress: Progress| {
            assert!((0.0..=1.0).contains(&progress.fraction));
            calls += 1;
            true
        };
        let state = ProgressCallbackState::new(&mut callback);
        std::thread::scope(|scope| {
            for _ in 0..4 {
                scope.spawn(|| {
                    for _ in 0..50 {
                        // SAFETY: the shared state outlives all scoped workers; mutex serializes it.
                        assert_eq!(
                            // SAFETY: scoped workers share a live, mutex-protected state.
                            unsafe {
                                trampoline(
                                    f64::INFINITY,
                                    std::ptr::null(),
                                    (&state as *const ProgressCallbackState<'_>)
                                        .cast_mut()
                                        .cast(),
                                )
                            },
                            1
                        );
                    }
                });
            }
        });
        assert!(state.interruption().is_none());
        drop(state);
        assert_eq!(calls, 200);
    }

    #[test]
    fn cancellation_and_panic_are_sticky_and_distinct() {
        for panics in [false, true] {
            let mut calls = 0;
            let mut callback = |_: Progress| {
                calls += 1;
                assert!(!panics, "callback panic");
                false
            };
            let mut state = ProgressCallbackState::new(&mut callback);
            for _ in 0..2 {
                // SAFETY: this synchronous test keeps state and callback alive.
                assert_eq!(
                    // SAFETY: state and callback remain alive for this invocation.
                    unsafe { trampoline(1.0, std::ptr::null(), state.user_data()) },
                    0
                );
            }
            assert_eq!(
                state.interruption(),
                Some(if panics {
                    ErrorKind::CallbackPanicked
                } else {
                    ErrorKind::Cancelled
                })
            );
            assert!(state.check().is_err());
            drop(state);
            assert_eq!(calls, 1);
        }
    }
}
