//! Type definitions and stable internal-helper indices for the bundled
//! stdlib registry.
//!
//! This module owns the registry entry type ([`StdlibFunction`]) and
//! the constants pinning the wasm-level slots of every internal
//! helper that body builders need to call without re-entering
//! [`super::registry::builtin_stdlib`].
//!
//! The helper indices live here (rather than in `index.rs`) because
//! they are written into the IR by body builders at construction time;
//! splitting them apart from the lookup machinery keeps the body
//! builders free of the lookup-fn dependency chain.
//!
//! See `super::registry::builtin_stdlib` for the ordered list whose
//! declaration order these indices pin.

use crate::ir::{IrType, TaggedOp};
use std::sync::{Arc, OnceLock};

/// v3+ a-4: stable slot of the `__casefold_lookup` internal helper in
/// the [`builtin_stdlib`] registry. Hardcoded so the `upper` / `lower`
/// body builders can emit the matching `Op::Call { fn_index }` without
/// recursing back into [`builtin_stdlib`] (which would infinite-loop
/// because those builders are called *from* [`builtin_stdlib`]). The
/// constant is sanity-checked at unit-test time in
/// `casefold_lookup_index_is_stable`; future additions to the
/// registry must `append` (per the module doc-comment) so this
/// constant never needs to change once it has shipped.
pub(crate) const CASEFOLD_LOOKUP_INDEX: u32 = 20;

/// v3++ b-4: stable slot of the `__is_combining_mark` internal helper
/// in the [`builtin_stdlib`] registry. Same cycle-breaking rationale
/// as [`CASEFOLD_LOOKUP_INDEX`] — the rewritten `title` / `upper` /
/// `lower` body builders emit `Op::Call { fn_index = COMBINING_MARK_INDEX }`
/// without re-entering [`builtin_stdlib`]. Unit-tested by
/// `combining_mark_index_is_stable`.
pub(crate) const COMBINING_MARK_INDEX: u32 = 21;

/// v3++ b-4: stable slot of the `__is_whitespace` internal helper in
/// the [`builtin_stdlib`] registry. Only the `title` body calls it;
/// `upper` / `lower` do not need word-boundary detection. Same
/// cycle-breaking rationale as [`CASEFOLD_LOOKUP_INDEX`].
pub(crate) const IS_WHITESPACE_INDEX: u32 = 22;

/// v3++ b-5: stable slot of the `__decomp_lookup(cp, table_addr) -> i32`
/// internal helper in the [`builtin_stdlib`] registry. Returns the
/// 32-bit-packed `(pool_off << 8) | pool_len` lookup result, or `0`
/// when `cp` is not in the table (pool_len of `0` is a sentinel - the
/// table never has a zero-length mapping). The four normalization
/// bodies share this helper across both NFD and NFKD table families;
/// the table address is the discriminator.
pub(crate) const DECOMP_LOOKUP_INDEX: u32 = 24;

/// v3++ b-5: stable slot of the `__ccc_lookup(cp, table_addr) -> i32`
/// internal helper. Returns the Canonical_Combining_Class of `cp`, or
/// `0` when `cp` is not in the table (matches the UCD convention that
/// absent entries default to Not_Reordered).
pub(crate) const CCC_LOOKUP_INDEX: u32 = 25;

/// v3++ b-5: stable slot of the `__compose_lookup(first, second,
/// table_addr) -> i32` internal helper. Returns the composed code
/// point when the `(first, second)` pair is present in the canonical
/// composition table, or `-1` when no composition is defined.
/// `-1` (a u32-as-i32 of `0xFFFF_FFFF`) is safe as a sentinel because
/// Unicode caps codepoints at `U+10FFFF`.
pub(crate) const COMPOSE_LOOKUP_INDEX: u32 = 26;

/// v3++ b-7 reframed: stable slot of the
/// `__full_casefold_lookup(cp, table_addr) -> i32` internal helper.
///
/// Binary-searches the FULL multi-codepoint folding table (20-byte
/// stride: `(in: u32, out0: u32, out1: u32, out2: u32, out_len: u32)`)
/// and returns the absolute address of the matched entry (i.e.
/// `table_addr + 4 + idx * 20`), or `0` on miss. Callers load `out_len`
/// from `entry + 16` and the up-to-three output codepoints from
/// `entry + 4 / 8 / 12`.
///
/// The address-return ABI keeps the helper signature at a single i32
/// while letting callers fetch every output slot without a second
/// helper round-trip — matches the shape of `__decomp_lookup` (which
/// also returns a packed integer rather than a scratch handle).
pub(crate) const FULL_CASEFOLD_LOOKUP_INDEX: u32 = 34;

/// v3++ b-7 reframed: stable slot of the
/// `__final_sigma_check(s_ptr, byte_offset, cased_addr, ignorable_addr) -> i32`
/// helper. Returns `1` when `Σ` at `byte_offset` in the input UTF-8
/// string `s_ptr` is at the end of a word per UAX #21 Final_Sigma —
/// i.e. preceded by at least one cased codepoint (skipping case-
/// ignorables), and either followed by only case-ignorables until end
/// of string or followed by a non-cased non-ignorable codepoint.
/// Returns `0` otherwise.
///
/// `s_ptr` is a String record pointer (the leading `u32 LE` length
/// header lives at `s_ptr + 0`; the payload bytes start at
/// `s_ptr + 4`). The helper does its own UTF-8 reverse / forward
/// decoding so callers don't need to materialise a codepoint array.
pub(crate) const FINAL_SIGMA_CHECK_INDEX: u32 = 35;

/// One bundled stdlib function — name, signature, and IR body.
///
/// Body uses the same op stream the lowering pass would produce for a
/// user-defined function: `LocalGet` indices refer to the function's
/// declared `params` slots in declaration order; the body must end
/// with a value on top of the virtual stack and an `Op::Return`. The
/// stdlib bodies are hand-written so they sidestep the lowering pass
/// entirely.
///
/// F-D2-G: the body is built lazily — the registry only holds the
/// metadata triple (`name`, `params`, `ret`) plus a `fn() -> Vec<TaggedOp>`
/// builder pointer. The first call to [`StdlibFunction::body`]
/// instantiates the op vector and caches it in an `OnceLock`; callers
/// that only consult the signature (e.g. the lowering pass picking the
/// right `Op::Call` shape) never pay the body-construction cost. The
/// `Arc` lets the lazy slot survive `Clone` (downstream `Func` lifts
/// the stdlib body into a separate `Vec<TaggedOp>` when JIT-inlining
/// or bytecode-inlining the callee).
pub struct StdlibFunction {
    /// Surface-level name the lowering pass looks up via
    /// [`crate::stdlib::stdlib_function_index`].
    pub name: &'static str,
    /// Parameter types in declaration order. Each maps to a wasm-
    /// level function-parameter slot consumed via `Op::LocalGet`.
    pub params: Vec<IrType>,
    /// Return type. Each stdlib function returns exactly one value.
    pub ret: IrType,
    /// Lazy cell holding the IR op stream forming the function body.
    /// Built on first access via [`StdlibFunction::body`].
    body: Arc<OnceLock<Vec<TaggedOp>>>,
    /// Pure builder constructing the body op stream. Invoked at most
    /// once per process per registry entry (the result is cached in
    /// `body`).
    body_builder: fn() -> Vec<TaggedOp>,
}

impl StdlibFunction {
    /// Construct a registry entry. `body_builder` is invoked lazily on
    /// the first call to [`StdlibFunction::body`].
    pub(super) fn new(
        name: &'static str,
        params: Vec<IrType>,
        ret: IrType,
        body_builder: fn() -> Vec<TaggedOp>,
    ) -> Self {
        StdlibFunction {
            name,
            params,
            ret,
            body: Arc::new(OnceLock::new()),
            body_builder,
        }
    }

    /// Force-instantiate the body and return a borrowed view. The
    /// first call runs `body_builder`; subsequent calls return the
    /// cached op vector for free.
    pub fn body(&self) -> &Vec<TaggedOp> {
        self.body.get_or_init(self.body_builder)
    }

    /// Force-instantiate the body and return an owned clone. Used by
    /// callers that need to move the op stream into a separate `Func`
    /// (cranelift JIT inlining, bytecode inlining) without borrowing
    /// from the static registry.
    pub fn body_owned(&self) -> Vec<TaggedOp> {
        self.body().clone()
    }
}

impl Clone for StdlibFunction {
    fn clone(&self) -> Self {
        StdlibFunction {
            name: self.name,
            params: self.params.clone(),
            ret: self.ret,
            // Share the lazy cell so a `clone()` after the body has
            // been built doesn't trigger a rebuild — the metadata
            // copy is cheap, the body is the expensive part.
            body: Arc::clone(&self.body),
            body_builder: self.body_builder,
        }
    }
}

impl std::fmt::Debug for StdlibFunction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StdlibFunction")
            .field("name", &self.name)
            .field("params", &self.params)
            .field("ret", &self.ret)
            .field("body_built", &self.body.get().is_some())
            .finish()
    }
}
