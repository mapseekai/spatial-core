use crate::operation::{Diagnostic, Report, Severity};
use std::{
    ffi::{CStr, CString, c_char, c_void},
    marker::PhantomData,
    panic::{AssertUnwindSafe, catch_unwind},
    rc::Rc,
    sync::Mutex,
};

const MAX_ENTRIES: usize = 64;
const MAX_MESSAGE_BYTES: usize = 4096;

#[derive(Default)]
struct Captured {
    report: Report,
    failures: usize,
}

/// The Box keeps the handler's user-data address stable when the guard moves.
pub(crate) struct Diagnostics {
    captured: Box<Mutex<Captured>>,
    previous: CplState,
    _thread: PhantomData<Rc<()>>,
}

impl Diagnostics {
    pub(crate) fn enter() -> Self {
        let captured = Box::new(Mutex::new(Captured::default()));
        let previous = CplState::save();
        // SAFETY: the boxed state outlives this thread-local handler; Drop pops it on this thread.
        unsafe {
            gdal_sys::CPLPushErrorHandlerEx(
                Some(collect),
                (&*captured as *const Mutex<Captured>).cast_mut().cast(),
            );
            gdal_sys::CPLErrorReset();
        }
        Self {
            captured,
            previous,
            _thread: PhantomData,
        }
    }

    pub(crate) fn failures(&self) -> usize {
        self.captured
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .failures
    }

    pub(crate) fn take_report(&self) -> Report {
        std::mem::take(
            &mut self
                .captured
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .report,
        )
    }
}

impl Drop for Diagnostics {
    fn drop(&mut self) {
        // SAFETY: this !Send guard is dropped on its installing thread, in stack order.
        unsafe { gdal_sys::CPLPopErrorHandler() };
        self.previous.restore();
    }
}

unsafe extern "C" fn collect(class: gdal_sys::CPLErr::Type, number: i32, message: *const c_char) {
    let _ = catch_unwind(AssertUnwindSafe(|| {
        let severity = match class {
            gdal_sys::CPLErr::CE_Warning => Severity::Warning,
            gdal_sys::CPLErr::CE_Failure => Severity::Failure,
            gdal_sys::CPLErr::CE_Fatal => Severity::Fatal,
            _ => return,
        };
        // SAFETY: GDAL invokes the active handler with its matching live user data.
        let data = unsafe { gdal_sys::CPLGetErrorHandlerUserData() }.cast::<Mutex<Captured>>();
        if data.is_null() {
            return;
        }
        // SAFETY: Diagnostics owns the stable allocation until this handler is popped.
        let captured = unsafe { &*data };
        let mut captured = captured.lock().unwrap_or_else(|e| e.into_inner());
        if class >= gdal_sys::CPLErr::CE_Failure {
            captured.failures = captured.failures.saturating_add(1);
        }
        if captured.report.diagnostics.len() == MAX_ENTRIES {
            captured.report.diagnostics_truncated = true;
            return;
        }
        // SAFETY: a non-null CPL message is NUL-terminated and live during the callback.
        let bytes = if message.is_null() {
            &[][..]
        } else {
            // SAFETY: the native handler supplies a NUL-terminated message.
            unsafe { CStr::from_ptr(message) }.to_bytes()
        };
        let mut truncated = bytes.len() > MAX_MESSAGE_BYTES;
        let mut text =
            String::from_utf8_lossy(&bytes[..bytes.len().min(MAX_MESSAGE_BYTES)]).into_owned();
        if text.len() > MAX_MESSAGE_BYTES {
            let mut end = MAX_MESSAGE_BYTES;
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            text.truncate(end);
            truncated = true;
        }
        captured.report.diagnostics_truncated |= truncated;
        captured.report.diagnostics.push(Diagnostic {
            severity,
            number,
            message: text,
            truncated,
        });
    }));
}

struct CplState {
    class: gdal_sys::CPLErr::Type,
    number: i32,
    message: CString,
}

impl CplState {
    fn save() -> Self {
        // SAFETY: CPL returns thread-local state and a valid NUL-terminated message, copied here.
        unsafe {
            Self {
                class: gdal_sys::CPLGetLastErrorType(),
                number: gdal_sys::CPLGetLastErrorNo(),
                message: CStr::from_ptr(gdal_sys::CPLGetLastErrorMsg()).to_owned(),
            }
        }
    }

    fn restore(&self) {
        // SAFETY: the owned message is live; SetState does not invoke an error handler.
        unsafe { gdal_sys::CPLErrorSetState(self.class, self.number, self.message.as_ptr()) };
    }
}

/// Isolates user code on the actual callback thread, including Warp workers
/// whose temporary accumulator would otherwise replay user errors later.
pub(crate) struct UserCallbackScope {
    previous: CplState,
    _thread: PhantomData<Rc<()>>,
}

impl UserCallbackScope {
    pub(crate) fn enter() -> Self {
        let previous = CplState::save();
        // SAFETY: the quiet handler is native; this !Send guard pops it in stack order.
        unsafe {
            gdal_sys::CPLPushErrorHandlerEx(
                Some(gdal_sys::CPLQuietErrorHandler),
                std::ptr::null_mut::<c_void>(),
            );
            gdal_sys::CPLErrorReset();
        }
        Self {
            previous,
            _thread: PhantomData,
        }
    }
}

impl Drop for UserCallbackScope {
    fn drop(&mut self) {
        // SAFETY: paired with enter on the same native thread.
        unsafe { gdal_sys::CPLPopErrorHandler() };
        self.previous.restore();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diagnostics_are_bounded_and_failure_survives_truncation() {
        let guard = Diagnostics::enter();
        let message = CString::new("界".repeat(5000)).unwrap();
        for _ in 0..70 {
            // SAFETY: %s receives a live, NUL-terminated string.
            unsafe {
                gdal_sys::CPLError(
                    gdal_sys::CPLErr::CE_Warning,
                    1,
                    c"%s".as_ptr(),
                    message.as_ptr(),
                )
            };
        }
        // SAFETY: the static format has no arguments.
        unsafe {
            gdal_sys::CPLError(
                gdal_sys::CPLErr::CE_Failure,
                2,
                c"failure after capacity".as_ptr(),
            )
        };
        assert_eq!(guard.failures(), 1);
        let report = guard.take_report();
        assert_eq!(report.diagnostics().len(), 64);
        assert!(report.diagnostics_truncated());
        assert!(
            report
                .diagnostics()
                .iter()
                .all(|d| d.message.len() <= 4096 && d.truncated)
        );
    }

    #[test]
    fn nested_callbacks_keep_diagnostics_and_error_state_separate() {
        let outer = Diagnostics::enter();
        // SAFETY: static format has no arguments.
        unsafe { gdal_sys::CPLError(gdal_sys::CPLErr::CE_Warning, 23, c"outer warning".as_ptr()) };
        {
            let _user = UserCallbackScope::enter();
            let inner = Diagnostics::enter();
            // SAFETY: static format has no arguments.
            unsafe {
                gdal_sys::CPLError(
                    gdal_sys::CPLErr::CE_Failure,
                    42,
                    c"handled inner error".as_ptr(),
                )
            };
            assert_eq!(inner.failures(), 1);
        }
        assert_eq!(outer.failures(), 0);
        assert_eq!(outer.take_report().diagnostics().len(), 1);
        // SAFETY: reads only this thread's state.
        assert_eq!(unsafe { gdal_sys::CPLGetLastErrorNo() }, 23);
    }
}
