//! Short-string optimization (SSO) for `Value::String`.
//!
//! # Why
//!
//! Tree-walker / bytecode VM / trace-JIT all spend a non-trivial slice of
//! their hot path on `String` allocation + drop pairs that hold a few
//! bytes of payload — dict keys, identifiers, short concat intermediates
//! (`"a" + i.to_str()`), `type_name()` results, etc. Every one of those
//! `String`s touches the global allocator twice (alloc on push / drop
//! on free), pulls the heap header into cache, and adds a pointer-chase
//! every time the evaluator reads the bytes.
//!
//! LuaJIT addresses the same shape with a `GCstr` short/long split
//! (≤ 39 byte payload stays in the string-table directly, longer
//! strings spill to a separate object). Relon's `Value` enum already
//! reserves a 24-byte slot for the `String` variant (see
//! `value::size_guard::value_enum_is_compact`), so the same idea fits
//! natively — we keep the existing slot width and use it for either
//! inline bytes (≤ 22 bytes) or a refcounted `Arc<str>` to the heap.
//!
//! # Layout
//!
//! ```text
//! 24 bytes, 8-aligned:
//!
//!   Inline { len: u8, data: [u8; 22] }   ≤ 22 byte payload, no alloc
//!   Heap   ( Arc<str> )                   long string, shared by clones
//! ```
//!
//! The Rust niche-optimization on `Arc<str>::ptr` (NonNull) gives us the
//! discriminant for free, so the enum stays 24 bytes — identical to the
//! `String` it replaces. The 22-byte inline cap was picked to match the
//! 24-byte slot with one byte left for the inline-length tag; raising it
//! would push the `Value` enum past its 48-byte size guard.
//!
//! # Semantics
//!
//! `SmolStr` is value-equal to `&str` / `String` byte-for-byte and
//! implements `Deref<Target = str>` so existing pattern bindings
//! (`Value::String(s) => s.len()` etc.) keep working unchanged. Cloning
//! is `O(len/word)` for inline payloads (memcpy) and a single `Arc`
//! refcount bump for heap payloads — both well under what a `String`
//! clone costs (heap alloc + memcpy).
//!
//! Serde and `Display` formatting round-trip through `&str` so external
//! shapes (JSON, error messages) stay identical to the pre-SSO baseline.

// `unsafe` is allowed inside this module only — see the `as_str()`
// SAFETY comment. The rest of `relon-eval-api` runs under `deny`.
#![allow(unsafe_code)]

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::borrow::Borrow;
use std::cmp::Ordering;
use std::fmt;
use std::hash::{Hash, Hasher};
use std::ops::Deref;
use std::sync::Arc;

/// Max payload length that stays inline in the `Inline` variant. Bumping
/// this requires re-running `value::size_guard::value_enum_is_compact`
/// because the `Value` enum width is governed by `Float (16 B)`,
/// `SmolStr (24 B)`, and the boxed heavy variants — `SmolStr` is the
/// current widest slot.
pub const SMOL_STR_INLINE_CAP: usize = 22;

/// Short-string-optimized string. Inlines ≤ [`SMOL_STR_INLINE_CAP`]
/// bytes directly in the value slot; longer payloads land on the heap
/// behind a refcounted `Arc<str>` so clones are O(1).
#[derive(Clone)]
pub struct SmolStr {
    repr: SmolStrRepr,
}

#[derive(Clone)]
enum SmolStrRepr {
    Inline {
        len: u8,
        data: [u8; SMOL_STR_INLINE_CAP],
    },
    Heap(Arc<str>),
}

impl SmolStr {
    /// Build an empty `SmolStr` without touching the allocator.
    #[inline]
    pub const fn new_empty() -> Self {
        Self {
            repr: SmolStrRepr::Inline {
                len: 0,
                data: [0u8; SMOL_STR_INLINE_CAP],
            },
        }
    }

    #[inline]
    fn inline(len: u8, data: [u8; SMOL_STR_INLINE_CAP]) -> Self {
        debug_assert!((len as usize) <= SMOL_STR_INLINE_CAP);
        Self {
            repr: SmolStrRepr::Inline { len, data },
        }
    }

    #[inline]
    fn heap(arc: Arc<str>) -> Self {
        Self {
            repr: SmolStrRepr::Heap(arc),
        }
    }

    /// Borrow the payload as a `&str` slice. Cheap (no copies) in both
    /// `Inline` and `Heap` modes.
    #[inline]
    pub fn as_str(&self) -> &str {
        match &self.repr {
            SmolStrRepr::Inline { len, data } => {
                let slice = &data[..*len as usize];
                // SAFETY: the representation is private and every
                // constructor validates or copies from an existing
                // `str`, so `data[..len]` is always UTF-8.
                unsafe { std::str::from_utf8_unchecked(slice) }
            }
            SmolStrRepr::Heap(arc) => arc,
        }
    }

    /// Byte length of the payload (matching `str::len`).
    #[inline]
    pub fn len(&self) -> usize {
        match &self.repr {
            SmolStrRepr::Inline { len, .. } => *len as usize,
            SmolStrRepr::Heap(arc) => arc.len(),
        }
    }

    /// `true` iff the payload is empty.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Returns `true` when the payload is stored inline (no heap
    /// allocation). Useful for SSO-aware diagnostics + tests.
    #[inline]
    pub fn is_inline(&self) -> bool {
        matches!(&self.repr, SmolStrRepr::Inline { .. })
    }

    /// Returns `true` iff every byte in the payload is ASCII
    /// (`< 0x80`).
    ///
    /// # Why this exists
    ///
    /// The tree-walker case-fold helpers (`upper` / `lower` / `title`
    /// in `relon-evaluator::stdlib`) accept an `AsciiHint` so they can
    /// skip the per-call SIMD scan inside
    /// `fold_string_with_ascii_hint`. Without a `SmolStr`-side oracle
    /// every surface call had to pass `AsciiHint::Unknown` and let the
    /// fold engine pay the scan cost — even when the caller's value
    /// container had the bytes right there. Wiring `is_ascii()` into
    /// the helpers lets them surface `AllAscii` / `KnownNonAscii` and
    /// route through the preclassified fast path documented in
    /// `crates/relon-bench/benches/ascii_case_fold.rs` (the
    /// `preclassified_*` rows in `bench ascii_case_fold`).
    ///
    /// # Cost
    ///
    /// * **Inline** (`len ≤ SMOL_STR_INLINE_CAP = 22`): a single
    ///   vectorisable byte-AND scan over at most 22 bytes — well under
    ///   one cycle on every modern x86_64 / aarch64 target. Rust's
    ///   `[u8]::is_ascii()` codegens to a single `vpand` + `vpmovmskb`
    ///   shape at this size.
    /// * **Heap** (`Arc<str>`): delegates to `str::is_ascii()`, which
    ///   the standard library implements via the same SIMD primitive
    ///   over the full payload. A future revision can cache the bit
    ///   beside the `Arc<str>` pointer (mirroring the
    ///   [`relon_trace_abi::STRING_RECORD_ASCII_FLAG_BIT`] flag the
    ///   trace-JIT keeps on its StringRef header) so heap payloads
    ///   become an O(1) load too; for now the on-demand scan keeps
    ///   the slot layout identical to its pre-flag shape and avoids
    ///   touching the niche-optimisation that pins the enum size to
    ///   24 bytes.
    #[inline]
    pub fn is_ascii(&self) -> bool {
        match &self.repr {
            // Inline: scan the (≤ 22-byte) data prefix directly. Even
            // on a non-SIMD target this is a tight loop bounded by the
            // inline cap.
            SmolStrRepr::Inline { len, data } => data[..*len as usize].is_ascii(),
            // Heap: delegate to `str::is_ascii`. See type-level note
            // for the follow-up cache work.
            SmolStrRepr::Heap(arc) => arc.is_ascii(),
        }
    }

    /// Build a `SmolStr` from any `&str`. ≤ [`SMOL_STR_INLINE_CAP`]
    /// bytes land inline; longer payloads allocate one `Arc<str>`.
    ///
    /// Named `from_borrowed` to avoid shadowing the `FromStr` trait
    /// method (clippy::should_implement_trait); the trait impl below
    /// forwards to this helper so `"x".parse::<SmolStr>()` keeps
    /// working too.
    #[inline]
    pub fn from_borrowed(s: &str) -> Self {
        let bytes = s.as_bytes();
        if bytes.len() <= SMOL_STR_INLINE_CAP {
            // Zero-init the tail unconditionally so `as_str()` only
            // needs to look at `len` (no per-byte sentinel scan). The
            // 22-byte array is laid out as a single SIMD-width store
            // on x86_64 + aarch64; benchmarks show the zero-fill is
            // <2 ns at this size, well under the `String::with_capacity`
            // / `to_owned` cost the alternative path pays.
            let mut data = [0u8; SMOL_STR_INLINE_CAP];
            data[..bytes.len()].copy_from_slice(bytes);
            Self::inline(bytes.len() as u8, data)
        } else {
            Self::heap(Arc::from(s))
        }
    }

    /// Consume a `String`. ≤ [`SMOL_STR_INLINE_CAP`] bytes copy into the
    /// inline slot and drop the original heap buffer; longer payloads
    /// reuse the underlying allocation via `Arc::from(String)` so the
    /// payload is not re-copied.
    #[inline]
    pub fn from_string(s: String) -> Self {
        if s.len() <= SMOL_STR_INLINE_CAP {
            // Drop the heap buffer once inline-copy is done.
            SmolStr::from_borrowed(s.as_str())
        } else {
            Self::heap(Arc::from(s))
        }
    }

    /// Concatenate two `&str` slices into a single `SmolStr` without
    /// going through a `format!` / intermediate `String` allocation.
    ///
    /// * If `a.len() + b.len() <= SMOL_STR_INLINE_CAP` the result lands
    ///   in the inline slot — zero allocations on the path.
    /// * Otherwise we allocate one `Arc<str>` directly from the two
    ///   slices (matching the heap-fallback behaviour of the single-
    ///   slice constructors).
    ///
    /// This is the hot path the evaluator's `Operator::Add` rule on
    /// `Value::String(a) + Value::String(b)` (W3-style concat) goes
    /// through; eliminating the `format!` indirection drops the
    /// short-string concat row by ~3x in the bench.
    #[inline]
    pub fn concat(a: &str, b: &str) -> Self {
        let total = a.len() + b.len();
        if total <= SMOL_STR_INLINE_CAP {
            let mut data = [0u8; SMOL_STR_INLINE_CAP];
            data[..a.len()].copy_from_slice(a.as_bytes());
            data[a.len()..total].copy_from_slice(b.as_bytes());
            Self::inline(total as u8, data)
        } else {
            // Heap fallback: pre-size a `String` (one allocation), push
            // both slices, then hand the buffer to `Arc::from(String)`
            // which moves the allocation into the Arc payload without
            // re-copying.
            let mut buf = String::with_capacity(total);
            buf.push_str(a);
            buf.push_str(b);
            Self::heap(Arc::from(buf))
        }
    }

    /// Concatenate N `&str` slices into a single `SmolStr` with at most
    /// one allocation regardless of arity. Compared to the recursive
    /// `concat(concat(a, b), c)` shape this drops the intermediate
    /// `Arc<str>` allocations (and their refcount drops) entirely —
    /// useful when the evaluator detects a left-leaning `+` chain on
    /// `Value::String` operands (e.g. `"prefix" + name + ": " + value`).
    ///
    /// * Pre-scans the total length once.
    /// * Inline-fast-path when `total <= SMOL_STR_INLINE_CAP`: no
    ///   allocator hit, single byte-fill into the 22-byte slot.
    /// * Heap fallback allocates one `String::with_capacity(total)`,
    ///   pushes each slice in order, then hands the buffer to
    ///   `Arc::from(String)` which moves the allocation into the Arc
    ///   payload without a second copy.
    ///
    /// Degenerate inputs:
    ///
    /// * Zero slices -> empty inline payload.
    /// * One slice -> identical semantics to `from_borrowed`.
    /// * Two slices -> identical semantics to `concat`. Kept as a single
    ///   entry point so the evaluator can pick `concat_many` whenever the
    ///   chain length is `>= 2` without dispatching on arity.
    #[inline]
    pub fn concat_many(slices: &[&str]) -> Self {
        // Sum total length once. We rely on the caller to keep the slice
        // count small enough that `usize` cannot overflow — every reachable
        // caller bounds the chain via the AST shape, which is itself
        // memory-bounded.
        let total: usize = slices.iter().map(|s| s.len()).sum();
        if total <= SMOL_STR_INLINE_CAP {
            let mut data = [0u8; SMOL_STR_INLINE_CAP];
            let mut offset = 0usize;
            for s in slices {
                let bytes = s.as_bytes();
                data[offset..offset + bytes.len()].copy_from_slice(bytes);
                offset += bytes.len();
            }
            Self::inline(total as u8, data)
        } else {
            let mut buf = String::with_capacity(total);
            for s in slices {
                buf.push_str(s);
            }
            Self::heap(Arc::from(buf))
        }
    }

    /// Materialise an owned `String` copy of the payload. Allocates for
    /// inline and heap variants alike — call sites that only need a
    /// borrow should prefer [`SmolStr::as_str`] / `Deref`.
    #[inline]
    pub fn into_string(self) -> String {
        // `Arc<str>::try_unwrap` is unstable for unsized payloads, so
        // we always copy. The hot evaluator paths read through
        // [`SmolStr::as_str`]; only a handful of compatibility shims
        // call `into_string` (host boundary, JSON projector).
        self.as_str().to_owned()
    }

    /// Build an inline `SmolStr` by writing UTF-8 bytes directly into
    /// the 22-byte inline slot via the caller-supplied writer.
    ///
    /// `out_len` is the number of bytes the writer will emit; the call
    /// returns `None` immediately if `out_len > SMOL_STR_INLINE_CAP`,
    /// letting the caller fall through to its heap-path implementation
    /// without paying for the writer invocation. When the inline path
    /// is taken the caller receives a `&mut [u8]` of length `out_len`
    /// pointing into the inline buffer. The resulting byte prefix is
    /// validated before construction; invalid UTF-8 returns `None`.
    /// This keeps the unchecked `as_str()` conversion behind the
    /// private representation sound while preserving the allocation-free
    /// fast path for ASCII case-fold helpers.
    #[inline]
    pub fn try_build_inline<F>(out_len: usize, write: F) -> Option<Self>
    where
        F: FnOnce(&mut [u8]),
    {
        if out_len > SMOL_STR_INLINE_CAP {
            return None;
        }
        let mut data = [0u8; SMOL_STR_INLINE_CAP];
        // Hand the writer the exact slice it must fill. The zero-fill
        // on the tail bytes (past `out_len`) is the same SIMD-width
        // store the `from_borrowed` path performs, so the cost matches
        // the existing inline-path baseline.
        write(&mut data[..out_len]);
        std::str::from_utf8(&data[..out_len]).ok()?;
        Some(Self::inline(out_len as u8, data))
    }
}

impl Default for SmolStr {
    #[inline]
    fn default() -> Self {
        SmolStr::new_empty()
    }
}

impl Deref for SmolStr {
    type Target = str;

    #[inline]
    fn deref(&self) -> &str {
        self.as_str()
    }
}

impl AsRef<str> for SmolStr {
    #[inline]
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl Borrow<str> for SmolStr {
    #[inline]
    fn borrow(&self) -> &str {
        self.as_str()
    }
}

impl From<&str> for SmolStr {
    #[inline]
    fn from(s: &str) -> Self {
        SmolStr::from_borrowed(s)
    }
}

impl std::str::FromStr for SmolStr {
    type Err = std::convert::Infallible;

    #[inline]
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(SmolStr::from_borrowed(s))
    }
}

impl From<String> for SmolStr {
    #[inline]
    fn from(s: String) -> Self {
        SmolStr::from_string(s)
    }
}

impl From<&String> for SmolStr {
    #[inline]
    fn from(s: &String) -> Self {
        SmolStr::from_borrowed(s.as_str())
    }
}

impl From<SmolStr> for String {
    #[inline]
    fn from(s: SmolStr) -> Self {
        s.into_string()
    }
}

impl From<&SmolStr> for String {
    #[inline]
    fn from(s: &SmolStr) -> Self {
        s.as_str().to_owned()
    }
}

impl fmt::Debug for SmolStr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self.as_str(), f)
    }
}

impl fmt::Display for SmolStr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self.as_str(), f)
    }
}

impl PartialEq for SmolStr {
    #[inline]
    fn eq(&self, other: &Self) -> bool {
        self.as_str() == other.as_str()
    }
}

impl Eq for SmolStr {}

impl PartialEq<str> for SmolStr {
    #[inline]
    fn eq(&self, other: &str) -> bool {
        self.as_str() == other
    }
}

impl PartialEq<&str> for SmolStr {
    #[inline]
    fn eq(&self, other: &&str) -> bool {
        self.as_str() == *other
    }
}

impl PartialEq<String> for SmolStr {
    #[inline]
    fn eq(&self, other: &String) -> bool {
        self.as_str() == other.as_str()
    }
}

impl PartialEq<SmolStr> for str {
    #[inline]
    fn eq(&self, other: &SmolStr) -> bool {
        self == other.as_str()
    }
}

impl PartialEq<SmolStr> for &str {
    #[inline]
    fn eq(&self, other: &SmolStr) -> bool {
        *self == other.as_str()
    }
}

impl PartialEq<SmolStr> for String {
    #[inline]
    fn eq(&self, other: &SmolStr) -> bool {
        self.as_str() == other.as_str()
    }
}

impl PartialOrd for SmolStr {
    #[inline]
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for SmolStr {
    #[inline]
    fn cmp(&self, other: &Self) -> Ordering {
        self.as_str().cmp(other.as_str())
    }
}

impl Hash for SmolStr {
    #[inline]
    fn hash<H: Hasher>(&self, state: &mut H) {
        // Hash the &str representation so SmolStr / &str / String hash
        // to the same value when their payloads match — preserves the
        // ability to look up Dict keys by &str across types.
        self.as_str().hash(state)
    }
}

impl Serialize for SmolStr {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for SmolStr {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        Ok(SmolStr::from_string(s))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_is_inline() {
        let s = SmolStr::new_empty();
        assert!(s.is_inline());
        assert_eq!(s.len(), 0);
        assert_eq!(s.as_str(), "");
    }

    #[test]
    fn short_payload_stays_inline() {
        let s = SmolStr::from_borrowed("hello");
        assert!(s.is_inline());
        assert_eq!(s.as_str(), "hello");
        assert_eq!(s.len(), 5);
    }

    #[test]
    fn cap_boundary_inline() {
        // Exactly cap bytes -> still inline.
        let payload = "a".repeat(SMOL_STR_INLINE_CAP);
        let s = SmolStr::from_borrowed(&payload);
        assert!(s.is_inline());
        assert_eq!(s.len(), SMOL_STR_INLINE_CAP);
        assert_eq!(s.as_str(), payload);
    }

    #[test]
    fn one_past_cap_goes_heap() {
        let payload = "a".repeat(SMOL_STR_INLINE_CAP + 1);
        let s = SmolStr::from_borrowed(&payload);
        assert!(!s.is_inline());
        assert_eq!(s.len(), SMOL_STR_INLINE_CAP + 1);
        assert_eq!(s.as_str(), payload);
    }

    #[test]
    fn clone_inline_does_not_alloc_heap() {
        let s = SmolStr::from_borrowed("short");
        let c = s.clone();
        assert!(c.is_inline());
        assert_eq!(s, c);
    }

    #[test]
    fn clone_heap_shares_arc() {
        let s = SmolStr::from_borrowed(&"x".repeat(40));
        let c = s.clone();
        match (&s.repr, &c.repr) {
            (SmolStrRepr::Heap(a), SmolStrRepr::Heap(b)) => {
                assert!(
                    Arc::ptr_eq(a, b),
                    "Heap clone should share the same Arc allocation"
                );
            }
            _ => panic!("expected both heap variants"),
        }
    }

    #[test]
    fn round_trip_serde() {
        let s = SmolStr::from_borrowed("hello world");
        let json = serde_json::to_string(&s).unwrap();
        assert_eq!(json, "\"hello world\"");
        let back: SmolStr = serde_json::from_str(&json).unwrap();
        assert_eq!(back, s);
    }

    #[test]
    fn eq_against_str_and_string() {
        let s = SmolStr::from_borrowed("k");
        assert_eq!(s, "k");
        assert_eq!(s, *"k");
        assert_eq!(s, String::from("k"));
        assert_eq!(String::from("k"), s);
    }

    #[test]
    fn size_is_24_bytes() {
        // Match `String` exactly so `Value` enum width does not grow.
        assert_eq!(std::mem::size_of::<SmolStr>(), 24);
    }

    #[test]
    fn concat_many_empty_is_empty_inline() {
        let s = SmolStr::concat_many(&[]);
        assert!(s.is_inline());
        assert_eq!(s.len(), 0);
        assert_eq!(s.as_str(), "");
    }

    #[test]
    fn concat_many_single_slice_matches_from_borrowed() {
        let s = SmolStr::concat_many(&["hello"]);
        assert!(s.is_inline());
        assert_eq!(s.as_str(), "hello");
    }

    #[test]
    fn concat_many_inline_path() {
        // 4 chunks of 5 bytes = 20 bytes, still inline.
        let s = SmolStr::concat_many(&["aaaaa", "bbbbb", "ccccc", "ddddd"]);
        assert!(s.is_inline());
        assert_eq!(s.as_str(), "aaaaabbbbbcccccddddd");
        assert_eq!(s.len(), 20);
    }

    #[test]
    fn concat_many_at_cap_inline() {
        // 22 bytes exactly -> still inline.
        let s = SmolStr::concat_many(&["a".repeat(11).as_str(), "b".repeat(11).as_str()]);
        assert!(s.is_inline());
        assert_eq!(s.len(), SMOL_STR_INLINE_CAP);
    }

    #[test]
    fn concat_many_heap_path() {
        // 4 chunks of 8 = 32 bytes, past cap -> heap.
        let s = SmolStr::concat_many(&["aaaaaaaa", "bbbbbbbb", "cccccccc", "dddddddd"]);
        assert!(!s.is_inline());
        assert_eq!(s.as_str(), "aaaaaaaabbbbbbbbccccccccdddddddd");
        assert_eq!(s.len(), 32);
    }

    #[test]
    fn try_build_inline_fills_inline_slot() {
        // Writer fills the slice byte-by-byte with the lower-case of
        // each ASCII letter — exercises the to_lower fast path shape
        // the stdlib helpers now use.
        let src = b"HELLO";
        let s = SmolStr::try_build_inline(src.len(), |out| {
            for (i, b) in src.iter().enumerate() {
                out[i] = b.to_ascii_lowercase();
            }
        })
        .expect("inline path should accept 5-byte payload");
        assert!(s.is_inline());
        assert_eq!(s.as_str(), "hello");
    }

    #[test]
    fn try_build_inline_at_cap_inline() {
        // Exactly 22 bytes — boundary of the inline slot.
        let s =
            SmolStr::try_build_inline(SMOL_STR_INLINE_CAP, |out| out.fill(b'x')).expect("22 fits");
        assert!(s.is_inline());
        assert_eq!(s.len(), SMOL_STR_INLINE_CAP);
    }

    #[test]
    fn try_build_inline_overflow_returns_none() {
        // 23 bytes — past the cap. Writer must not be invoked; we
        // assert via a panicking closure to catch a hypothetical
        // regression.
        let s = SmolStr::try_build_inline(SMOL_STR_INLINE_CAP + 1, |_out| {
            panic!("writer must not run when out_len exceeds cap");
        });
        assert!(s.is_none());
    }

    #[test]
    fn try_build_inline_rejects_invalid_utf8() {
        let s = SmolStr::try_build_inline(1, |out| out[0] = 0xff);
        assert!(s.is_none());
    }

    #[test]
    fn try_build_inline_zero_length_is_empty() {
        let s = SmolStr::try_build_inline(0, |_out| { /* nothing */ })
            .expect("zero-length always inline");
        assert!(s.is_inline());
        assert_eq!(s.as_str(), "");
    }

    #[test]
    fn is_ascii_inline_empty() {
        // Empty payload is vacuously ASCII.
        let s = SmolStr::new_empty();
        assert!(s.is_inline());
        assert!(s.is_ascii());
    }

    #[test]
    fn is_ascii_inline_pure_ascii() {
        let s = SmolStr::from_borrowed("hello");
        assert!(s.is_inline());
        assert!(s.is_ascii());
    }

    #[test]
    fn is_ascii_inline_with_high_byte() {
        // 'caf' + U+00E9 (encoded as 0xC3 0xA9). Built from raw bytes
        // so the source file stays pure-ASCII while the SmolStr
        // payload contains a byte >= 0x80, forcing `is_ascii()` to
        // false.
        let raw = vec![b'c', b'a', b'f', 0xC3, 0xA9];
        let payload = String::from_utf8(raw).expect("valid UTF-8");
        let s = SmolStr::from_borrowed(&payload);
        assert!(s.is_inline());
        assert!(!s.is_ascii());
    }

    #[test]
    fn is_ascii_inline_at_cap_boundary() {
        // 22-byte ASCII payload sits exactly at the inline cap.
        let payload = "a".repeat(SMOL_STR_INLINE_CAP);
        let s = SmolStr::from_borrowed(&payload);
        assert!(s.is_inline());
        assert!(s.is_ascii());
    }

    #[test]
    fn is_ascii_heap_pure_ascii() {
        let payload = "x".repeat(SMOL_STR_INLINE_CAP + 8);
        let s = SmolStr::from_borrowed(&payload);
        assert!(!s.is_inline());
        assert!(s.is_ascii());
    }

    #[test]
    fn is_ascii_heap_with_non_ascii() {
        // Heap-sized payload (> 22 bytes) that contains a non-ASCII
        // codepoint near the end — exercises the heap-path delegation
        // to `str::is_ascii`. We append U+00E9 (encoded as 0xC3 0xA9
        // raw bytes) so the source file stays pure-ASCII while the
        // runtime payload contains a byte >= 0x80.
        let mut payload = "x".repeat(SMOL_STR_INLINE_CAP).into_bytes();
        payload.extend_from_slice(&[b'y', b'y', b'z', 0xC3, 0xA9]);
        let payload = String::from_utf8(payload).expect("valid UTF-8");
        let s = SmolStr::from_borrowed(&payload);
        assert!(!s.is_inline());
        assert!(!s.is_ascii());
    }

    #[test]
    fn concat_many_matches_nested_concat() {
        // Result must be byte-identical to the recursive shape so the
        // evaluator can swap in `concat_many` without changing user-
        // visible string values.
        let leaves = ["foo_", "bar_", "baz_", "qux_"];
        let nested = {
            let mut acc = SmolStr::new_empty();
            for leaf in leaves.iter() {
                acc = SmolStr::concat(acc.as_str(), leaf);
            }
            acc
        };
        let folded = SmolStr::concat_many(&leaves);
        assert_eq!(nested.as_str(), folded.as_str());
    }
}
