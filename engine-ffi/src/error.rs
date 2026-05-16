use std::cell::RefCell;
use std::ffi::CString;

// FFI error codes — keep in sync with engine.h.
pub const GURUKUL_OK: i32 = 0;
pub const GURUKUL_ERR_UNKNOWN: i32 = -1;
pub const GURUKUL_ERR_INVALID_HANDLE: i32 = -2;
pub const GURUKUL_ERR_BUILD_FAILED: i32 = -3;
/// Returned by future functions that look up a resource by id and find nothing.
/// Currently not used directly in function return codes (resolve functions
/// return `GURUKUL_INVALID_PORT` instead), but defined here for completeness
/// and kept in sync with the engine.h header.
#[allow(dead_code)]
pub const GURUKUL_ERR_NOT_FOUND: i32 = -4;
pub const GURUKUL_ERR_BLOCK_TOO_BIG: i32 = -5;

thread_local! {
    static LAST_ERROR: RefCell<Option<CString>> = const { RefCell::new(None) };
}

/// Store `msg` as the thread-local last-error message.
pub fn set_last_error(msg: &str) {
    // CString::new fails only if the string contains an interior NUL byte, which
    // is not expected in our error messages. Fall back to a generic message.
    let cstring = CString::new(msg)
        .unwrap_or_else(|_| CString::new("error message contained interior NUL byte").unwrap());
    LAST_ERROR.with(|cell| {
        *cell.borrow_mut() = Some(cstring);
    });
}

/// Clear the thread-local last-error message.
pub fn clear_last_error() {
    LAST_ERROR.with(|cell| {
        *cell.borrow_mut() = None;
    });
}

/// Return a pointer to the last-error C string, or null if none is set.
///
/// The returned pointer is valid until the next call that sets a new error,
/// or until thread exit. Never free this pointer.
pub fn last_error_ptr() -> *const std::os::raw::c_char {
    LAST_ERROR.with(|cell| match cell.borrow().as_ref() {
        Some(s) => s.as_ptr(),
        None => std::ptr::null(),
    })
}
