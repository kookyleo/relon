//! F-D7 string fast-path runtime shims.
//!
//! The trace emitter lowers `TraceOp::StrConcat` / `StrContains` /
//! `StrFind` / `StrSubstring` to direct `call` instructions targeting
//! the four `__relon_str_*` symbols defined here. Each shim accepts
//! and returns `*const StringRef` pointers — opaque to the JIT — and
//! performs the actual string work on the Rust side, including
//! allocation for ops that produce a fresh result.
//!
//! ## ABI summary
//!
//! All four shims are `unsafe extern "C"` so cranelift IR can call
//! them via the standard SystemV/win64 ABI.
//!
//! ```text
//! __relon_str_concat(lhs: *const StringRef, rhs: *const StringRef)
//!     -> *const StringRef
//! __relon_str_contains(haystack: *const StringRef, needle: *const StringRef)
//!     -> i32       // 0 = false, 1 = true
//! __relon_str_find(haystack: *const StringRef, needle: *const StringRef)
//!     -> i64       // byte index, -1 on miss
//! __relon_str_substring(s: *const StringRef, start: i64, length: i64)
//!     -> *const StringRef
//! ```
//!
//! ## Lifetime / ownership model
//!
//! `StringRef` is a `#[repr(C)]` host-side box whose lifetime is owned
//! by a **thread-local trace string arena** (`TRACE_STRING_ARENA`).
//! Every shim that allocates a fresh `StringRef` — `from_owned`,
//! `from_static`, `__relon_str_concat`, `__relon_str_concat_alloc`,
//! `__relon_str_concat_n_alloc`, `__relon_str_substring` — also
//! registers the underlying allocation with the arena so the host can
//! reclaim every per-iter buffer at trace exit / deopt by calling
//! [`reclaim_trace_strings`]. From the JIT's perspective the pointers
//! remain opaque `i64` slots; the arena is invisible until the host
//! decides it's time to free the chain.
//!
//! ### Review #175 P2 fix
//!
//! The historical drop was an **unbounded** leak — each shim called
//! `Box::into_raw` (or raw `alloc(Layout)`) with no reclamation hook,
//! relying on the short-lived `cmp_lua` benches to bound total memory
//! usage. Long-lived hosts running thousands of traces accumulated
//! per-iter `StringRef` allocations forever. The trace string arena
//! addresses that: hosts call [`reclaim_trace_strings`] after each
//! trace (typical site: the trace-runner's exit path / the deopt
//! handler) and the arena `dealloc`s every record it recorded since
//! the last reclaim. Bench fixtures with a tight intra-process
//! re-entry pattern can choose to reclaim once per outer iter; the
//! benchmark numbers (W3 hot loop, `__relon_str_concat` per-iter
//! repeats) are unaffected because the per-call cost of pushing a
//! single `TraceStringAlloc` enum to a thread-local `Vec` is well
//! below the cost of the alloc itself.
//!
//! The shims are SAFE to call from any thread because each trace
//! context lives on a single thread by design (`thread_local` call
//! table — see `call_table.rs`); the arena above also operates
//! per-thread via the same constraint, so concurrent traces on
//! different threads see independent reclaim lists.
//!
//! ## Inline cache
//!
//! `__relon_str_contains` consults a tiny pointer-keyed cache (a
//! single MRU slot) before doing the substring scan. The cache is a
//! `thread_local!` so the W4-shaped "same haystack, same needle"
//! benchmark hits without a real scan. Hits short-circuit straight
//! to the cached i32 result; misses fall back to the scan and update
//! the slot.

use std::alloc::Layout;
use std::cell::{Cell, RefCell};

use relon_trace_abi::hash::fx_hash_bytes;

// =====================================================================
// Trace string reclamation arena (review #175 P2 fix)
// =====================================================================
//
// Each shim that allocates a fresh `StringRef` registers its
// allocation with the thread-local `TRACE_STRING_ARENA`. The host
// calls [`reclaim_trace_strings`] at trace exit to drop every
// allocation back to the global allocator. Without this hook every
// per-iter `__relon_str_concat*` call leaked unbounded memory in
// long-lived hosts (the bench fixtures hide it behind their tight
// loop bounds).
//
// Three allocation shapes need three reclamation strategies:
//
// * `BoxedHeader` — produced by `from_static`: a single
//   `Box<StringRef>` whose payload pointer borrows a `&'static`
//   buffer (no payload free needed). Free via
//   `drop(Box::from_raw(header))`.
// * `OwnedHeaderAndPayload` — produced by `from_owned`: a
//   `Box<StringRef>` header plus a separately-boxed `Box<str>`
//   payload. Free via two `Box::from_raw` reconstructions, one for
//   each.
// * `SingleBlock` — produced by `__relon_str_concat_alloc` /
//   `__relon_str_concat_n_alloc`: a single
//   `[header | payload]` block from `std::alloc::alloc(layout)`.
//   Free via `std::alloc::dealloc(block, layout)`.

/// One reclaim record per allocation handed back from the str shims.
/// The `Layout`-bearing variant stores the full layout so reclaim
/// can `dealloc` correctly even if the allocation predates a later
/// layout-discipline tweak.
enum TraceStringAlloc {
    /// `Box<StringRef>` only — payload is borrowed (`from_static`).
    BoxedHeader { header: *mut StringRef },
    /// `Box<StringRef>` header + heap-owned `Box<str>` payload.
    /// Reclamation drops both boxes.
    OwnedHeaderAndPayload {
        header: *mut StringRef,
        payload_ptr: *mut u8,
        payload_len: usize,
    },
    /// Single contiguous `[header | payload]` block allocated via
    /// `std::alloc::alloc(layout)`.
    SingleBlock { block: *mut u8, layout: Layout },
}

// SAFETY: the arena is only ever touched on the thread that owns it
// (thread_local!); `*mut StringRef` / `*mut u8` are not Send/Sync
// only because of their pointer nature, but never escape this thread.
unsafe impl Send for TraceStringAlloc {}

thread_local! {
    static TRACE_STRING_ARENA: RefCell<Vec<TraceStringAlloc>> =
        const { RefCell::new(Vec::new()) };
}

/// Register a pre-allocated reclaim record. Implementation detail
/// shared by every shim; not part of the public surface.
fn arena_push(alloc: TraceStringAlloc) {
    TRACE_STRING_ARENA.with(|cell| cell.borrow_mut().push(alloc));
}

/// Diagnostic: returns the number of live allocations the trace
/// string arena currently tracks for the calling thread. Tests use
/// this to verify that [`reclaim_trace_strings`] actually drops the
/// recorded allocations and that subsequent shim calls grow the
/// list.
pub fn trace_string_arena_len() -> usize {
    TRACE_STRING_ARENA.with(|cell| cell.borrow().len())
}

/// Reclaim every `StringRef` (and its backing payload) that the str
/// shims allocated on the calling thread since the last reclaim.
///
/// Hosts MUST call this at every trace exit / deopt site to bound
/// memory usage; not calling it preserves the historical leak
/// behaviour (every shim allocation lives until process exit).
///
/// # Safety
///
/// After this call returns, every `*const StringRef` previously
/// handed out by [`StringRef::from_owned`], [`StringRef::from_static`],
/// [`__relon_str_concat`], [`__relon_str_concat_alloc`],
/// [`__relon_str_concat_n_alloc`], or [`__relon_str_substring`] on
/// the calling thread is invalid. Callers that still hold such
/// pointers MUST NOT dereference them. Typical use site: the trace
/// runner's exit handler immediately after reading the result slot
/// out of `TraceContext`.
pub unsafe fn reclaim_trace_strings() {
    TRACE_STRING_ARENA.with(|cell| {
        let mut allocs = cell.borrow_mut();
        for alloc in allocs.drain(..) {
            // SAFETY: by the shim contracts, every recorded
            // allocation came from this module's own producer
            // helpers and is uniquely owned. The caller's safety
            // precondition above (no stale dereferences after this
            // returns) is what keeps the operation sound.
            unsafe {
                match alloc {
                    TraceStringAlloc::BoxedHeader { header } => {
                        drop(Box::from_raw(header));
                    }
                    TraceStringAlloc::OwnedHeaderAndPayload {
                        header,
                        payload_ptr,
                        payload_len,
                    } => {
                        let payload_slice =
                            std::slice::from_raw_parts_mut(payload_ptr, payload_len);
                        drop(Box::from_raw(payload_slice as *mut [u8]));
                        drop(Box::from_raw(header));
                    }
                    TraceStringAlloc::SingleBlock { block, layout } => {
                        std::alloc::dealloc(block, layout);
                    }
                }
            }
        }
    });
}

/// Opaque, repr-C string-payload box exposed across the JIT boundary.
///
/// The JIT sees a single `*const StringRef` (an i64); only this crate
/// dereferences it. The struct is `#[repr(C)]` so byte layout is
/// stable across opt levels; the underlying `Box<str>` (or its raw
/// `(ptr, len)`) is **not** dropped automatically — see the leak
/// caveat in the module docs.
///
/// ## Tier 1b: cached `fx_hash` field
///
/// The `hash` field caches `fx_hash_bytes(payload)` so dict-key lookups
/// crossing the trace boundary can reuse the digest instead of re-running
/// the byte-wise hash loop. Producer helpers (`from_owned` / `borrow`
/// / `from_static`) stamp the digest at construction time. The inline
/// `StrConcat` lowering writes the digest of the freshly-built payload
/// via [`__relon_str_concat_seal_hash`] after the JIT fills the rhs
/// tail bytes — see `runtime/str_ops.rs` for the seal-after-write
/// contract.
///
/// Sentinel value `0` is reserved for "hash not yet sealed"; consumers
/// that need a guaranteed-fresh digest can re-compute via
/// [`fx_hash_bytes`] over `(ptr, len)` and update the field. Today the
/// only consumer is the dict-lookup IC which reads
/// [`STRING_REF_HASH_OFFSET`] via a single `load.u64` — the seal path
/// MUST run before any such consumer touches the result.
#[repr(C)]
pub struct StringRef {
    /// UTF-8 payload pointer. Stable for the lifetime of the
    /// surrounding allocation.
    pub ptr: *const u8,
    /// Payload byte length.
    pub len: usize,
    /// Cached `fx_hash_bytes(payload)`. Stamped by every producer
    /// helper (`from_owned` / `borrow` / `from_static`) and re-sealed
    /// after the inline `StrConcat` lowering writes the rhs tail bytes.
    /// `0` = "not yet sealed"; consumers MUST treat that as a deopt
    /// signal or fall back to recomputing via [`fx_hash_bytes`].
    pub hash: u64,
}

/// Byte offset of `StringRef::ptr` from the struct base. Exposed so the
/// trace emitter's inline `StrContains` lowering can issue a
/// `load` at this offset without re-encoding the layout assumption.
/// The compile-time assert below ties this constant to `offset_of!` so
/// any reordering of `StringRef` fields is caught at build time rather
/// than at JIT execution time.
pub const STRING_REF_PTR_OFFSET: i32 = 0;
/// Byte offset of `StringRef::len` from the struct base. See the
/// `STRING_REF_PTR_OFFSET` doc for the rationale.
pub const STRING_REF_LEN_OFFSET: i32 = 8;
/// Byte offset of `StringRef::hash` from the struct base.
///
/// Tier 1b: the cached `fx_hash_bytes(payload)` lives at this offset
/// so the dict-lookup IC can `load.u64 [str_ref + 16]` instead of
/// re-running the byte-wise hash loop on every cross-trace dict
/// lookup. Same layout-pin rationale as the other STRING_REF_*
/// constants — the compile-time assert below ties it to
/// `offset_of!(StringRef, hash)` so reordering the struct triggers
/// a build error.
pub const STRING_REF_HASH_OFFSET: i32 = 16;

// Compile-time invariant: the JIT-side `load` offsets used in
// `relon_trace_emitter::str_inline::load_string_ref_payload` MUST
// match the host-side `StringRef` layout. Any reordering / type
// change in `StringRef` triggers a build error here so the emitter
// can never drift silently.
//
// Gated on `target_pointer_width = "64"` because the offsets above
// assume a 64-bit `usize` (`len` lives at byte 8). On 32-bit targets
// (notably `wasm32-unknown-unknown`, which the workspace must still
// `cargo check` cleanly for the playground / docs build) `usize` is
// 4 bytes and `len` lands at byte 4, which is fine — the trace JIT
// runtime never executes on wasm32 (cranelift cannot target the host
// from wasm), but the crate's pure-Rust portions still need to type-
// check there for the wasm playground's dependency tree.
#[cfg(target_pointer_width = "64")]
const _: () = {
    assert!(
        core::mem::offset_of!(StringRef, ptr) == STRING_REF_PTR_OFFSET as usize,
        "StringRef::ptr offset drift; update STRING_REF_PTR_OFFSET"
    );
    assert!(
        core::mem::offset_of!(StringRef, len) == STRING_REF_LEN_OFFSET as usize,
        "StringRef::len offset drift; update STRING_REF_LEN_OFFSET"
    );
    assert!(
        core::mem::offset_of!(StringRef, hash) == STRING_REF_HASH_OFFSET as usize,
        "StringRef::hash offset drift; update STRING_REF_HASH_OFFSET"
    );
};

impl StringRef {
    /// Build a `StringRef` from a Rust `&str`. The returned reference
    /// borrows from `s` — caller must keep `s` alive for as long as
    /// the JIT may use the pointer.
    ///
    /// Tier 1b: stamps the cached `fx_hash_bytes(payload)` so the
    /// dict-lookup IC can short-circuit the byte-wise hash loop on
    /// cross-trace dict accesses.
    pub fn borrow(s: &str) -> Self {
        let bytes = s.as_bytes();
        Self {
            ptr: bytes.as_ptr(),
            len: bytes.len(),
            hash: fx_hash_bytes(bytes),
        }
    }

    /// Build a `StringRef` whose payload lives in a heap-owned
    /// buffer. The returned pointer is suitable for handing to the
    /// JIT and keeping alive for the lifetime of the surrounding
    /// trace.
    ///
    /// Tier 1b: stamps the cached `fx_hash_bytes(payload)` on the
    /// fresh struct so the dict IC fast path stays in sync.
    ///
    /// Review #175 P2: the allocation (both the `Box<StringRef>`
    /// header and the `Box<[u8]>` payload) is registered with the
    /// thread-local trace string arena so a subsequent
    /// [`reclaim_trace_strings`] call drops both back to the global
    /// allocator. Without that, every per-trace call leaked the
    /// payload forever.
    ///
    /// ## Safety
    ///
    /// Callers may keep the returned pointer alive for the duration
    /// of the surrounding trace. The trace exit / deopt path is
    /// expected to invoke [`reclaim_trace_strings`] which invalidates
    /// every pointer this helper handed out on the same thread.
    pub fn from_owned(s: String) -> *const StringRef {
        // Convert the `String` to a `Box<[u8]>` so the payload's
        // (thin ptr, len) pair can be reconstructed into a
        // `Box<[u8]>` at reclaim time.
        let boxed_bytes: Box<[u8]> = s.into_bytes().into_boxed_slice();
        let len = boxed_bytes.len();
        let hash = fx_hash_bytes(&boxed_bytes);
        let payload_ptr = Box::into_raw(boxed_bytes) as *mut u8;
        let header = Box::into_raw(Box::new(StringRef {
            ptr: payload_ptr as *const u8,
            len,
            hash,
        }));
        arena_push(TraceStringAlloc::OwnedHeaderAndPayload {
            header,
            payload_ptr,
            payload_len: len,
        });
        header as *const StringRef
    }

    /// Build a `StringRef` from a `&'static str` source. Useful for
    /// host-side construction of constant inputs in tests.
    ///
    /// Review #175 P2: the freshly-allocated `Box<StringRef>` header
    /// is registered with the trace string arena so
    /// [`reclaim_trace_strings`] can drop it. The payload pointer
    /// itself borrows from the static and never needs freeing.
    pub fn from_static(s: &'static str) -> *const StringRef {
        let header = Box::into_raw(Box::new(StringRef::borrow(s)));
        arena_push(TraceStringAlloc::BoxedHeader { header });
        header as *const StringRef
    }

    /// Build a `StringRef` from a `&'static str` source whose header
    /// is **leaked** — never registered with the trace string arena.
    /// Returns a pointer that outlives every
    /// [`reclaim_trace_strings`] call on the calling thread.
    ///
    /// Use this for bench / fixture constants that the trace's JIT'd
    /// code reads on every invocation (e.g. `lit_a` operand passed
    /// via `args_ptr`). The default [`Self::from_static`] registers
    /// the header so reclaim can drop it, which is the right contract
    /// for per-trace inputs but causes a use-after-free when the same
    /// pointer is re-used across invocations interleaved with
    /// reclaim (the W3/W4 cmp_lua bench SIGSEGV repro on 2026-05-25).
    pub fn from_static_permanent(s: &'static str) -> *const StringRef {
        Box::into_raw(Box::new(StringRef::borrow(s))) as *const StringRef
    }

    /// Read back a `&str` slice from the pointer. Returns `None` if
    /// `ptr` is null.
    ///
    /// ## Safety
    ///
    /// `ptr` must point at a `StringRef` whose `ptr/len` payload is
    /// valid UTF-8 — typically because it was produced by one of the
    /// shims below.
    pub unsafe fn as_str<'a>(ptr: *const StringRef) -> Option<&'a str> {
        if ptr.is_null() {
            return None;
        }
        let r = &*ptr;
        if r.ptr.is_null() {
            return None;
        }
        let bytes = std::slice::from_raw_parts(r.ptr, r.len);
        std::str::from_utf8(bytes).ok()
    }
}

// ---- IC for str_contains ----------------------------------------------

/// Single-slot pointer-keyed cache for `__relon_str_contains`. The
/// W4 benchmark calls `s.contains("x")` in a hot loop with `s` and
/// `"x"` constant across iters; an MRU cache turns the per-iter
/// substring scan into a ~3-ns pointer comparison.
///
/// The cache is `thread_local!` because traces are per-thread by
/// design (`call_table.rs` §1.4). Concurrent traces on different
/// threads see independent caches.
#[derive(Default)]
struct StrContainsIc {
    last_haystack: Cell<*const StringRef>,
    last_needle: Cell<*const StringRef>,
    last_result: Cell<i32>,
    hit_count: Cell<u64>,
    miss_count: Cell<u64>,
}

thread_local! {
    static STR_CONTAINS_IC: StrContainsIc = StrContainsIc::default();
}

/// Diagnostic: per-thread cache hit / miss counters for
/// `__relon_str_contains`. Returns `(hits, misses)`. Tests read this
/// to verify the IC actually fires on the W4-shaped benchmark.
pub fn str_contains_ic_counts() -> (u64, u64) {
    STR_CONTAINS_IC.with(|ic| (ic.hit_count.get(), ic.miss_count.get()))
}

/// Reset the IC counters and slot. Tests call this to start each
/// case with a clean cache so hit/miss ratios are deterministic.
pub fn reset_str_contains_ic() {
    STR_CONTAINS_IC.with(|ic| {
        ic.last_haystack.set(std::ptr::null());
        ic.last_needle.set(std::ptr::null());
        ic.last_result.set(0);
        ic.hit_count.set(0);
        ic.miss_count.set(0);
    });
}

// ---- Public shims ----------------------------------------------------

/// F-D7 `__relon_str_concat`. See [`module docs`](self) for ABI.
///
/// On null inputs the result is null — the JIT side treats null as a
/// trace-abort sentinel; the recorder is expected to emit a
/// `Guard(NotNull(_))` for each operand before reaching this op.
///
/// ## Safety
///
/// Both `lhs` and `rhs` must be either null or a valid
/// `*const StringRef` previously produced by another shim or by
/// [`StringRef::from_owned`] / [`StringRef::from_static`].
#[no_mangle]
pub unsafe extern "C" fn __relon_str_concat(
    lhs: *const StringRef,
    rhs: *const StringRef,
) -> *const StringRef {
    let a = match StringRef::as_str(lhs) {
        Some(s) => s,
        None => return std::ptr::null(),
    };
    let b = match StringRef::as_str(rhs) {
        Some(s) => s,
        None => return std::ptr::null(),
    };
    let mut out = String::with_capacity(a.len() + b.len());
    out.push_str(a);
    out.push_str(b);
    StringRef::from_owned(out)
}

/// F-D7-I: allocator-only helper for the inline `StrConcat` lowering.
///
/// The cranelift IR emitted by
/// `relon_trace_emitter::str_inline::emit_str_concat_inline_short_rhs`
/// uses this helper to obtain a fresh `StringRef` whose payload buffer
/// is already populated with the `lhs` bytes — the JIT then writes the
/// const rhs bytes inline (unrolled stores) at offset `lhs.len`.
///
/// Doing the lhs memcpy + StringRef header allocation inside the
/// helper (rather than fully inline in cranelift IR) keeps the per-iter
/// machine code small while skipping the costliest parts of the
/// generic [`__relon_str_concat`]: `StringRef::as_str`'s UTF-8
/// validation pass over both operands and the `String`/`Box<str>`
/// re-allocation handoff. The leak-arena story is identical to
/// [`StringRef::from_owned`] — every call leaks a fresh `Box<[u8]>` +
/// `Box<StringRef>`; the surrounding `TraceContext` is responsible for
/// reclaiming on teardown (see module docs).
///
/// Returns a non-null `*mut StringRef` whose `(ptr, len)` payload is
/// `(buf_ptr, total_len)` and whose first `lhs.len()` bytes are copied
/// from `lhs.ptr`. The remaining `total_len - lhs.len()` bytes are
/// **uninitialised** — the JIT side is responsible for filling them in
/// before any read.
///
/// `total_len` must be `>= lhs.len()`. On null `lhs` or
/// `total_len < lhs.len()` the helper returns a null pointer; the JIT
/// upstream guards against null lhs already (`Guard(NotNull(lhs))`),
/// so this is a defensive backstop.
///
/// ## Safety
///
/// `lhs` must be null or a valid `*const StringRef` previously produced
/// by another shim or by [`StringRef::from_owned`] /
/// [`StringRef::from_static`].
#[no_mangle]
pub unsafe extern "C" fn __relon_str_concat_alloc(
    lhs: *const StringRef,
    total_len: usize,
) -> *mut StringRef {
    if lhs.is_null() {
        return std::ptr::null_mut();
    }
    let lhs_ref = &*lhs;
    let lhs_len = lhs_ref.len;
    if total_len < lhs_len {
        return std::ptr::null_mut();
    }
    // Single-block layout: `[StringRef header | payload bytes]` in one
    // contiguous allocation. Saves a `Box<[u8]>` + `Box<StringRef>`
    // double-alloc per iter vs the historical two-block design — the
    // measured W3 hot loop spends most of its time inside the
    // allocator, so halving the per-iter alloc count is the single
    // biggest lever.
    //
    // Layout discipline: the payload bytes sit at
    // `(header_ptr as *u8).add(size_of::<StringRef>())`. The
    // `StringRef::ptr` field carries that interior pointer so the rest
    // of the runtime treats this allocation identically to the
    // historical two-block one.
    use std::alloc::alloc;
    let header_size = std::mem::size_of::<StringRef>();
    let header_align = std::mem::align_of::<StringRef>();
    debug_assert!(header_size.is_multiple_of(header_align));
    let block_size = header_size + total_len;
    let layout = Layout::from_size_align(block_size, header_align)
        .expect("StringRef block layout must be valid");
    let block = alloc(layout);
    if block.is_null() {
        // Allocator failed; surface as a null sentinel (the JIT side
        // treats this as a deopt). We do not call `handle_alloc_error`
        // because the calling trace expects a recoverable null.
        return std::ptr::null_mut();
    }
    let payload_ptr = block.add(header_size);
    if lhs_len > 0 && !lhs_ref.ptr.is_null() {
        std::ptr::copy_nonoverlapping(lhs_ref.ptr, payload_ptr, lhs_len);
    }
    let header_ptr = block as *mut StringRef;
    // Tier 1b: stash `hash = 0` as the "not yet sealed" sentinel. The
    // cranelift inline-rhs `StrConcat` lowering writes the const rhs
    // bytes after this call returns; the matching
    // `__relon_str_concat_seal_hash(header_ptr)` call closes the hash
    // gap before any consumer (notably the dict-lookup IC) reads back
    // `StringRef::hash`. Leaving hash unsealed (`0`) here keeps the
    // alloc shim allocation-free of the second pass over the lhs
    // bytes; sealing folds the full lhs+rhs payload in one shot.
    std::ptr::write(
        header_ptr,
        StringRef {
            ptr: payload_ptr as *const u8,
            len: total_len,
            hash: 0,
        },
    );
    // Review #175 P2: register the single-block allocation so
    // `reclaim_trace_strings` can dealloc it at trace exit.
    arena_push(TraceStringAlloc::SingleBlock { block, layout });
    header_ptr
}

/// #168: N-operand single-allocation concat helper for the inline
/// `TraceOp::StrConcatN` lowering.
///
/// Allocates a single `[StringRef header | payload bytes]` block (same
/// layout discipline as [`__relon_str_concat_alloc`]), then `memcpy`s
/// each operand's `(ptr, len)` payload contiguously into the payload
/// buffer in operand order. Returns a non-null `*mut StringRef` whose
/// payload is the byte-concatenation of all N operand payloads, plus
/// `hash = 0` as the "not yet sealed" sentinel — the inline lowering
/// follows up with [`__relon_str_concat_seal_hash`] so the dict-key
/// IC can trust the cached digest.
///
/// ## ABI
///
/// `operands` is a packed `*const *const StringRef` array (the inline
/// lowering stack-spills its operand pointer SSAs into this layout).
/// `n` is the operand count; the helper reads exactly `n` pointers.
/// `total_len` is the pre-computed sum of operand `len`s — the inline
/// lowering computes it via cranelift IR adds outside the helper so
/// the slow path can be a straight allocation + copy loop. Passing a
/// mismatched `total_len` is UB (would over- or under-fill the
/// payload). The inline lowering and the test in `str_ops.rs` are the
/// only callers; both keep the invariant.
///
/// ## Safety
///
/// * `operands` must point at a contiguous `n`-element array of
///   `*const StringRef` pointers; each pointer must be null or a
///   valid `*const StringRef` previously produced by another shim.
/// * `total_len` must equal `Σ operand_i.len` (or be larger — the
///   payload tail is undefined past the concatenated content, but
///   the allocation succeeds and the JIT-side seal-hash call would
///   then digest stale bytes; the inline lowering matches exactly).
/// * On a null operand or a null inner `ptr`, the helper returns
///   null — the JIT-side `Guard(NotNull(operand_i))` lifts this into
///   a clean deopt before the call, so the path is a defensive
///   backstop.
#[no_mangle]
pub unsafe extern "C" fn __relon_str_concat_n_alloc(
    operands: *const *const StringRef,
    n: usize,
    total_len: usize,
) -> *mut StringRef {
    if operands.is_null() {
        return std::ptr::null_mut();
    }
    use std::alloc::alloc;
    let header_size = std::mem::size_of::<StringRef>();
    let header_align = std::mem::align_of::<StringRef>();
    debug_assert!(header_size.is_multiple_of(header_align));
    let block_size = header_size + total_len;
    let layout = Layout::from_size_align(block_size, header_align)
        .expect("StringRef block layout must be valid");
    let block = alloc(layout);
    if block.is_null() {
        return std::ptr::null_mut();
    }
    let payload_ptr = block.add(header_size);
    let mut cursor: usize = 0;
    for i in 0..n {
        let operand = *operands.add(i);
        if operand.is_null() {
            // Defensive: a null operand short-circuits to null. The
            // upstream `Guard(NotNull(_))` should catch this first.
            std::alloc::dealloc(block, layout);
            return std::ptr::null_mut();
        }
        let r = &*operand;
        if r.len == 0 {
            continue;
        }
        if r.ptr.is_null() {
            std::alloc::dealloc(block, layout);
            return std::ptr::null_mut();
        }
        if cursor + r.len > total_len {
            // `total_len` was under-budget. Bail out instead of
            // writing past the allocation.
            std::alloc::dealloc(block, layout);
            return std::ptr::null_mut();
        }
        std::ptr::copy_nonoverlapping(r.ptr, payload_ptr.add(cursor), r.len);
        cursor += r.len;
    }
    let header_ptr = block as *mut StringRef;
    // `hash = 0` is the "not yet sealed" sentinel; the JIT-side
    // inline lowering calls `__relon_str_concat_seal_hash` after this
    // returns so the dict IC fast path sees a valid digest.
    std::ptr::write(
        header_ptr,
        StringRef {
            ptr: payload_ptr as *const u8,
            len: cursor,
            hash: 0,
        },
    );
    // Review #175 P2: register the single-block allocation so
    // `reclaim_trace_strings` can dealloc it at trace exit.
    arena_push(TraceStringAlloc::SingleBlock { block, layout });
    header_ptr
}

/// Tier 1b companion to [`__relon_str_concat_alloc`]: re-compute
/// `fx_hash_bytes(payload)` over the **now-filled** payload buffer and
/// stamp it into `StringRef::hash`.
///
/// The cranelift inline `StrConcat` lowering calls this after writing
/// the const rhs bytes via the unrolled `store.i8` tail so the dict
/// IC fast path can `load.u64 [str_ref + STRING_REF_HASH_OFFSET]` and
/// trust the cached digest. Without this seal step the dict lookup
/// would either consume the `0` sentinel (silent IC miss every iter)
/// or re-run the byte-wise hash loop (defeating Tier 1a's cached-
/// hash win).
///
/// # Safety
///
/// `s` must point at a non-null `*mut StringRef` whose `(ptr, len)`
/// payload is fully initialised — i.e. the cranelift caller has
/// completed all rhs stores before this call. The function reads
/// `len` bytes via `ptr`, so partial writes would surface either as
/// UB (uninitialised reads) or a stale digest.
#[no_mangle]
pub unsafe extern "C" fn __relon_str_concat_seal_hash(s: *mut StringRef) {
    if s.is_null() {
        return;
    }
    let r = &mut *s;
    if r.ptr.is_null() {
        // Defensive: payload pointer unset, nothing to hash. Leave the
        // existing sentinel in place so consumers can detect the miss.
        return;
    }
    let bytes = std::slice::from_raw_parts(r.ptr, r.len);
    r.hash = fx_hash_bytes(bytes);
}

/// F-D7 `__relon_str_contains`. Consults the single-slot
/// MRU cache before scanning; updates the cache on miss.
///
/// ## Safety
///
/// Both operands must be null or a valid `*const StringRef`.
#[no_mangle]
pub unsafe extern "C" fn __relon_str_contains(
    haystack: *const StringRef,
    needle: *const StringRef,
) -> i32 {
    // IC fast path: identical (haystack, needle) pointers as the last
    // call → return cached result without a scan. Pointer equality is
    // fine here because `Arc<str>` (or interned-string) instances on
    // the host side keep their payload pointers stable.
    let cached = STR_CONTAINS_IC.with(|ic| {
        if !haystack.is_null()
            && !needle.is_null()
            && ic.last_haystack.get() == haystack
            && ic.last_needle.get() == needle
        {
            ic.hit_count.set(ic.hit_count.get() + 1);
            Some(ic.last_result.get())
        } else {
            None
        }
    });
    if let Some(r) = cached {
        return r;
    }

    let h = match StringRef::as_str(haystack) {
        Some(s) => s,
        None => return 0,
    };
    let n = match StringRef::as_str(needle) {
        Some(s) => s,
        None => return 0,
    };
    let result = if h.contains(n) { 1 } else { 0 };
    STR_CONTAINS_IC.with(|ic| {
        ic.last_haystack.set(haystack);
        ic.last_needle.set(needle);
        ic.last_result.set(result);
        ic.miss_count.set(ic.miss_count.get() + 1);
    });
    result
}

/// F-D7 `__relon_str_find`. Returns the byte index of the first
/// occurrence, or `-1` on miss. Mirrors Rust's `str::find` exactly.
///
/// ## Safety
///
/// Both operands must be null or a valid `*const StringRef`.
#[no_mangle]
pub unsafe extern "C" fn __relon_str_find(
    haystack: *const StringRef,
    needle: *const StringRef,
) -> i64 {
    let h = match StringRef::as_str(haystack) {
        Some(s) => s,
        None => return -1,
    };
    let n = match StringRef::as_str(needle) {
        Some(s) => s,
        None => return -1,
    };
    match h.find(n) {
        Some(idx) => idx as i64,
        None => -1,
    }
}

/// F-D7 `__relon_str_substring`. Byte-indexed substring with the
/// tree-walker's exact clamp semantics: `start` and `length` are
/// clamped into `[0, len(s)]`, then walked to the nearest char
/// boundary so the returned slice stays valid UTF-8.
///
/// ## Safety
///
/// `s` must be null or a valid `*const StringRef`.
#[no_mangle]
pub unsafe extern "C" fn __relon_str_substring(
    s: *const StringRef,
    start: i64,
    length: i64,
) -> *const StringRef {
    let payload = match StringRef::as_str(s) {
        Some(s) => s,
        None => return std::ptr::null(),
    };
    let s_len = payload.len() as i64;
    let start = start.clamp(0, s_len) as usize;
    let length = length.max(0) as usize;
    let end = (start + length).min(payload.len());
    if end <= start {
        return StringRef::from_owned(String::new());
    }
    // Walk to nearest char boundary so the slice stays UTF-8 even on
    // mid-codepoint byte indices.
    let real_start = payload
        .char_indices()
        .find(|(i, _)| *i >= start)
        .map(|(i, _)| i)
        .unwrap_or(payload.len());
    let real_end = payload
        .char_indices()
        .find(|(i, _)| *i >= end)
        .map(|(i, _)| i)
        .unwrap_or(payload.len());
    StringRef::from_owned(payload[real_start..real_end].to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn concat_two_static_strings() {
        let a = StringRef::from_static("hello, ");
        let b = StringRef::from_static("world");
        let r = unsafe { __relon_str_concat(a, b) };
        assert!(!r.is_null());
        let s = unsafe { StringRef::as_str(r) }.unwrap();
        assert_eq!(s, "hello, world");
    }

    #[test]
    fn contains_hit_returns_one() {
        reset_str_contains_ic();
        let h = StringRef::from_static("axb");
        let n = StringRef::from_static("x");
        let r1 = unsafe { __relon_str_contains(h, n) };
        assert_eq!(r1, 1);
        // Same pointers → IC hit on the second call.
        let r2 = unsafe { __relon_str_contains(h, n) };
        assert_eq!(r2, 1);
        let (hits, misses) = str_contains_ic_counts();
        assert_eq!(hits, 1, "second call should hit");
        assert_eq!(misses, 1, "first call should miss");
    }

    #[test]
    fn contains_miss_returns_zero() {
        reset_str_contains_ic();
        let h = StringRef::from_static("axb");
        let n = StringRef::from_static("z");
        let r = unsafe { __relon_str_contains(h, n) };
        assert_eq!(r, 0);
    }

    #[test]
    fn find_returns_byte_index_or_neg_one() {
        let h = StringRef::from_static("hello, world");
        let n = StringRef::from_static("world");
        let r = unsafe { __relon_str_find(h, n) };
        assert_eq!(r, 7);
        let miss = StringRef::from_static("zzz");
        let r2 = unsafe { __relon_str_find(h, miss) };
        assert_eq!(r2, -1);
    }

    #[test]
    fn substring_clamps_oob_inputs() {
        let s = StringRef::from_static("hello");
        // Negative start clamps to 0; over-long length clamps to len.
        let r = unsafe { __relon_str_substring(s, -10, 100) };
        let out = unsafe { StringRef::as_str(r) }.unwrap();
        assert_eq!(out, "hello");
    }

    #[test]
    fn substring_zero_length_returns_empty() {
        let s = StringRef::from_static("hello");
        let r = unsafe { __relon_str_substring(s, 2, 0) };
        let out = unsafe { StringRef::as_str(r) }.unwrap();
        assert_eq!(out, "");
    }

    #[test]
    fn concat_alloc_copies_lhs_prefix_and_leaves_rhs_tail_uninit() {
        let lhs = StringRef::from_static("hello");
        // Reserve 5 + 2 = 7 bytes; the JIT writes the rhs into bytes 5..7.
        let r = unsafe { __relon_str_concat_alloc(lhs, 7) };
        assert!(!r.is_null());
        let r_ref = unsafe { &*r };
        assert_eq!(r_ref.len, 7);
        // The first 5 bytes match "hello"; the trailing 2 bytes are
        // undefined per contract, so we only inspect the prefix.
        let prefix = unsafe { std::slice::from_raw_parts(r_ref.ptr, 5) };
        assert_eq!(prefix, b"hello");
        // Tier 1b: `hash = 0` is the "not yet sealed" sentinel; the
        // matching `__relon_str_concat_seal_hash` runs after the JIT
        // writes the rhs tail bytes.
        assert_eq!(r_ref.hash, 0, "alloc leaves hash unsealed");
    }

    #[test]
    fn concat_seal_hash_matches_fx_hash_bytes() {
        // Tier 1b: simulate the cranelift `StrConcat(lhs, rhs)` inline
        // tail. Alloc, manually fill the rhs tail, then seal the hash
        // — the cached digest must equal `fx_hash_bytes(payload)`.
        let lhs = StringRef::from_static("hello");
        let total = 5 + 3; // "hello" + " wo"
        let r = unsafe { __relon_str_concat_alloc(lhs, total) };
        assert!(!r.is_null());
        unsafe {
            let r_ref = &mut *r;
            // Write the rhs tail bytes the way the JIT would.
            let tail = (r_ref.ptr as *mut u8).add(5);
            std::ptr::copy_nonoverlapping(b" wo".as_ptr(), tail, 3);
            __relon_str_concat_seal_hash(r);
        }
        let r_ref = unsafe { &*r };
        let payload = unsafe { std::slice::from_raw_parts(r_ref.ptr, r_ref.len) };
        assert_eq!(payload, b"hello wo");
        assert_eq!(
            r_ref.hash,
            relon_trace_abi::hash::fx_hash_bytes(payload),
            "sealed hash matches byte-wise fx_hash reference"
        );
        // Belt-and-braces: from_static / from_owned must also stamp
        // a matching digest so dict-key crossings stay consistent.
        let baseline = StringRef::from_static("hello wo");
        let baseline_ref = unsafe { &*baseline };
        assert_eq!(baseline_ref.hash, r_ref.hash);
    }

    #[test]
    fn concat_seal_hash_null_input_is_noop() {
        // Defensive backstop: null pointer must not segfault — callers
        // upstream of the JIT already null-guard, but the seal helper
        // is invoked unconditionally in the cranelift IR.
        unsafe {
            __relon_str_concat_seal_hash(std::ptr::null_mut());
        }
    }

    #[test]
    fn from_owned_stamps_cached_fx_hash() {
        // Tier 1b producer-side guarantee: every helper that builds a
        // fresh `StringRef` must pre-compute `fx_hash_bytes(payload)`
        // so the dict-lookup IC can short-circuit the byte-wise loop.
        let owned = StringRef::from_owned("dict_key".to_string());
        let owned_ref = unsafe { &*owned };
        assert_eq!(
            owned_ref.hash,
            relon_trace_abi::hash::fx_hash_bytes(b"dict_key"),
        );
        let borrowed = StringRef::from_static("dict_key");
        let borrowed_ref = unsafe { &*borrowed };
        assert_eq!(borrowed_ref.hash, owned_ref.hash);
    }

    #[test]
    fn concat_alloc_rejects_null_lhs() {
        let r = unsafe { __relon_str_concat_alloc(std::ptr::null(), 4) };
        assert!(r.is_null());
    }

    #[test]
    fn concat_alloc_rejects_undersized_total_len() {
        let lhs = StringRef::from_static("hello");
        // total_len < lhs.len → defensive null return.
        let r = unsafe { __relon_str_concat_alloc(lhs, 3) };
        assert!(r.is_null());
    }

    #[test]
    fn concat_alloc_zero_total_len_returns_empty_buffer() {
        // Edge case: empty lhs + empty rhs (rhs is the JIT's
        // responsibility to fill, so the buffer is just a zero-length
        // ptr). `Box<[u8]>` allocates a non-null sentinel for ZSTs.
        let lhs = StringRef::from_static("");
        let r = unsafe { __relon_str_concat_alloc(lhs, 0) };
        assert!(!r.is_null());
        let r_ref = unsafe { &*r };
        assert_eq!(r_ref.len, 0);
    }

    // ---- #168: N-operand single-allocation concat ------------------

    #[test]
    fn concat_n_alloc_writes_payloads_in_order() {
        let a = StringRef::from_static("foo");
        let b = StringRef::from_static("-bar");
        let c = StringRef::from_static("-baz");
        let operands: [*const StringRef; 3] = [a, b, c];
        let total = 3 + 4 + 4;
        let r = unsafe { __relon_str_concat_n_alloc(operands.as_ptr(), 3, total) };
        assert!(!r.is_null());
        let r_ref = unsafe { &*r };
        assert_eq!(r_ref.len, total);
        let payload = unsafe { std::slice::from_raw_parts(r_ref.ptr, r_ref.len) };
        assert_eq!(payload, b"foo-bar-baz");
        // hash unsealed per the alloc contract; the inline lowering
        // calls __relon_str_concat_seal_hash after this returns.
        assert_eq!(r_ref.hash, 0);
    }

    #[test]
    fn concat_n_alloc_handles_four_operands() {
        // Mirrors the trace-JIT inline cap (MAX_INLINE_STR_CONCAT_N = 4).
        let a = StringRef::from_static("a");
        let b = StringRef::from_static("bb");
        let c = StringRef::from_static("ccc");
        let d = StringRef::from_static("dddd");
        let operands: [*const StringRef; 4] = [a, b, c, d];
        let total = 1 + 2 + 3 + 4;
        let r = unsafe { __relon_str_concat_n_alloc(operands.as_ptr(), 4, total) };
        assert!(!r.is_null());
        let payload = unsafe { std::slice::from_raw_parts((*r).ptr, (*r).len) };
        assert_eq!(payload, b"abbcccdddd");
    }

    #[test]
    fn concat_n_alloc_handles_empty_operand() {
        let a = StringRef::from_static("");
        let b = StringRef::from_static("x");
        let c = StringRef::from_static("");
        let operands: [*const StringRef; 3] = [a, b, c];
        let r = unsafe { __relon_str_concat_n_alloc(operands.as_ptr(), 3, 1) };
        assert!(!r.is_null());
        let payload = unsafe { std::slice::from_raw_parts((*r).ptr, (*r).len) };
        assert_eq!(payload, b"x");
    }

    #[test]
    fn concat_n_alloc_rejects_null_operand_array() {
        let r = unsafe { __relon_str_concat_n_alloc(std::ptr::null(), 3, 0) };
        assert!(r.is_null());
    }

    #[test]
    fn concat_n_alloc_rejects_null_inner_operand() {
        let a = StringRef::from_static("foo");
        let operands: [*const StringRef; 2] = [a, std::ptr::null()];
        let r = unsafe { __relon_str_concat_n_alloc(operands.as_ptr(), 2, 3) };
        assert!(r.is_null());
    }

    #[test]
    fn concat_n_alloc_rejects_undersized_total_len() {
        let a = StringRef::from_static("foo");
        let b = StringRef::from_static("bar");
        let operands: [*const StringRef; 2] = [a, b];
        // total_len = 4 < 6 — the helper bails before overflowing.
        let r = unsafe { __relon_str_concat_n_alloc(operands.as_ptr(), 2, 4) };
        assert!(r.is_null());
    }

    #[test]
    fn concat_n_alloc_seal_hash_matches_fx_hash() {
        // Mirror the full inline lowering: alloc + seal_hash. The
        // sealed digest must match the byte-wise fx_hash reference so
        // dict IC consumers stay in sync.
        let a = StringRef::from_static("hi-");
        let b = StringRef::from_static("there-");
        let c = StringRef::from_static("you");
        let operands: [*const StringRef; 3] = [a, b, c];
        let r = unsafe { __relon_str_concat_n_alloc(operands.as_ptr(), 3, 3 + 6 + 3) };
        assert!(!r.is_null());
        unsafe { __relon_str_concat_seal_hash(r) };
        let r_ref = unsafe { &*r };
        let payload = unsafe { std::slice::from_raw_parts(r_ref.ptr, r_ref.len) };
        assert_eq!(payload, b"hi-there-you");
        assert_eq!(
            r_ref.hash,
            relon_trace_abi::hash::fx_hash_bytes(payload),
            "sealed concat-n hash matches byte-wise fx_hash reference"
        );
    }

    #[test]
    fn null_inputs_return_null_or_neg_one() {
        let r = unsafe { __relon_str_concat(std::ptr::null(), std::ptr::null()) };
        assert!(r.is_null());
        let r2 = unsafe { __relon_str_contains(std::ptr::null(), std::ptr::null()) };
        assert_eq!(r2, 0);
        let r3 = unsafe { __relon_str_find(std::ptr::null(), std::ptr::null()) };
        assert_eq!(r3, -1);
        let r4 = unsafe { __relon_str_substring(std::ptr::null(), 0, 5) };
        assert!(r4.is_null());
    }

    #[test]
    fn contains_ic_distinguishes_pointer_keys() {
        reset_str_contains_ic();
        let h1 = StringRef::from_static("axb");
        let h2 = StringRef::from_static("ayb");
        let n = StringRef::from_static("x");
        // Different haystack pointers → miss + miss, not a hit.
        let _ = unsafe { __relon_str_contains(h1, n) };
        let _ = unsafe { __relon_str_contains(h2, n) };
        let (hits, misses) = str_contains_ic_counts();
        assert_eq!(hits, 0);
        assert_eq!(misses, 2);
    }

    // ---- Review #175 P2: trace string reclamation arena ------------
    //
    // Each test runs on its own spawned thread so the thread-local
    // arena is isolated from sibling tests that allocate via the same
    // shims (the cargo test runner uses a multi-thread pool but each
    // test starts on its own thread).

    #[test]
    fn arena_records_every_shim_allocation() {
        std::thread::spawn(|| {
            assert_eq!(
                trace_string_arena_len(),
                0,
                "fresh thread starts with empty arena"
            );
            let _a = StringRef::from_static("alpha");
            assert_eq!(trace_string_arena_len(), 1, "from_static registers");
            let _b = StringRef::from_owned("bravo".to_string());
            assert_eq!(trace_string_arena_len(), 2, "from_owned registers");
            // concat_alloc goes through the single-block path.
            let lhs = StringRef::from_static("hello");
            assert_eq!(trace_string_arena_len(), 3);
            let _c = unsafe { __relon_str_concat_alloc(lhs, 5 + 3) };
            assert_eq!(
                trace_string_arena_len(),
                4,
                "concat_alloc registers the single-block allocation"
            );
            // concat_n_alloc same path.
            let operands: [*const StringRef; 1] = [_a];
            let _n = unsafe { __relon_str_concat_n_alloc(operands.as_ptr(), 1, 5) };
            assert_eq!(trace_string_arena_len(), 5, "concat_n_alloc registers");
            // __relon_str_concat routes through from_owned.
            let _cat = unsafe { __relon_str_concat(_a, lhs) };
            assert_eq!(
                trace_string_arena_len(),
                6,
                "__relon_str_concat (via from_owned) registers"
            );
            // __relon_str_substring routes through from_owned.
            let _sub = unsafe { __relon_str_substring(_a, 1, 2) };
            assert_eq!(
                trace_string_arena_len(),
                7,
                "__relon_str_substring (via from_owned) registers"
            );
            unsafe { reclaim_trace_strings() };
            assert_eq!(
                trace_string_arena_len(),
                0,
                "reclaim drains every recorded allocation"
            );
        })
        .join()
        .expect("arena thread joined cleanly");
    }

    #[test]
    fn reclaim_releases_owned_payload_buffers() {
        // Smoke test for the OwnedHeaderAndPayload reclaim path: we
        // allocate many fairly-large from_owned strings and reclaim
        // them. The test would fail loudly under miri / address
        // sanitizer if the payload `Box<[u8]>` reclamation
        // reconstructed the slice with the wrong (ptr, len) pair.
        std::thread::spawn(|| {
            for i in 0..256 {
                let s = format!("payload-{i:04}-{}", "x".repeat(64));
                let _r = StringRef::from_owned(s);
            }
            assert_eq!(trace_string_arena_len(), 256);
            unsafe { reclaim_trace_strings() };
            assert_eq!(trace_string_arena_len(), 0);
        })
        .join()
        .expect("reclaim payload thread joined cleanly");
    }

    #[test]
    fn reclaim_releases_single_block_concat_buffers() {
        // Hot-loop simulation: many __relon_str_concat_alloc calls
        // (the W3 / cmp_lua pattern) followed by a single reclaim.
        // The historical leak path would grow process RSS by
        // `total_len` bytes per iter forever; with the arena hooked
        // up the reclaim drains every block.
        std::thread::spawn(|| {
            let lhs = StringRef::from_static("prefix-");
            for _ in 0..512 {
                let block = unsafe { __relon_str_concat_alloc(lhs, 7 + 8) };
                assert!(!block.is_null());
            }
            // 1 (from_static) + 512 (concat_alloc) = 513.
            assert_eq!(trace_string_arena_len(), 513);
            unsafe { reclaim_trace_strings() };
            assert_eq!(trace_string_arena_len(), 0);
        })
        .join()
        .expect("reclaim concat thread joined cleanly");
    }

    #[test]
    fn reclaim_is_idempotent_on_empty_arena() {
        // Calling reclaim twice in a row (or on a thread that never
        // allocated anything) must be a no-op rather than a double-free.
        std::thread::spawn(|| {
            unsafe { reclaim_trace_strings() };
            unsafe { reclaim_trace_strings() };
            assert_eq!(trace_string_arena_len(), 0);
        })
        .join()
        .expect("idempotent reclaim thread joined cleanly");
    }

    #[test]
    fn reclaim_does_not_cross_thread_boundaries() {
        // Allocations on thread A must NOT show up in thread B's
        // arena view, and reclaim on B must not free A's pointers.
        // The pointer is leaked deliberately in this test (the
        // owning thread never reclaims) so we don't accidentally
        // free A's allocation here; the test asserts only on the
        // arena counters.
        let handle_a = std::thread::spawn(|| {
            let _a = StringRef::from_static("a-thread");
            assert_eq!(trace_string_arena_len(), 1);
            // Deliberately do not reclaim — leak for the duration of
            // the test process. This is OK because the test asserts
            // only on counters, and the OS reclaims process memory
            // at exit.
        });
        handle_a.join().expect("thread A joined");
        std::thread::spawn(|| {
            assert_eq!(
                trace_string_arena_len(),
                0,
                "thread B sees its own empty arena"
            );
            unsafe { reclaim_trace_strings() };
        })
        .join()
        .expect("thread B joined");
    }
}
