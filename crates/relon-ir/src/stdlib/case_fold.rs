//! Body builders for the case-fold (`upper` / `lower` / `title`)
//! stdlib bodies, plus the internal Unicode helpers they call into.
//!
//! Bodies in this file:
//!   * Surface bodies: `upper(String)`, `lower(String)`,
//!     `title(String)`, `upper_locale(String, String)`,
//!     `lower_locale(String, String)`, `title_locale(String, String)`.
//!   * Internal helpers: `__casefold_lookup`, `__is_combining_mark`,
//!     `__is_whitespace`, `__full_casefold_lookup`,
//!     `__final_sigma_check`.
//!
//! The helpers carry an `__` prefix to signal they are not part of
//! the user-visible surface; they are reached only by the case-fold
//! bodies via `Op::Call { fn_index = CASEFOLD_LOOKUP_INDEX, .. }` and
//! peers. The hard-coded indices live in [`super::signatures`].
//!
//! [`CaseFoldMode`] is the per-mode discriminator that drives the
//! shared body generator [`case_fold_body_inner_body`].

use crate::ir::{IrType, Op, TaggedOp, TrapKind};

use super::defs::tt;
use super::signatures::{
    StdlibFunction, CASEFOLD_LOOKUP_INDEX, COMBINING_MARK_INDEX, FINAL_SIGMA_CHECK_INDEX,
    FULL_CASEFOLD_LOOKUP_INDEX, IS_WHITESPACE_INDEX,
};

/// v3++ b-4 case-folding mode driving [`case_fold_body`].
///
/// `Upper` and `Lower` are straight per-codepoint folds against the
/// matching simple-folding table. `Title` extends the pipeline with a
/// per-word boundary tracker plus combining-mark detection — the
/// first cased codepoint of each word maps through the upper table,
/// every subsequent codepoint maps through the lower table, and
/// Unicode combining marks (Mn + Mc + Me) never reset the boundary
/// because they belong to their base codepoint's grapheme cluster.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CaseFoldMode {
    /// `upper(s)` — every codepoint goes through the upper table.
    Upper,
    /// `lower(s)` — every codepoint goes through the lower table.
    Lower,
    /// `title(s)` — first cased codepoint of each whitespace-separated
    /// word maps through the upper table; the rest map through the
    /// lower table; combining marks pass through unchanged and do
    /// **not** flip the word-boundary flag.
    Title,
}

/// v3+ a-4 Unicode-aware body for `upper(s: String) -> String`.
///
/// Replaces the Phase 4.c-2 ASCII fast-path. The body now decodes
/// each input codepoint from its UTF-8 byte sequence, looks the
/// codepoint up in the simple upper case-folding table embedded in
/// the wasm data section, and re-encodes the (possibly different)
/// output codepoint back into UTF-8. The fold table is keyed off
/// [`Op::CaseFoldTableAddr { upper: true }`]; only mappings that
/// produce a **single** replacement codepoint are honoured (German
/// `ß` -> `SS` and other multi-codepoint cases stay un-folded, a
/// v3++ item).
///
/// v3++ b-4: Unicode combining marks (Mn + Mc + Me) skip the case-
/// fold lookup and pass through verbatim. Marks are non-cased in
/// Unicode, so the lookup happened to be identity in v3+ a-4; the
/// explicit skip keeps the fold table semantically honest and lays
/// groundwork for the v3++ b-6 full-folding pass that may map cased
/// marks through context-sensitive rules.
pub(super) fn upper_string() -> StdlibFunction {
    case_fold_body("upper", CaseFoldMode::Upper)
}

/// v3+ a-4 mirror of [`upper_string`] looking up the simple lower
/// case-folding table. Same decode/encode pipeline, different table
/// address — driven by the `upper: false` arm of
/// [`Op::CaseFoldTableAddr`].
///
/// v3++ b-4: same combining-mark skip as [`upper_string`].
pub(super) fn lower_string() -> StdlibFunction {
    case_fold_body("lower", CaseFoldMode::Lower)
}

/// v3++ b-6 locale-aware variant: `upper_locale(s, locale) -> String`.
///
/// Body wraps [`case_fold_body`] with a runtime locale dispatch. When
/// the second parameter (a `String` holding the locale tag) starts
/// with `tr` / `TR` / `az` / `AZ`, the per-codepoint fold consults
/// the Turkish / Azerbaijani override table before falling back to
/// the default simple folding chain. All other locale values
/// degenerate to the default UAX #21 behaviour.
///
/// The full multi-codepoint Unicode mappings (`ß` -> `SS`, ligatures,
/// `İ` -> `i\u{0307}`) and the Greek final-sigma context are handled
/// on the tree-walk evaluator side; the wasm-AOT body keeps the v3+
/// a-4 simple-folding contract for the moment, with the locale
/// extension layered on top. See the `crates/relon-evaluator/src/stdlib.rs`
/// `fold_string` helper for the reference behaviour the wasm-AOT
/// future-pass aligns with.
pub(super) fn upper_locale_string() -> StdlibFunction {
    locale_aware_case_fold_body("upper_locale", CaseFoldMode::Upper)
}

/// Mirror of [`upper_locale_string`] for lowercasing.
pub(super) fn lower_locale_string() -> StdlibFunction {
    locale_aware_case_fold_body("lower_locale", CaseFoldMode::Lower)
}

/// Mirror of [`upper_locale_string`] for title-casing.
pub(super) fn title_locale_string() -> StdlibFunction {
    locale_aware_case_fold_body("title_locale", CaseFoldMode::Title)
}

/// v3++ b-4 word-boundary aware body for `title(s: String) -> String`.
///
/// Splits the input on Unicode whitespace; the first codepoint of
/// each word maps through the simple **upper** fold table while every
/// subsequent codepoint maps through the simple **lower** fold table.
/// Whitespace codepoints themselves pass through unchanged and reset
/// the per-word boundary state.
///
/// Grapheme-cluster contract: Unicode combining marks (Mn + Mc + Me)
/// belong to their base codepoint's cluster. The body emits each mark
/// verbatim and does **not** flip the boundary flag — so e.g.
/// `cafe\u{0301} bar` still produces `Cafe\u{0301} Bar` rather than
/// `Cafe\u{0301} bar` or `Cafe\u{0301} bAr` (which a naive
/// codepoint-level walk would produce by treating the combining
/// acute as "the second character of cafe-acute").
///
/// Limitations (deferred):
///   * Only simple whitespace-based word boundaries. UAX #29 Extended
///     Grapheme Cluster + Word Boundary (handling punctuation as
///     boundaries, joiner / extender rules, etc.) is on v3++ b-6.
///   * Combining marks following a non-cased base (whitespace +
///     mark — invalid Unicode but possible in malformed input) reset
///     the boundary; matches Rust's `to_titlecase` behaviour.
pub(super) fn title_string() -> StdlibFunction {
    case_fold_body("title", CaseFoldMode::Title)
}

/// Shared body generator for `upper` / `lower` / `title` with
/// Unicode-aware case folding.
///
/// Algorithm:
///   1. Read `s_len` (byte count of the input UTF-8 record).
///   2. Allocate a scratch String record sized `4 + s_len * 4`. The
///      worst-case output growth is 4x (a single-byte ASCII char
///      mapping to a 4-byte UTF-8 codepoint), but practically the
///      ratio is closer to 1x — we over-provision to keep the body
///      branchless on the resize side.
///   3. Walk the input byte-by-byte. For each codepoint: (a) decode
///      the codepoint and its byte count from the UTF-8 leading
///      byte's bit pattern — truncated tails or invalid leading bytes
///      trap as `InvalidUtf8`; (b) decide the per-codepoint mapping
///      based on the [`CaseFoldMode`]:
///         * `Upper` / `Lower` — if the codepoint is a combining
///           mark, pass it through unchanged; otherwise call the
///           bundled `__casefold_lookup` helper with the matching
///           `Op::CaseFoldTableAddr` address.
///         * `Title` — if the codepoint is a combining mark, pass it
///           through unchanged and leave `at_word_start` alone (the
///           grapheme-cluster contract — see v3++ b-4 notes). Else
///           if the codepoint is Unicode whitespace, pass it through
///           and set `at_word_start = 1`. Otherwise, look the cp up
///           in the upper table when `at_word_start == 1` (first
///           cased cp of the word) or the lower table otherwise; then
///           clear `at_word_start`.
///
///      (c) encode the resolved (possibly folded) codepoint back into
///      UTF-8 starting at `base + 4 + j` and advance `j` by the
///      byte count.
///   4. Once `i == s_len`, store the final `j` into the record's
///      length prefix and return the record pointer.
///
/// Locals:
///   * 0  — `s_len:    I32`  total input byte count
///   * 1  — `base:     I32`  pointer to the scratch result record
///   * 2  — `i:        I32`  read cursor (byte offset into payload)
///   * 3  — `j:        I32`  write cursor (byte offset into payload)
///   * 4  — `cp:       I32`  decoded codepoint
///   * 5  — `cp_bytes: I32`  byte count of the decoded codepoint
///   * 6  — `folded:   I32`  case-folded codepoint
///   * 7  — `b0:       I32`  leading byte buffer
///   * 8  — `b_tmp:    I32`  continuation byte buffer
///   * 9  — `sink:     I32`  drop slot for If-arm placeholders
///   * 10 — `at_word_start: I32`  Title-mode word boundary flag
///   * 11 — `is_mark:  I32`  Title/Upper/Lower combining-mark cache
// The body builder is structurally a streaming push-sequence; clippy's
// `vec_init_then_push` lint would force every intermediate sequence
// into a literal `vec![..]` macro, which loses the per-line comment
// alignment that makes the UTF-8 decode/encode pipeline reviewable.
// The lint stays off across the function — every helper closure here
// produces a `Vec<TaggedOp>` it appends to by-line.
#[allow(clippy::vec_init_then_push)]
fn case_fold_body(name: &'static str, mode: CaseFoldMode) -> StdlibFunction {
    let body_builder: fn() -> Vec<TaggedOp> = match mode {
        CaseFoldMode::Upper => || case_fold_body_inner_body(CaseFoldMode::Upper, false),
        CaseFoldMode::Lower => || case_fold_body_inner_body(CaseFoldMode::Lower, false),
        CaseFoldMode::Title => || case_fold_body_inner_body(CaseFoldMode::Title, false),
    };
    StdlibFunction::new(name, vec![IrType::String], IrType::String, body_builder)
}

/// v3++ b-6 locale-aware variant of [`case_fold_body`]. Same single-
/// codepoint fold pipeline, but the body takes a second `String`
/// parameter (the locale tag); when the first two bytes match
/// `tr` / `TR` / `az` / `AZ`, the per-cp lookup first consults the
/// Turkish / Azerbaijani override table before falling back to the
/// default simple folding table.
///
/// Limitations vs the tree-walk reference (`fold_string` in
/// `crates/relon-evaluator/src/stdlib.rs`):
///
///   * Multi-codepoint outputs (e.g. `ß` -> `SS`, `ﬁ` -> `FI`, the
///     Turkish `I` -> `i\u{0307}` form) are not yet emitted by the
///     wasm-AOT body — those cases pass through identity for now.
///   * Greek final-sigma context (`Σ` -> `ς` vs `σ`) is similarly
///     deferred on the wasm-AOT side.
///
/// Both follow-ups are tracked as v3++ b-6 wasm-AOT items; the
/// tree-walk evaluator handles them correctly today so functional
/// tests that exercise those paths pass when run through the host
/// evaluator.
#[allow(clippy::vec_init_then_push)]
fn locale_aware_case_fold_body(name: &'static str, mode: CaseFoldMode) -> StdlibFunction {
    let body_builder: fn() -> Vec<TaggedOp> = match mode {
        CaseFoldMode::Upper => || case_fold_body_inner_body(CaseFoldMode::Upper, true),
        CaseFoldMode::Lower => || case_fold_body_inner_body(CaseFoldMode::Lower, true),
        CaseFoldMode::Title => || case_fold_body_inner_body(CaseFoldMode::Title, true),
    };
    StdlibFunction::new(
        name,
        vec![IrType::String, IrType::String],
        IrType::String,
        body_builder,
    )
}

#[allow(clippy::vec_init_then_push)]
fn case_fold_body_inner_body(mode: CaseFoldMode, locale_aware: bool) -> Vec<TaggedOp> {
    const S_LEN: u32 = 0;
    const BASE: u32 = 1;
    const I: u32 = 2;
    const J: u32 = 3;
    const CP: u32 = 4;
    const CP_BYTES: u32 = 5;
    const FOLDED: u32 = 6;
    const B0: u32 = 7;
    const B_TMP: u32 = 8;
    // Sink slot for the i32 placeholder each `Op::If { result_ty: I32 }`
    // branch leaves on the operand stack. Keeping the sink separate
    // from CP_BYTES avoids the trap-arm trick clobbering the
    // legitimate `cp_bytes` value the branch just wrote.
    const SINK: u32 = 9;
    // v3++ b-4: word-boundary flag for `Title` mode. Initialised to 1
    // before the first codepoint so the very first cased cp goes
    // through the upper table. Unused (but still allocated) in
    // `Upper` / `Lower` mode — the wasm verifier doesn't penalise
    // unused locals.
    const AT_WORD_START: u32 = 10;
    // v3++ b-4: cached `__is_combining_mark(cp)` result for the
    // current codepoint. Kept in a let-local so the encode step can
    // re-use the same i32 without re-running the binary search.
    const IS_MARK: u32 = 11;
    // v3++ b-6: locale-aware dispatch flag. `1` when the caller-
    // supplied locale tag is `tr` / `TR` / `az` / `AZ` (case-
    // insensitive two-letter prefix matching, with optional `-` /
    // `_` subtag separator). The per-cp fold consults the Turkish
    // override table first when this flag is set; otherwise it
    // shortcircuits straight to the default simple folding table.
    const IS_TURKISH: u32 = 12;
    // Scratch for the Turkish-lookup hit detection: we call the
    // standard `__casefold_lookup` helper against the Turkish table
    // and stash the result; if the result differs from the input cp
    // we know we hit the override, otherwise we fall back to the
    // default table.
    const TURKISH_RESULT: u32 = 13;
    // v3++ b-7 reframed: `1` when fold_seq actually consulted a
    // case-folding table (i.e. not a combining mark and not a
    // Title-mode whitespace pass-through). The multi-cp / final-sigma
    // overlay only runs when this flag is set; otherwise the existing
    // single-cp emit path runs unchanged.
    const FOLD_TABLE_FIRED: u32 = 14;
    // v3++ b-7 reframed: `1` when the effective per-codepoint mode is
    // Upper, `0` when it is Lower. For `Upper` / `Lower` bodies this
    // is a compile-time constant; for `Title` it captures the value of
    // `at_word_start` **before** the cased-cp consumption clears it.
    // Used by the FULL multi-cp lookup to pick between the upper and
    // lower FULL tables.
    const EFFECTIVE_UPPER: u32 = 15;
    // v3++ b-7 reframed: matched FULL-table entry address from
    // `__full_casefold_lookup`. `0` means "no FULL entry" — fall back
    // to the simple-table FOLDED single-cp emit. On a hit the multi-cp
    // emit loop reads `out_len` from `FULL_ENTRY + 16` and the three
    // output codepoints from `FULL_ENTRY + 4 / 8 / 12`.
    const FULL_ENTRY: u32 = 16;
    // v3++ b-7 reframed: scratch for the multi-cp emit loop index
    // (0 .. FULL_OUT_LEN). The loop body writes the indexed output
    // codepoint into FOLDED and re-runs the existing UTF-8 encode
    // sequence per slot.
    const FULL_SLOT_IDX: u32 = 17;
    // v3++ b-7 reframed: `out_len` (1..=3) read once from the matched
    // entry so the multi-cp loop's bound stays loop-invariant.
    const FULL_OUT_LEN: u32 = 18;

    // Helper: load the `i`-th byte of the input payload. Pushes one
    // i32 onto the stack.
    let load_input_byte = |off: i32| {
        vec![
            tt(Op::LocalGet(0)),
            tt(Op::ConstI32(4 + off)),
            tt(Op::Add(IrType::I32)),
            tt(Op::LetGet {
                idx: I,
                ty: IrType::I32,
            }),
            tt(Op::Add(IrType::I32)),
            tt(Op::LoadI8UAtAbsolute { offset: 0 }),
        ]
    };

    // Helper: trap as InvalidUtf8 from within an if/else that needs
    // an i32 placeholder on each arm. The `Trap` op marks downstream
    // code unreachable; the placeholder satisfies the wasm verifier's
    // both-arms-typed contract.
    let trap_invalid_utf8 = || {
        vec![
            tt(Op::Trap {
                kind: TrapKind::InvalidUtf8,
            }),
            tt(Op::ConstI32(0)),
        ]
    };

    // Helper: decode a continuation byte at offset `+n` into B_TMP
    // (masked with 0x3F so callers can directly OR via `+`).
    let load_continuation = |n: i32| {
        let mut out = load_input_byte(n);
        out.push(tt(Op::ConstI32(0x3F)));
        out.push(tt(Op::BitAnd(IrType::I32)));
        out.push(tt(Op::LetSet {
            idx: B_TMP,
            ty: IrType::I32,
        }));
        out
    };

    // ----- UTF-8 decode step -----
    // Stack precondition: empty. Postcondition: empty. Sets CP and
    // CP_BYTES from the byte at offset `i` (and possibly more).
    let mut decode_seq: Vec<TaggedOp> = Vec::new();
    decode_seq.extend(load_input_byte(0));
    decode_seq.push(tt(Op::LetSet {
        idx: B0,
        ty: IrType::I32,
    }));
    // outer If: 1-byte vs multi-byte
    decode_seq.push(tt(Op::LetGet {
        idx: B0,
        ty: IrType::I32,
    }));
    decode_seq.push(tt(Op::ConstI32(0x80)));
    decode_seq.push(tt(Op::Lt(IrType::I32)));
    decode_seq.push(tt(Op::If {
        result_ty: IrType::I32,
        then_body: {
            // 1-byte ASCII: cp = b0, cp_bytes = 1
            let mut v = Vec::new();
            v.push(tt(Op::LetGet {
                idx: B0,
                ty: IrType::I32,
            }));
            v.push(tt(Op::LetSet {
                idx: CP,
                ty: IrType::I32,
            }));
            v.push(tt(Op::ConstI32(1)));
            v.push(tt(Op::LetSet {
                idx: CP_BYTES,
                ty: IrType::I32,
            }));
            v.push(tt(Op::ConstI32(0)));
            v
        },
        else_body: {
            // multi-byte path. Trap when b0 is a continuation byte
            // (`0x80..=0xBF`) or beyond the 4-byte range
            // (`0xF8..=0xFF`).
            let mut v = Vec::new();
            // Reject lone continuation byte / overlong: b0 < 0xC2.
            v.push(tt(Op::LetGet {
                idx: B0,
                ty: IrType::I32,
            }));
            v.push(tt(Op::ConstI32(0xC2)));
            v.push(tt(Op::Lt(IrType::I32)));
            v.push(tt(Op::If {
                result_ty: IrType::I32,
                then_body: trap_invalid_utf8(),
                else_body: {
                    let mut v2 = Vec::new();
                    // 2-byte: 0xC2..=0xDF
                    v2.push(tt(Op::LetGet {
                        idx: B0,
                        ty: IrType::I32,
                    }));
                    v2.push(tt(Op::ConstI32(0xE0)));
                    v2.push(tt(Op::Lt(IrType::I32)));
                    v2.push(tt(Op::If {
                        result_ty: IrType::I32,
                        then_body: {
                            let mut t = Vec::new();
                            // require i + 1 < s_len
                            t.push(tt(Op::LetGet {
                                idx: I,
                                ty: IrType::I32,
                            }));
                            t.push(tt(Op::ConstI32(1)));
                            t.push(tt(Op::Add(IrType::I32)));
                            t.push(tt(Op::LetGet {
                                idx: S_LEN,
                                ty: IrType::I32,
                            }));
                            t.push(tt(Op::Ge(IrType::I32)));
                            t.push(tt(Op::If {
                                result_ty: IrType::I32,
                                then_body: trap_invalid_utf8(),
                                else_body: vec![tt(Op::ConstI32(0))],
                            }));
                            t.push(tt(Op::LetSet {
                                idx: SINK,
                                ty: IrType::I32,
                            }));
                            // cp = (b0 & 0x1F) * 64 + (b1 & 0x3F)
                            t.extend(load_continuation(1));
                            t.push(tt(Op::LetGet {
                                idx: B0,
                                ty: IrType::I32,
                            }));
                            t.push(tt(Op::ConstI32(0x1F)));
                            t.push(tt(Op::BitAnd(IrType::I32)));
                            t.push(tt(Op::ConstI32(64)));
                            t.push(tt(Op::Mul(IrType::I32)));
                            t.push(tt(Op::LetGet {
                                idx: B_TMP,
                                ty: IrType::I32,
                            }));
                            t.push(tt(Op::Add(IrType::I32)));
                            t.push(tt(Op::LetSet {
                                idx: CP,
                                ty: IrType::I32,
                            }));
                            t.push(tt(Op::ConstI32(2)));
                            t.push(tt(Op::LetSet {
                                idx: CP_BYTES,
                                ty: IrType::I32,
                            }));
                            t.push(tt(Op::ConstI32(0)));
                            t
                        },
                        else_body: {
                            let mut e = Vec::new();
                            // 3-byte vs 4-byte: split on 0xF0
                            e.push(tt(Op::LetGet {
                                idx: B0,
                                ty: IrType::I32,
                            }));
                            e.push(tt(Op::ConstI32(0xF0)));
                            e.push(tt(Op::Lt(IrType::I32)));
                            e.push(tt(Op::If {
                                result_ty: IrType::I32,
                                then_body: {
                                    let mut t = Vec::new();
                                    // require i + 2 < s_len
                                    t.push(tt(Op::LetGet {
                                        idx: I,
                                        ty: IrType::I32,
                                    }));
                                    t.push(tt(Op::ConstI32(2)));
                                    t.push(tt(Op::Add(IrType::I32)));
                                    t.push(tt(Op::LetGet {
                                        idx: S_LEN,
                                        ty: IrType::I32,
                                    }));
                                    t.push(tt(Op::Ge(IrType::I32)));
                                    t.push(tt(Op::If {
                                        result_ty: IrType::I32,
                                        then_body: trap_invalid_utf8(),
                                        else_body: vec![tt(Op::ConstI32(0))],
                                    }));
                                    t.push(tt(Op::LetSet {
                                        idx: SINK,
                                        ty: IrType::I32,
                                    }));
                                    // cp = (b0 & 0x0F) * 4096
                                    t.push(tt(Op::LetGet {
                                        idx: B0,
                                        ty: IrType::I32,
                                    }));
                                    t.push(tt(Op::ConstI32(0x0F)));
                                    t.push(tt(Op::BitAnd(IrType::I32)));
                                    t.push(tt(Op::ConstI32(4096)));
                                    t.push(tt(Op::Mul(IrType::I32)));
                                    // + (b1 & 0x3F) * 64
                                    t.extend(load_continuation(1));
                                    t.push(tt(Op::LetGet {
                                        idx: B_TMP,
                                        ty: IrType::I32,
                                    }));
                                    t.push(tt(Op::ConstI32(64)));
                                    t.push(tt(Op::Mul(IrType::I32)));
                                    t.push(tt(Op::Add(IrType::I32)));
                                    // + (b2 & 0x3F)
                                    t.extend(load_continuation(2));
                                    t.push(tt(Op::LetGet {
                                        idx: B_TMP,
                                        ty: IrType::I32,
                                    }));
                                    t.push(tt(Op::Add(IrType::I32)));
                                    t.push(tt(Op::LetSet {
                                        idx: CP,
                                        ty: IrType::I32,
                                    }));
                                    t.push(tt(Op::ConstI32(3)));
                                    t.push(tt(Op::LetSet {
                                        idx: CP_BYTES,
                                        ty: IrType::I32,
                                    }));
                                    t.push(tt(Op::ConstI32(0)));
                                    t
                                },
                                else_body: {
                                    let mut e2 = Vec::new();
                                    // 4-byte (0xF0..=0xF7) — reject 0xF8+
                                    e2.push(tt(Op::LetGet {
                                        idx: B0,
                                        ty: IrType::I32,
                                    }));
                                    e2.push(tt(Op::ConstI32(0xF8)));
                                    e2.push(tt(Op::Ge(IrType::I32)));
                                    e2.push(tt(Op::If {
                                        result_ty: IrType::I32,
                                        then_body: trap_invalid_utf8(),
                                        else_body: vec![tt(Op::ConstI32(0))],
                                    }));
                                    e2.push(tt(Op::LetSet {
                                        idx: SINK,
                                        ty: IrType::I32,
                                    }));
                                    // require i + 3 < s_len
                                    e2.push(tt(Op::LetGet {
                                        idx: I,
                                        ty: IrType::I32,
                                    }));
                                    e2.push(tt(Op::ConstI32(3)));
                                    e2.push(tt(Op::Add(IrType::I32)));
                                    e2.push(tt(Op::LetGet {
                                        idx: S_LEN,
                                        ty: IrType::I32,
                                    }));
                                    e2.push(tt(Op::Ge(IrType::I32)));
                                    e2.push(tt(Op::If {
                                        result_ty: IrType::I32,
                                        then_body: trap_invalid_utf8(),
                                        else_body: vec![tt(Op::ConstI32(0))],
                                    }));
                                    e2.push(tt(Op::LetSet {
                                        idx: SINK,
                                        ty: IrType::I32,
                                    }));
                                    // cp = (b0 & 0x07) * 262144
                                    e2.push(tt(Op::LetGet {
                                        idx: B0,
                                        ty: IrType::I32,
                                    }));
                                    e2.push(tt(Op::ConstI32(0x07)));
                                    e2.push(tt(Op::BitAnd(IrType::I32)));
                                    e2.push(tt(Op::ConstI32(262144)));
                                    e2.push(tt(Op::Mul(IrType::I32)));
                                    // + (b1 & 0x3F) * 4096
                                    e2.extend(load_continuation(1));
                                    e2.push(tt(Op::LetGet {
                                        idx: B_TMP,
                                        ty: IrType::I32,
                                    }));
                                    e2.push(tt(Op::ConstI32(4096)));
                                    e2.push(tt(Op::Mul(IrType::I32)));
                                    e2.push(tt(Op::Add(IrType::I32)));
                                    // + (b2 & 0x3F) * 64
                                    e2.extend(load_continuation(2));
                                    e2.push(tt(Op::LetGet {
                                        idx: B_TMP,
                                        ty: IrType::I32,
                                    }));
                                    e2.push(tt(Op::ConstI32(64)));
                                    e2.push(tt(Op::Mul(IrType::I32)));
                                    e2.push(tt(Op::Add(IrType::I32)));
                                    // + (b3 & 0x3F)
                                    e2.extend(load_continuation(3));
                                    e2.push(tt(Op::LetGet {
                                        idx: B_TMP,
                                        ty: IrType::I32,
                                    }));
                                    e2.push(tt(Op::Add(IrType::I32)));
                                    e2.push(tt(Op::LetSet {
                                        idx: CP,
                                        ty: IrType::I32,
                                    }));
                                    e2.push(tt(Op::ConstI32(4)));
                                    e2.push(tt(Op::LetSet {
                                        idx: CP_BYTES,
                                        ty: IrType::I32,
                                    }));
                                    e2.push(tt(Op::ConstI32(0)));
                                    e2
                                },
                            }));
                            e
                        },
                    }));
                    v2
                },
            }));
            v
        },
    }));
    decode_seq.push(tt(Op::LetSet {
        idx: SINK,
        ty: IrType::I32,
    }));

    // ----- grapheme-aware fold lookup -----
    // Stack precondition: empty. Postcondition: empty. Writes into
    // FOLDED + IS_MARK, and (for `Title` mode) updates AT_WORD_START.
    //
    // Cross-mode shape:
    //   * Compute `is_mark = __is_combining_mark(cp, ranges_addr)`,
    //     stash it in IS_MARK.
    //   * If `is_mark == 1`: FOLDED = cp (identity passthrough).
    //   * Else: per-mode logic decides FOLDED + AT_WORD_START update.
    //
    // Indices are hardcoded — see [`CASEFOLD_LOOKUP_INDEX`] /
    // [`COMBINING_MARK_INDEX`] / [`IS_WHITESPACE_INDEX`] for the
    // cycle-breaking rationale. Unit-tested by the three
    // `*_index_is_stable` checks.
    let casefold_idx = CASEFOLD_LOOKUP_INDEX;
    let combining_idx = COMBINING_MARK_INDEX;
    let whitespace_idx = IS_WHITESPACE_INDEX;
    let mut fold_seq: Vec<TaggedOp> = Vec::new();

    // Default state for the multi-cp overlay: no table fired, lower
    // mode. Each non-passthrough branch flips these as appropriate so
    // the post-fold stage can dispatch FULL lookup correctly.
    fold_seq.push(tt(Op::ConstI32(0)));
    fold_seq.push(tt(Op::LetSet {
        idx: FOLD_TABLE_FIRED,
        ty: IrType::I32,
    }));
    fold_seq.push(tt(Op::ConstI32(0)));
    fold_seq.push(tt(Op::LetSet {
        idx: EFFECTIVE_UPPER,
        ty: IrType::I32,
    }));

    // is_mark = __is_combining_mark(cp, combining_marks_addr)
    fold_seq.push(tt(Op::LetGet {
        idx: CP,
        ty: IrType::I32,
    }));
    fold_seq.push(tt(Op::CombiningMarkRangesAddr));
    fold_seq.push(tt(Op::Call {
        fn_index: combining_idx,
        arg_count: 2,
        param_tys: vec![IrType::I32, IrType::I32],
        ret_ty: IrType::I32,
    }));
    fold_seq.push(tt(Op::LetSet {
        idx: IS_MARK,
        ty: IrType::I32,
    }));

    // Per-mode FOLDED + AT_WORD_START decision.
    //
    // Build helper closures that produce the inner i32-typed if-arms
    // — every arm leaves a placeholder i32 on the operand stack so
    // the outer `Op::If { result_ty: I32 }` typechecks; the trailing
    // `LetSet SINK` drops it.
    let cp_as_folded = || -> Vec<TaggedOp> {
        // FOLDED = cp
        vec![
            tt(Op::LetGet {
                idx: CP,
                ty: IrType::I32,
            }),
            tt(Op::LetSet {
                idx: FOLDED,
                ty: IrType::I32,
            }),
            tt(Op::ConstI32(0)),
        ]
    };

    let lookup_through_table = |upper: bool| -> Vec<TaggedOp> {
        // Default path: FOLDED = __casefold_lookup(cp, default-table-addr).
        // Also flip the multi-cp overlay's gate: the table fired, and
        // EFFECTIVE_UPPER captures the per-cp mode so the FULL-table
        // fall-through can pick the right table family.
        let default_seq: Vec<TaggedOp> = vec![
            tt(Op::LetGet {
                idx: CP,
                ty: IrType::I32,
            }),
            tt(Op::CaseFoldTableAddr { upper }),
            tt(Op::Call {
                fn_index: casefold_idx,
                arg_count: 2,
                param_tys: vec![IrType::I32, IrType::I32],
                ret_ty: IrType::I32,
            }),
            tt(Op::LetSet {
                idx: FOLDED,
                ty: IrType::I32,
            }),
            tt(Op::ConstI32(1)),
            tt(Op::LetSet {
                idx: FOLD_TABLE_FIRED,
                ty: IrType::I32,
            }),
            tt(Op::ConstI32(if upper { 1 } else { 0 })),
            tt(Op::LetSet {
                idx: EFFECTIVE_UPPER,
                ty: IrType::I32,
            }),
            tt(Op::ConstI32(0)),
        ];

        if !locale_aware {
            return default_seq;
        }

        // Locale-aware path: when IS_TURKISH == 1, consult the
        // Turkish / Azerbaijani override table first; on a miss the
        // helper returns the input cp unchanged, in which case we
        // fall back to the default table. The detection is purely
        // arithmetic — the override table never maps a codepoint to
        // itself, so `result != cp` exactly distinguishes hit from
        // miss without needing a separate found flag.
        let mut v: Vec<TaggedOp> = Vec::new();
        v.push(tt(Op::LetGet {
            idx: IS_TURKISH,
            ty: IrType::I32,
        }));
        v.push(tt(Op::ConstI32(1)));
        v.push(tt(Op::Eq(IrType::I32)));
        v.push(tt(Op::If {
            result_ty: IrType::I32,
            then_body: {
                let mut t: Vec<TaggedOp> = Vec::new();
                // turkish_result = __casefold_lookup(cp, turkish_addr)
                t.push(tt(Op::LetGet {
                    idx: CP,
                    ty: IrType::I32,
                }));
                t.push(tt(Op::TurkishCaseFoldTableAddr { upper }));
                t.push(tt(Op::Call {
                    fn_index: casefold_idx,
                    arg_count: 2,
                    param_tys: vec![IrType::I32, IrType::I32],
                    ret_ty: IrType::I32,
                }));
                t.push(tt(Op::LetSet {
                    idx: TURKISH_RESULT,
                    ty: IrType::I32,
                }));
                // if turkish_result != cp { FOLDED = turkish_result }
                // else { fall through to default lookup }
                t.push(tt(Op::LetGet {
                    idx: TURKISH_RESULT,
                    ty: IrType::I32,
                }));
                t.push(tt(Op::LetGet {
                    idx: CP,
                    ty: IrType::I32,
                }));
                t.push(tt(Op::Eq(IrType::I32)));
                t.push(tt(Op::If {
                    result_ty: IrType::I32,
                    then_body: default_seq.clone(),
                    else_body: {
                        // Turkish hit — `default_seq` is skipped so we
                        // must mirror its FOLD_TABLE_FIRED /
                        // EFFECTIVE_UPPER bookkeeping here. Otherwise
                        // the post-fold FULL / final-sigma overlay
                        // would see "table did not fire" and emit the
                        // wrong cp.
                        let mut inner = Vec::new();
                        inner.push(tt(Op::LetGet {
                            idx: TURKISH_RESULT,
                            ty: IrType::I32,
                        }));
                        inner.push(tt(Op::LetSet {
                            idx: FOLDED,
                            ty: IrType::I32,
                        }));
                        inner.push(tt(Op::ConstI32(1)));
                        inner.push(tt(Op::LetSet {
                            idx: FOLD_TABLE_FIRED,
                            ty: IrType::I32,
                        }));
                        inner.push(tt(Op::ConstI32(if upper { 1 } else { 0 })));
                        inner.push(tt(Op::LetSet {
                            idx: EFFECTIVE_UPPER,
                            ty: IrType::I32,
                        }));
                        inner.push(tt(Op::ConstI32(0)));
                        inner
                    },
                }));
                t.push(tt(Op::LetSet {
                    idx: SINK,
                    ty: IrType::I32,
                }));
                t.push(tt(Op::ConstI32(0)));
                t
            },
            else_body: default_seq,
        }));
        v.push(tt(Op::LetSet {
            idx: SINK,
            ty: IrType::I32,
        }));
        v.push(tt(Op::ConstI32(0)));
        v
    };

    // The `if is_mark == 1 { FOLDED = cp } else { <mode-specific> }`
    // outer wrapper — common to every mode.
    let non_mark_branch: Vec<TaggedOp> = match mode {
        CaseFoldMode::Upper => {
            // All non-mark codepoints map through the upper table.
            lookup_through_table(true)
        }
        CaseFoldMode::Lower => {
            // All non-mark codepoints map through the lower table.
            lookup_through_table(false)
        }
        CaseFoldMode::Title => {
            // is_ws = __is_whitespace(cp, whitespace_ranges_addr)
            //   if 1: FOLDED = cp; at_word_start = 1
            //   else: lookup against upper or lower table depending
            //         on at_word_start; then clear at_word_start.
            let mut v: Vec<TaggedOp> = Vec::new();
            // tmp = __is_whitespace(cp, ws_addr)
            v.push(tt(Op::LetGet {
                idx: CP,
                ty: IrType::I32,
            }));
            v.push(tt(Op::WhitespaceRangesAddr));
            v.push(tt(Op::Call {
                fn_index: whitespace_idx,
                arg_count: 2,
                param_tys: vec![IrType::I32, IrType::I32],
                ret_ty: IrType::I32,
            }));
            // if is_ws == 1 { FOLDED = cp; at_word_start = 1 }
            // else { fold + clear }
            v.push(tt(Op::ConstI32(1)));
            v.push(tt(Op::Eq(IrType::I32)));
            v.push(tt(Op::If {
                result_ty: IrType::I32,
                then_body: {
                    let mut t = Vec::new();
                    // FOLDED = cp
                    t.push(tt(Op::LetGet {
                        idx: CP,
                        ty: IrType::I32,
                    }));
                    t.push(tt(Op::LetSet {
                        idx: FOLDED,
                        ty: IrType::I32,
                    }));
                    // at_word_start = 1
                    t.push(tt(Op::ConstI32(1)));
                    t.push(tt(Op::LetSet {
                        idx: AT_WORD_START,
                        ty: IrType::I32,
                    }));
                    t.push(tt(Op::ConstI32(0)));
                    t
                },
                else_body: {
                    // if at_word_start == 1 { upper } else { lower }
                    let mut e = Vec::new();
                    e.push(tt(Op::LetGet {
                        idx: AT_WORD_START,
                        ty: IrType::I32,
                    }));
                    e.push(tt(Op::ConstI32(1)));
                    e.push(tt(Op::Eq(IrType::I32)));
                    e.push(tt(Op::If {
                        result_ty: IrType::I32,
                        then_body: lookup_through_table(true),
                        else_body: lookup_through_table(false),
                    }));
                    // Drop the placeholder i32 produced by the inner If.
                    e.push(tt(Op::LetSet {
                        idx: SINK,
                        ty: IrType::I32,
                    }));
                    // at_word_start = 0 (cased cp consumed)
                    e.push(tt(Op::ConstI32(0)));
                    e.push(tt(Op::LetSet {
                        idx: AT_WORD_START,
                        ty: IrType::I32,
                    }));
                    e.push(tt(Op::ConstI32(0)));
                    e
                },
            }));
            // Drop the placeholder i32 produced by the whitespace If.
            v.push(tt(Op::LetSet {
                idx: SINK,
                ty: IrType::I32,
            }));
            v.push(tt(Op::ConstI32(0)));
            v
        }
    };

    // if is_mark == 1 { FOLDED = cp } else { <non_mark_branch> }
    fold_seq.push(tt(Op::LetGet {
        idx: IS_MARK,
        ty: IrType::I32,
    }));
    fold_seq.push(tt(Op::ConstI32(1)));
    fold_seq.push(tt(Op::Eq(IrType::I32)));
    fold_seq.push(tt(Op::If {
        result_ty: IrType::I32,
        then_body: cp_as_folded(),
        else_body: non_mark_branch,
    }));
    fold_seq.push(tt(Op::LetSet {
        idx: SINK,
        ty: IrType::I32,
    }));

    // ----- UTF-8 encode step -----
    // Stack precondition: empty. Postcondition: empty. Writes the
    // FOLDED codepoint starting at `base + 4 + j` and advances `j`
    // by 1..=4. Mirrors the 4-arm split on output codepoint range.
    let store_byte = |off: i32, byte_expr: Vec<TaggedOp>| {
        let mut v = Vec::new();
        // addr = base + 4 + j + off
        v.push(tt(Op::LetGet {
            idx: BASE,
            ty: IrType::I32,
        }));
        v.push(tt(Op::ConstI32(4 + off)));
        v.push(tt(Op::Add(IrType::I32)));
        v.push(tt(Op::LetGet {
            idx: J,
            ty: IrType::I32,
        }));
        v.push(tt(Op::Add(IrType::I32)));
        v.extend(byte_expr);
        v.push(tt(Op::StoreI8AtAbsolute { offset: 0 }));
        v
    };
    // Convenience: build the i32 byte expression for `prefix |
    // (FOLDED >> shift_div_pow2 & 0x3F)`. `shift_div_pow2` is the
    // divisor that emulates the wasm-missing `i32.shr_u` op via
    // signed `i32.div_s` — safe because FOLDED is always
    // non-negative (it's a Unicode codepoint <= 0x10FFFF).
    let prefix_plus_shifted = |prefix: i32, shift_div: i32, mask: bool| {
        let mut v = vec![tt(Op::ConstI32(prefix))];
        // shifted = (FOLDED / shift_div)
        v.push(tt(Op::LetGet {
            idx: FOLDED,
            ty: IrType::I32,
        }));
        v.push(tt(Op::ConstI32(shift_div)));
        v.push(tt(Op::Div(IrType::I32)));
        if mask {
            v.push(tt(Op::ConstI32(0x3F)));
            v.push(tt(Op::BitAnd(IrType::I32)));
        }
        v.push(tt(Op::Add(IrType::I32)));
        v
    };

    let mut encode_seq: Vec<TaggedOp> = Vec::new();
    // outer If: FOLDED < 0x80
    encode_seq.push(tt(Op::LetGet {
        idx: FOLDED,
        ty: IrType::I32,
    }));
    encode_seq.push(tt(Op::ConstI32(0x80)));
    encode_seq.push(tt(Op::Lt(IrType::I32)));
    encode_seq.push(tt(Op::If {
        result_ty: IrType::I32,
        then_body: {
            // 1-byte: store FOLDED at base + 4 + j; j += 1
            let mut v = Vec::new();
            v.extend(store_byte(
                0,
                vec![tt(Op::LetGet {
                    idx: FOLDED,
                    ty: IrType::I32,
                })],
            ));
            v.push(tt(Op::LetGet {
                idx: J,
                ty: IrType::I32,
            }));
            v.push(tt(Op::ConstI32(1)));
            v.push(tt(Op::Add(IrType::I32)));
            v.push(tt(Op::LetSet {
                idx: J,
                ty: IrType::I32,
            }));
            v.push(tt(Op::ConstI32(0)));
            v
        },
        else_body: {
            let mut v = Vec::new();
            // FOLDED < 0x800?
            v.push(tt(Op::LetGet {
                idx: FOLDED,
                ty: IrType::I32,
            }));
            v.push(tt(Op::ConstI32(0x800)));
            v.push(tt(Op::Lt(IrType::I32)));
            v.push(tt(Op::If {
                result_ty: IrType::I32,
                then_body: {
                    // 2-byte: 0xC0 | (cp >> 6), 0x80 | (cp & 0x3F)
                    let mut t = Vec::new();
                    t.extend(store_byte(0, prefix_plus_shifted(0xC0, 64, false)));
                    t.extend(store_byte(1, {
                        let mut e = vec![tt(Op::ConstI32(0x80))];
                        e.push(tt(Op::LetGet {
                            idx: FOLDED,
                            ty: IrType::I32,
                        }));
                        e.push(tt(Op::ConstI32(0x3F)));
                        e.push(tt(Op::BitAnd(IrType::I32)));
                        e.push(tt(Op::Add(IrType::I32)));
                        e
                    }));
                    t.push(tt(Op::LetGet {
                        idx: J,
                        ty: IrType::I32,
                    }));
                    t.push(tt(Op::ConstI32(2)));
                    t.push(tt(Op::Add(IrType::I32)));
                    t.push(tt(Op::LetSet {
                        idx: J,
                        ty: IrType::I32,
                    }));
                    t.push(tt(Op::ConstI32(0)));
                    t
                },
                else_body: {
                    let mut e = Vec::new();
                    // FOLDED < 0x10000?
                    e.push(tt(Op::LetGet {
                        idx: FOLDED,
                        ty: IrType::I32,
                    }));
                    e.push(tt(Op::ConstI32(0x10000)));
                    e.push(tt(Op::Lt(IrType::I32)));
                    e.push(tt(Op::If {
                        result_ty: IrType::I32,
                        then_body: {
                            // 3-byte
                            let mut t = Vec::new();
                            t.extend(store_byte(0, prefix_plus_shifted(0xE0, 4096, false)));
                            t.extend(store_byte(1, prefix_plus_shifted(0x80, 64, true)));
                            t.extend(store_byte(2, {
                                let mut ee = vec![tt(Op::ConstI32(0x80))];
                                ee.push(tt(Op::LetGet {
                                    idx: FOLDED,
                                    ty: IrType::I32,
                                }));
                                ee.push(tt(Op::ConstI32(0x3F)));
                                ee.push(tt(Op::BitAnd(IrType::I32)));
                                ee.push(tt(Op::Add(IrType::I32)));
                                ee
                            }));
                            t.push(tt(Op::LetGet {
                                idx: J,
                                ty: IrType::I32,
                            }));
                            t.push(tt(Op::ConstI32(3)));
                            t.push(tt(Op::Add(IrType::I32)));
                            t.push(tt(Op::LetSet {
                                idx: J,
                                ty: IrType::I32,
                            }));
                            t.push(tt(Op::ConstI32(0)));
                            t
                        },
                        else_body: {
                            // 4-byte (FOLDED < 0x110000 guaranteed by
                            // table contents; we don't re-check)
                            let mut t = Vec::new();
                            t.extend(store_byte(0, prefix_plus_shifted(0xF0, 262144, false)));
                            t.extend(store_byte(1, prefix_plus_shifted(0x80, 4096, true)));
                            t.extend(store_byte(2, prefix_plus_shifted(0x80, 64, true)));
                            t.extend(store_byte(3, {
                                let mut ee = vec![tt(Op::ConstI32(0x80))];
                                ee.push(tt(Op::LetGet {
                                    idx: FOLDED,
                                    ty: IrType::I32,
                                }));
                                ee.push(tt(Op::ConstI32(0x3F)));
                                ee.push(tt(Op::BitAnd(IrType::I32)));
                                ee.push(tt(Op::Add(IrType::I32)));
                                ee
                            }));
                            t.push(tt(Op::LetGet {
                                idx: J,
                                ty: IrType::I32,
                            }));
                            t.push(tt(Op::ConstI32(4)));
                            t.push(tt(Op::Add(IrType::I32)));
                            t.push(tt(Op::LetSet {
                                idx: J,
                                ty: IrType::I32,
                            }));
                            t.push(tt(Op::ConstI32(0)));
                            t
                        },
                    }));
                    e
                },
            }));
            v
        },
    }));
    encode_seq.push(tt(Op::LetSet {
        idx: SINK,
        ty: IrType::I32,
    }));

    // ----- v3++ b-7 reframed multi-cp overlay -----
    //
    // Bridges the gap between the simple-table FOLDED single-cp emit
    // (existing) and the UAX #21 expansions that map a single input
    // codepoint to up to 3 output codepoints (`ß` -> `SS`, `ﬁ` -> `FI`,
    // `İ` -> `i` + combining-dot-above, …) plus the Greek final-sigma
    // context rule (`Σ` -> `ς` at end of word, `σ` otherwise).
    //
    // Shape:
    //   if FOLD_TABLE_FIRED == 0 { <encode FOLDED once> }
    //   else if cp == 0x03A3 && EFFECTIVE_UPPER == 0 {
    //       // final-sigma override; never multi-cp
    //       if __final_sigma_check(s, i, cased, ignorable) == 1 {
    //           FOLDED = 0x03C2  (ς)
    //       }
    //       <encode FOLDED once>
    //   } else {
    //       FULL_ENTRY = __full_casefold_lookup(cp,
    //           FullCaseFoldTableAddr{ upper: EFFECTIVE_UPPER })
    //       if FULL_ENTRY != 0 {
    //           FULL_OUT_LEN = load(FULL_ENTRY + 16)
    //           for slot in 0..FULL_OUT_LEN {
    //               FOLDED = load(FULL_ENTRY + 4 + 4*slot)
    //               <encode FOLDED once>
    //           }
    //       } else {
    //           <encode FOLDED once>
    //       }
    //   }
    //
    // The "Turkish FULL" path is *not* needed today — every entry in
    // the Turkish override table is 1:1, so the simple-table path
    // (already wired through `lookup_through_table`) already handles
    // them via `encode_simple_view_bytes`. If a future UCD revision
    // adds a multi-cp Turkish override, this overlay is the place to
    // dispatch the alternate FULL table.

    // Helper: emit FOLDED via the existing encode_seq, cloned once
    // per call site (FOLDED is mutated between emits in the multi-cp
    // loop). The clone is cheap — the IR tree is small.
    let emit_folded_once = || encode_seq.clone();

    // Helper: load `FULL_ENTRY + lit_off` as i32. Used to fetch
    // out_len and the three output codepoints.
    let load_full_entry_off = |lit_off: u32| -> Vec<TaggedOp> {
        vec![
            tt(Op::LetGet {
                idx: FULL_ENTRY,
                ty: IrType::I32,
            }),
            tt(Op::LoadI32AtAbsolute { offset: lit_off }),
        ]
    };

    // ----- FULL lookup helpers -----
    // Compile the FULL-table lookup once per `EFFECTIVE_UPPER` value.
    // For Title mode both compile-time options are reachable; for
    // Upper / Lower only one is. We always emit both branches for
    // simplicity — the dead arm is pruned by cranelift / wasm DCE.
    let full_lookup_for = |upper: bool| -> Vec<TaggedOp> {
        vec![
            tt(Op::LetGet {
                idx: CP,
                ty: IrType::I32,
            }),
            tt(Op::FullCaseFoldTableAddr { upper }),
            tt(Op::Call {
                fn_index: FULL_CASEFOLD_LOOKUP_INDEX,
                arg_count: 2,
                param_tys: vec![IrType::I32, IrType::I32],
                ret_ty: IrType::I32,
            }),
            tt(Op::LetSet {
                idx: FULL_ENTRY,
                ty: IrType::I32,
            }),
        ]
    };

    // FULL_ENTRY = upper ? full_lookup(true) : full_lookup(false).
    // Branches on EFFECTIVE_UPPER at runtime so Title mode picks the
    // right table per codepoint.
    let full_lookup_dispatch: Vec<TaggedOp> = vec![
        tt(Op::LetGet {
            idx: EFFECTIVE_UPPER,
            ty: IrType::I32,
        }),
        tt(Op::ConstI32(1)),
        tt(Op::Eq(IrType::I32)),
        tt(Op::If {
            result_ty: IrType::I32,
            then_body: {
                let mut t = full_lookup_for(true);
                t.push(tt(Op::ConstI32(0)));
                t
            },
            else_body: {
                let mut e = full_lookup_for(false);
                e.push(tt(Op::ConstI32(0)));
                e
            },
        }),
        tt(Op::LetSet {
            idx: SINK,
            ty: IrType::I32,
        }),
    ];

    // Multi-cp emit loop: read FULL_OUT_LEN once, then iterate
    // FULL_SLOT_IDX from 0 to FULL_OUT_LEN; each iteration loads the
    // slot's codepoint into FOLDED and runs the existing encode_seq.
    let multi_emit_loop: Vec<TaggedOp> = {
        let mut v: Vec<TaggedOp> = Vec::new();
        // FULL_OUT_LEN = load(FULL_ENTRY + 16)
        v.extend(load_full_entry_off(16));
        v.push(tt(Op::LetSet {
            idx: FULL_OUT_LEN,
            ty: IrType::I32,
        }));
        // FULL_SLOT_IDX = 0
        v.push(tt(Op::ConstI32(0)));
        v.push(tt(Op::LetSet {
            idx: FULL_SLOT_IDX,
            ty: IrType::I32,
        }));
        v.push(tt(Op::Block {
            result_ty: None,
            body: vec![tt(Op::Loop {
                result_ty: None,
                body: {
                    let mut l: Vec<TaggedOp> = Vec::new();
                    // exit when FULL_SLOT_IDX >= FULL_OUT_LEN
                    l.push(tt(Op::LetGet {
                        idx: FULL_SLOT_IDX,
                        ty: IrType::I32,
                    }));
                    l.push(tt(Op::LetGet {
                        idx: FULL_OUT_LEN,
                        ty: IrType::I32,
                    }));
                    l.push(tt(Op::Ge(IrType::I32)));
                    l.push(tt(Op::BrIf { label_depth: 1 }));
                    // FOLDED = load(FULL_ENTRY + 4 + 4 * FULL_SLOT_IDX)
                    l.push(tt(Op::LetGet {
                        idx: FULL_ENTRY,
                        ty: IrType::I32,
                    }));
                    l.push(tt(Op::ConstI32(4)));
                    l.push(tt(Op::Add(IrType::I32)));
                    l.push(tt(Op::LetGet {
                        idx: FULL_SLOT_IDX,
                        ty: IrType::I32,
                    }));
                    l.push(tt(Op::ConstI32(4)));
                    l.push(tt(Op::Mul(IrType::I32)));
                    l.push(tt(Op::Add(IrType::I32)));
                    l.push(tt(Op::LoadI32AtAbsolute { offset: 0 }));
                    l.push(tt(Op::LetSet {
                        idx: FOLDED,
                        ty: IrType::I32,
                    }));
                    // <encode FOLDED once>
                    l.extend(emit_folded_once());
                    // FULL_SLOT_IDX += 1
                    l.push(tt(Op::LetGet {
                        idx: FULL_SLOT_IDX,
                        ty: IrType::I32,
                    }));
                    l.push(tt(Op::ConstI32(1)));
                    l.push(tt(Op::Add(IrType::I32)));
                    l.push(tt(Op::LetSet {
                        idx: FULL_SLOT_IDX,
                        ty: IrType::I32,
                    }));
                    l.push(tt(Op::Br { label_depth: 0 }));
                    l
                },
            })],
        }));
        v
    };

    // Final-sigma override: only for the Lower effective mode at cp =
    // U+03A3. The simple table already maps Σ -> σ (U+03C3); we
    // override to ς (U+03C2) when the context-scan helper returns 1.
    let final_sigma_override: Vec<TaggedOp> = vec![
        tt(Op::LocalGet(0)),
        tt(Op::LetGet {
            idx: I,
            ty: IrType::I32,
        }),
        tt(Op::CasedRangesAddr),
        tt(Op::CaseIgnorableRangesAddr),
        tt(Op::Call {
            fn_index: FINAL_SIGMA_CHECK_INDEX,
            arg_count: 4,
            param_tys: vec![IrType::I32, IrType::I32, IrType::I32, IrType::I32],
            ret_ty: IrType::I32,
        }),
        tt(Op::ConstI32(1)),
        tt(Op::Eq(IrType::I32)),
        tt(Op::If {
            result_ty: IrType::I32,
            then_body: vec![
                tt(Op::ConstI32(0x03C2)),
                tt(Op::LetSet {
                    idx: FOLDED,
                    ty: IrType::I32,
                }),
                tt(Op::ConstI32(0)),
            ],
            else_body: vec![tt(Op::ConstI32(0))],
        }),
        tt(Op::LetSet {
            idx: SINK,
            ty: IrType::I32,
        }),
    ];

    // Sigma-detection guard: cp == 0x03A3 && EFFECTIVE_UPPER == 0.
    // Two separate Eq results sum to 2 exactly when both hold, then
    // an `== 2` collapses to the boolean outcome.
    let is_sigma_lower: Vec<TaggedOp> = vec![
        tt(Op::LetGet {
            idx: CP,
            ty: IrType::I32,
        }),
        tt(Op::ConstI32(0x03A3)),
        tt(Op::Eq(IrType::I32)),
        tt(Op::LetGet {
            idx: EFFECTIVE_UPPER,
            ty: IrType::I32,
        }),
        tt(Op::ConstI32(0)),
        tt(Op::Eq(IrType::I32)),
        tt(Op::Add(IrType::I32)),
        tt(Op::ConstI32(2)),
        tt(Op::Eq(IrType::I32)),
    ];

    let multi_cp_overlay: Vec<TaggedOp> = vec![
        tt(Op::LetGet {
            idx: FOLD_TABLE_FIRED,
            ty: IrType::I32,
        }),
        tt(Op::ConstI32(0)),
        tt(Op::Eq(IrType::I32)),
        tt(Op::If {
            result_ty: IrType::I32,
            then_body: {
                // No table fired (combining mark, Title-mode
                // whitespace) — single-cp passthrough.
                let mut t = emit_folded_once();
                t.push(tt(Op::ConstI32(0)));
                t
            },
            else_body: {
                let mut e: Vec<TaggedOp> = Vec::new();
                // Inner If: sigma-lower?
                e.extend(is_sigma_lower);
                e.push(tt(Op::If {
                    result_ty: IrType::I32,
                    then_body: {
                        let mut t = final_sigma_override;
                        t.extend(emit_folded_once());
                        t.push(tt(Op::ConstI32(0)));
                        t
                    },
                    else_body: {
                        // Try FULL lookup.
                        let mut e2 = full_lookup_dispatch;
                        // if FULL_ENTRY != 0 { multi-emit } else
                        // { single-emit }
                        e2.push(tt(Op::LetGet {
                            idx: FULL_ENTRY,
                            ty: IrType::I32,
                        }));
                        e2.push(tt(Op::ConstI32(0)));
                        e2.push(tt(Op::Ne(IrType::I32)));
                        e2.push(tt(Op::If {
                            result_ty: IrType::I32,
                            then_body: {
                                let mut t = multi_emit_loop;
                                t.push(tt(Op::ConstI32(0)));
                                t
                            },
                            else_body: {
                                let mut t = emit_folded_once();
                                t.push(tt(Op::ConstI32(0)));
                                t
                            },
                        }));
                        e2.push(tt(Op::LetSet {
                            idx: SINK,
                            ty: IrType::I32,
                        }));
                        e2.push(tt(Op::ConstI32(0)));
                        e2
                    },
                }));
                e.push(tt(Op::LetSet {
                    idx: SINK,
                    ty: IrType::I32,
                }));
                e.push(tt(Op::ConstI32(0)));
                e
            },
        }),
        tt(Op::LetSet {
            idx: SINK,
            ty: IrType::I32,
        }),
    ];

    // ----- assemble the full body -----
    let mut body: Vec<TaggedOp> = Vec::new();
    // s_len = i32.load(s, 0)
    body.push(tt(Op::LocalGet(0)));
    body.push(tt(Op::LoadI32AtAbsolute { offset: 0 }));
    body.push(tt(Op::LetSet {
        idx: S_LEN,
        ty: IrType::I32,
    }));
    // base = alloc_scratch_dyn(4 + s_len * 4)
    body.push(tt(Op::ConstI32(4)));
    body.push(tt(Op::LetGet {
        idx: S_LEN,
        ty: IrType::I32,
    }));
    body.push(tt(Op::ConstI32(4)));
    body.push(tt(Op::Mul(IrType::I32)));
    body.push(tt(Op::Add(IrType::I32)));
    body.push(tt(Op::AllocScratchDyn));
    body.push(tt(Op::LetSet {
        idx: BASE,
        ty: IrType::I32,
    }));
    // i = 0; j = 0; at_word_start = 1
    //
    // `at_word_start` is initialised to 1 in every mode (Upper / Lower
    // ignore it; Title relies on it). The choice keeps the loop body
    // mode-agnostic — no branchy "if Title" guards around the
    // initialisation.
    body.push(tt(Op::ConstI32(0)));
    body.push(tt(Op::LetSet {
        idx: I,
        ty: IrType::I32,
    }));
    body.push(tt(Op::ConstI32(0)));
    body.push(tt(Op::LetSet {
        idx: J,
        ty: IrType::I32,
    }));
    body.push(tt(Op::ConstI32(1)));
    body.push(tt(Op::LetSet {
        idx: AT_WORD_START,
        ty: IrType::I32,
    }));
    // v3++ b-6: when the body is locale-aware, decode the leading two
    // ASCII letters of the locale parameter and set IS_TURKISH to 1
    // iff they spell `tr`/`TR`/`az`/`AZ` (with optional `-` / `_`
    // subtag separator after position 2). Empty / single-byte / non-
    // ASCII locales fall through to IS_TURKISH = 0, matching the
    // tree-walk evaluator's `is_turkish_locale` helper.
    if locale_aware {
        // Default IS_TURKISH = 0; the conditionals below flip to 1
        // when the locale tag matches.
        body.push(tt(Op::ConstI32(0)));
        body.push(tt(Op::LetSet {
            idx: IS_TURKISH,
            ty: IrType::I32,
        }));
        // locale_len = i32.load(locale_ptr + 0)
        body.push(tt(Op::LocalGet(1)));
        body.push(tt(Op::LoadI32AtAbsolute { offset: 0 }));
        body.push(tt(Op::LetSet {
            idx: TURKISH_RESULT, /* scratch: locale_len */
            ty: IrType::I32,
        }));
        // if locale_len >= 2 { check first two bytes }
        body.push(tt(Op::LetGet {
            idx: TURKISH_RESULT,
            ty: IrType::I32,
        }));
        body.push(tt(Op::ConstI32(2)));
        body.push(tt(Op::Ge(IrType::I32)));
        body.push(tt(Op::If {
            result_ty: IrType::I32,
            then_body: {
                let mut t: Vec<TaggedOp> = Vec::new();
                // Boundary check: when locale_len > 2 the third byte
                // must be `-` (0x2D) or `_` (0x5F), otherwise the
                // two-letter prefix is just a prefix of a longer
                // language code (e.g. `tron`).
                // boundary_ok = (locale_len == 2) || (byte2 == '-') || (byte2 == '_')
                t.push(tt(Op::LetGet {
                    idx: TURKISH_RESULT,
                    ty: IrType::I32,
                }));
                t.push(tt(Op::ConstI32(2)));
                t.push(tt(Op::Eq(IrType::I32)));
                t.push(tt(Op::If {
                    result_ty: IrType::I32,
                    then_body: vec![tt(Op::ConstI32(1))],
                    else_body: {
                        let mut e: Vec<TaggedOp> = Vec::new();
                        // byte2 = i32.load_u8(locale_ptr + 4 + 2)
                        e.push(tt(Op::LocalGet(1)));
                        e.push(tt(Op::ConstI32(6)));
                        e.push(tt(Op::Add(IrType::I32)));
                        e.push(tt(Op::LoadI8UAtAbsolute { offset: 0 }));
                        e.push(tt(Op::LetSet {
                            idx: B0,
                            ty: IrType::I32,
                        }));
                        // (byte2 == 0x2D) || (byte2 == 0x5F)
                        // Encoded as (a + b) where a, b ∈ {0, 1} are
                        // disjoint Eq results — the sum is the OR.
                        e.push(tt(Op::LetGet {
                            idx: B0,
                            ty: IrType::I32,
                        }));
                        e.push(tt(Op::ConstI32(0x2D)));
                        e.push(tt(Op::Eq(IrType::I32)));
                        e.push(tt(Op::LetGet {
                            idx: B0,
                            ty: IrType::I32,
                        }));
                        e.push(tt(Op::ConstI32(0x5F)));
                        e.push(tt(Op::Eq(IrType::I32)));
                        e.push(tt(Op::Add(IrType::I32)));
                        e
                    },
                }));
                t.push(tt(Op::LetSet {
                    idx: B_TMP, /* scratch for boundary_ok */
                    ty: IrType::I32,
                }));
                // Load byte0 and byte1 raw. We compare each against
                // both lowercase and uppercase forms directly (rather
                // than normalising via `| 0x20`, since the IR has no
                // BitOr op today).
                t.push(tt(Op::LocalGet(1)));
                t.push(tt(Op::ConstI32(4)));
                t.push(tt(Op::Add(IrType::I32)));
                t.push(tt(Op::LoadI8UAtAbsolute { offset: 0 }));
                t.push(tt(Op::LetSet {
                    idx: B0,
                    ty: IrType::I32,
                }));
                t.push(tt(Op::LocalGet(1)));
                t.push(tt(Op::ConstI32(5)));
                t.push(tt(Op::Add(IrType::I32)));
                t.push(tt(Op::LoadI8UAtAbsolute { offset: 0 }));
                t.push(tt(Op::LetSet {
                    idx: CP,
                    ty: IrType::I32,
                }));
                // is_tr = (b0 == 't' || b0 == 'T') && (b1 == 'r' || b1 == 'R')
                //
                // Each clause is an Eq returning 0/1, summed via Add to
                // emulate OR over disjoint booleans. Then BitAnd
                // multiplies the two clause results into the final
                // 0/1 outcome.
                t.push(tt(Op::LetGet {
                    idx: B0,
                    ty: IrType::I32,
                }));
                t.push(tt(Op::ConstI32(b't' as i32)));
                t.push(tt(Op::Eq(IrType::I32)));
                t.push(tt(Op::LetGet {
                    idx: B0,
                    ty: IrType::I32,
                }));
                t.push(tt(Op::ConstI32(b'T' as i32)));
                t.push(tt(Op::Eq(IrType::I32)));
                t.push(tt(Op::Add(IrType::I32)));
                t.push(tt(Op::LetGet {
                    idx: CP,
                    ty: IrType::I32,
                }));
                t.push(tt(Op::ConstI32(b'r' as i32)));
                t.push(tt(Op::Eq(IrType::I32)));
                t.push(tt(Op::LetGet {
                    idx: CP,
                    ty: IrType::I32,
                }));
                t.push(tt(Op::ConstI32(b'R' as i32)));
                t.push(tt(Op::Eq(IrType::I32)));
                t.push(tt(Op::Add(IrType::I32)));
                t.push(tt(Op::BitAnd(IrType::I32)));
                // is_az = (b0 == 'a' || b0 == 'A') && (b1 == 'z' || b1 == 'Z')
                t.push(tt(Op::LetGet {
                    idx: B0,
                    ty: IrType::I32,
                }));
                t.push(tt(Op::ConstI32(b'a' as i32)));
                t.push(tt(Op::Eq(IrType::I32)));
                t.push(tt(Op::LetGet {
                    idx: B0,
                    ty: IrType::I32,
                }));
                t.push(tt(Op::ConstI32(b'A' as i32)));
                t.push(tt(Op::Eq(IrType::I32)));
                t.push(tt(Op::Add(IrType::I32)));
                t.push(tt(Op::LetGet {
                    idx: CP,
                    ty: IrType::I32,
                }));
                t.push(tt(Op::ConstI32(b'z' as i32)));
                t.push(tt(Op::Eq(IrType::I32)));
                t.push(tt(Op::LetGet {
                    idx: CP,
                    ty: IrType::I32,
                }));
                t.push(tt(Op::ConstI32(b'Z' as i32)));
                t.push(tt(Op::Eq(IrType::I32)));
                t.push(tt(Op::Add(IrType::I32)));
                t.push(tt(Op::BitAnd(IrType::I32)));
                // is_tr OR is_az — both are {0,1} so Add gives the OR.
                t.push(tt(Op::Add(IrType::I32)));
                // (is_tr || is_az) && boundary_ok
                t.push(tt(Op::LetGet {
                    idx: B_TMP,
                    ty: IrType::I32,
                }));
                t.push(tt(Op::BitAnd(IrType::I32)));
                t.push(tt(Op::LetSet {
                    idx: IS_TURKISH,
                    ty: IrType::I32,
                }));
                t.push(tt(Op::ConstI32(0)));
                t
            },
            else_body: vec![tt(Op::ConstI32(0))],
        }));
        body.push(tt(Op::LetSet {
            idx: SINK,
            ty: IrType::I32,
        }));
    }
    // block { loop { ... } }
    let mut loop_body: Vec<TaggedOp> = Vec::new();
    // exit when i >= s_len
    loop_body.push(tt(Op::LetGet {
        idx: I,
        ty: IrType::I32,
    }));
    loop_body.push(tt(Op::LetGet {
        idx: S_LEN,
        ty: IrType::I32,
    }));
    loop_body.push(tt(Op::Ge(IrType::I32)));
    loop_body.push(tt(Op::BrIf { label_depth: 1 }));
    // decode -> fold -> (b-7 reframed) multi-cp / final-sigma overlay
    // wraps the original encode_seq so the loop covers FULL and Σ
    // expansions in addition to the simple 1:1 case-fold map.
    loop_body.extend(decode_seq);
    loop_body.extend(fold_seq);
    loop_body.extend(multi_cp_overlay);
    // i += cp_bytes
    loop_body.push(tt(Op::LetGet {
        idx: I,
        ty: IrType::I32,
    }));
    loop_body.push(tt(Op::LetGet {
        idx: CP_BYTES,
        ty: IrType::I32,
    }));
    loop_body.push(tt(Op::Add(IrType::I32)));
    loop_body.push(tt(Op::LetSet {
        idx: I,
        ty: IrType::I32,
    }));
    loop_body.push(tt(Op::Br { label_depth: 0 }));
    body.push(tt(Op::Block {
        result_ty: None,
        body: vec![tt(Op::Loop {
            result_ty: None,
            body: loop_body,
        })],
    }));
    // write header: i32.store(base + 0, j)
    body.push(tt(Op::LetGet {
        idx: BASE,
        ty: IrType::I32,
    }));
    body.push(tt(Op::LetGet {
        idx: J,
        ty: IrType::I32,
    }));
    body.push(tt(Op::StoreI32AtAbsolute { offset: 0 }));
    // return base
    body.push(tt(Op::LetGet {
        idx: BASE,
        ty: IrType::I32,
    }));
    body.push(tt(Op::Return));

    // F-D2-G: the params / ret / name tuple is supplied by the
    // outer `StdlibFunction::new` call; this function only emits the
    // op stream. `locale_aware` stays as the sole flag that mutates
    // the body shape (see the locale-dispatch insertions above); the
    // param vector itself is fixed at the caller (single-arg for the
    // default fold, two-arg for the locale variant).
    body
}

/// v3+ a-4 internal helper: `__casefold_lookup(cp: I32, table_addr: I32) -> I32`.
///
/// Binary-searches the simple case-folding table embedded in the
/// wasm data section and returns the mapped codepoint, or the input
/// codepoint unchanged when no entry matches. The table layout is
/// `[count: u32 LE][(input_cp: u32, output_cp: u32) × count]`; the
/// helper rebases each midpoint against `table_addr + 4 + mid * 8`
/// to pick the entry's input/output pair.
///
/// The helper is **not** part of the user-facing surface — it is
/// only invoked by the rewritten `upper` / `lower` bodies and never
/// surfaces through [`stdlib_method_index`] or
/// [`stdlib_function_index`] (the name leads with `__` to make that
/// intent visible to anyone scanning the registry). DCE keeps it
/// alive iff at least one reachable function reaches `upper` or
/// `lower`, since those are the only callers.
///
/// Locals:
///   * 0 — `count: I32`  table entry count (loaded from header)
///   * 1 — `lo:    I32`  inclusive low bound of the search window
///   * 2 — `hi:    I32`  exclusive high bound of the search window
///   * 3 — `mid:   I32`  midpoint of the current window
///   * 4 — `entry: I32`  absolute address of the entry's input slot
///   * 5 — `key:   I32`  decoded input codepoint of the midpoint
pub(super) fn casefold_lookup_helper() -> StdlibFunction {
    StdlibFunction::new(
        "__casefold_lookup",
        vec![IrType::I32, IrType::I32],
        IrType::I32,
        casefold_lookup_helper_body,
    )
}

fn casefold_lookup_helper_body() -> Vec<TaggedOp> {
    const COUNT: u32 = 0;
    const LO: u32 = 1;
    const HI: u32 = 2;
    const MID: u32 = 3;
    const ENTRY: u32 = 4;
    const KEY: u32 = 5;
    /// Sink slot for the i32 sentinel each `Op::If { result_ty: I32 }`
    /// arm leaves on the operand stack. Kept distinct from MID /
    /// LO / HI so the placeholder doesn't trample the binary-search
    /// state between the "match?" check and the window narrowing.
    const SINK: u32 = 6;
    /// Search result — initialised to the input codepoint so a miss
    /// falls through to "identity" without needing a separate "found"
    /// flag. On a hit the search-loop body writes the mapped
    /// codepoint into this slot and `br 1`s out of the surrounding
    /// block; the function tail simply pushes RESULT and returns.
    const RESULT: u32 = 7;
    vec![
        // count = i32.load(table_addr + 0)
        tt(Op::LocalGet(1)),
        tt(Op::LoadI32AtAbsolute { offset: 0 }),
        tt(Op::LetSet {
            idx: COUNT,
            ty: IrType::I32,
        }),
        // lo = 0; hi = count
        tt(Op::ConstI32(0)),
        tt(Op::LetSet {
            idx: LO,
            ty: IrType::I32,
        }),
        tt(Op::LetGet {
            idx: COUNT,
            ty: IrType::I32,
        }),
        tt(Op::LetSet {
            idx: HI,
            ty: IrType::I32,
        }),
        // result = cp (identity-on-miss).
        tt(Op::LocalGet(0)),
        tt(Op::LetSet {
            idx: RESULT,
            ty: IrType::I32,
        }),
        // block { loop { ... } } — search loop. `Br 1` (outer
        // block) exits the search; the body writes the mapped
        // codepoint into RESULT before the `Br 1` on a match.
        // `Op::Return` cannot be used for early-out — the IR
        // codegen treats it as the function-end marker only and
        // does not emit a wasm `return` instruction at the call
        // site (see codegen `Op::Return` arm). The block-and-flag
        // shape gives the same control flow.
        tt(Op::Block {
            result_ty: None,
            body: vec![tt(Op::Loop {
                result_ty: None,
                body: vec![
                    // exit when lo >= hi
                    tt(Op::LetGet {
                        idx: LO,
                        ty: IrType::I32,
                    }),
                    tt(Op::LetGet {
                        idx: HI,
                        ty: IrType::I32,
                    }),
                    tt(Op::Ge(IrType::I32)),
                    tt(Op::BrIf { label_depth: 1 }),
                    // mid = (lo + hi) / 2
                    tt(Op::LetGet {
                        idx: LO,
                        ty: IrType::I32,
                    }),
                    tt(Op::LetGet {
                        idx: HI,
                        ty: IrType::I32,
                    }),
                    tt(Op::Add(IrType::I32)),
                    tt(Op::ConstI32(2)),
                    tt(Op::Div(IrType::I32)),
                    tt(Op::LetSet {
                        idx: MID,
                        ty: IrType::I32,
                    }),
                    // entry = table_addr + 4 + mid * 8
                    tt(Op::LocalGet(1)),
                    tt(Op::ConstI32(4)),
                    tt(Op::Add(IrType::I32)),
                    tt(Op::LetGet {
                        idx: MID,
                        ty: IrType::I32,
                    }),
                    tt(Op::ConstI32(8)),
                    tt(Op::Mul(IrType::I32)),
                    tt(Op::Add(IrType::I32)),
                    tt(Op::LetSet {
                        idx: ENTRY,
                        ty: IrType::I32,
                    }),
                    // key = i32.load(entry + 0)
                    tt(Op::LetGet {
                        idx: ENTRY,
                        ty: IrType::I32,
                    }),
                    tt(Op::LoadI32AtAbsolute { offset: 0 }),
                    tt(Op::LetSet {
                        idx: KEY,
                        ty: IrType::I32,
                    }),
                    // if key == cp { RESULT = load(entry+4); br 1 }
                    tt(Op::LetGet {
                        idx: KEY,
                        ty: IrType::I32,
                    }),
                    tt(Op::LocalGet(0)),
                    tt(Op::Eq(IrType::I32)),
                    tt(Op::If {
                        result_ty: IrType::I32,
                        then_body: vec![
                            // RESULT = i32.load(entry + 4)
                            tt(Op::LetGet {
                                idx: ENTRY,
                                ty: IrType::I32,
                            }),
                            tt(Op::LoadI32AtAbsolute { offset: 4 }),
                            tt(Op::LetSet {
                                idx: RESULT,
                                ty: IrType::I32,
                            }),
                            // br 2 exits the enclosing block
                            // (depth 0 = If, 1 = Loop, 2 = Block)
                            tt(Op::Br { label_depth: 2 }),
                            tt(Op::ConstI32(0)),
                        ],
                        else_body: vec![tt(Op::ConstI32(0))],
                    }),
                    // sink for the placeholder i32 from the
                    // match-check above. MID stays untouched so
                    // the narrowing arms below can still read the
                    // current midpoint.
                    tt(Op::LetSet {
                        idx: SINK,
                        ty: IrType::I32,
                    }),
                    // narrow the window: if key < cp { lo = mid + 1 } else { hi = mid }
                    tt(Op::LetGet {
                        idx: KEY,
                        ty: IrType::I32,
                    }),
                    tt(Op::LocalGet(0)),
                    tt(Op::Lt(IrType::I32)),
                    tt(Op::If {
                        result_ty: IrType::I32,
                        then_body: vec![
                            tt(Op::LetGet {
                                idx: MID,
                                ty: IrType::I32,
                            }),
                            tt(Op::ConstI32(1)),
                            tt(Op::Add(IrType::I32)),
                            tt(Op::LetSet {
                                idx: LO,
                                ty: IrType::I32,
                            }),
                            tt(Op::ConstI32(0)),
                        ],
                        else_body: vec![
                            tt(Op::LetGet {
                                idx: MID,
                                ty: IrType::I32,
                            }),
                            tt(Op::LetSet {
                                idx: HI,
                                ty: IrType::I32,
                            }),
                            tt(Op::ConstI32(0)),
                        ],
                    }),
                    tt(Op::LetSet {
                        idx: SINK,
                        ty: IrType::I32,
                    }),
                    tt(Op::Br { label_depth: 0 }),
                ],
            })],
        }),
        // RESULT either holds the matched codepoint or the input
        // codepoint (identity-on-miss). Return it.
        tt(Op::LetGet {
            idx: RESULT,
            ty: IrType::I32,
        }),
        tt(Op::Return),
    ]
}

/// v3++ b-4 internal helper:
/// `__is_combining_mark(cp: I32, table_addr: I32) -> I32`.
///
/// Binary-searches the embedded Unicode Mark range table (`Mn + Mc +
/// Me`) for the codepoint `cp`. Returns `1` when `cp` falls inside
/// any `(start, end)` range (inclusive), else `0`.
///
/// Table layout: `[count: u32 LE][(start: u32 LE, end: u32 LE) × count]`.
/// The helper rebases each midpoint against `table_addr + 4 + mid * 8`
/// to load the `(start, end)` pair — same arithmetic shape as
/// [`casefold_lookup_helper`].
///
/// Not surfaced through user-facing dispatch. Only called from the
/// rewritten `title` / `upper` / `lower` bodies to honour the
/// grapheme-cluster contract (see the v3++ b-4 notes on `title` for
/// the rationale).
///
/// Locals:
///   * 0 — `count: I32`  range entry count (loaded from header)
///   * 1 — `lo:    I32`  inclusive low bound
///   * 2 — `hi:    I32`  exclusive high bound
///   * 3 — `mid:   I32`  midpoint
///   * 4 — `entry: I32`  absolute address of the current range
///   * 5 — `start: I32`  inclusive range start
///   * 6 — `end:   I32`  inclusive range end
///   * 7 — `sink:  I32`  drop slot for If-arm placeholders
///   * 8 — `result: I32` 0 by default, 1 on a hit
pub(super) fn is_combining_mark_helper() -> StdlibFunction {
    range_membership_helper("__is_combining_mark")
}

/// v3++ b-4 internal helper:
/// `__is_whitespace(cp: I32, table_addr: I32) -> I32`.
///
/// ASCII fast path then range-membership search. Returns `1` when
/// `cp` is in the Unicode `White_Space` property set, else `0`. The
/// ASCII fast path covers `0x09..=0x0D` (HT/LF/VT/FF/CR) and `0x20`
/// (SPACE); non-ASCII codepoints fall through to a binary search of
/// the embedded non-ASCII whitespace ranges (`U+00A0`, `U+1680`,
/// `U+2000..=U+200A`, `U+2028..=U+2029`, `U+202F`, `U+205F`,
/// `U+3000`).
///
/// Implementation reuses [`range_membership_helper`] for the binary
/// search core — the helper builder accepts the surface name and
/// emits the same range-membership shape. The ASCII fast path is
/// stitched in front so the common case never touches the table.
pub(super) fn is_whitespace_helper() -> StdlibFunction {
    StdlibFunction::new(
        "__is_whitespace",
        vec![IrType::I32, IrType::I32],
        IrType::I32,
        is_whitespace_helper_body,
    )
}

fn is_whitespace_helper_body() -> Vec<TaggedOp> {
    // Locals mirror `range_membership_helper`.
    const COUNT: u32 = 0;
    const LO: u32 = 1;
    const HI: u32 = 2;
    const MID: u32 = 3;
    const ENTRY: u32 = 4;
    const START: u32 = 5;
    const END: u32 = 6;
    const SINK: u32 = 7;
    const RESULT: u32 = 8;
    let body = vec![
        // result = 0
        tt(Op::ConstI32(0)),
        tt(Op::LetSet {
            idx: RESULT,
            ty: IrType::I32,
        }),
        // ASCII fast path:
        //   if 0x09 <= cp <= 0x0D { result = 1; goto end }
        //   if cp == 0x20         { result = 1; goto end }
        // Implemented as `if cp >= 0x09 && cp <= 0x0D` via Lt/Ge.
        // Use an outer Block + Br to short-circuit on any hit so the
        // non-ASCII search stays untouched.
        tt(Op::Block {
            result_ty: None,
            body: vec![
                // if cp >= 0x09 { ... }
                tt(Op::LocalGet(0)),
                tt(Op::ConstI32(0x09)),
                tt(Op::Ge(IrType::I32)),
                tt(Op::If {
                    result_ty: IrType::I32,
                    then_body: vec![
                        // if cp <= 0x0D { result = 1; br 2 }
                        tt(Op::LocalGet(0)),
                        tt(Op::ConstI32(0x0D)),
                        tt(Op::Le(IrType::I32)),
                        tt(Op::If {
                            result_ty: IrType::I32,
                            then_body: vec![
                                tt(Op::ConstI32(1)),
                                tt(Op::LetSet {
                                    idx: RESULT,
                                    ty: IrType::I32,
                                }),
                                tt(Op::Br { label_depth: 2 }),
                                tt(Op::ConstI32(0)),
                            ],
                            else_body: vec![tt(Op::ConstI32(0))],
                        }),
                        tt(Op::LetSet {
                            idx: SINK,
                            ty: IrType::I32,
                        }),
                        tt(Op::ConstI32(0)),
                    ],
                    else_body: vec![tt(Op::ConstI32(0))],
                }),
                tt(Op::LetSet {
                    idx: SINK,
                    ty: IrType::I32,
                }),
                // if cp == 0x20 { result = 1; br 1 }
                tt(Op::LocalGet(0)),
                tt(Op::ConstI32(0x20)),
                tt(Op::Eq(IrType::I32)),
                tt(Op::If {
                    result_ty: IrType::I32,
                    then_body: vec![
                        tt(Op::ConstI32(1)),
                        tt(Op::LetSet {
                            idx: RESULT,
                            ty: IrType::I32,
                        }),
                        tt(Op::Br { label_depth: 1 }),
                        tt(Op::ConstI32(0)),
                    ],
                    else_body: vec![tt(Op::ConstI32(0))],
                }),
                tt(Op::LetSet {
                    idx: SINK,
                    ty: IrType::I32,
                }),
                // non-ASCII search — only entered when ASCII fast
                // path missed. Mirrors `range_membership_search`.
                // count = i32.load(table_addr + 0)
                tt(Op::LocalGet(1)),
                tt(Op::LoadI32AtAbsolute { offset: 0 }),
                tt(Op::LetSet {
                    idx: COUNT,
                    ty: IrType::I32,
                }),
                // lo = 0; hi = count
                tt(Op::ConstI32(0)),
                tt(Op::LetSet {
                    idx: LO,
                    ty: IrType::I32,
                }),
                tt(Op::LetGet {
                    idx: COUNT,
                    ty: IrType::I32,
                }),
                tt(Op::LetSet {
                    idx: HI,
                    ty: IrType::I32,
                }),
                tt(Op::Block {
                    result_ty: None,
                    body: vec![tt(Op::Loop {
                        result_ty: None,
                        body: range_search_loop_body(LO, HI, MID, ENTRY, START, END, SINK, RESULT),
                    })],
                }),
            ],
        }),
        // Return result.
        tt(Op::LetGet {
            idx: RESULT,
            ty: IrType::I32,
        }),
        tt(Op::Return),
    ];
    body
}

/// Shared body generator for the range-membership helpers
/// `__is_combining_mark`. The same binary-search shape services the
/// whitespace helper through [`range_search_loop_body`] — kept as a
/// standalone builder so the wasm body's local layout stays
/// hand-auditable instead of buried inside a higher-order helper.
fn range_membership_helper(name: &'static str) -> StdlibFunction {
    StdlibFunction::new(
        name,
        vec![IrType::I32, IrType::I32],
        IrType::I32,
        range_membership_helper_body,
    )
}

fn range_membership_helper_body() -> Vec<TaggedOp> {
    const COUNT: u32 = 0;
    const LO: u32 = 1;
    const HI: u32 = 2;
    const MID: u32 = 3;
    const ENTRY: u32 = 4;
    const START: u32 = 5;
    const END: u32 = 6;
    const SINK: u32 = 7;
    const RESULT: u32 = 8;
    vec![
        // count = i32.load(table_addr + 0)
        tt(Op::LocalGet(1)),
        tt(Op::LoadI32AtAbsolute { offset: 0 }),
        tt(Op::LetSet {
            idx: COUNT,
            ty: IrType::I32,
        }),
        // lo = 0; hi = count
        tt(Op::ConstI32(0)),
        tt(Op::LetSet {
            idx: LO,
            ty: IrType::I32,
        }),
        tt(Op::LetGet {
            idx: COUNT,
            ty: IrType::I32,
        }),
        tt(Op::LetSet {
            idx: HI,
            ty: IrType::I32,
        }),
        // result = 0 (default miss)
        tt(Op::ConstI32(0)),
        tt(Op::LetSet {
            idx: RESULT,
            ty: IrType::I32,
        }),
        tt(Op::Block {
            result_ty: None,
            body: vec![tt(Op::Loop {
                result_ty: None,
                body: range_search_loop_body(LO, HI, MID, ENTRY, START, END, SINK, RESULT),
            })],
        }),
        tt(Op::LetGet {
            idx: RESULT,
            ty: IrType::I32,
        }),
        tt(Op::Return),
    ]
}

/// Inner loop body for the range-membership binary search. Encoded
/// as a free-standing helper so both `__is_combining_mark` and the
/// `__is_whitespace` non-ASCII path share the exact same control
/// shape — easier to reason about than two parallel sites that drift
/// over time.
///
/// Each entry is 8 bytes: `(start: u32 LE, end: u32 LE)`. The loop
/// narrows the window by comparing `cp` against `start..=end` —
/// `cp < start` shifts `hi`, `cp > end` shifts `lo`, an in-range hit
/// stores `1` into RESULT and `br 2`s out of the enclosing block.
///
/// Caller invariants:
///   * `lo`, `hi`, `result` are initialised before the surrounding
///     block.
///   * Block/loop nesting is `block { loop { <this body> } }` — the
///     `br 1` exits the loop on `lo >= hi`; the `br 2` on a match
///     exits the enclosing block.
#[allow(clippy::too_many_arguments)]
fn range_search_loop_body(
    lo: u32,
    hi: u32,
    mid: u32,
    entry: u32,
    start: u32,
    end: u32,
    sink: u32,
    result: u32,
) -> Vec<TaggedOp> {
    vec![
        // exit when lo >= hi
        tt(Op::LetGet {
            idx: lo,
            ty: IrType::I32,
        }),
        tt(Op::LetGet {
            idx: hi,
            ty: IrType::I32,
        }),
        tt(Op::Ge(IrType::I32)),
        tt(Op::BrIf { label_depth: 1 }),
        // mid = (lo + hi) / 2
        tt(Op::LetGet {
            idx: lo,
            ty: IrType::I32,
        }),
        tt(Op::LetGet {
            idx: hi,
            ty: IrType::I32,
        }),
        tt(Op::Add(IrType::I32)),
        tt(Op::ConstI32(2)),
        tt(Op::Div(IrType::I32)),
        tt(Op::LetSet {
            idx: mid,
            ty: IrType::I32,
        }),
        // entry = table_addr + 4 + mid * 8
        tt(Op::LocalGet(1)),
        tt(Op::ConstI32(4)),
        tt(Op::Add(IrType::I32)),
        tt(Op::LetGet {
            idx: mid,
            ty: IrType::I32,
        }),
        tt(Op::ConstI32(8)),
        tt(Op::Mul(IrType::I32)),
        tt(Op::Add(IrType::I32)),
        tt(Op::LetSet {
            idx: entry,
            ty: IrType::I32,
        }),
        // start = i32.load(entry + 0); end = i32.load(entry + 4)
        tt(Op::LetGet {
            idx: entry,
            ty: IrType::I32,
        }),
        tt(Op::LoadI32AtAbsolute { offset: 0 }),
        tt(Op::LetSet {
            idx: start,
            ty: IrType::I32,
        }),
        tt(Op::LetGet {
            idx: entry,
            ty: IrType::I32,
        }),
        tt(Op::LoadI32AtAbsolute { offset: 4 }),
        tt(Op::LetSet {
            idx: end,
            ty: IrType::I32,
        }),
        // if cp < start { hi = mid }
        tt(Op::LocalGet(0)),
        tt(Op::LetGet {
            idx: start,
            ty: IrType::I32,
        }),
        tt(Op::Lt(IrType::I32)),
        tt(Op::If {
            result_ty: IrType::I32,
            then_body: vec![
                tt(Op::LetGet {
                    idx: mid,
                    ty: IrType::I32,
                }),
                tt(Op::LetSet {
                    idx: hi,
                    ty: IrType::I32,
                }),
                tt(Op::ConstI32(0)),
            ],
            else_body: vec![
                // else if cp > end { lo = mid + 1 } else { match }
                tt(Op::LocalGet(0)),
                tt(Op::LetGet {
                    idx: end,
                    ty: IrType::I32,
                }),
                tt(Op::Gt(IrType::I32)),
                tt(Op::If {
                    result_ty: IrType::I32,
                    then_body: vec![
                        tt(Op::LetGet {
                            idx: mid,
                            ty: IrType::I32,
                        }),
                        tt(Op::ConstI32(1)),
                        tt(Op::Add(IrType::I32)),
                        tt(Op::LetSet {
                            idx: lo,
                            ty: IrType::I32,
                        }),
                        tt(Op::ConstI32(0)),
                    ],
                    else_body: vec![
                        // match — result = 1; br 3 exits the
                        // enclosing block (depth 0 = inner If, 1 =
                        // outer If, 2 = Loop, 3 = Block).
                        tt(Op::ConstI32(1)),
                        tt(Op::LetSet {
                            idx: result,
                            ty: IrType::I32,
                        }),
                        tt(Op::Br { label_depth: 3 }),
                        tt(Op::ConstI32(0)),
                    ],
                }),
                tt(Op::LetSet {
                    idx: sink,
                    ty: IrType::I32,
                }),
                tt(Op::ConstI32(0)),
            ],
        }),
        tt(Op::LetSet {
            idx: sink,
            ty: IrType::I32,
        }),
        tt(Op::Br { label_depth: 0 }),
    ]
}

/// v3++ b-7 reframed internal helper:
/// `__full_casefold_lookup(cp: I32, table_addr: I32) -> I32`.
///
/// Binary-searches the FULL multi-codepoint case-folding table embedded
/// in the const data section. The table layout matches
/// [`crate::full_case_folding::encode_full_table_bytes`]:
///
/// ```text
/// [count: u32 LE]
/// [(in: u32 LE, out0: u32 LE, out1: u32 LE, out2: u32 LE,
///   out_len: u32 LE) x count]
/// ```
///
/// 20-byte per-entry stride; the rebase formula is
/// `table_addr + 4 + mid * 20`.
///
/// Return value: the absolute address of the matched entry on a hit
/// (so the caller can load `out_len` from `entry + 16` and the
/// up-to-three output codepoints from `entry + 4`, `+ 8`, `+ 12`), or
/// `0` on a miss. The `0` sentinel is unambiguous because a valid
/// entry address is always at least `table_addr + 4` — and codegen
/// never plants the const pool at address `0` of the runtime arena
/// (the prologue reserves a guard region).
///
/// Pure mirror of [`casefold_lookup_helper`] modulo the stride and the
/// "return entry address" twist; the binary-search shape is identical.
///
/// Locals:
///   * 0 — `count: I32`   table entry count (loaded from header)
///   * 1 — `lo:    I32`   inclusive low bound of the search window
///   * 2 — `hi:    I32`   exclusive high bound of the search window
///   * 3 — `mid:   I32`   midpoint of the current window
///   * 4 — `entry: I32`   absolute address of the entry's input slot
///   * 5 — `key:   I32`   decoded input codepoint of the midpoint
///   * 6 — `sink:  I32`   drop slot for If-arm placeholders
///   * 7 — `result: I32`  matched entry address; defaults to 0 (miss)
pub(super) fn full_casefold_lookup_helper() -> StdlibFunction {
    StdlibFunction::new(
        "__full_casefold_lookup",
        vec![IrType::I32, IrType::I32],
        IrType::I32,
        full_casefold_lookup_helper_body,
    )
}

#[allow(clippy::vec_init_then_push)]
fn full_casefold_lookup_helper_body() -> Vec<TaggedOp> {
    const COUNT: u32 = 0;
    const LO: u32 = 1;
    const HI: u32 = 2;
    const MID: u32 = 3;
    const ENTRY: u32 = 4;
    const KEY: u32 = 5;
    const SINK: u32 = 6;
    const RESULT: u32 = 7;
    vec![
        // count = i32.load(table_addr + 0)
        tt(Op::LocalGet(1)),
        tt(Op::LoadI32AtAbsolute { offset: 0 }),
        tt(Op::LetSet {
            idx: COUNT,
            ty: IrType::I32,
        }),
        // lo = 0; hi = count
        tt(Op::ConstI32(0)),
        tt(Op::LetSet {
            idx: LO,
            ty: IrType::I32,
        }),
        tt(Op::LetGet {
            idx: COUNT,
            ty: IrType::I32,
        }),
        tt(Op::LetSet {
            idx: HI,
            ty: IrType::I32,
        }),
        // result = 0 (miss sentinel)
        tt(Op::ConstI32(0)),
        tt(Op::LetSet {
            idx: RESULT,
            ty: IrType::I32,
        }),
        tt(Op::Block {
            result_ty: None,
            body: vec![tt(Op::Loop {
                result_ty: None,
                body: vec![
                    // exit when lo >= hi
                    tt(Op::LetGet {
                        idx: LO,
                        ty: IrType::I32,
                    }),
                    tt(Op::LetGet {
                        idx: HI,
                        ty: IrType::I32,
                    }),
                    tt(Op::Ge(IrType::I32)),
                    tt(Op::BrIf { label_depth: 1 }),
                    // mid = (lo + hi) / 2
                    tt(Op::LetGet {
                        idx: LO,
                        ty: IrType::I32,
                    }),
                    tt(Op::LetGet {
                        idx: HI,
                        ty: IrType::I32,
                    }),
                    tt(Op::Add(IrType::I32)),
                    tt(Op::ConstI32(2)),
                    tt(Op::Div(IrType::I32)),
                    tt(Op::LetSet {
                        idx: MID,
                        ty: IrType::I32,
                    }),
                    // entry = table_addr + 4 + mid * 20
                    tt(Op::LocalGet(1)),
                    tt(Op::ConstI32(4)),
                    tt(Op::Add(IrType::I32)),
                    tt(Op::LetGet {
                        idx: MID,
                        ty: IrType::I32,
                    }),
                    tt(Op::ConstI32(20)),
                    tt(Op::Mul(IrType::I32)),
                    tt(Op::Add(IrType::I32)),
                    tt(Op::LetSet {
                        idx: ENTRY,
                        ty: IrType::I32,
                    }),
                    // key = i32.load(entry + 0)
                    tt(Op::LetGet {
                        idx: ENTRY,
                        ty: IrType::I32,
                    }),
                    tt(Op::LoadI32AtAbsolute { offset: 0 }),
                    tt(Op::LetSet {
                        idx: KEY,
                        ty: IrType::I32,
                    }),
                    // if key == cp { RESULT = entry; br 2 }
                    tt(Op::LetGet {
                        idx: KEY,
                        ty: IrType::I32,
                    }),
                    tt(Op::LocalGet(0)),
                    tt(Op::Eq(IrType::I32)),
                    tt(Op::If {
                        result_ty: IrType::I32,
                        then_body: vec![
                            tt(Op::LetGet {
                                idx: ENTRY,
                                ty: IrType::I32,
                            }),
                            tt(Op::LetSet {
                                idx: RESULT,
                                ty: IrType::I32,
                            }),
                            tt(Op::Br { label_depth: 2 }),
                            tt(Op::ConstI32(0)),
                        ],
                        else_body: vec![tt(Op::ConstI32(0))],
                    }),
                    tt(Op::LetSet {
                        idx: SINK,
                        ty: IrType::I32,
                    }),
                    // if key < cp { lo = mid + 1 } else { hi = mid }
                    tt(Op::LetGet {
                        idx: KEY,
                        ty: IrType::I32,
                    }),
                    tt(Op::LocalGet(0)),
                    tt(Op::Lt(IrType::I32)),
                    tt(Op::If {
                        result_ty: IrType::I32,
                        then_body: vec![
                            tt(Op::LetGet {
                                idx: MID,
                                ty: IrType::I32,
                            }),
                            tt(Op::ConstI32(1)),
                            tt(Op::Add(IrType::I32)),
                            tt(Op::LetSet {
                                idx: LO,
                                ty: IrType::I32,
                            }),
                            tt(Op::ConstI32(0)),
                        ],
                        else_body: vec![
                            tt(Op::LetGet {
                                idx: MID,
                                ty: IrType::I32,
                            }),
                            tt(Op::LetSet {
                                idx: HI,
                                ty: IrType::I32,
                            }),
                            tt(Op::ConstI32(0)),
                        ],
                    }),
                    tt(Op::LetSet {
                        idx: SINK,
                        ty: IrType::I32,
                    }),
                    tt(Op::Br { label_depth: 0 }),
                ],
            })],
        }),
        tt(Op::LetGet {
            idx: RESULT,
            ty: IrType::I32,
        }),
        tt(Op::Return),
    ]
}

/// v3++ b-7 reframed internal helper:
/// `__final_sigma_check(s_ptr, byte_offset, cased_addr, ignorable_addr) -> i32`.
///
/// UAX #21 Final_Sigma context test. Returns `1` when `Σ` (U+03A3) at
/// `byte_offset` inside the UTF-8 string `s_ptr` is at the end of a
/// word — preceded by at least one cased codepoint (skipping case-
/// ignorables), and followed only by case-ignorables until the end of
/// the string (or by a non-cased non-ignorable codepoint). Returns `0`
/// otherwise.
///
/// Tree-walk mirror: [`crate::full_case_folding::is_final_sigma_context`].
///
/// String layout: `[len: u32 LE][bytes × len]`. `s_ptr` points at the
/// header; payload bytes live at `s_ptr + 4`. `byte_offset` is the
/// payload-relative offset of the sigma's first UTF-8 byte (0xCE
/// 0xA3); the helper scans `[..byte_offset)` backwards and
/// `[byte_offset + 2..)` forwards, decoding one codepoint at a time.
///
/// The UTF-8 reverse decode finds the start byte by scanning leftward
/// until a byte with the top two bits != `10` is seen (i.e. either
/// ASCII < 0x80 or a 0xC0..=0xFF lead byte). The forward decode reuses
/// the same byte-length-from-lead-byte arithmetic as
/// [`case_fold_body_inner`].
///
/// Calls `__is_combining_mark`-shaped range membership against the
/// `cased_addr` and `ignorable_addr` tables (both share the
/// `[count: u32 LE][(start: u32, end: u32) x count]` layout); the
/// helper reaches them via the [`Op::CasedRangesAddr`] /
/// [`Op::CaseIgnorableRangesAddr`] addresses the caller threads in.
///
/// Locals:
///   * 0  — `s_len: I32`     payload byte length (from header)
///   * 1  — `back:  I32`     reverse scan byte cursor
///   * 2  — `fwd:   I32`     forward scan byte cursor
///   * 3  — `cp:    I32`     codepoint of the currently inspected byte
///   * 4  — `b0:    I32`     scratch for a UTF-8 lead/start byte
///   * 5  — `b1:    I32`     scratch for the byte at offset 1
///   * 6  — `b2:    I32`     scratch for the byte at offset 2
///   * 7  — `b3:    I32`     scratch for the byte at offset 3
///   * 8  — `width: I32`     byte width of the decoded codepoint
///   * 9  — `sink:  I32`     drop slot for If-arm placeholders
///   * 10 — `result: I32`    accumulated return (0 = not final)
///   * 11 — `seen_cased_before: I32`  left-scan saw a cased cp
///   * 12 — `start: I32`     scratch for reverse-walk start cursor
pub(super) fn final_sigma_check_helper() -> StdlibFunction {
    StdlibFunction::new(
        "__final_sigma_check",
        vec![IrType::I32, IrType::I32, IrType::I32, IrType::I32],
        IrType::I32,
        final_sigma_check_helper_body,
    )
}

#[allow(clippy::vec_init_then_push, clippy::too_many_lines)]
fn final_sigma_check_helper_body() -> Vec<TaggedOp> {
    const S_LEN: u32 = 0;
    const BACK: u32 = 1;
    const FWD: u32 = 2;
    const CP: u32 = 3;
    const B0: u32 = 4;
    const B1: u32 = 5;
    const B2: u32 = 6;
    const B3: u32 = 7;
    const WIDTH: u32 = 8;
    const SINK: u32 = 9;
    const RESULT: u32 = 10;
    const SEEN_CASED_BEFORE: u32 = 11;
    const START: u32 = 12;

    // Param indices:
    //   LocalGet(0) = s_ptr (String record pointer)
    //   LocalGet(1) = byte_offset of the sigma's first UTF-8 byte
    //   LocalGet(2) = cased_ranges_addr
    //   LocalGet(3) = ignorable_ranges_addr

    let combining_idx = COMBINING_MARK_INDEX; // reused as generic range-membership

    // Helper: invoke `__is_combining_mark(cp, table_addr)` against an
    // arbitrary range table. Both `CASED_RANGES` and
    // `CASE_IGNORABLE_RANGES` share the same 8-byte stride layout so
    // the helper works without modification.
    let range_lookup = |cp_idx: u32, table_local_idx: u32| -> Vec<TaggedOp> {
        vec![
            tt(Op::LetGet {
                idx: cp_idx,
                ty: IrType::I32,
            }),
            tt(Op::LocalGet(table_local_idx)),
            tt(Op::Call {
                fn_index: combining_idx,
                arg_count: 2,
                param_tys: vec![IrType::I32, IrType::I32],
                ret_ty: IrType::I32,
            }),
        ]
    };

    // Helper: load i32u byte at `s_ptr + 4 + idx`. Pushes one i32 on
    // the stack.
    let load_payload_byte = |idx_local: u32| -> Vec<TaggedOp> {
        vec![
            tt(Op::LocalGet(0)),
            tt(Op::ConstI32(4)),
            tt(Op::Add(IrType::I32)),
            tt(Op::LetGet {
                idx: idx_local,
                ty: IrType::I32,
            }),
            tt(Op::Add(IrType::I32)),
            tt(Op::LoadI8UAtAbsolute { offset: 0 }),
        ]
    };

    // Helper: load i32u byte at `s_ptr + 4 + base_local + literal_off`.
    let load_payload_byte_off = |base_local: u32, lit_off: i32| -> Vec<TaggedOp> {
        vec![
            tt(Op::LocalGet(0)),
            tt(Op::ConstI32(4 + lit_off)),
            tt(Op::Add(IrType::I32)),
            tt(Op::LetGet {
                idx: base_local,
                ty: IrType::I32,
            }),
            tt(Op::Add(IrType::I32)),
            tt(Op::LoadI8UAtAbsolute { offset: 0 }),
        ]
    };

    // Forward UTF-8 decode at `fwd_idx_local`: reads up to 4 bytes,
    // writes the codepoint into CP and the byte width into WIDTH.
    // Mirrors the 4-arm split in `case_fold_body_inner`. Defensive:
    // when the lead byte is malformed (< 0xC2 in the non-ASCII path,
    // or beyond 0xF7), the helper still produces *some* well-formed
    // value — we set CP to the lead byte and WIDTH to 1 so the scan
    // makes forward progress. This path is only ever reached on
    // already-valid UTF-8 (callers guarantee the input is a valid
    // String record), so the defensive branches are dead in practice;
    // they exist only to keep the verifier happy.
    let forward_decode = |idx_local: u32| -> Vec<TaggedOp> {
        let mut seq: Vec<TaggedOp> = Vec::new();
        // b0 = byte at fwd
        seq.extend(load_payload_byte(idx_local));
        seq.push(tt(Op::LetSet {
            idx: B0,
            ty: IrType::I32,
        }));
        // if b0 < 0x80 { cp = b0; width = 1 }
        seq.push(tt(Op::LetGet {
            idx: B0,
            ty: IrType::I32,
        }));
        seq.push(tt(Op::ConstI32(0x80)));
        seq.push(tt(Op::Lt(IrType::I32)));
        seq.push(tt(Op::If {
            result_ty: IrType::I32,
            then_body: vec![
                tt(Op::LetGet {
                    idx: B0,
                    ty: IrType::I32,
                }),
                tt(Op::LetSet {
                    idx: CP,
                    ty: IrType::I32,
                }),
                tt(Op::ConstI32(1)),
                tt(Op::LetSet {
                    idx: WIDTH,
                    ty: IrType::I32,
                }),
                tt(Op::ConstI32(0)),
            ],
            else_body: {
                let mut e: Vec<TaggedOp> = Vec::new();
                // if b0 < 0xE0 { 2-byte }
                e.push(tt(Op::LetGet {
                    idx: B0,
                    ty: IrType::I32,
                }));
                e.push(tt(Op::ConstI32(0xE0)));
                e.push(tt(Op::Lt(IrType::I32)));
                e.push(tt(Op::If {
                    result_ty: IrType::I32,
                    then_body: {
                        let mut t: Vec<TaggedOp> = Vec::new();
                        t.extend(load_payload_byte_off(idx_local, 1));
                        t.push(tt(Op::LetSet {
                            idx: B1,
                            ty: IrType::I32,
                        }));
                        // cp = (b0 & 0x1F) * 64 + (b1 & 0x3F)
                        t.push(tt(Op::LetGet {
                            idx: B0,
                            ty: IrType::I32,
                        }));
                        t.push(tt(Op::ConstI32(0x1F)));
                        t.push(tt(Op::BitAnd(IrType::I32)));
                        t.push(tt(Op::ConstI32(64)));
                        t.push(tt(Op::Mul(IrType::I32)));
                        t.push(tt(Op::LetGet {
                            idx: B1,
                            ty: IrType::I32,
                        }));
                        t.push(tt(Op::ConstI32(0x3F)));
                        t.push(tt(Op::BitAnd(IrType::I32)));
                        t.push(tt(Op::Add(IrType::I32)));
                        t.push(tt(Op::LetSet {
                            idx: CP,
                            ty: IrType::I32,
                        }));
                        t.push(tt(Op::ConstI32(2)));
                        t.push(tt(Op::LetSet {
                            idx: WIDTH,
                            ty: IrType::I32,
                        }));
                        t.push(tt(Op::ConstI32(0)));
                        t
                    },
                    else_body: {
                        let mut e2: Vec<TaggedOp> = Vec::new();
                        // if b0 < 0xF0 { 3-byte }
                        e2.push(tt(Op::LetGet {
                            idx: B0,
                            ty: IrType::I32,
                        }));
                        e2.push(tt(Op::ConstI32(0xF0)));
                        e2.push(tt(Op::Lt(IrType::I32)));
                        e2.push(tt(Op::If {
                            result_ty: IrType::I32,
                            then_body: {
                                let mut t: Vec<TaggedOp> = Vec::new();
                                t.extend(load_payload_byte_off(idx_local, 1));
                                t.push(tt(Op::LetSet {
                                    idx: B1,
                                    ty: IrType::I32,
                                }));
                                t.extend(load_payload_byte_off(idx_local, 2));
                                t.push(tt(Op::LetSet {
                                    idx: B2,
                                    ty: IrType::I32,
                                }));
                                // cp = (b0 & 0x0F)*4096 + (b1 & 0x3F)*64 + (b2 & 0x3F)
                                t.push(tt(Op::LetGet {
                                    idx: B0,
                                    ty: IrType::I32,
                                }));
                                t.push(tt(Op::ConstI32(0x0F)));
                                t.push(tt(Op::BitAnd(IrType::I32)));
                                t.push(tt(Op::ConstI32(4096)));
                                t.push(tt(Op::Mul(IrType::I32)));
                                t.push(tt(Op::LetGet {
                                    idx: B1,
                                    ty: IrType::I32,
                                }));
                                t.push(tt(Op::ConstI32(0x3F)));
                                t.push(tt(Op::BitAnd(IrType::I32)));
                                t.push(tt(Op::ConstI32(64)));
                                t.push(tt(Op::Mul(IrType::I32)));
                                t.push(tt(Op::Add(IrType::I32)));
                                t.push(tt(Op::LetGet {
                                    idx: B2,
                                    ty: IrType::I32,
                                }));
                                t.push(tt(Op::ConstI32(0x3F)));
                                t.push(tt(Op::BitAnd(IrType::I32)));
                                t.push(tt(Op::Add(IrType::I32)));
                                t.push(tt(Op::LetSet {
                                    idx: CP,
                                    ty: IrType::I32,
                                }));
                                t.push(tt(Op::ConstI32(3)));
                                t.push(tt(Op::LetSet {
                                    idx: WIDTH,
                                    ty: IrType::I32,
                                }));
                                t.push(tt(Op::ConstI32(0)));
                                t
                            },
                            else_body: {
                                // 4-byte (0xF0..0xF7 assumed; defensive
                                // tail when the byte is beyond 0xF7
                                // collapses to the 4-byte arm and lets
                                // the scan move past it).
                                let mut t: Vec<TaggedOp> = Vec::new();
                                t.extend(load_payload_byte_off(idx_local, 1));
                                t.push(tt(Op::LetSet {
                                    idx: B1,
                                    ty: IrType::I32,
                                }));
                                t.extend(load_payload_byte_off(idx_local, 2));
                                t.push(tt(Op::LetSet {
                                    idx: B2,
                                    ty: IrType::I32,
                                }));
                                t.extend(load_payload_byte_off(idx_local, 3));
                                t.push(tt(Op::LetSet {
                                    idx: B3,
                                    ty: IrType::I32,
                                }));
                                // cp = (b0 & 0x07)*262144 + (b1 & 0x3F)*4096
                                //    + (b2 & 0x3F)*64 + (b3 & 0x3F)
                                t.push(tt(Op::LetGet {
                                    idx: B0,
                                    ty: IrType::I32,
                                }));
                                t.push(tt(Op::ConstI32(0x07)));
                                t.push(tt(Op::BitAnd(IrType::I32)));
                                t.push(tt(Op::ConstI32(262144)));
                                t.push(tt(Op::Mul(IrType::I32)));
                                t.push(tt(Op::LetGet {
                                    idx: B1,
                                    ty: IrType::I32,
                                }));
                                t.push(tt(Op::ConstI32(0x3F)));
                                t.push(tt(Op::BitAnd(IrType::I32)));
                                t.push(tt(Op::ConstI32(4096)));
                                t.push(tt(Op::Mul(IrType::I32)));
                                t.push(tt(Op::Add(IrType::I32)));
                                t.push(tt(Op::LetGet {
                                    idx: B2,
                                    ty: IrType::I32,
                                }));
                                t.push(tt(Op::ConstI32(0x3F)));
                                t.push(tt(Op::BitAnd(IrType::I32)));
                                t.push(tt(Op::ConstI32(64)));
                                t.push(tt(Op::Mul(IrType::I32)));
                                t.push(tt(Op::Add(IrType::I32)));
                                t.push(tt(Op::LetGet {
                                    idx: B3,
                                    ty: IrType::I32,
                                }));
                                t.push(tt(Op::ConstI32(0x3F)));
                                t.push(tt(Op::BitAnd(IrType::I32)));
                                t.push(tt(Op::Add(IrType::I32)));
                                t.push(tt(Op::LetSet {
                                    idx: CP,
                                    ty: IrType::I32,
                                }));
                                t.push(tt(Op::ConstI32(4)));
                                t.push(tt(Op::LetSet {
                                    idx: WIDTH,
                                    ty: IrType::I32,
                                }));
                                t.push(tt(Op::ConstI32(0)));
                                t
                            },
                        }));
                        e2
                    },
                }));
                e
            },
        }));
        seq.push(tt(Op::LetSet {
            idx: SINK,
            ty: IrType::I32,
        }));
        seq
    };

    // Reverse UTF-8 decode terminating at the byte just before
    // `back_idx_local`. Walks backwards by up to 4 bytes until a byte
    // whose top two bits != `10` appears (that's the lead byte of the
    // preceding codepoint). Writes CP and WIDTH; updates `back_idx_local`
    // to point at the lead byte of the decoded codepoint (so a
    // subsequent iteration can keep walking left from `back_idx_local`).
    //
    // Strategy: store the candidate lead offset in START, decrement
    // `back_idx_local` to start scanning from `back - 1`, then probe
    // up to 4 leftward bytes. On a hit, set `back = start` and forward-
    // decode from there to fill CP/WIDTH. Forward-decoding the one
    // codepoint at the freshly-located start guarantees the cp value
    // matches the canonical 4-arm decode shape — re-using the same
    // forward decode keeps the two paths byte-identical.
    let reverse_decode = |back_idx_local: u32| -> Vec<TaggedOp> {
        let mut seq: Vec<TaggedOp> = Vec::new();
        // start = back - 1
        seq.push(tt(Op::LetGet {
            idx: back_idx_local,
            ty: IrType::I32,
        }));
        seq.push(tt(Op::ConstI32(1)));
        seq.push(tt(Op::Sub(IrType::I32)));
        seq.push(tt(Op::LetSet {
            idx: START,
            ty: IrType::I32,
        }));
        // probe leftwards: while (byte at start) & 0xC0 == 0x80 and we
        // haven't walked past 3 bytes already, start -= 1. Block + Br
        // shape so we can break early on a lead byte.
        seq.push(tt(Op::Block {
            result_ty: None,
            body: vec![tt(Op::Loop {
                result_ty: None,
                body: vec![
                    // if start < 0 || (back - start) > 4 { br 1 }
                    // i.e. we've gone too far — give up and decode a
                    // single byte. (Shouldn't happen on valid UTF-8.)
                    tt(Op::LetGet {
                        idx: START,
                        ty: IrType::I32,
                    }),
                    tt(Op::ConstI32(0)),
                    tt(Op::Lt(IrType::I32)),
                    tt(Op::BrIf { label_depth: 1 }),
                    // Inspect byte at START.
                    tt(Op::LocalGet(0)),
                    tt(Op::ConstI32(4)),
                    tt(Op::Add(IrType::I32)),
                    tt(Op::LetGet {
                        idx: START,
                        ty: IrType::I32,
                    }),
                    tt(Op::Add(IrType::I32)),
                    tt(Op::LoadI8UAtAbsolute { offset: 0 }),
                    tt(Op::LetSet {
                        idx: B0,
                        ty: IrType::I32,
                    }),
                    // is_continuation = (b0 & 0xC0) == 0x80
                    tt(Op::LetGet {
                        idx: B0,
                        ty: IrType::I32,
                    }),
                    tt(Op::ConstI32(0xC0)),
                    tt(Op::BitAnd(IrType::I32)),
                    tt(Op::ConstI32(0x80)),
                    tt(Op::Eq(IrType::I32)),
                    tt(Op::If {
                        result_ty: IrType::I32,
                        then_body: vec![
                            // start -= 1; continue
                            tt(Op::LetGet {
                                idx: START,
                                ty: IrType::I32,
                            }),
                            tt(Op::ConstI32(1)),
                            tt(Op::Sub(IrType::I32)),
                            tt(Op::LetSet {
                                idx: START,
                                ty: IrType::I32,
                            }),
                            tt(Op::ConstI32(0)),
                        ],
                        else_body: vec![
                            // Hit a lead byte — break.
                            tt(Op::Br { label_depth: 2 }),
                            tt(Op::ConstI32(0)),
                        ],
                    }),
                    tt(Op::LetSet {
                        idx: SINK,
                        ty: IrType::I32,
                    }),
                    // Safety net: bound the probe to 4 iterations. If
                    // we've stepped 4 bytes back and still see only
                    // continuations, the input is malformed — give up
                    // and decode whatever single byte sits at start.
                    tt(Op::LetGet {
                        idx: back_idx_local,
                        ty: IrType::I32,
                    }),
                    tt(Op::LetGet {
                        idx: START,
                        ty: IrType::I32,
                    }),
                    tt(Op::Sub(IrType::I32)),
                    tt(Op::ConstI32(4)),
                    tt(Op::Ge(IrType::I32)),
                    tt(Op::BrIf { label_depth: 1 }),
                    tt(Op::Br { label_depth: 0 }),
                ],
            })],
        }));
        // Clamp start to >= 0 in case the safety nets fired.
        seq.push(tt(Op::LetGet {
            idx: START,
            ty: IrType::I32,
        }));
        seq.push(tt(Op::ConstI32(0)));
        seq.push(tt(Op::Lt(IrType::I32)));
        seq.push(tt(Op::If {
            result_ty: IrType::I32,
            then_body: vec![
                tt(Op::ConstI32(0)),
                tt(Op::LetSet {
                    idx: START,
                    ty: IrType::I32,
                }),
                tt(Op::ConstI32(0)),
            ],
            else_body: vec![tt(Op::ConstI32(0))],
        }));
        seq.push(tt(Op::LetSet {
            idx: SINK,
            ty: IrType::I32,
        }));
        // Forward-decode one codepoint from START to fill CP / WIDTH.
        seq.extend(forward_decode(START));
        // back = start (caller continues leftwards from here).
        seq.push(tt(Op::LetGet {
            idx: START,
            ty: IrType::I32,
        }));
        seq.push(tt(Op::LetSet {
            idx: back_idx_local,
            ty: IrType::I32,
        }));
        seq
    };

    vec![
        // s_len = i32.load(s_ptr + 0)
        tt(Op::LocalGet(0)),
        tt(Op::LoadI32AtAbsolute { offset: 0 }),
        tt(Op::LetSet {
            idx: S_LEN,
            ty: IrType::I32,
        }),
        // result = 0 (default: not final)
        tt(Op::ConstI32(0)),
        tt(Op::LetSet {
            idx: RESULT,
            ty: IrType::I32,
        }),
        // seen_cased_before = 0
        tt(Op::ConstI32(0)),
        tt(Op::LetSet {
            idx: SEEN_CASED_BEFORE,
            ty: IrType::I32,
        }),
        // back = byte_offset (will scan leftwards)
        tt(Op::LocalGet(1)),
        tt(Op::LetSet {
            idx: BACK,
            ty: IrType::I32,
        }),
        // ----- LEFT SCAN -----
        // while back > 0 { reverse-decode one cp; if cased: set
        // seen_cased_before=1 and break; if !ignorable: break; else
        // continue. } The scan requires at least one cased cp
        // before the sigma (modulo ignorables) for the form to be
        // final.
        tt(Op::Block {
            result_ty: None,
            body: vec![tt(Op::Loop {
                result_ty: None,
                body: {
                    let mut v: Vec<TaggedOp> = Vec::new();
                    // exit if back == 0 (or < 0)
                    v.push(tt(Op::LetGet {
                        idx: BACK,
                        ty: IrType::I32,
                    }));
                    v.push(tt(Op::ConstI32(1)));
                    v.push(tt(Op::Lt(IrType::I32)));
                    v.push(tt(Op::BrIf { label_depth: 1 }));
                    // reverse-decode: writes CP, WIDTH, and resets
                    // back to the lead byte of the decoded cp.
                    v.extend(reverse_decode(BACK));
                    // is_cased = range_lookup(CP, cased_addr)
                    v.extend(range_lookup(CP, 2));
                    v.push(tt(Op::If {
                        result_ty: IrType::I32,
                        then_body: vec![
                            tt(Op::ConstI32(1)),
                            tt(Op::LetSet {
                                idx: SEEN_CASED_BEFORE,
                                ty: IrType::I32,
                            }),
                            // br 2 exits the enclosing block.
                            tt(Op::Br { label_depth: 2 }),
                            tt(Op::ConstI32(0)),
                        ],
                        else_body: vec![tt(Op::ConstI32(0))],
                    }));
                    v.push(tt(Op::LetSet {
                        idx: SINK,
                        ty: IrType::I32,
                    }));
                    // is_ignorable = range_lookup(CP, ignorable_addr)
                    v.extend(range_lookup(CP, 3));
                    v.push(tt(Op::If {
                        result_ty: IrType::I32,
                        then_body: vec![tt(Op::ConstI32(0))],
                        else_body: vec![
                            // Non-cased, non-ignorable — break left
                            // scan (without flipping seen flag).
                            tt(Op::Br { label_depth: 2 }),
                            tt(Op::ConstI32(0)),
                        ],
                    }));
                    v.push(tt(Op::LetSet {
                        idx: SINK,
                        ty: IrType::I32,
                    }));
                    // continue.
                    v.push(tt(Op::Br { label_depth: 0 }));
                    v
                },
            })],
        }),
        // If we never saw a cased cp on the left, sigma is not
        // final — return 0 directly.
        tt(Op::LetGet {
            idx: SEEN_CASED_BEFORE,
            ty: IrType::I32,
        }),
        tt(Op::ConstI32(0)),
        tt(Op::Eq(IrType::I32)),
        tt(Op::If {
            result_ty: IrType::I32,
            then_body: vec![
                tt(Op::ConstI32(0)),
                tt(Op::LetSet {
                    idx: RESULT,
                    ty: IrType::I32,
                }),
                tt(Op::ConstI32(0)),
            ],
            else_body: vec![tt(Op::ConstI32(0))],
        }),
        tt(Op::LetSet {
            idx: SINK,
            ty: IrType::I32,
        }),
        // Only continue the right scan when seen_cased_before == 1.
        tt(Op::LetGet {
            idx: SEEN_CASED_BEFORE,
            ty: IrType::I32,
        }),
        tt(Op::ConstI32(1)),
        tt(Op::Eq(IrType::I32)),
        tt(Op::If {
            result_ty: IrType::I32,
            then_body: {
                let mut t: Vec<TaggedOp> = Vec::new();
                // fwd = byte_offset + 2 (Σ is 0xCE 0xA3 = 2 bytes)
                t.push(tt(Op::LocalGet(1)));
                t.push(tt(Op::ConstI32(2)));
                t.push(tt(Op::Add(IrType::I32)));
                t.push(tt(Op::LetSet {
                    idx: FWD,
                    ty: IrType::I32,
                }));
                // result = 1 (default until proven non-final by a
                // cased cp on the right; falls back to 0 only via
                // the explicit override below).
                t.push(tt(Op::ConstI32(1)));
                t.push(tt(Op::LetSet {
                    idx: RESULT,
                    ty: IrType::I32,
                }));
                // ----- RIGHT SCAN -----
                t.push(tt(Op::Block {
                    result_ty: None,
                    body: vec![tt(Op::Loop {
                        result_ty: None,
                        body: {
                            let mut v: Vec<TaggedOp> = Vec::new();
                            // exit if fwd >= s_len
                            v.push(tt(Op::LetGet {
                                idx: FWD,
                                ty: IrType::I32,
                            }));
                            v.push(tt(Op::LetGet {
                                idx: S_LEN,
                                ty: IrType::I32,
                            }));
                            v.push(tt(Op::Ge(IrType::I32)));
                            v.push(tt(Op::BrIf { label_depth: 1 }));
                            // forward-decode: writes CP and WIDTH.
                            v.extend(forward_decode(FWD));
                            // is_cased = range_lookup(CP, cased_addr)
                            v.extend(range_lookup(CP, 2));
                            v.push(tt(Op::If {
                                result_ty: IrType::I32,
                                then_body: vec![
                                    // Cased cp — sigma is not final.
                                    tt(Op::ConstI32(0)),
                                    tt(Op::LetSet {
                                        idx: RESULT,
                                        ty: IrType::I32,
                                    }),
                                    tt(Op::Br { label_depth: 2 }),
                                    tt(Op::ConstI32(0)),
                                ],
                                else_body: vec![tt(Op::ConstI32(0))],
                            }));
                            v.push(tt(Op::LetSet {
                                idx: SINK,
                                ty: IrType::I32,
                            }));
                            // is_ignorable = range_lookup(CP, ignorable_addr)
                            v.extend(range_lookup(CP, 3));
                            v.push(tt(Op::If {
                                result_ty: IrType::I32,
                                then_body: vec![tt(Op::ConstI32(0))],
                                else_body: vec![
                                    // Non-cased, non-ignorable —
                                    // word-final. Keep result = 1
                                    // and exit.
                                    tt(Op::Br { label_depth: 2 }),
                                    tt(Op::ConstI32(0)),
                                ],
                            }));
                            v.push(tt(Op::LetSet {
                                idx: SINK,
                                ty: IrType::I32,
                            }));
                            // fwd += width
                            v.push(tt(Op::LetGet {
                                idx: FWD,
                                ty: IrType::I32,
                            }));
                            v.push(tt(Op::LetGet {
                                idx: WIDTH,
                                ty: IrType::I32,
                            }));
                            v.push(tt(Op::Add(IrType::I32)));
                            v.push(tt(Op::LetSet {
                                idx: FWD,
                                ty: IrType::I32,
                            }));
                            v.push(tt(Op::Br { label_depth: 0 }));
                            v
                        },
                    })],
                }));
                t.push(tt(Op::ConstI32(0)));
                t
            },
            else_body: vec![tt(Op::ConstI32(0))],
        }),
        tt(Op::LetSet {
            idx: SINK,
            ty: IrType::I32,
        }),
        tt(Op::LetGet {
            idx: RESULT,
            ty: IrType::I32,
        }),
        tt(Op::Return),
    ]
}
