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

// ────────────────────────────────────────────────────────────────────────────
// PR 1.4.8.2: runtime port enumeration + engine_read_port
// ────────────────────────────────────────────────────────────────────────────

/// Two-node world used to exercise enumeration with cap < n:
///   SynthSine("src")  ──audio_out──►  Passthrough("pt").audio_in ──audio_out──► out
const TWO_NODE_WORLD_JSON: &str = r#"{
    "world_version": 1,
    "in_ports": [],
    "out_ports": [{ "id": "out" }],
    "nodes": [
        { "id": "src", "type": "SynthSine", "params": { "freq": 440.0, "amplitude": 0.5 } },
        { "id": "pt",  "type": "Passthrough" }
    ],
    "connections": [
        { "from": "src.audio_out", "to": "pt.audio_in" },
        { "from": "pt.audio_out",  "to": "out" }
    ]
}"#;

#[test]
fn smoke_node_ids_enumeration() {
    let world_json = cstr(WORLD_JSON);
    let mut engine_ptr: *mut engine_ffi::GurukulEngine = ptr::null_mut();
    let rc = engine_ffi::engine_build(world_json.as_ptr(), 48_000, 64, &mut engine_ptr);
    assert_eq!(rc, 0);

    // Count-only query.
    let n = engine_ffi::engine_node_ids(engine_ptr, ptr::null_mut(), 0);
    assert_eq!(n, 1, "WORLD_JSON has one node");

    // Buffer exactly large enough.
    let mut tiny: [*const std::os::raw::c_char; 1] = [ptr::null()];
    let n2 = engine_ffi::engine_node_ids(engine_ptr, tiny.as_mut_ptr(), 1);
    assert_eq!(n2, 1);
    assert!(!tiny[0].is_null());
    // SAFETY: pointer is engine-owned, valid until engine_free.
    let id = unsafe { CStr::from_ptr(tiny[0]) }.to_str().unwrap();
    assert_eq!(id, "src");

    engine_ffi::engine_free(engine_ptr);
}

#[test]
fn smoke_node_ids_truncation_writes_first_cap_entries() {
    // Two-node world; cap=1 must write the first id and still return total=2.
    let world_json = cstr(TWO_NODE_WORLD_JSON);
    let mut engine_ptr: *mut engine_ffi::GurukulEngine = ptr::null_mut();
    let rc = engine_ffi::engine_build(world_json.as_ptr(), 48_000, 64, &mut engine_ptr);
    assert_eq!(rc, 0);

    // Count-only confirms the world has 2 nodes.
    let total = engine_ffi::engine_node_ids(engine_ptr, ptr::null_mut(), 0);
    assert_eq!(total, 2);

    // Sentinel after the first slot — must not be overwritten when cap=1.
    let sentinel: *const std::os::raw::c_char = 0xDEAD_BEEF as *const _;
    let mut buf: [*const std::os::raw::c_char; 2] = [ptr::null(), sentinel];
    let n = engine_ffi::engine_node_ids(engine_ptr, buf.as_mut_ptr(), 1);
    assert_eq!(n, 2, "total count returned even when cap < total");
    assert!(!buf[0].is_null(), "first slot must be written");
    assert_eq!(
        buf[1], sentinel,
        "out-of-range slots must NOT be touched when cap < total"
    );

    engine_ffi::engine_free(engine_ptr);
}

#[test]
fn smoke_out_port_names_enumeration() {
    let world_json = cstr(WORLD_JSON);
    let mut engine_ptr: *mut engine_ffi::GurukulEngine = ptr::null_mut();
    let rc = engine_ffi::engine_build(world_json.as_ptr(), 48_000, 64, &mut engine_ptr);
    assert_eq!(rc, 0);

    // SynthSine has one output: audio_out.
    let node_id = cstr("src");
    let mut total: usize = 999;
    let rc = engine_ffi::engine_out_port_names(
        engine_ptr,
        node_id.as_ptr(),
        ptr::null_mut(),
        0,
        &mut total,
    );
    assert_eq!(rc, 0);
    assert_eq!(total, 1);

    let mut buf: [*const std::os::raw::c_char; 4] = [ptr::null(); 4];
    let mut total2: usize = 0;
    let rc2 = engine_ffi::engine_out_port_names(
        engine_ptr,
        node_id.as_ptr(),
        buf.as_mut_ptr(),
        4,
        &mut total2,
    );
    assert_eq!(rc2, 0);
    assert_eq!(total2, 1);
    // SAFETY: pointer is engine-owned, valid until engine_free.
    let name = unsafe { CStr::from_ptr(buf[0]) }.to_str().unwrap();
    assert_eq!(name, "audio_out");

    // Unknown node returns GURUKUL_ERR_NOT_FOUND; *out_total is not modified.
    let bogus = cstr("nope");
    let mut total3: usize = 999;
    let rc3 = engine_ffi::engine_out_port_names(
        engine_ptr,
        bogus.as_ptr(),
        ptr::null_mut(),
        0,
        &mut total3,
    );
    assert_eq!(rc3, -4, "GURUKUL_ERR_NOT_FOUND for unknown node");
    assert_eq!(total3, 999, "*out_total must not be modified on not-found");
    let msg_ptr = engine_ffi::engine_last_error_message();
    assert!(!msg_ptr.is_null());

    engine_ffi::engine_free(engine_ptr);
}

#[test]
fn smoke_enumeration_pointers_survive_process_block() {
    // The cached CString pointers must remain valid (same address, same bytes)
    // across engine_process_block calls — that is the contract the header
    // claims ("valid until engine_free").
    let world_json = cstr(WORLD_JSON);
    let mut engine_ptr: *mut engine_ffi::GurukulEngine = ptr::null_mut();
    let rc = engine_ffi::engine_build(world_json.as_ptr(), 48_000, 64, &mut engine_ptr);
    assert_eq!(rc, 0);

    let mut nodes: [*const std::os::raw::c_char; 1] = [ptr::null()];
    let _ = engine_ffi::engine_node_ids(engine_ptr, nodes.as_mut_ptr(), 1);
    let before = nodes[0];
    assert!(!before.is_null());

    // SAFETY: pointer valid until engine_free; copy the bytes for later cmp.
    let before_bytes = unsafe { CStr::from_ptr(before) }.to_bytes().to_vec();

    let rc = engine_ffi::engine_process_block(engine_ptr, 64);
    assert_eq!(rc, 0);

    // Re-enumerate and confirm pointer identity + contents survive.
    let mut nodes2: [*const std::os::raw::c_char; 1] = [ptr::null()];
    let _ = engine_ffi::engine_node_ids(engine_ptr, nodes2.as_mut_ptr(), 1);
    assert_eq!(nodes2[0], before, "cached pointer must be the same address");
    let after_bytes = unsafe { CStr::from_ptr(nodes2[0]) }.to_bytes();
    assert_eq!(after_bytes, before_bytes.as_slice());

    engine_ffi::engine_free(engine_ptr);
}

#[test]
fn smoke_read_port_round_trips_against_out_port() {
    // engine_read_port must produce the same samples as engine_out_port for
    // the same underlying buffer when called immediately after process_block.
    let world_json = cstr(WORLD_JSON);
    let mut engine_ptr: *mut engine_ffi::GurukulEngine = ptr::null_mut();
    let rc = engine_ffi::engine_build(world_json.as_ptr(), 48_000, 64, &mut engine_ptr);
    assert_eq!(rc, 0);

    // Run one block.
    let rc = engine_ffi::engine_process_block(engine_ptr, 64);
    assert_eq!(rc, 0);

    // Read via engine_read_port — caller provides the destination buffer.
    let node = cstr("src");
    let port = cstr("audio_out");
    let mut read_buf = vec![0.0f32; 64];
    let mut written: usize = 0;
    let rc = engine_ffi::engine_read_port(
        engine_ptr,
        node.as_ptr(),
        port.as_ptr(),
        read_buf.as_mut_ptr(),
        read_buf.len(),
        &mut written,
    );
    assert_eq!(rc, 0, "engine_read_port must return GURUKUL_OK");
    assert_eq!(
        written, 64,
        "written must match the n_frames just processed"
    );

    // Read the boundary-mapped same port via engine_out_port for comparison.
    let out_id = cstr("out");
    let h_out = engine_ffi::engine_resolve_out_port(engine_ptr, out_id.as_ptr());
    assert_ne!(h_out, GURUKUL_INVALID_PORT);
    let mut bnd_ptr: *const f32 = ptr::null();
    let mut bnd_len: usize = 0;
    let rc = engine_ffi::engine_out_port(engine_ptr, h_out, &mut bnd_ptr, &mut bnd_len);
    assert_eq!(rc, 0);

    // SAFETY: engine_out_port pointer is valid until next process_block.
    let bnd_slice = unsafe { std::slice::from_raw_parts(bnd_ptr, bnd_len) };
    assert_eq!(
        &read_buf[..written],
        bnd_slice,
        "engine_read_port(src.audio_out) must equal engine_out_port(out) — \
         they address the same underlying buffer"
    );

    engine_ffi::engine_free(engine_ptr);
}

#[test]
fn smoke_read_port_short_dst_truncates() {
    // dst shorter than block size: the leading prefix is copied, written
    // reflects what fit. This is the documented partial-copy behaviour.
    let world_json = cstr(WORLD_JSON);
    let mut engine_ptr: *mut engine_ffi::GurukulEngine = ptr::null_mut();
    let rc = engine_ffi::engine_build(world_json.as_ptr(), 48_000, 64, &mut engine_ptr);
    assert_eq!(rc, 0);

    let _ = engine_ffi::engine_process_block(engine_ptr, 64);

    let node = cstr("src");
    let port = cstr("audio_out");
    let mut short_buf = vec![0.0f32; 16];
    let mut written: usize = 0;
    let rc = engine_ffi::engine_read_port(
        engine_ptr,
        node.as_ptr(),
        port.as_ptr(),
        short_buf.as_mut_ptr(),
        short_buf.len(),
        &mut written,
    );
    assert_eq!(rc, 0);
    assert_eq!(
        written, 16,
        "short dst should receive its capacity, no more"
    );

    engine_ffi::engine_free(engine_ptr);
}

#[test]
fn smoke_read_port_unknown_returns_not_found() {
    let world_json = cstr(WORLD_JSON);
    let mut engine_ptr: *mut engine_ffi::GurukulEngine = ptr::null_mut();
    let rc = engine_ffi::engine_build(world_json.as_ptr(), 48_000, 64, &mut engine_ptr);
    assert_eq!(rc, 0);

    // Process one block so there's a buffer to read.
    let _ = engine_ffi::engine_process_block(engine_ptr, 64);

    let bogus_node = cstr("nope");
    let port = cstr("audio_out");
    let mut buf = vec![0.0f32; 64];
    let mut written: usize = 0;
    let rc = engine_ffi::engine_read_port(
        engine_ptr,
        bogus_node.as_ptr(),
        port.as_ptr(),
        buf.as_mut_ptr(),
        buf.len(),
        &mut written,
    );
    assert_eq!(
        rc, -4,
        "expected GURUKUL_ERR_NOT_FOUND (-4) for unknown node"
    );

    let good_node = cstr("src");
    let bogus_port = cstr("no_such_port");
    let rc2 = engine_ffi::engine_read_port(
        engine_ptr,
        good_node.as_ptr(),
        bogus_port.as_ptr(),
        buf.as_mut_ptr(),
        buf.len(),
        &mut written,
    );
    assert_eq!(
        rc2, -4,
        "expected GURUKUL_ERR_NOT_FOUND for unknown port on a known node"
    );

    engine_ffi::engine_free(engine_ptr);
}

#[test]
fn smoke_read_port_null_args_return_invalid_handle() {
    // All null-pointer paths must return GURUKUL_ERR_INVALID_HANDLE without
    // crashing. Each variant exercises a different null check.
    let world_json = cstr(WORLD_JSON);
    let mut engine_ptr: *mut engine_ffi::GurukulEngine = ptr::null_mut();
    let rc = engine_ffi::engine_build(world_json.as_ptr(), 48_000, 64, &mut engine_ptr);
    assert_eq!(rc, 0);

    let node = cstr("src");
    let port = cstr("audio_out");
    let mut buf = vec![0.0f32; 64];
    let mut written: usize = 0;

    // 1. null engine.
    let rc1 = engine_ffi::engine_read_port(
        ptr::null(),
        node.as_ptr(),
        port.as_ptr(),
        buf.as_mut_ptr(),
        buf.len(),
        &mut written,
    );
    assert_eq!(rc1, -2);

    // 2. null node_id.
    let rc2 = engine_ffi::engine_read_port(
        engine_ptr,
        ptr::null(),
        port.as_ptr(),
        buf.as_mut_ptr(),
        buf.len(),
        &mut written,
    );
    assert_eq!(rc2, -2);

    // 3. null port.
    let rc3 = engine_ffi::engine_read_port(
        engine_ptr,
        node.as_ptr(),
        ptr::null(),
        buf.as_mut_ptr(),
        buf.len(),
        &mut written,
    );
    assert_eq!(rc3, -2);

    // 4. null dst with non-zero capacity.
    let rc4 = engine_ffi::engine_read_port(
        engine_ptr,
        node.as_ptr(),
        port.as_ptr(),
        ptr::null_mut(),
        64,
        &mut written,
    );
    assert_eq!(rc4, -2);

    // 5. null written.
    let rc5 = engine_ffi::engine_read_port(
        engine_ptr,
        node.as_ptr(),
        port.as_ptr(),
        buf.as_mut_ptr(),
        buf.len(),
        ptr::null_mut(),
    );
    assert_eq!(rc5, -2);

    // 6. null dst with zero capacity is allowed (caller asked for nothing).
    let rc6 = engine_ffi::engine_read_port(
        engine_ptr,
        node.as_ptr(),
        port.as_ptr(),
        ptr::null_mut(),
        0,
        &mut written,
    );
    assert_eq!(rc6, 0, "null dst with capacity 0 must succeed");
    assert_eq!(written, 0, "zero-capacity read writes nothing");

    engine_ffi::engine_free(engine_ptr);
}
