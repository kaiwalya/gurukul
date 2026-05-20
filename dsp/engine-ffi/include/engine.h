#ifndef GURUKUL_ENGINE_H
#define GURUKUL_ENGINE_H

/*
 * gurukul engine C ABI
 *
 * Stable C interface for driving the gurukul audio engine from any host
 * language (Swift, C, Kotlin, ...).  The ABI is treated as a public contract:
 * removing or changing a function signature is a versioned breaking change.
 *
 * Threading model
 * ---------------
 * Build-time / setup calls (engine_build, engine_resolve_*, engine_free,
 * engine_reset) may touch the thread-local error state and should be called
 * from a non-realtime thread.
 *
 * Audio-thread calls (engine_in_port, engine_out_port, engine_read_port,
 * engine_process_block) are realtime-safe when called between
 * engine_process_block calls: no allocation, no locks.  engine_in_port and
 * engine_out_port avoid string lookup (handle-based); engine_read_port does
 * a name lookup on each call and is only intended for ports addressed by
 * path (debug UI, scopes).  These may still return error codes for
 * programmer errors (null handle, oversized block), which the host should
 * treat as fatal bugs caught during development.
 *
 * Enumeration calls (engine_node_ids, engine_out_port_names) are NOT
 * realtime-safe: intended for build-time or picker-open use.
 *
 * Error reporting
 * ---------------
 * Functions that can fail return int32_t.  On failure they also write a
 * human-readable message to a thread-local buffer retrievable via
 * engine_last_error_message().
 */

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/* ── Opaque handle ────────────────────────────────────────────────────────── */

/**
 * Opaque engine handle.  Obtain via engine_build; release via engine_free.
 * Never allocate or copy this struct — always use the pointer.
 */
typedef struct GurukulEngine GurukulEngine;

/* ── Port handles ─────────────────────────────────────────────────────────── */

/** Handle to a boundary input port.  Resolved once at build time. */
typedef uint32_t GurukulInPort;

/** Handle to a boundary output port.  Resolved once at build time. */
typedef uint32_t GurukulOutPort;

/**
 * Sentinel value returned by engine_resolve_in_port / engine_resolve_out_port
 * when the requested id is not found.  Check for this before passing the
 * handle to engine_in_port / engine_out_port.
 */
#define GURUKUL_INVALID_PORT UINT32_MAX

/* ── Error codes ──────────────────────────────────────────────────────────── */

/** Success. */
#define GURUKUL_OK                  0

/** Generic / unknown error.  Check engine_last_error_message for details. */
#define GURUKUL_ERR_UNKNOWN        -1

/**
 * Invalid handle: engine pointer is NULL, or a port handle is
 * GURUKUL_INVALID_PORT, or an out-parameter pointer is NULL.
 */
#define GURUKUL_ERR_INVALID_HANDLE -2

/**
 * World build failed: JSON parse error or graph validation error.
 * engine_last_error_message() provides details.
 */
#define GURUKUL_ERR_BUILD_FAILED   -3

/** The requested port id was not found. */
#define GURUKUL_ERR_NOT_FOUND      -4

/** n_frames passed to engine_process_block exceeds block_size. */
#define GURUKUL_ERR_BLOCK_TOO_BIG  -5

/* ── Lifecycle ────────────────────────────────────────────────────────────── */

/**
 * Build and initialise an engine from a World JSON string.
 *
 * On success  : *out_engine is set to a freshly allocated engine handle and
 *               GURUKUL_OK (0) is returned.
 * On failure  : *out_engine is set to NULL, a negative error code is returned,
 *               and engine_last_error_message() returns a human-readable
 *               explanation.
 *
 * The caller must call engine_free(*out_engine) exactly once when done.
 *
 * Parameters
 *   world_json   Null-terminated UTF-8 JSON string describing the World.
 *   sample_rate  Sample rate in Hz (e.g. 48000).
 *   block_size   Maximum block size in frames; buffers are pre-allocated to
 *                this size.  engine_process_block accepts any n <= block_size.
 *   out_engine   Receives the engine pointer on success, NULL on failure.
 *                Must not be NULL itself.
 */
int32_t engine_build(
    const char     *world_json,
    uint32_t        sample_rate,
    size_t          block_size,
    GurukulEngine **out_engine);

/**
 * Free an engine previously obtained from engine_build.
 *
 * Must be called exactly once per engine.  Passing NULL is a no-op.
 * After this call the pointer is invalid; do not dereference it.
 */
void engine_free(GurukulEngine *engine);

/* ── Introspection ────────────────────────────────────────────────────────── */

/** Return the sample rate the engine was built with, or 0 if engine is NULL. */
uint32_t engine_sample_rate(const GurukulEngine *engine);

/**
 * Return the maximum block size (in frames) the engine was built with,
 * or 0 if engine is NULL.
 */
size_t engine_block_size(const GurukulEngine *engine);

/** Return the number of boundary input ports, or 0 if engine is NULL. */
size_t engine_num_in_ports(const GurukulEngine *engine);

/** Return the number of boundary output ports, or 0 if engine is NULL. */
size_t engine_num_out_ports(const GurukulEngine *engine);

/**
 * Return the id of the boundary input port at index as a null-terminated
 * UTF-8 string.
 *
 * The returned pointer is into engine-owned memory and is valid until
 * engine_free is called.  Returns NULL if index is out of range or engine
 * is NULL.
 *
 * Note: name and description fields are available in the Rust API but are not
 * yet exposed through the C ABI (follow-up: add engine_in_port_name /
 * engine_in_port_description if needed by a future cabinet).
 */
const char *engine_in_port_id(const GurukulEngine *engine, size_t index);

/**
 * Return the id of the boundary output port at index as a null-terminated
 * UTF-8 string.
 *
 * The returned pointer is into engine-owned memory and is valid until
 * engine_free is called.  Returns NULL if index is out of range or engine
 * is NULL.
 */
const char *engine_out_port_id(const GurukulEngine *engine, size_t index);

/* ── Port resolution ──────────────────────────────────────────────────────── */

/**
 * Resolve a boundary input port id to a GurukulInPort handle.
 *
 * Returns GURUKUL_INVALID_PORT if not found or if engine / id is NULL.
 * Call this once at setup time; cache the handle for audio-thread use.
 * Do not call on the audio thread.
 */
GurukulInPort engine_resolve_in_port(const GurukulEngine *engine, const char *id);

/**
 * Resolve a boundary output port id to a GurukulOutPort handle.
 *
 * Returns GURUKUL_INVALID_PORT if not found or if engine / id is NULL.
 * Call this once at setup time; cache the handle for audio-thread use.
 * Do not call on the audio thread.
 */
GurukulOutPort engine_resolve_out_port(const GurukulEngine *engine, const char *id);

/* ── Runtime port enumeration ─────────────────────────────────────────────── */

/**
 * Fill `out` with up to `cap` pointers to node ids (in topological / process
 * order) and return the total number of nodes in the engine.
 *
 * If the return value is greater than `cap`, the caller's buffer was too small
 * and only the first `cap` entries were written.  Re-allocate and call again.
 * Passing out=NULL and cap=0 is a valid way to query the count.
 *
 * Returned pointers are into engine-owned memory and are valid until
 * engine_free is called.  Never free them.  Returns 0 if engine is NULL.
 *
 * NOT realtime-safe: intended for build-time or picker-open use (e.g.
 * populating a debug UI's node dropdown).  Do not call per audio callback.
 */
size_t engine_node_ids(
    const GurukulEngine  *engine,
    const char          **out,
    size_t                cap);

/**
 * Fill `out` with up to `cap` pointers to output port names of `node_id`
 * (in declaration order).  On success, writes the total port count to
 * *out_total and returns GURUKUL_OK.  If *out_total > cap the caller's
 * buffer was too small; re-allocate and call again.  Passing out=NULL and
 * cap=0 is a valid way to query the count (out_total still required).
 *
 * Same pointer lifetime as engine_node_ids (valid until engine_free).
 *
 * Returns GURUKUL_ERR_INVALID_HANDLE for null engine / node_id / out_total.
 * Returns GURUKUL_ERR_NOT_FOUND if node_id is not a recognised node in this
 * engine; in that case *out_total is not modified.
 *
 * NOT realtime-safe.
 */
int32_t engine_out_port_names(
    const GurukulEngine  *engine,
    const char           *node_id,
    const char          **out,
    size_t                cap,
    size_t               *out_total);

/* ── I/O buffer access ────────────────────────────────────────────────────── */

/**
 * Get a writable pointer to the boundary input buffer for handle.
 *
 * On success: *out_ptr points to float[*out_len] and GURUKUL_OK is returned.
 * *out_len equals the block_size the engine was built with — NOT the n_frames
 * that will be passed to engine_process_block.  When processing a partial
 * block (n_frames < block_size), write into only the first n_frames slots;
 * the rest of the buffer is ignored.
 *
 * Write audio data into this buffer before calling engine_process_block.
 *
 * The pointer is valid until the next engine_process_block or engine_free.
 *
 * Returns GURUKUL_ERR_INVALID_HANDLE if engine is NULL, handle is
 * GURUKUL_INVALID_PORT, handle is out of range, or out_ptr / out_len is NULL.
 */
int32_t engine_in_port(
    GurukulEngine  *engine,
    GurukulInPort   handle,
    float         **out_ptr,
    size_t         *out_len);

/**
 * Get a read-only pointer to the boundary output buffer for handle.
 *
 * On success: *out_ptr points to const float[*out_len] and GURUKUL_OK is
 * returned.  Read processed audio from this buffer after engine_process_block.
 *
 * *out_len equals the n_frames passed to the most recent engine_process_block
 * call (0 before any call).
 *
 * The pointer is valid until the next engine_process_block or engine_free.
 *
 * Returns GURUKUL_ERR_INVALID_HANDLE if engine is NULL, handle is
 * GURUKUL_INVALID_PORT, handle is out of range, or out_ptr / out_len is NULL.
 */
int32_t engine_out_port(
    const GurukulEngine  *engine,
    GurukulOutPort        handle,
    const float         **out_ptr,
    size_t               *out_len);

/**
 * Read the last block written to any node's output port, addressed by
 * (node_id, port) strings.
 *
 * On success: *out_ptr points to const float[*out_len] of the most-recent
 * block's samples and GURUKUL_OK is returned.  The engine copies frames
 * into the caller-provided destination buffer; once this call returns the
 * caller owns the bytes — subsequent engine_process_block / engine_reset /
 * engine_free activity will not affect them.
 *
 * dst_capacity should be at least engine_block_size(engine) so the whole
 * block fits.  A shorter buffer is permitted; the leading prefix is copied
 * and the rest of the block is silently dropped (use *written to detect).
 * *written is set to min(dst_capacity, last_block_n_frames).
 *
 * This is the prescriptive-border read API. No engine-owned pointer
 * escapes; callers do not need to obey a "valid until next process_block"
 * lifetime contract.
 *
 * Threading: realtime-safe ONLY when called from the audio thread between
 * engine_process_block calls.  Reading from a different thread races
 * against the audio thread's writes and will see torn data.  Cabinets
 * should push the resulting bytes into an SPSC slot here and let the UI
 * read the slot, never the engine.
 *
 * String lookup happens on every call (that is the point — read by name,
 * not by handle).  For a port consumed every block, prefer engine_out_port
 * with a pre-resolved handle.  Use engine_read_port for ports addressed by
 * name: debug UI selections, scopes, future subscription consumers.
 *
 * dst may be NULL only if dst_capacity is 0 (zero-capacity reads succeed
 * and set *written to 0).
 *
 * Returns GURUKUL_ERR_INVALID_HANDLE for null pointers (other than the
 * NULL-dst-with-zero-capacity case), or GURUKUL_ERR_NOT_FOUND if node_id
 * or port is not recognised.
 */
int32_t engine_read_port(
    const GurukulEngine  *engine,
    const char           *node_id,
    const char           *port,
    float                *dst,
    size_t                dst_capacity,
    size_t               *written);

/* ── Hot path ─────────────────────────────────────────────────────────────── */

/**
 * Process one block of n_frames audio samples.
 *
 * n_frames must be <= the block_size passed to engine_build.
 *
 * Returns GURUKUL_OK on success, GURUKUL_ERR_BLOCK_TOO_BIG if n_frames
 * exceeds block_size, or GURUKUL_ERR_INVALID_HANDLE if engine is NULL.
 *
 * Call sequence per audio callback:
 *   1. Write into engine_in_port buffers.
 *   2. Call engine_process_block(engine, n_frames).
 *   3. Read from engine_out_port buffers.
 */
int32_t engine_process_block(GurukulEngine *engine, size_t n_frames);

/**
 * Reset all internal node state and zero boundary port buffers.
 *
 * Call after an audio interruption (route change, phone call, OS-level
 * pause/resume) to prevent stale state from corrupting the next run.
 *
 * NOT realtime-safe — call from a non-audio thread (e.g. after stopping
 * AVAudioEngine, before restarting it).
 *
 * No-op if engine is NULL.
 */
void engine_reset(GurukulEngine *engine);

/* ── Error reporting ──────────────────────────────────────────────────────── */

/**
 * Return a pointer to a thread-local null-terminated string describing the
 * most recent error set on this thread.
 *
 * Returns NULL if no error has been recorded on this thread yet (or since the
 * last successful engine_build, which clears the slot).  This message may be
 * STALE — only engine_build clears the slot on success; other calls leave
 * a prior error in place.  Always inspect the return code from the call you
 * just made first; consult this message only when that return code indicates
 * failure.
 *
 * Lifetime: valid until the next FFI call on this thread that sets a new
 * error, or until thread exit.  Never free this pointer.
 */
const char *engine_last_error_message(void);

#ifdef __cplusplus
}
#endif

#endif /* GURUKUL_ENGINE_H */
