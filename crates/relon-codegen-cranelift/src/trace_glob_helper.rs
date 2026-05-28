//! Trace-JIT host helper backing `TraceOp::StrGlobMatch`.
//!
//! 2026-05-21 Tier-2: the trace emitter lowers
//! `TraceOp::StrGlobMatch { dst, s, pattern }` to a direct
//! `call __relon_str_glob_match(s, pattern) -> i32` against the
//! host-provided FuncId. The helper body lives here (rather than in
//! `relon-trace-jit::runtime`) because the matcher implementation
//! itself sits in [`relon_ir::glob::glob_match`] and the trace JIT
//! crate intentionally has no `relon-ir` dependency (see the
//! `relon-trace-jit` Cargo.toml note).
//!
//! ## ABI
//!
//! ```text
//! extern "C" fn __relon_str_glob_match(
//!     s:       *const StringRef,   // haystack
//!     pattern: *const StringRef,   // glob pattern
//! ) -> i32                          // 1 = match, 0 = no-match
//! ```
//!
//! Both arguments are opaque `*const StringRef` pointers — the exact
//! pointer shape the F-D7 `__relon_str_*` shims already use. A null
//! pointer in either slot, or invalid UTF-8 in the payload, surfaces
//! as `0` (no-match) — matching `StrContains`'s defensive defaults
//! rather than letting a host-side panic escape into the trace.

use relon_trace_jit::runtime::StringRef;

/// Trace-callable helper that runs [`relon_ir::glob::glob_match`]
/// against the two `StringRef` payloads.
///
/// # Safety
///
/// `s` and `pattern` must each be either null or a valid
/// `*const StringRef` whose `(ptr, len)` payload outlives the call.
/// The trace emitter only produces this call against SSAs that came
/// from `LocalGet` / `Load(StringRef::ptr_offset, StringRef::len_offset)`
/// pairs the host arranged ahead of trace entry, so the pointer
/// lifetime contract holds by construction.
#[no_mangle]
pub unsafe extern "C" fn __relon_str_glob_match(
    s: *const StringRef,
    pattern: *const StringRef,
) -> i32 {
    // SAFETY: caller upholds the pointer lifetime contract above; the
    // `as_str` helper itself null-checks before dereferencing.
    let s_str = match unsafe { StringRef::as_str(s) } {
        Some(v) => v,
        None => return 0,
    };
    let p_str = match unsafe { StringRef::as_str(pattern) } {
        Some(v) => v,
        None => return 0,
    };
    if relon_ir::glob::glob_match(s_str, p_str) {
        1
    } else {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Round-trip both arms of the matcher through the trace helper so
    /// the wrapper logic (null guards + UTF-8 decode) stays
    /// behaviour-equivalent with the underlying algorithm.
    #[test]
    fn matches_simple_glob() {
        let s = StringRef::from_static("hello world");
        let p = StringRef::from_static("hello *");
        let r = unsafe { __relon_str_glob_match(s, p) };
        assert_eq!(r, 1);
    }

    #[test]
    fn rejects_non_matching_glob() {
        let s = StringRef::from_static("hello world");
        let p = StringRef::from_static("goodbye *");
        let r = unsafe { __relon_str_glob_match(s, p) };
        assert_eq!(r, 0);
    }

    #[test]
    fn null_pointer_returns_no_match() {
        let s = StringRef::from_static("anything");
        let r1 = unsafe { __relon_str_glob_match(std::ptr::null(), s) };
        let r2 = unsafe { __relon_str_glob_match(s, std::ptr::null()) };
        let r3 = unsafe { __relon_str_glob_match(std::ptr::null(), std::ptr::null()) };
        assert_eq!(r1, 0);
        assert_eq!(r2, 0);
        assert_eq!(r3, 0);
    }

    #[test]
    fn unicode_payload_matches_through_helper() {
        // 4-byte UTF-8 emoji + 2-byte Greek + ASCII mixed, same shape
        // the AOT `glob_helper` exercises.
        let s = StringRef::from_static("αβγ🦀");
        let p = StringRef::from_static("α*🦀");
        let r = unsafe { __relon_str_glob_match(s, p) };
        assert_eq!(r, 1);
    }

    #[test]
    fn question_mark_matches_one_codepoint() {
        let s = StringRef::from_static("ab");
        let p = StringRef::from_static("a?");
        let r = unsafe { __relon_str_glob_match(s, p) };
        assert_eq!(r, 1);
    }
}
