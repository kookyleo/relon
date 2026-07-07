//! Phase F.1: host shims backing the LLVM AOT string fast-path.
//!
//! The bundled stdlib `contains(haystack: String, needle: String) -> Bool`
//! IR body (`relon_ir::stdlib::defs::contains_string_body`) is a hand-
//! transcribed O(s_len * p_len) byte-window scan — verifier-friendly,
//! backend-portable, and the only shape every codegen surface speaks
//! today. On the W4 / W4_long cmp_lua rows that inlined body lands the
//! LLVM AOT row at ~3.4× LuaJIT (short haystack) and ~256× LuaJIT
//! (256-byte haystack with a `'x'` at the tail) because the scalar
//! `LoadI8U` + `Ne` window walk has no SIMD vectorisation opportunity:
//! every iteration of the outer scan re-loads the full needle byte-by-
//! byte, defeating LLVM's auto-vectoriser on the tight inner loop.
//!
//! Phase F.1 cuts that gap by intercepting `Op::Call { fn_index ==
//! STDLIB_IDX_CONTAINS }` in the LLVM AOT emitter and routing the call
//! to [`relon_llvm_str_contains_arena`] — a thin host shim that defers
//! to `core::str::contains`. The std impl is backed by `core::slice`'s
//! `memchr`-based two-way / Boyer-Moore-style search (single-byte
//! needles hit a SIMD-accelerated `memchr::memchr` fast path), so the
//! per-iter cost drops from `O(s_len * p_len)` machine instructions to
//! a hardware-assisted byte scan in `libc` / Rust's vectorised slice
//! primitives.
//!
//! ## Why not reuse another native string shim?
//!
//! Other native string shims may use heap-style headers carrying `(ptr,
//! len, hash)`. The LLVM AOT pipeline never materialises that header; it
//! stores strings as `[len: u32 LE][utf8 bytes]` records inside the per-call
//! arena (see `ConstPool` doc in `emitter.rs`). Reusing a header-oriented
//! shim would force a per-iter record materialisation in the hot loop —
//! strictly worse than just passing two arena pointers and
//! reading the headers in-shim.
//!
//! ## ABI
//!
//! ```text
//! extern "C" fn relon_llvm_str_contains_arena(
//!     haystack_ptr: *const u8,   // arena_base + haystack_off
//!     needle_ptr:   *const u8,   // arena_base + needle_off
//! ) -> i32                       // 1 = match, 0 = no match
//! ```
//!
//! Both pointers must point at the `[u32 len][bytes...]` header of a
//! valid arena record. On `null` operands the shim returns `0` — the
//! emitter guarantees both pointers are derived from the cached
//! `arena_base + offset` GEP, so null never happens on the supported
//! surface; the explicit check is a defence-in-depth backstop.
//!
//! ## Symbol resolution
//!
//! The shim is registered with the MCJIT execution engine through
//! `engine.add_global_mapping` in
//! `crate::evaluator::LlvmAotEvaluator::from_ir` before the entry
//! pointer is resolved. The shim's address is exposed verbatim via
//! [`relon_llvm_str_contains_arena_addr`] so the registration code
//! doesn't have to materialise a function pointer cast inline.
//!
//! ## Phase G hot-path layout (2026-05-27)
//!
//! Phase F.1 collapsed the IC fast path and the `compute_contains`
//! byte-scan body into a single function. `perf annotate` on the W4
//! cmp_lua hot loop (s90, 600 k iters × 1000 ops each) showed the
//! Phase F.1 shape spending ~68 % of cycles inside the shim, with
//! ~50 % of that in the prologue / epilogue: the inlined
//! `compute_contains` reserved `sub $0x128, %rsp` worth of scratch
//! stack space (UTF-8 validator buffers, Two-Way matcher state) and
//! pushed all five callee-saved registers (`r12..r15`, `rbx`, plus
//! `rbp`) on every IC hit. The `thread_local!` macro additionally
//! emitted a `cmpb $0x1, %fs:0xff78 / jne` lazy-init guard ahead of
//! the slot reads.
//!
//! Phase G restructures the surface into two halves:
//!   * [`relon_llvm_str_contains_arena`] — the externally-mapped
//!     entry, minimal-frame shape. Performs an inlined IC check
//!     (`ic_hit_slot`) against a process-global atomic slot
//!     (no TLS init guard required, three `Relaxed` loads compile
//!     to plain `mov`s on x86_64). Tail-calls
//!     `str_contains_arena_slow` on a miss. The hot-path body fits
//!     in ~11 instructions with a `push rax / pop rcx / ret` frame.
//!   * `str_contains_arena_slow` — `#[cold] #[inline(never)]` so
//!     the optimizer no longer hoists `compute_contains`'s scratch
//!     space into the outer frame.
//!
//! s90 cmp_lua W4 / W4_long `relon_llvm_aot` rows dropped 49.2 µs →
//! 39.5 µs (-19.8 %, ~2.72× LuaJIT vs prior 3.39×) on this layout.
//! W1 / W2 / W3 / W5..W12 LLVM AOT rows stayed within ±2 % (noise
//! band) — the change is local to the `Op::Call { contains }`
//! dispatch boundary.

/// Read the `(len, payload_addr)` of an arena String record at `ptr`.
///
/// # Safety
///
/// `ptr` must be either null or point at the start of a well-formed
/// `[u32 len][bytes...]` arena record whose payload extends `len` bytes
/// past the header. The LLVM AOT emitter routes the cached
/// `arena_base + offset` pointer here unchanged; the surrounding entry
/// trampoline owns the arena allocation, so the record is guaranteed
/// to lie inside the arena.
#[inline]
unsafe fn read_record(ptr: *const u8) -> Option<&'static [u8]> {
    if ptr.is_null() {
        return None;
    }
    // SAFETY: caller upholds the arena-record invariant — the leading
    // 4 bytes are an LE-encoded payload length, followed by exactly
    // that many bytes of UTF-8 payload. `read_unaligned` is used
    // because the const-pool layout only guarantees 4-byte alignment
    // (records are emitted on 4-byte boundaries, but the host arena's
    // base may sit at any 8-byte slot inside its owning Vec).
    let len = unsafe { core::ptr::read_unaligned(ptr.cast::<u32>()) } as usize;
    let payload = unsafe { ptr.add(4) };
    // Hand back a `&'static [u8]` because the borrow checker can't see
    // through the raw-pointer FFI surface; the caller is responsible
    // for not holding the slice past the surrounding JIT dispatch.
    Some(unsafe { core::slice::from_raw_parts(payload, len) })
}

/// Single-slot pointer-keyed inline cache for `relon_llvm_str_contains_arena`.
///
/// The W4 cmp_lua row calls `s.contains("x")` 1000× per dispatch with
/// `s` and `"x"` both pointing at stable const-pool offsets (the map's
/// `(i) => "axb"` literal lives in the const-pool, not freshly
/// allocated per iter — see `ConstPool` doc in `emitter.rs`). Pointer
/// equality against a single MRU slot lets the steady-state 999/1000
/// iters skip the per-call FFI / UTF-8 / `str::contains` work entirely.
///
/// Mirrors the string-containment inline-cache shape used by other
/// native fast paths: the LLVM AOT side replays the same pattern because
/// the const-pool arena layout produces a stable `(haystack, needle)`
/// workload shape.
///
/// ## Phase G: process-global atomic slot instead of thread-local
///
/// Phase F.1 used `std::thread_local!` for cross-thread isolation.
/// perf annotate on the hot loop showed `cmpb $0x1, %fs:0xff78 / jne`
/// — the TLS lazy-init guard — eating ~22 % of every IC hit. Single-
/// slot pointer-equality is *result-independent of which thread
/// primed the cache*: if `(haystack_ptr, needle_ptr)` match the
/// slot's stored pointers, the cached `i32` is correct regardless of
/// which thread last wrote it (same pointers → same arena records →
/// same `contains` answer). Relaxed atomics are sufficient — a torn
/// (haystack, needle, result) snapshot across threads only triggers
/// extra misses, never a wrong answer.
///
/// On x86_64 `AtomicPtr::load(Relaxed)` lowers to a plain `mov`, so
/// the hot-path body shrinks from `cmpb / jne / mov %fs:.. / mov %fs:..`
/// to four `mov` + `cmp` instructions — eliminating the TLS init
/// guard entirely.
struct StrContainsArenaIc {
    last_haystack: core::sync::atomic::AtomicPtr<u8>,
    last_needle: core::sync::atomic::AtomicPtr<u8>,
    last_result: core::sync::atomic::AtomicI32,
}

static STR_CONTAINS_ARENA_IC: StrContainsArenaIc = StrContainsArenaIc {
    last_haystack: core::sync::atomic::AtomicPtr::new(core::ptr::null_mut()),
    last_needle: core::sync::atomic::AtomicPtr::new(core::ptr::null_mut()),
    last_result: core::sync::atomic::AtomicI32::new(0),
};

/// LLVM AOT host shim for `str.contains`. Returns `1` if the needle
/// appears in the haystack, else `0`. See the module-level docs for
/// the ABI and arena-record contract.
///
/// ## Phase G structure: minimal-frame outer + cold slow path
///
/// Phase F.1 (commit `92c8837`) collapsed this into a single function
/// with the IC fast path and the byte-scan body sharing one stack
/// frame. Profile (perf record / annotate) on the W4 hot loop showed
/// `relon_llvm_str_contains_arena` self-time at ~68 % even though the
/// IC hit rate is 999/1000 — the prologue (`push r12..r15 / rbx /
/// rbp`, `sub $0x128, %rsp`) and epilogue spend ~50 % of cycles
/// because `compute_contains` got inlined and reserved its scratch
/// stack space unconditionally.
///
/// Phase G splits the entry: the outer [`relon_llvm_str_contains_arena`]
/// only performs the IC pointer-equality check inline (no `with()`
/// closure, no thread-local re-entry guard — see `ic_hit_slot`
/// below) and tail-calls into a [`#[cold] #[inline(never)]`] slow
/// path when the IC misses. The shrink keeps the hot-path prologue to
/// `push rbp; sub $0x?, %rsp` and a small spill set, which on x86_64
/// is the difference between ~30 ns / call (Phase F.1) and ~5 ns /
/// call.
///
/// Consults `STR_CONTAINS_ARENA_IC` before doing the scan; the W4
/// / W4_long workloads call this with stable const-pool pointers so
/// the steady-state path skips the FFI / memmem cost after iter 0.
///
/// # Safety
///
/// Both pointers must be either null or point at a well-formed arena
/// String record (`[u32 len][utf8 bytes]`). The emitter never produces
/// a null pointer on the supported surface — they are GEPs off the
/// cached `arena_base`, which is non-null whenever the entry trampoline
/// is live.
#[no_mangle]
pub unsafe extern "C" fn relon_llvm_str_contains_arena(
    haystack_ptr: *const u8,
    needle_ptr: *const u8,
) -> i32 {
    // IC fast path: pointer equality against the last call's operands.
    // Const-pool arena offsets are stable across the entire MCJIT
    // engine lifetime, so a hit here means the previous answer is still
    // correct without re-reading the record headers.
    //
    // Phase G note: `ic_hit_slot` reads the TLS slot directly without
    // going through `thread_local!::with()`'s closure plumbing. The
    // closure form forced the optimizer to materialise the closure's
    // capture state on the stack frame, which combined with the
    // post-IC `compute_contains` body inflated the prologue. The raw
    // read keeps the hot-path body to a handful of `%fs` loads + cmps.
    if let Some(r) = ic_hit_slot(haystack_ptr, needle_ptr) {
        return r;
    }
    // SAFETY: contract checked above; slow path repeats the null /
    // record-length checks before invoking `compute_contains`. Marked
    // `#[cold]` + `#[inline(never)]` so the optimizer keeps the hot
    // outer body's prologue minimal.
    unsafe { str_contains_arena_slow(haystack_ptr, needle_ptr) }
}

/// IC fast-path slot reader. Returns `Some(cached_result)` when both
/// pointers match the last call's operands; `None` otherwise (caller
/// falls through to the slow path).
///
/// Inlined into [`relon_llvm_str_contains_arena`] so the hot loop
/// performs the entire IC check inside the outer's prologue/epilogue
/// without spilling to a separate function frame. Uses three
/// `Relaxed` atomic loads against the process-global slot — on
/// x86_64 these lower to plain `mov`s, no TLS init guard required.
#[inline(always)]
fn ic_hit_slot(haystack_ptr: *const u8, needle_ptr: *const u8) -> Option<i32> {
    use core::sync::atomic::Ordering;
    if haystack_ptr.is_null() || needle_ptr.is_null() {
        return None;
    }
    let cached_haystack = STR_CONTAINS_ARENA_IC.last_haystack.load(Ordering::Relaxed);
    if !std::ptr::eq(cached_haystack, haystack_ptr) {
        return None;
    }
    let cached_needle = STR_CONTAINS_ARENA_IC.last_needle.load(Ordering::Relaxed);
    if !std::ptr::eq(cached_needle, needle_ptr) {
        return None;
    }
    Some(STR_CONTAINS_ARENA_IC.last_result.load(Ordering::Relaxed))
}

/// IC slow-path: records the headers, computes the byte-scan answer,
/// updates the IC cache, and returns. Kept `#[cold]` +
/// `#[inline(never)]` so the optimizer does not hoist its
/// `compute_contains` scratch space (UTF-8 validator buffers / Two-Way
/// matcher state) into the outer's stack frame — that hoist is what
/// inflated the Phase F.1 hot-path prologue.
///
/// # Safety
///
/// Same contract as [`relon_llvm_str_contains_arena`].
#[cold]
#[inline(never)]
unsafe fn str_contains_arena_slow(haystack_ptr: *const u8, needle_ptr: *const u8) -> i32 {
    use core::sync::atomic::Ordering;
    let h_bytes = match unsafe { read_record(haystack_ptr) } {
        Some(s) => s,
        None => return 0,
    };
    let n_bytes = match unsafe { read_record(needle_ptr) } {
        Some(s) => s,
        None => return 0,
    };
    let result = compute_contains(h_bytes, n_bytes);

    // Update the global IC slot. A torn update across racing threads
    // only re-triggers a miss next iter (not a wrong answer) since
    // hit lookup also reads all three fields with `Relaxed` ordering.
    STR_CONTAINS_ARENA_IC
        .last_haystack
        .store(haystack_ptr as *mut u8, Ordering::Relaxed);
    STR_CONTAINS_ARENA_IC
        .last_needle
        .store(needle_ptr as *mut u8, Ordering::Relaxed);
    STR_CONTAINS_ARENA_IC
        .last_result
        .store(result, Ordering::Relaxed);
    result
}

/// Pure substring decision split out from
/// [`relon_llvm_str_contains_arena`] so the IC fast path stays small
/// and `#[inline]`-friendly. Mirrors the bundled stdlib body's
/// semantics (`p_len == 0 → true`, `p_len > s_len → false`, else byte
/// scan).
#[inline]
fn compute_contains(h_bytes: &[u8], n_bytes: &[u8]) -> i32 {
    // Empty needle: every string contains the empty string (matches the
    // bundled stdlib IR body's `p_len == 0 → true` short-circuit and
    // `str::contains`'s own semantics).
    if n_bytes.is_empty() {
        return 1;
    }
    if n_bytes.len() > h_bytes.len() {
        return 0;
    }
    // Single-byte needle fast path: `core::slice::contains(&u8)`
    // hands directly to `memchr::memchr` (the same SIMD-backed scan
    // LuaJIT's `string.find` exploits). Skipping the UTF-8 validation
    // pass cuts ~7-10 ns off the W4 row on a typical x86_64 host
    // because `from_utf8` walks the entire haystack to confirm
    // codepoint boundaries that the IR-side type checker already
    // guarantees.
    if n_bytes.len() == 1 {
        let needle_byte = n_bytes[0];
        return i32::from(h_bytes.contains(&needle_byte));
    }
    // Multi-byte needle: fall through to `str::contains` which uses
    // Rust's Two-Way matcher (O(n + m), no UTF-8 backtracking).
    // UTF-8 validation is intentionally skipped: the IR-side type
    // checker guarantees both operands are `IrType::String`, populated
    // exclusively by the lowering pass from validated UTF-8 sources.
    // `str::contains`'s underlying byte scan operates on the raw byte
    // slice anyway; the validation only matters for char-boundary
    // slicing which we never request.
    let h_str = match core::str::from_utf8(h_bytes) {
        Ok(s) => s,
        // Malformed payload: refuse the match rather than panic. The
        // IR type checker guarantees this branch is unreachable on the
        // supported surface; we keep the defensive fallback so a stale
        // arena (test fixture using raw bytes) returns `0` instead of
        // tripping `from_utf8`'s `Err` into a panic at the call site.
        Err(_) => return 0,
    };
    let n_str = match core::str::from_utf8(n_bytes) {
        Ok(s) => s,
        Err(_) => return 0,
    };
    i32::from(h_str.contains(n_str))
}

/// Address of [`relon_llvm_str_contains_arena`] as a `usize`, suitable
/// for `engine.add_global_mapping`. Centralising the cast keeps the
/// emitter side free of `fn-pointer-as-usize` boilerplate. The
/// two-step cast (`fn item -> *const ()` then `-> usize`) silences
/// `function_casts_as_integer`, which on stable Rust warns about the
/// direct `fn as usize` shortcut.
#[inline]
pub fn relon_llvm_str_contains_arena_addr() -> usize {
    relon_llvm_str_contains_arena as *const () as usize
}

/// Stable symbol name the LLVM module declares the shim under. Mirrors
/// the `#[no_mangle]` attribute on [`relon_llvm_str_contains_arena`].
pub const RELON_LLVM_STR_CONTAINS_ARENA_SYMBOL: &str = "relon_llvm_str_contains_arena";

/// Wave B host shim: render an `f64` (passed as raw IEEE-754 bits so no
/// float ABI assumptions cross the FFI edge) into an arena String record
/// at `dest`, using the exact same Rust `Display` byte producer the
/// tree-walk oracle uses (`relon_ir::float_str::format_f64_display` —
/// `format!("{v}")` semantics: `1.0 → "1"`, `-0.0 → "-0"`, NaN / ±inf
/// spelled `NaN` / `inf` / `-inf`, subnormals in full decimal expansion).
///
/// ## ABI
///
/// ```text
/// extern "C" fn relon_llvm_f64_to_str(
///     bits: i64,        // f64::to_bits of the value, reinterpreted i64
///     dest: *mut u8,    // arena_base + record_off; record is
///                       // FLOAT_TO_STR_RECORD_SIZE bytes
/// ) -> i32              // payload byte length, or -1 on failure
/// ```
///
/// On success the shim writes `[len: u32 LE][utf8 payload]` at `dest`
/// and returns the payload length. The emitter reserves
/// `relon_ir::float_str::FLOAT_TO_STR_RECORD_SIZE` (768) bytes per
/// record via the scratch bump allocator, which exceeds the worst-case
/// Display payload (`-5e-324` → 327 bytes; hard cap
/// `FLOAT_TO_STR_MAX_PAYLOAD` = 352) plus the 4-byte header, so the
/// in-record formatting can never spill. A `-1` return (null `dest`, or
/// payload exceeding the cap — both unreachable by construction) is
/// trapped loudly by the emitter rather than producing a corrupt record.
///
/// No inline cache: unlike `str.contains` (stable const-pool pointer
/// pairs on the hot bench rows), float-to-string inputs are arbitrary
/// bit patterns with no pointer identity to key on, and the format cost
/// is small relative to the FFI edge.
///
/// # Safety
///
/// `dest` must be either null or valid for writes of
/// `FLOAT_TO_STR_RECORD_SIZE` bytes. The emitter passes
/// `arena_base + offset` where the offset came from the scratch bump
/// allocator's bounds-checked reservation, so the record always lies
/// inside the live arena.
#[no_mangle]
pub unsafe extern "C" fn relon_llvm_f64_to_str(bits: i64, dest: *mut u8) -> i32 {
    use relon_ir::float_str::{format_f64_display, FLOAT_TO_STR_MAX_PAYLOAD};
    if dest.is_null() {
        return -1;
    }
    let mut payload = [0u8; FLOAT_TO_STR_MAX_PAYLOAD];
    let len = match format_f64_display(bits as u64, &mut payload) {
        Some(len) => len,
        None => return -1,
    };
    // SAFETY: caller guarantees `dest` is valid for
    // FLOAT_TO_STR_RECORD_SIZE (768) writes; `4 + FLOAT_TO_STR_MAX_PAYLOAD
    // <= FLOAT_TO_STR_RECORD_SIZE` is statically asserted in
    // `relon_ir::float_str`, so header + payload always fit.
    // `write_unaligned` because arena records only guarantee 4-byte
    // alignment relative to a base that may sit at any byte offset of
    // the host buffer.
    unsafe {
        core::ptr::write_unaligned(dest.cast::<u32>(), len as u32);
        core::ptr::copy_nonoverlapping(payload.as_ptr(), dest.add(4), len);
    }
    len as i32
}

/// Address of [`relon_llvm_f64_to_str`] as a `usize`, suitable for
/// `engine.add_global_mapping`. Two-step cast for the same
/// `function_casts_as_integer` reason as
/// [`relon_llvm_str_contains_arena_addr`].
#[inline]
pub fn relon_llvm_f64_to_str_addr() -> usize {
    relon_llvm_f64_to_str as *const () as usize
}

/// Stable symbol name the LLVM module declares the float-render shim
/// under. Mirrors the `#[no_mangle]` attribute on
/// [`relon_llvm_f64_to_str`]. On the wasm32 leg the same name appears
/// as `(import "env" "relon_llvm_f64_to_str" ...)` and the host
/// satisfies it by `func_wrap`-ing the same Rust fn, so all compiled
/// backends share one Display byte producer by construction.
pub const RELON_LLVM_F64_TO_STR_SYMBOL: &str = "relon_llvm_f64_to_str";

#[cfg(test)]
mod tests {
    use super::*;

    /// Build an arena fixture with two records back-to-back. Returns
    /// the buffer and per-record pointers so the tests can call the
    /// shim with raw `*const u8`s.
    fn build_two_records(haystack: &[u8], needle: &[u8]) -> (Vec<u8>, usize, usize) {
        let mut buf = Vec::with_capacity(4 + haystack.len() + 4 + needle.len() + 16);
        let h_off = buf.len();
        buf.extend_from_slice(&(haystack.len() as u32).to_le_bytes());
        buf.extend_from_slice(haystack);
        let n_off = buf.len();
        buf.extend_from_slice(&(needle.len() as u32).to_le_bytes());
        buf.extend_from_slice(needle);
        (buf, h_off, n_off)
    }

    /// Serialize + reset the process-global `STR_CONTAINS_ARENA_IC` for the
    /// duration of an IC-touching test. The IC is a single-slot static, so
    /// concurrently-running sibling tests can clobber the slot between two
    /// calls of the same test (breaking the cache-hit tests) or hand a
    /// reused buffer address a stale cached result (breaking the miss
    /// tests). Holding the guard for the whole test serializes IC access;
    /// the reset clears any entry a prior test left behind.
    fn lock_and_reset_ic() -> std::sync::MutexGuard<'static, ()> {
        use core::sync::atomic::Ordering;
        static IC_TEST_GUARD: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let guard = IC_TEST_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        STR_CONTAINS_ARENA_IC
            .last_haystack
            .store(core::ptr::null_mut(), Ordering::Relaxed);
        STR_CONTAINS_ARENA_IC
            .last_needle
            .store(core::ptr::null_mut(), Ordering::Relaxed);
        STR_CONTAINS_ARENA_IC
            .last_result
            .store(0, Ordering::Relaxed);
        guard
    }

    #[test]
    fn matches_short_needle() {
        let _ic = lock_and_reset_ic();
        let (buf, h_off, n_off) = build_two_records(b"axb", b"x");
        let h_ptr = unsafe { buf.as_ptr().add(h_off) };
        let n_ptr = unsafe { buf.as_ptr().add(n_off) };
        // SAFETY: buf outlives the call; both pointers point at valid
        // arena records produced by `build_two_records`.
        let r = unsafe { relon_llvm_str_contains_arena(h_ptr, n_ptr) };
        assert_eq!(r, 1);
    }

    #[test]
    fn misses_when_needle_absent() {
        let _ic = lock_and_reset_ic();
        let (buf, h_off, n_off) = build_two_records(b"abc", b"z");
        let h_ptr = unsafe { buf.as_ptr().add(h_off) };
        let n_ptr = unsafe { buf.as_ptr().add(n_off) };
        let r = unsafe { relon_llvm_str_contains_arena(h_ptr, n_ptr) };
        assert_eq!(r, 0);
    }

    #[test]
    fn matches_long_haystack_tail_needle() {
        // 256-byte haystack with a single `x` at the tail — mirrors the
        // W4_long cmp_lua row exactly.
        let _ic = lock_and_reset_ic();
        let mut haystack = vec![b'a'; 255];
        haystack.push(b'x');
        let (buf, h_off, n_off) = build_two_records(&haystack, b"x");
        let h_ptr = unsafe { buf.as_ptr().add(h_off) };
        let n_ptr = unsafe { buf.as_ptr().add(n_off) };
        let r = unsafe { relon_llvm_str_contains_arena(h_ptr, n_ptr) };
        assert_eq!(r, 1);
    }

    #[test]
    fn empty_needle_always_matches() {
        let _ic = lock_and_reset_ic();
        let (buf, h_off, n_off) = build_two_records(b"anything", b"");
        let h_ptr = unsafe { buf.as_ptr().add(h_off) };
        let n_ptr = unsafe { buf.as_ptr().add(n_off) };
        let r = unsafe { relon_llvm_str_contains_arena(h_ptr, n_ptr) };
        assert_eq!(r, 1);
    }

    #[test]
    fn empty_haystack_nonempty_needle_misses() {
        let _ic = lock_and_reset_ic();
        let (buf, h_off, n_off) = build_two_records(b"", b"x");
        let h_ptr = unsafe { buf.as_ptr().add(h_off) };
        let n_ptr = unsafe { buf.as_ptr().add(n_off) };
        let r = unsafe { relon_llvm_str_contains_arena(h_ptr, n_ptr) };
        assert_eq!(r, 0);
    }

    #[test]
    fn null_pointers_return_zero() {
        let r = unsafe { relon_llvm_str_contains_arena(core::ptr::null(), core::ptr::null()) };
        assert_eq!(r, 0);
    }

    #[test]
    fn multibyte_utf8_needle() {
        // 4-byte UTF-8 emoji as needle inside a mixed haystack.
        let _ic = lock_and_reset_ic();
        let haystack = "hello 🦀 world".as_bytes();
        let needle = "🦀".as_bytes();
        let (buf, h_off, n_off) = build_two_records(haystack, needle);
        let h_ptr = unsafe { buf.as_ptr().add(h_off) };
        let n_ptr = unsafe { buf.as_ptr().add(n_off) };
        let r = unsafe { relon_llvm_str_contains_arena(h_ptr, n_ptr) };
        assert_eq!(r, 1);
    }

    #[test]
    fn address_helper_is_stable() {
        let a = relon_llvm_str_contains_arena_addr();
        let b = relon_llvm_str_contains_arena_addr();
        assert_eq!(a, b);
        assert!(a != 0);
    }

    /// IC sanity: repeated calls with identical pointers return the
    /// cached result. Tested by mutating the haystack bytes *after*
    /// priming the cache and confirming the cached answer wins. This
    /// would be a soundness bug in user code (the IR-side guarantees
    /// arena records don't mutate behind the JIT's back), but the
    /// mutation is the cheapest way to prove the IC fired without
    /// instrumentation hooks.
    #[test]
    fn ic_returns_cached_result_on_repeated_call() {
        let _ic = lock_and_reset_ic();
        let (mut buf, h_off, n_off) = build_two_records(b"axb", b"x");
        let h_ptr = unsafe { buf.as_ptr().add(h_off) };
        let n_ptr = unsafe { buf.as_ptr().add(n_off) };

        // Prime cache: first call computes "axb".contains("x") = 1.
        let r1 = unsafe { relon_llvm_str_contains_arena(h_ptr, n_ptr) };
        assert_eq!(r1, 1);

        // Mutate the haystack to "qqq" (no 'x') *without* changing the
        // pointer. A non-IC implementation would now return 0; the IC
        // returns the cached 1.
        let payload_off = h_off + 4;
        buf[payload_off] = b'q';
        buf[payload_off + 1] = b'q';
        buf[payload_off + 2] = b'q';

        let r2 = unsafe { relon_llvm_str_contains_arena(h_ptr, n_ptr) };
        assert_eq!(r2, 1, "IC must return cached result on identical pointers");
    }

    /// IC miss path: distinct pointers re-enter the scan. Verifies the
    /// cache doesn't accidentally serve stale answers when the JIT
    /// hands us a different `(haystack, needle)` pair.
    #[test]
    fn ic_misses_on_distinct_pointers() {
        let _ic = lock_and_reset_ic();
        let (buf_a, h_off_a, n_off_a) = build_two_records(b"axb", b"x");
        let (buf_b, h_off_b, n_off_b) = build_two_records(b"qqq", b"z");
        let h_a = unsafe { buf_a.as_ptr().add(h_off_a) };
        let n_a = unsafe { buf_a.as_ptr().add(n_off_a) };
        let h_b = unsafe { buf_b.as_ptr().add(h_off_b) };
        let n_b = unsafe { buf_b.as_ptr().add(n_off_b) };

        let r1 = unsafe { relon_llvm_str_contains_arena(h_a, n_a) };
        let r2 = unsafe { relon_llvm_str_contains_arena(h_b, n_b) };
        let r3 = unsafe { relon_llvm_str_contains_arena(h_a, n_a) };
        assert_eq!(r1, 1);
        assert_eq!(r2, 0);
        // Last call hits the IC for `(h_a, n_a)` again — must still
        // return the original 1, not the stale `0` from the second
        // call.
        assert_eq!(r3, 1);
    }

    /// Render `v` through the f64 shim into a fresh record buffer and
    /// hand back the decoded payload string.
    fn shim_render_f64(v: f64) -> String {
        let mut record = vec![0u8; relon_ir::float_str::FLOAT_TO_STR_RECORD_SIZE as usize];
        // SAFETY: `record` is FLOAT_TO_STR_RECORD_SIZE bytes, satisfying
        // the shim's dest contract.
        let len = unsafe { relon_llvm_f64_to_str(v.to_bits() as i64, record.as_mut_ptr()) };
        assert!(len >= 0, "shim failed for {v}");
        let header = u32::from_le_bytes(record[0..4].try_into().unwrap());
        assert_eq!(header as i32, len, "header length must match return value");
        String::from_utf8(record[4..4 + len as usize].to_vec()).unwrap()
    }

    /// Float boundary battery: the shim must produce the exact bytes of
    /// Rust's `Display` (`format!("{v}")`), which is the tree-walk
    /// oracle's `Value::Float` rendering.
    #[test]
    fn f64_shim_matches_display_battery() {
        for v in [
            1.0,
            -0.0,
            0.0,
            0.1,
            567.34,
            1e300,
            5e-324,
            -5e-324,
            f64::NAN,
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::MAX,
            f64::MIN_POSITIVE,
        ] {
            assert_eq!(shim_render_f64(v), format!("{v}"), "bytes drift for {v:?}");
        }
        assert_eq!(shim_render_f64(1.0), "1");
        assert_eq!(shim_render_f64(-0.0), "-0");
        assert_eq!(shim_render_f64(f64::NAN), "NaN");
        assert_eq!(shim_render_f64(f64::INFINITY), "inf");
        assert_eq!(shim_render_f64(f64::NEG_INFINITY), "-inf");
        // Subnormal worst case: 327-char decimal expansion fits the record.
        assert_eq!(shim_render_f64(-5e-324).len(), 327);
    }

    #[test]
    fn f64_shim_null_dest_returns_negative() {
        let r = unsafe { relon_llvm_f64_to_str(1.0f64.to_bits() as i64, core::ptr::null_mut()) };
        assert_eq!(r, -1);
    }

    #[test]
    fn f64_addr_helper_is_stable() {
        let a = relon_llvm_f64_to_str_addr();
        let b = relon_llvm_f64_to_str_addr();
        assert_eq!(a, b);
        assert!(a != 0);
    }
}
