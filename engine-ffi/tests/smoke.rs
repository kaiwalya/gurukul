//! Smoke test for the engine-ffi C ABI surface.
//!
//! Exercises the complete lifecycle via the Rust-callable FFI functions
//! (possible because `engine-ffi` includes `rlib` in `crate-type`):
//!   build → introspect → resolve → process → read → free
//!
//! # DLL-backed SubEngineNode walk-through
//!
//! A hypothetical DLL-backed SubEngineNode that wraps a child engine would
//! call the FFI surface as follows.  Every call listed below is exposed by
//! `engine-ffi`.
//!
//! ```text
//! SubEngineNode::new(world_json, sr, block_size):
//!   engine_build(world_json, sr, block_size, &eng)          // ✓ exposed
//!   engine_sample_rate(eng)                                  // ✓ exposed
//!   engine_block_size(eng)                                   // ✓ exposed
//!   for i in 0..engine_num_in_ports(eng):                   // ✓ exposed
//!       id = engine_in_port_id(eng, i)                      // ✓ exposed
//!       h  = engine_resolve_in_port(eng, id)                // ✓ exposed
//!   for i in 0..engine_num_out_ports(eng):                  // ✓ exposed
//!       id = engine_out_port_id(eng, i)                     // ✓ exposed
//!       h  = engine_resolve_out_port(eng, id)               // ✓ exposed
//!
//! SubEngineNode::process(inputs, outputs, n_frames):
//!   for each (input_slice, h_in):
//!       engine_in_port(eng, h_in, &ptr, &len)               // ✓ exposed
//!       memcpy(ptr, input_slice.data, n_frames * 4)
//!   engine_process_block(eng, n_frames)                      // ✓ exposed
//!   for each (output_slice, h_out):
//!       engine_out_port(eng, h_out, &ptr, &len)             // ✓ exposed
//!       memcpy(output_slice.data, ptr, n_frames * 4)
//!
//! SubEngineNode::reset():
//!   engine_reset(eng)                                        // ✓ exposed
//!
//! Drop:
//!   engine_free(eng)                                         // ✓ exposed
//! ```
//!
//! All required FFI calls are covered.  The full list: `engine_build`,
//! `engine_free`, `engine_reset`, `engine_sample_rate`, `engine_block_size`,
//! `engine_num_in_ports`, `engine_num_out_ports`, `engine_in_port_id`,
//! `engine_out_port_id`, `engine_resolve_in_port`, `engine_resolve_out_port`,
//! `engine_in_port`, `engine_out_port`, `engine_process_block`.

use engine_ffi::GURUKUL_INVALID_PORT;
use std::ffi::{CStr, CString};
use std::ptr;

/// Minimal World JSON: a SynthSine node wired to a boundary out-port "out".
///
/// Uses `world_version: 1` and the current schema shape (out_ports declared
/// at top level, edges use bare ids for boundary ports).
const WORLD_JSON: &str = r#"{
    "world_version": 1,
    "in_ports": [],
    "out_ports": [
        { "id": "out" }
    ],
    "nodes": [
        { "id": "src", "type": "SynthSine", "params": { "freq": 440.0, "amplitude": 0.5 } }
    ],
    "connections": [
        { "from": "src.audio_out", "to": "out" }
    ]
}"#;

/// World with a passthrough node: in-port "mic" → Passthrough → out-port "out".
/// Passthrough's ports are named "audio_in" and "audio_out".
const PASSTHROUGH_WORLD_JSON: &str = r#"{
    "world_version": 1,
    "in_ports":  [{ "id": "mic" }],
    "out_ports": [{ "id": "out" }],
    "nodes": [
        { "id": "pt", "type": "Passthrough" }
    ],
    "connections": [
        { "from": "mic",         "to": "pt.audio_in" },
        { "from": "pt.audio_out","to": "out"          }
    ]
}"#;

/// Helper: convert a Rust `&str` to a temporary `CString` for FFI calls.
fn cstr(s: &str) -> CString {
    CString::new(s).expect("test string must not contain interior NUL")
}

#[test]
fn smoke_build_process_read_free() {
    // ── Step 1: build ─────────────────────────────────────────────────────
    let world_json = cstr(WORLD_JSON);
    let sample_rate: u32 = 48_000;
    let block_size: usize = 64;

    let mut engine_ptr: *mut engine_ffi::GurukulEngine = ptr::null_mut();
    let rc = engine_ffi::engine_build(
        world_json.as_ptr(),
        sample_rate,
        block_size,
        &mut engine_ptr,
    );
    assert_eq!(rc, 0, "engine_build must return GURUKUL_OK");
    assert!(
        !engine_ptr.is_null(),
        "engine pointer must be non-null after successful build"
    );

    // ── Step 2: introspect sample_rate / block_size ───────────────────────
    let sr = engine_ffi::engine_sample_rate(engine_ptr);
    assert_eq!(sr, sample_rate, "engine_sample_rate round-trip");

    let bs = engine_ffi::engine_block_size(engine_ptr);
    assert_eq!(bs, block_size, "engine_block_size round-trip");

    // ── Step 3: introspect out-ports ─────────────────────────────────────
    let n_out = engine_ffi::engine_num_out_ports(engine_ptr);
    assert_eq!(n_out, 1, "expected 1 out-port");

    let n_in = engine_ffi::engine_num_in_ports(engine_ptr);
    assert_eq!(n_in, 0, "expected 0 in-ports");

    let raw_id = engine_ffi::engine_out_port_id(engine_ptr, 0);
    assert!(
        !raw_id.is_null(),
        "engine_out_port_id must return non-null for index 0"
    );
    // SAFETY: pointer is engine-owned, valid until engine_free.
    let id_str = unsafe { CStr::from_ptr(raw_id) }
        .to_str()
        .expect("port id must be valid UTF-8");
    assert_eq!(
        id_str, "out",
        "out-port id must match the world declaration"
    );

    // ── Step 4: resolve out-port ─────────────────────────────────────────
    let out_id = cstr("out");
    let out_handle = engine_ffi::engine_resolve_out_port(engine_ptr, out_id.as_ptr());
    assert_ne!(
        out_handle, GURUKUL_INVALID_PORT,
        "engine_resolve_out_port must find 'out'"
    );

    // ── Step 5: process one block ─────────────────────────────────────────
    let rc = engine_ffi::engine_process_block(engine_ptr, block_size);
    assert_eq!(rc, 0, "engine_process_block must return GURUKUL_OK");

    // ── Step 6: read output buffer ────────────────────────────────────────
    let mut out_ptr: *const f32 = ptr::null();
    let mut out_len: usize = 0;
    let rc = engine_ffi::engine_out_port(engine_ptr, out_handle, &mut out_ptr, &mut out_len);
    assert_eq!(rc, 0, "engine_out_port must return GURUKUL_OK");
    assert!(!out_ptr.is_null(), "out_ptr must be non-null");
    assert_eq!(out_len, block_size, "out_len must equal n_frames processed");

    // SAFETY: out_ptr points to engine-owned memory, valid until next
    // process_block or engine_free.
    let samples = unsafe { std::slice::from_raw_parts(out_ptr, out_len) };
    assert!(
        samples.iter().any(|&s| s.abs() > 1e-6),
        "at least one sample must be non-zero (SynthSine at 440 Hz, amplitude 0.5)"
    );
    assert!(
        samples.iter().all(|s| s.is_finite()),
        "all samples must be finite"
    );

    // ── Step 7: free ──────────────────────────────────────────────────────
    engine_ffi::engine_free(engine_ptr);
    // engine_ptr is now dangling — do not use it after this line.
}

#[test]
fn smoke_invalid_port_sentinel() {
    let world_json = cstr(WORLD_JSON);
    let mut engine_ptr: *mut engine_ffi::GurukulEngine = ptr::null_mut();
    let rc = engine_ffi::engine_build(world_json.as_ptr(), 48_000, 64, &mut engine_ptr);
    assert_eq!(rc, 0);

    // Resolving a non-existent port must return GURUKUL_INVALID_PORT.
    let bad_id = cstr("nope");
    let handle = engine_ffi::engine_resolve_out_port(engine_ptr, bad_id.as_ptr());
    assert_eq!(
        handle, GURUKUL_INVALID_PORT,
        "unknown id must yield GURUKUL_INVALID_PORT"
    );

    // Also check the in-port direction.
    let handle2 = engine_ffi::engine_resolve_in_port(engine_ptr, bad_id.as_ptr());
    assert_eq!(handle2, GURUKUL_INVALID_PORT);

    engine_ffi::engine_free(engine_ptr);
}

#[test]
fn smoke_build_failure_sets_last_error() {
    // Passing malformed JSON must return GURUKUL_ERR_BUILD_FAILED and set
    // the last-error message.
    let bad_json = cstr("{this is not valid json}");
    let mut engine_ptr: *mut engine_ffi::GurukulEngine = ptr::null_mut();
    let rc = engine_ffi::engine_build(bad_json.as_ptr(), 48_000, 64, &mut engine_ptr);
    assert!(
        rc < 0,
        "build from bad JSON must return a negative error code"
    );
    assert!(
        engine_ptr.is_null(),
        "engine pointer must be null on failure"
    );

    let msg_ptr = engine_ffi::engine_last_error_message();
    assert!(
        !msg_ptr.is_null(),
        "last-error message must be set after build failure"
    );
    // SAFETY: pointer is thread-local, valid until next FFI call on this thread.
    let msg = unsafe { CStr::from_ptr(msg_ptr) }
        .to_str()
        .expect("message must be UTF-8");
    assert!(!msg.is_empty(), "last-error message must not be empty");
}

#[test]
fn smoke_block_too_big() {
    let world_json = cstr(WORLD_JSON);
    let mut engine_ptr: *mut engine_ffi::GurukulEngine = ptr::null_mut();
    let rc = engine_ffi::engine_build(world_json.as_ptr(), 48_000, 64, &mut engine_ptr);
    assert_eq!(rc, 0);

    // Passing n_frames > block_size must return GURUKUL_ERR_BLOCK_TOO_BIG.
    let rc = engine_ffi::engine_process_block(engine_ptr, 128);
    assert_eq!(rc, -5, "expected GURUKUL_ERR_BLOCK_TOO_BIG (-5)");

    engine_ffi::engine_free(engine_ptr);
}

#[test]
fn smoke_reset_zeros_state() {
    // Build a world with an in-port so we can write data in, process, reset,
    // then confirm the output is zeroed.
    let world_json = cstr(PASSTHROUGH_WORLD_JSON);
    let mut engine_ptr: *mut engine_ffi::GurukulEngine = ptr::null_mut();
    let rc = engine_ffi::engine_build(world_json.as_ptr(), 48_000, 64, &mut engine_ptr);
    assert_eq!(rc, 0, "passthrough world build failed");

    let mic_id = cstr("mic");
    let out_id = cstr("out");
    let h_in = engine_ffi::engine_resolve_in_port(engine_ptr, mic_id.as_ptr());
    let h_out = engine_ffi::engine_resolve_out_port(engine_ptr, out_id.as_ptr());
    assert_ne!(h_in, GURUKUL_INVALID_PORT);
    assert_ne!(h_out, GURUKUL_INVALID_PORT);

    // Write non-zero data and process.
    {
        let mut in_ptr: *mut f32 = ptr::null_mut();
        let mut in_len: usize = 0;
        let rc = engine_ffi::engine_in_port(engine_ptr, h_in, &mut in_ptr, &mut in_len);
        assert_eq!(rc, 0);
        // SAFETY: in_ptr points to engine-owned memory, valid until next process_block.
        let buf = unsafe { std::slice::from_raw_parts_mut(in_ptr, in_len) };
        buf.fill(1.0);
    }
    let rc = engine_ffi::engine_process_block(engine_ptr, 64);
    assert_eq!(rc, 0);

    // Confirm output is non-zero.
    {
        let mut out_ptr: *const f32 = ptr::null();
        let mut out_len: usize = 0;
        let rc = engine_ffi::engine_out_port(engine_ptr, h_out, &mut out_ptr, &mut out_len);
        assert_eq!(rc, 0);
        // SAFETY: out_ptr points to engine-owned memory, valid until next process_block.
        let samples = unsafe { std::slice::from_raw_parts(out_ptr, out_len) };
        assert!(
            samples.iter().any(|&s| s.abs() > 0.5),
            "output must be non-zero before reset"
        );
    }

    // Reset and confirm output is zeroed.
    engine_ffi::engine_reset(engine_ptr);
    {
        let mut out_ptr: *const f32 = ptr::null();
        let mut out_len: usize = 0;
        let rc = engine_ffi::engine_out_port(engine_ptr, h_out, &mut out_ptr, &mut out_len);
        assert_eq!(rc, 0);
        // SAFETY: out_ptr points to engine-owned memory, valid until next process_block.
        let samples = unsafe { std::slice::from_raw_parts(out_ptr, out_len) };
        assert!(
            samples.iter().all(|&s| s == 0.0),
            "output must be zeroed after reset"
        );
    }

    engine_ffi::engine_free(engine_ptr);
}

#[test]
fn smoke_null_engine_returns_error() {
    // Passing NULL for the engine pointer must return an error, not crash.
    let rc = engine_ffi::engine_process_block(ptr::null_mut(), 64);
    assert!(rc < 0, "null engine must return a negative error code");

    let mut dummy_ptr: *mut f32 = ptr::null_mut();
    let mut dummy_len: usize = 0;
    let rc2 = engine_ffi::engine_in_port(ptr::null_mut(), 0, &mut dummy_ptr, &mut dummy_len);
    assert!(rc2 < 0);
}
