//! C-ABI wrapper around the gurukul `engine` crate.
//!
//! # Safety contract
//!
//! - Every `GurukulEngine*` pointer passed to these functions must have been
//!   obtained from `engine_build` and must not have been freed yet.
//! - `engine_free` must be called exactly once per engine returned by
//!   `engine_build`. Calling it twice is undefined behaviour.
//! - Buffer pointers returned by `engine_in_port` / `engine_out_port` are
//!   valid until the next call to `engine_process_block` or `engine_free`.
//! - All `const char*` string arguments must be valid UTF-8, null-terminated,
//!   and live for the duration of the call.
//! - This library is thread-safe in the sense that separate engines on separate
//!   threads are independent. Sharing one engine across threads without external
//!   synchronisation is not supported.

// All public `extern "C"` functions take raw pointer arguments and dereference
// them after an explicit null check. This is the intended contract for a C FFI
// boundary: callers must uphold pointer validity. Clippy's lint would require
// marking every entry point `unsafe`, which defeats the error-handling and
// null-guard layer we deliberately provide.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

mod error;

use engine::{Engine, InPortHandle, NodeRegistry, OutPortHandle, World};
use error::{
    GURUKUL_ERR_BLOCK_TOO_BIG, GURUKUL_ERR_BUILD_FAILED, GURUKUL_ERR_INVALID_HANDLE,
    GURUKUL_ERR_UNKNOWN, GURUKUL_OK,
};
use std::ffi::{CStr, CString};
use std::os::raw::c_char;
use std::panic::AssertUnwindSafe;

/// Sentinel value returned by `engine_resolve_in_port` / `engine_resolve_out_port`
/// when the requested id is not found.
pub const GURUKUL_INVALID_PORT: u32 = u32::MAX;

// ─── Registry ────────────────────────────────────────────────────────────────

/// Build a `NodeRegistry` containing every node type shipped with gurukul.
///
/// This mirrors `build_registry()` in `cli/src/main.rs`. The registry is
/// intentionally constructed fresh per `engine_build` call — it is cheap and
/// stateless once the engine is running.
fn default_registry() -> NodeRegistry {
    let mut r = NodeRegistry::new();
    node_synth_sine::register(&mut r);
    node_synth_vibrato_sine::register(&mut r);
    node_synth_pink_noise::register(&mut r);
    node_mix_sum::register(&mut r);
    node_rms_meter::register(&mut r);
    node_assert_near::register(&mut r);
    node_gain::register(&mut r);
    node_passthrough::register(&mut r);
    node_null_sink::register(&mut r);
    node_pitch_error::register(&mut r);
    node_pitch_yin::register(&mut r);
    node_tracer::register(&mut r);
    node_vibrato::register(&mut r);
    node_synth_onsets::register(&mut r);
    node_onset::register(&mut r);
    node_synth_breath::register(&mut r);
    node_breath::register(&mut r);
    r
}

// ─── Opaque handle type ──────────────────────────────────────────────────────

/// Opaque engine handle. Obtain via `engine_build`; free via `engine_free`.
///
/// This type is never constructed directly — it exists solely so that C code
/// can hold a typed pointer (`GurukulEngine*`) rather than a bare `void*`.
pub struct GurukulEngine {
    engine: Engine,
    /// Null-terminated copies of in-port ids. Returned by `engine_in_port_id`.
    /// Stored here so we can hand a stable `*const c_char` to C callers that
    /// is valid for the lifetime of the engine.
    in_port_ids: Vec<CString>,
    /// Null-terminated copies of out-port ids. Returned by `engine_out_port_id`.
    out_port_ids: Vec<CString>,
}

// ─── Lifecycle ───────────────────────────────────────────────────────────────

/// Build and initialise an engine from a World JSON string.
///
/// On success, `*out_engine` is set to a freshly allocated engine handle and
/// `0` is returned.
///
/// On failure, `*out_engine` is set to `NULL`, a negative error code is
/// returned, and `engine_last_error_message` returns a human-readable
/// explanation.
///
/// The caller is responsible for calling `engine_free` exactly once when done.
///
/// # Parameters
/// - `world_json`  — null-terminated UTF-8 JSON string describing the World.
/// - `sample_rate` — sample rate in Hz (e.g. 48000).
/// - `block_size`  — maximum block size in frames; buffers are pre-allocated to this
///   size. `engine_process_block` accepts any `n ≤ block_size`.
/// - `out_engine`  — receives the engine pointer on success, `NULL` on failure.
#[unsafe(no_mangle)]
pub extern "C" fn engine_build(
    world_json: *const c_char,
    sample_rate: u32,
    block_size: usize,
    out_engine: *mut *mut GurukulEngine,
) -> i32 {
    // Wrap in AssertUnwindSafe because raw pointers are not UnwindSafe.
    // We uphold the invariant manually: on panic we set last error and return
    // the generic error code; no resources are leaked because the engine was
    // never handed to the caller.
    let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
        if out_engine.is_null() {
            error::set_last_error("out_engine pointer is null");
            return GURUKUL_ERR_INVALID_HANDLE;
        }

        if world_json.is_null() {
            error::set_last_error("world_json pointer is null");
            // SAFETY: out_engine is non-null (checked above).
            unsafe { *out_engine = std::ptr::null_mut() };
            return GURUKUL_ERR_INVALID_HANDLE;
        }

        // SAFETY: caller guarantees a null-terminated UTF-8 string.
        let json_str = match unsafe { CStr::from_ptr(world_json) }.to_str() {
            Ok(s) => s,
            Err(_) => {
                error::set_last_error("world_json is not valid UTF-8");
                unsafe { *out_engine = std::ptr::null_mut() };
                return GURUKUL_ERR_INVALID_HANDLE;
            }
        };

        let world: World = match serde_json::from_str(json_str) {
            Ok(w) => w,
            Err(e) => {
                error::set_last_error(&format!("world JSON parse error: {e}"));
                // SAFETY: out_engine is non-null (checked above).
                unsafe { *out_engine = std::ptr::null_mut() };
                return GURUKUL_ERR_BUILD_FAILED;
            }
        };

        let registry = default_registry();
        match Engine::build(&world, &registry, sample_rate, block_size) {
            Ok(engine) => {
                // Pre-compute null-terminated id strings so we can return stable
                // *const c_char pointers from engine_in_port_id / engine_out_port_id.
                let in_port_ids: Vec<CString> = engine
                    .in_port_specs()
                    .iter()
                    .map(|s| {
                        // Port ids are validated to be ASCII by the engine; no interior NUL.
                        CString::new(s.id.as_str()).unwrap_or_else(|_| CString::new("?").unwrap())
                    })
                    .collect();
                let out_port_ids: Vec<CString> = engine
                    .out_port_specs()
                    .iter()
                    .map(|s| {
                        CString::new(s.id.as_str()).unwrap_or_else(|_| CString::new("?").unwrap())
                    })
                    .collect();
                let boxed = Box::new(GurukulEngine {
                    engine,
                    in_port_ids,
                    out_port_ids,
                });
                // SAFETY: out_engine is non-null (checked above).
                unsafe { *out_engine = Box::into_raw(boxed) };
                error::clear_last_error();
                GURUKUL_OK
            }
            Err(e) => {
                error::set_last_error(&format!("engine build error: {e}"));
                // SAFETY: out_engine is non-null (checked above).
                unsafe { *out_engine = std::ptr::null_mut() };
                GURUKUL_ERR_BUILD_FAILED
            }
        }
    }));

    match result {
        Ok(code) => code,
        Err(_) => {
            error::set_last_error("panic in engine_build");
            GURUKUL_ERR_UNKNOWN
        }
    }
}

/// Free an engine previously obtained from `engine_build`.
///
/// Must be called exactly once. Passing `NULL` is a no-op.
#[unsafe(no_mangle)]
pub extern "C" fn engine_free(engine: *mut GurukulEngine) {
    if engine.is_null() {
        return;
    }
    // SAFETY: pointer came from Box::into_raw in engine_build; freed exactly once.
    let _ = unsafe { Box::from_raw(engine) };
}

// ─── Introspection ───────────────────────────────────────────────────────────

/// Return the sample rate the engine was built with.
#[unsafe(no_mangle)]
pub extern "C" fn engine_sample_rate(engine: *const GurukulEngine) -> u32 {
    if engine.is_null() {
        return 0;
    }
    // SAFETY: pointer came from engine_build and has not been freed.
    unsafe { (*engine).engine.sample_rate() }
}

/// Return the block size (maximum frames per `engine_process_block` call).
#[unsafe(no_mangle)]
pub extern "C" fn engine_block_size(engine: *const GurukulEngine) -> usize {
    if engine.is_null() {
        return 0;
    }
    // SAFETY: pointer came from engine_build and has not been freed.
    unsafe { (*engine).engine.block_size() }
}

/// Return the number of boundary input ports.
#[unsafe(no_mangle)]
pub extern "C" fn engine_num_in_ports(engine: *const GurukulEngine) -> usize {
    if engine.is_null() {
        return 0;
    }
    // SAFETY: pointer came from engine_build and has not been freed.
    unsafe { (*engine).engine.in_port_specs().len() }
}

/// Return the number of boundary output ports.
#[unsafe(no_mangle)]
pub extern "C" fn engine_num_out_ports(engine: *const GurukulEngine) -> usize {
    if engine.is_null() {
        return 0;
    }
    // SAFETY: pointer came from engine_build and has not been freed.
    unsafe { (*engine).engine.out_port_specs().len() }
}

/// Return the id of the boundary input port at `index` as a null-terminated
/// UTF-8 string.
///
/// The returned pointer is into engine-owned memory and is valid until
/// `engine_free` is called. Returns `NULL` if `index` is out of range or
/// `engine` is null.
///
/// Note: `name` and `description` fields are not yet exposed through the C ABI
/// (follow-up: add `engine_in_port_name` / `engine_in_port_description` if
/// needed by a future cabinet).
#[unsafe(no_mangle)]
pub extern "C" fn engine_in_port_id(engine: *const GurukulEngine, index: usize) -> *const c_char {
    if engine.is_null() {
        return std::ptr::null();
    }
    // SAFETY: pointer came from engine_build and has not been freed.
    let ids = unsafe { &(*engine).in_port_ids };
    match ids.get(index) {
        Some(cstr) => cstr.as_ptr(),
        None => std::ptr::null(),
    }
}

/// Return the id of the boundary output port at `index` as a null-terminated
/// UTF-8 string.
///
/// The returned pointer is into engine-owned memory and is valid until
/// `engine_free` is called. Returns `NULL` if `index` is out of range or
/// `engine` is null.
#[unsafe(no_mangle)]
pub extern "C" fn engine_out_port_id(engine: *const GurukulEngine, index: usize) -> *const c_char {
    if engine.is_null() {
        return std::ptr::null();
    }
    // SAFETY: pointer came from engine_build and has not been freed.
    let ids = unsafe { &(*engine).out_port_ids };
    match ids.get(index) {
        Some(cstr) => cstr.as_ptr(),
        None => std::ptr::null(),
    }
}

// ─── Port resolution ─────────────────────────────────────────────────────────

/// Resolve a boundary input port id to an `InPortHandle`.
///
/// Returns `GURUKUL_INVALID_PORT` (`UINT32_MAX`) if not found. This is a
/// build-time / setup call — do not call on the audio thread.
#[unsafe(no_mangle)]
pub extern "C" fn engine_resolve_in_port(engine: *const GurukulEngine, id: *const c_char) -> u32 {
    let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
        if engine.is_null() {
            error::set_last_error("engine pointer is null");
            return GURUKUL_INVALID_PORT;
        }
        if id.is_null() {
            error::set_last_error("id pointer is null");
            return GURUKUL_INVALID_PORT;
        }
        // SAFETY: caller guarantees a null-terminated UTF-8 string.
        let id_str = match unsafe { CStr::from_ptr(id) }.to_str() {
            Ok(s) => s,
            Err(_) => {
                error::set_last_error("id is not valid UTF-8");
                return GURUKUL_INVALID_PORT;
            }
        };
        // SAFETY: pointer came from engine_build and has not been freed.
        match unsafe { (*engine).engine.resolve_in_port(id_str) } {
            Ok(h) => h.as_u32(),
            Err(_) => {
                error::set_last_error(&format!("in-port '{id_str}' not found"));
                GURUKUL_INVALID_PORT
            }
        }
    }));
    result.unwrap_or_else(|_| {
        error::set_last_error("panic in engine_resolve_in_port");
        GURUKUL_INVALID_PORT
    })
}

/// Resolve a boundary output port id to an `OutPortHandle`.
///
/// Returns `GURUKUL_INVALID_PORT` (`UINT32_MAX`) if not found. Build-time
/// call only — do not call on the audio thread.
#[unsafe(no_mangle)]
pub extern "C" fn engine_resolve_out_port(engine: *const GurukulEngine, id: *const c_char) -> u32 {
    let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
        if engine.is_null() {
            error::set_last_error("engine pointer is null");
            return GURUKUL_INVALID_PORT;
        }
        if id.is_null() {
            error::set_last_error("id pointer is null");
            return GURUKUL_INVALID_PORT;
        }
        // SAFETY: caller guarantees a null-terminated UTF-8 string.
        let id_str = match unsafe { CStr::from_ptr(id) }.to_str() {
            Ok(s) => s,
            Err(_) => {
                error::set_last_error("id is not valid UTF-8");
                return GURUKUL_INVALID_PORT;
            }
        };
        // SAFETY: pointer came from engine_build and has not been freed.
        match unsafe { (*engine).engine.resolve_out_port(id_str) } {
            Ok(h) => h.as_u32(),
            Err(_) => {
                error::set_last_error(&format!("out-port '{id_str}' not found"));
                GURUKUL_INVALID_PORT
            }
        }
    }));
    result.unwrap_or_else(|_| {
        error::set_last_error("panic in engine_resolve_out_port");
        GURUKUL_INVALID_PORT
    })
}

// ─── I/O buffer access ───────────────────────────────────────────────────────

/// Get a writable pointer to the boundary input buffer for `handle`.
///
/// On success: `*out_ptr` points to `float[*out_len]` and `0` is returned.
/// `*out_len` equals the engine's `block_size`, **not** the `n_frames` that
/// will be passed to `engine_process_block`. When processing a partial block,
/// fill only the first `n_frames` slots — the rest is ignored.
///
/// The host writes audio data into this buffer before calling
/// `engine_process_block`.
///
/// The pointer is valid until the next `engine_process_block` or
/// `engine_free` call.
///
/// Returns `GURUKUL_ERR_INVALID_HANDLE` if `engine` is null, `handle` is
/// `GURUKUL_INVALID_PORT`, or `handle` is out of range for this engine.
#[unsafe(no_mangle)]
pub extern "C" fn engine_in_port(
    engine: *mut GurukulEngine,
    handle: u32,
    out_ptr: *mut *mut f32,
    out_len: *mut usize,
) -> i32 {
    let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
        if engine.is_null() {
            error::set_last_error("engine pointer is null");
            return GURUKUL_ERR_INVALID_HANDLE;
        }
        if handle == GURUKUL_INVALID_PORT {
            error::set_last_error("handle is GURUKUL_INVALID_PORT");
            return GURUKUL_ERR_INVALID_HANDLE;
        }
        if out_ptr.is_null() || out_len.is_null() {
            error::set_last_error("out_ptr or out_len is null");
            return GURUKUL_ERR_INVALID_HANDLE;
        }

        // SAFETY: pointer came from engine_build and has not been freed.
        let num = unsafe { (*engine).engine.in_port_specs().len() } as u32;
        if handle >= num {
            error::set_last_error(&format!(
                "in-port handle {handle} out of range (have {num} in-ports)"
            ));
            return GURUKUL_ERR_INVALID_HANDLE;
        }

        // SAFETY: pointer came from engine_build and has not been freed; handle
        // is bounds-checked above.
        let slice = unsafe { (*engine).engine.in_port(InPortHandle::from_raw(handle)) };
        // SAFETY: out_ptr and out_len are non-null (checked above).
        unsafe {
            *out_ptr = slice.as_mut_ptr();
            *out_len = slice.len();
        }
        GURUKUL_OK
    }));
    result.unwrap_or_else(|_| {
        error::set_last_error("panic in engine_in_port");
        GURUKUL_ERR_UNKNOWN
    })
}

/// Get a read-only pointer to the boundary output buffer for `handle`.
///
/// On success: `*out_ptr` points to `const float[*out_len]` and `0` is
/// returned. The host reads processed audio from this buffer after calling
/// `engine_process_block`.
///
/// The slice length equals the `n_frames` passed to the most recent
/// `engine_process_block` call (0 before any call).
///
/// The pointer is valid until the next `engine_process_block` or
/// `engine_free` call.
///
/// Returns `GURUKUL_ERR_INVALID_HANDLE` if `engine` is null, `handle` is
/// `GURUKUL_INVALID_PORT`, or `handle` is out of range for this engine.
#[unsafe(no_mangle)]
pub extern "C" fn engine_out_port(
    engine: *const GurukulEngine,
    handle: u32,
    out_ptr: *mut *const f32,
    out_len: *mut usize,
) -> i32 {
    let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
        if engine.is_null() {
            error::set_last_error("engine pointer is null");
            return GURUKUL_ERR_INVALID_HANDLE;
        }
        if handle == GURUKUL_INVALID_PORT {
            error::set_last_error("handle is GURUKUL_INVALID_PORT");
            return GURUKUL_ERR_INVALID_HANDLE;
        }
        if out_ptr.is_null() || out_len.is_null() {
            error::set_last_error("out_ptr or out_len is null");
            return GURUKUL_ERR_INVALID_HANDLE;
        }

        // SAFETY: pointer came from engine_build and has not been freed.
        let num = unsafe { (*engine).engine.out_port_specs().len() } as u32;
        if handle >= num {
            error::set_last_error(&format!(
                "out-port handle {handle} out of range (have {num} out-ports)"
            ));
            return GURUKUL_ERR_INVALID_HANDLE;
        }

        // SAFETY: pointer came from engine_build and has not been freed; handle
        // is bounds-checked above.
        let slice = unsafe { (*engine).engine.out_port(OutPortHandle::from_raw(handle)) };
        // SAFETY: out_ptr and out_len are non-null (checked above).
        unsafe {
            *out_ptr = slice.as_ptr();
            *out_len = slice.len();
        }
        GURUKUL_OK
    }));
    result.unwrap_or_else(|_| {
        error::set_last_error("panic in engine_out_port");
        GURUKUL_ERR_UNKNOWN
    })
}

// ─── Hot path ────────────────────────────────────────────────────────────────

/// Process one block of `n_frames` audio samples.
///
/// `n_frames` must be ≤ the `block_size` passed to `engine_build`.
///
/// Returns `0` on success, `GURUKUL_ERR_BLOCK_TOO_BIG` if `n_frames` exceeds
/// `block_size`, or `GURUKUL_ERR_INVALID_HANDLE` if `engine` is null.
///
/// In a production build, passing an oversized block causes a debug assertion
/// inside the engine. This function gates on that before calling into the
/// engine so that release builds return an error code rather than exhibiting
/// undefined behaviour.
#[unsafe(no_mangle)]
pub extern "C" fn engine_process_block(engine: *mut GurukulEngine, n_frames: usize) -> i32 {
    let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
        if engine.is_null() {
            error::set_last_error("engine pointer is null");
            return GURUKUL_ERR_INVALID_HANDLE;
        }

        // SAFETY: pointer came from engine_build and has not been freed.
        let eng = unsafe { &mut (*engine).engine };

        if n_frames > eng.block_size() {
            error::set_last_error(&format!(
                "n_frames ({n_frames}) > block_size ({})",
                eng.block_size()
            ));
            return GURUKUL_ERR_BLOCK_TOO_BIG;
        }

        eng.process_block(n_frames);
        GURUKUL_OK
    }));
    result.unwrap_or_else(|_| {
        error::set_last_error("panic in engine_process_block");
        GURUKUL_ERR_UNKNOWN
    })
}

/// Reset all internal node state and zero boundary port buffers.
///
/// Call after an audio interruption (route change, phone call, OS-level
/// pause/resume) to prevent stale state from corrupting the next run.
/// This is NOT realtime-safe — call off the audio thread.
///
/// Always succeeds for a non-null engine. No-op if `engine` is null.
#[unsafe(no_mangle)]
pub extern "C" fn engine_reset(engine: *mut GurukulEngine) {
    if engine.is_null() {
        return;
    }
    let _ = std::panic::catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: pointer came from engine_build and has not been freed.
        unsafe { (*engine).engine.reset() };
    }));
}

// ─── Error reporting ─────────────────────────────────────────────────────────

/// Return a pointer to a thread-local null-terminated string describing the
/// most recent error set on this thread.
///
/// Returns `NULL` if no error has been recorded on this thread yet (or since
/// the last successful `engine_build`, which clears the slot). The message
/// may be stale — only `engine_build` clears on success. Always check the
/// return code of the call you made; consult this message only on failure.
///
/// The returned pointer is valid until the next FFI call on this thread that
/// sets a new error, or until thread exit. Never free this pointer.
#[unsafe(no_mangle)]
pub extern "C" fn engine_last_error_message() -> *const c_char {
    error::last_error_ptr()
}
