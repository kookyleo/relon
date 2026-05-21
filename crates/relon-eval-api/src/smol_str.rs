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
pub enum SmolStr {
    /// Inline payload. `len` is the active prefix length of `data`;
    /// bytes past `len` are zeroed at construction.
    Inline {
        len: u8,
        data: [u8; SMOL_STR_INLINE_CAP],
    },
    /// Heap payload. `Arc<str>` shares the bytes across clones; the
    /// `NonNull` pointer inside the `Arc` provides the niche the enum
    /// discriminant rides on.
    Heap(Arc<str>),
}

impl SmolStr {
    /// Build an empty `SmolStr` without touching the allocator.
    #[inline]
    pub const fn new_empty() -> Self {
        SmolStr::Inline {
            len: 0,
            data: [0u8; SMOL_STR_INLINE_CAP],
        }
    }

    /// Borrow the payload as a `&str` slice. Cheap (no copies) in both
    /// `Inline` and `Heap` modes.
    #[inline]
    pub fn as_str(&self) -> &str {
        match self {
            SmolStr::Inline { len, data } => {
                let slice = &data[..*len as usize];
                // SAFETY: every public constructor fills `data[..len]`
                // from a `&str` / `String`, so the bytes are valid
                // UTF-8 by construction. We never expose a way to
                // mutate `data` past construction.
                unsafe { std::str::from_utf8_unchecked(slice) }
            }
            SmolStr::Heap(arc) => arc,
        }
    }

    /// Byte length of the payload (matching `str::len`).
    #[inline]
    pub fn len(&self) -> usize {
        match self {
            SmolStr::Inline { len, .. } => *len as usize,
            SmolStr::Heap(arc) => arc.len(),
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
        matches!(self, SmolStr::Inline { .. })
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
            SmolStr::Inline {
                len: bytes.len() as u8,
                data,
            }
        } else {
            SmolStr::Heap(Arc::from(s))
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
            SmolStr::Heap(Arc::from(s))
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
            SmolStr::Inline {
                len: total as u8,
                data,
            }
        } else {
            // Heap fallback: pre-size a `String` (one allocation), push
            // both slices, then hand the buffer to `Arc::from(String)`
            // which moves the allocation into the Arc payload without
            // re-copying.
            let mut buf = String::with_capacity(total);
            buf.push_str(a);
            buf.push_str(b);
            SmolStr::Heap(Arc::from(buf))
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
        match (&s, &s.clone()) {
            (SmolStr::Heap(a), SmolStr::Heap(b)) => {
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
}
