//! Body builders for the four Unicode normalization stdlib bodies
//! (`nfd` / `nfkd` / `nfc` / `nfkc`) plus the internal lookup helpers
//! they share.
//!
//! Bodies in this file:
//!   * Surface bodies: `nfd(String)`, `nfkd(String)`, `nfc(String)`,
//!     `nfkc(String)` — see UAX #15.
//!   * Internal helpers: `__decomp_lookup`, `__ccc_lookup`,
//!     `__compose_lookup`. The hard-coded slot indices live in
//!     [`super::signatures`].
//!
//! [`NormForm`] is the discriminator threaded through
//! [`normalize_body_ops`], the shared body generator that emits the
//! decompose -> reorder -> (optional) compose pipeline.

use crate::ir::{IrType, Op, TaggedOp, TrapKind};

use super::defs::tt;
use super::signatures::{
    StdlibFunction, CCC_LOOKUP_INDEX, COMPOSE_LOOKUP_INDEX, DECOMP_LOOKUP_INDEX,
};

/// v3++ b-5 normalization form discriminator for [`normalize_body_ops`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NormForm {
    /// NFD - canonical decomposition + canonical reorder.
    Nfd,
    /// NFKD - compatibility decomposition + canonical reorder.
    Nfkd,
    /// NFC - canonical decomposition + reorder + canonical composition.
    Nfc,
    /// NFKC - compatibility decomposition + reorder + canonical composition.
    Nfkc,
}

impl NormForm {
    /// `true` when the form uses the compatibility (NFKD) decomposition
    /// table; `false` for canonical (NFD).
    fn use_compatibility(self) -> bool {
        matches!(self, NormForm::Nfkd | NormForm::Nfkc)
    }

    /// `true` when the form runs a composition pass after decomposition
    /// + reorder.
    fn composes(self) -> bool {
        matches!(self, NormForm::Nfc | NormForm::Nfkc)
    }
}

///
/// Binary-searches the canonical (NFD) or compatibility (NFKD)
/// decomposition index embedded in the wasm data section. The table
/// layout is `[count: u32 LE][(cp: u32, off: u32, len: u32) × count]`
/// (12-byte stride). Returns `(off << 8) | len` on a hit so the
/// caller can decode both in one i32 — len is bounded by 18 (UCD 14
/// worst case is U+FDFA at 18 cps) and off fits in 24 bits since the
/// pool is well under 16M entries. Returns `0` on miss; len of `0` is
/// a valid "no decomposition" sentinel because the table never stores
/// zero-length entries.
pub(super) fn decomp_lookup_helper() -> StdlibFunction {
    StdlibFunction::new(
        "__decomp_lookup",
        vec![IrType::I32, IrType::I32],
        IrType::I32,
        decomp_lookup_helper_body,
    )
}

fn decomp_lookup_helper_body() -> Vec<TaggedOp> {
    const COUNT: u32 = 0;
    const LO: u32 = 1;
    const HI: u32 = 2;
    const MID: u32 = 3;
    const ENTRY: u32 = 4;
    const KEY: u32 = 5;
    const SINK: u32 = 6;
    const RESULT: u32 = 7;
    vec![
        tt(Op::LocalGet(1)),
        tt(Op::LoadI32AtAbsolute { offset: 0 }),
        tt(Op::LetSet {
            idx: COUNT,
            ty: IrType::I32,
        }),
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
                    // entry = table_addr + 4 + mid * 12
                    tt(Op::LocalGet(1)),
                    tt(Op::ConstI32(4)),
                    tt(Op::Add(IrType::I32)),
                    tt(Op::LetGet {
                        idx: MID,
                        ty: IrType::I32,
                    }),
                    tt(Op::ConstI32(12)),
                    tt(Op::Mul(IrType::I32)),
                    tt(Op::Add(IrType::I32)),
                    tt(Op::LetSet {
                        idx: ENTRY,
                        ty: IrType::I32,
                    }),
                    tt(Op::LetGet {
                        idx: ENTRY,
                        ty: IrType::I32,
                    }),
                    tt(Op::LoadI32AtAbsolute { offset: 0 }),
                    tt(Op::LetSet {
                        idx: KEY,
                        ty: IrType::I32,
                    }),
                    tt(Op::LetGet {
                        idx: KEY,
                        ty: IrType::I32,
                    }),
                    tt(Op::LocalGet(0)),
                    tt(Op::Eq(IrType::I32)),
                    tt(Op::If {
                        result_ty: IrType::I32,
                        then_body: vec![
                            // result = (load(entry+4) << 8) | load(entry+8)
                            tt(Op::LetGet {
                                idx: ENTRY,
                                ty: IrType::I32,
                            }),
                            tt(Op::LoadI32AtAbsolute { offset: 4 }),
                            tt(Op::ConstI32(256)),
                            tt(Op::Mul(IrType::I32)),
                            tt(Op::LetGet {
                                idx: ENTRY,
                                ty: IrType::I32,
                            }),
                            tt(Op::LoadI32AtAbsolute { offset: 8 }),
                            tt(Op::Add(IrType::I32)),
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

/// v3++ b-5 internal helper: `__ccc_lookup(cp, table_addr) -> i32`.
///
/// Binary-searches the Canonical_Combining_Class table for `cp`.
/// Returns the CCC value on a hit, or `0` on a miss (the UCD
/// convention: absent entries default to Not_Reordered).
pub(super) fn ccc_lookup_helper() -> StdlibFunction {
    StdlibFunction::new(
        "__ccc_lookup",
        vec![IrType::I32, IrType::I32],
        IrType::I32,
        ccc_lookup_helper_body,
    )
}

fn ccc_lookup_helper_body() -> Vec<TaggedOp> {
    const COUNT: u32 = 0;
    const LO: u32 = 1;
    const HI: u32 = 2;
    const MID: u32 = 3;
    const ENTRY: u32 = 4;
    const KEY: u32 = 5;
    const SINK: u32 = 6;
    const RESULT: u32 = 7;
    vec![
        tt(Op::LocalGet(1)),
        tt(Op::LoadI32AtAbsolute { offset: 0 }),
        tt(Op::LetSet {
            idx: COUNT,
            ty: IrType::I32,
        }),
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
                    tt(Op::LetGet {
                        idx: ENTRY,
                        ty: IrType::I32,
                    }),
                    tt(Op::LoadI32AtAbsolute { offset: 0 }),
                    tt(Op::LetSet {
                        idx: KEY,
                        ty: IrType::I32,
                    }),
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
                            tt(Op::LoadI32AtAbsolute { offset: 4 }),
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

/// v3++ b-5 internal helper:
/// `__compose_lookup(first, second, table_addr) -> i32`.
///
/// Binary-searches the canonical composition pair table sorted
/// lexicographically by `(first, second)`. Returns the composed
/// codepoint on a hit, or `-1` on a miss. Composition exclusions are
/// filtered at generation time so the runtime needs no extra check.
pub(super) fn compose_lookup_helper() -> StdlibFunction {
    StdlibFunction::new(
        "__compose_lookup",
        vec![IrType::I32, IrType::I32, IrType::I32],
        IrType::I32,
        compose_lookup_helper_body,
    )
}

fn compose_lookup_helper_body() -> Vec<TaggedOp> {
    const COUNT: u32 = 0;
    const LO: u32 = 1;
    const HI: u32 = 2;
    const MID: u32 = 3;
    const ENTRY: u32 = 4;
    const KEY_A: u32 = 5;
    const KEY_B: u32 = 6;
    const SINK: u32 = 7;
    const RESULT: u32 = 8;
    vec![
        tt(Op::LocalGet(2)),
        tt(Op::LoadI32AtAbsolute { offset: 0 }),
        tt(Op::LetSet {
            idx: COUNT,
            ty: IrType::I32,
        }),
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
        // sentinel -1: codegen represents as 0xFFFFFFFF i32
        tt(Op::ConstI32(-1)),
        tt(Op::LetSet {
            idx: RESULT,
            ty: IrType::I32,
        }),
        tt(Op::Block {
            result_ty: None,
            body: vec![tt(Op::Loop {
                result_ty: None,
                body: vec![
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
                    // entry = table_addr + 4 + mid * 12
                    tt(Op::LocalGet(2)),
                    tt(Op::ConstI32(4)),
                    tt(Op::Add(IrType::I32)),
                    tt(Op::LetGet {
                        idx: MID,
                        ty: IrType::I32,
                    }),
                    tt(Op::ConstI32(12)),
                    tt(Op::Mul(IrType::I32)),
                    tt(Op::Add(IrType::I32)),
                    tt(Op::LetSet {
                        idx: ENTRY,
                        ty: IrType::I32,
                    }),
                    tt(Op::LetGet {
                        idx: ENTRY,
                        ty: IrType::I32,
                    }),
                    tt(Op::LoadI32AtAbsolute { offset: 0 }),
                    tt(Op::LetSet {
                        idx: KEY_A,
                        ty: IrType::I32,
                    }),
                    tt(Op::LetGet {
                        idx: ENTRY,
                        ty: IrType::I32,
                    }),
                    tt(Op::LoadI32AtAbsolute { offset: 4 }),
                    tt(Op::LetSet {
                        idx: KEY_B,
                        ty: IrType::I32,
                    }),
                    // if key_a == first && key_b == second { match }
                    tt(Op::LetGet {
                        idx: KEY_A,
                        ty: IrType::I32,
                    }),
                    tt(Op::LocalGet(0)),
                    tt(Op::Eq(IrType::I32)),
                    tt(Op::If {
                        result_ty: IrType::I32,
                        then_body: vec![
                            tt(Op::LetGet {
                                idx: KEY_B,
                                ty: IrType::I32,
                            }),
                            tt(Op::LocalGet(1)),
                            tt(Op::Eq(IrType::I32)),
                            tt(Op::If {
                                result_ty: IrType::I32,
                                then_body: vec![
                                    tt(Op::LetGet {
                                        idx: ENTRY,
                                        ty: IrType::I32,
                                    }),
                                    tt(Op::LoadI32AtAbsolute { offset: 8 }),
                                    tt(Op::LetSet {
                                        idx: RESULT,
                                        ty: IrType::I32,
                                    }),
                                    // Depth 0 = inner If, 1 = outer If,
                                    // 2 = Loop, 3 = Block. Jump out of
                                    // the search Block on a hit.
                                    tt(Op::Br { label_depth: 3 }),
                                    tt(Op::ConstI32(0)),
                                ],
                                else_body: vec![tt(Op::ConstI32(0))],
                            }),
                        ],
                        else_body: vec![tt(Op::ConstI32(0))],
                    }),
                    tt(Op::LetSet {
                        idx: SINK,
                        ty: IrType::I32,
                    }),
                    // Compare (key_a, key_b) < (first, second)?
                    // if key_a < first OR (key_a == first AND
                    //                       key_b < second): lo = mid + 1
                    // else: hi = mid
                    tt(Op::LetGet {
                        idx: KEY_A,
                        ty: IrType::I32,
                    }),
                    tt(Op::LocalGet(0)),
                    tt(Op::Lt(IrType::I32)),
                    tt(Op::If {
                        result_ty: IrType::I32,
                        then_body: vec![tt(Op::ConstI32(1))],
                        else_body: vec![
                            // key_a == first?
                            tt(Op::LetGet {
                                idx: KEY_A,
                                ty: IrType::I32,
                            }),
                            tt(Op::LocalGet(0)),
                            tt(Op::Eq(IrType::I32)),
                            tt(Op::If {
                                result_ty: IrType::I32,
                                then_body: vec![
                                    // key_b < second?
                                    tt(Op::LetGet {
                                        idx: KEY_B,
                                        ty: IrType::I32,
                                    }),
                                    tt(Op::LocalGet(1)),
                                    tt(Op::Lt(IrType::I32)),
                                ],
                                else_body: vec![tt(Op::ConstI32(0))],
                            }),
                        ],
                    }),
                    // top of stack: 1 if (key_a, key_b) < (first, second)
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

pub(super) fn nfd_string() -> StdlibFunction {
    StdlibFunction::new("nfd", vec![IrType::String], IrType::String, || {
        normalize_body_ops(NormForm::Nfd)
    })
}

pub(super) fn nfkd_string() -> StdlibFunction {
    StdlibFunction::new("nfkd", vec![IrType::String], IrType::String, || {
        normalize_body_ops(NormForm::Nfkd)
    })
}

pub(super) fn nfc_string() -> StdlibFunction {
    StdlibFunction::new("nfc", vec![IrType::String], IrType::String, || {
        normalize_body_ops(NormForm::Nfc)
    })
}

pub(super) fn nfkc_string() -> StdlibFunction {
    StdlibFunction::new("nfkc", vec![IrType::String], IrType::String, || {
        normalize_body_ops(NormForm::Nfkc)
    })
}

/// Shared body builder for `nfd` / `nfkd` / `nfc` / `nfkc`.
///
/// ## v3++ b-5 scope
///
/// The body implements the full UAX #15 pipeline against the embedded
/// UCD 14.0.0 tables: decompose -> canonical reorder -> (optional)
/// canonical composition -> re-encode. Hangul syllables are decomposed
/// and composed algorithmically per UAX #15 section 16 so the table
/// data section does not have to embed the ~11K-entry syllable block.
///
/// ### Layout
///
/// Two scratch buffers are bump-allocated up front:
///
///   * `cp_buf`: `[len: u32 LE][u32 entries x len]` - the working
///     codepoint buffer, sized worst-case `4 + s.len() * 18 * 4` bytes
///     (UCD 14's longest decomposition is U+FDFA at 18 cps).
///   * `out_buf`: `[len: u32 LE][u8 utf8 x len]` - the encoded result,
///     sized worst-case `4 + s.len() * 4` bytes (a 1-byte ASCII cp
///     mapping to a 4-byte UTF-8 sequence). The body returns this
///     buffer's address.
///
/// ### Phases
///
///   1. Decompose: walk the input byte-by-byte, decode each UTF-8 cp,
///      and append the cp (or its decomposition / Hangul jamo
///      sequence) to `cp_buf`.
///   2. Canonical reorder: walk `cp_buf` and run an in-place insertion
///      sort on each maximal run of non-starters, ordered by CCC.
///   3. Composition (NFC / NFKC only): scan `cp_buf` left-to-right
///      maintaining a "last starter index" and "last CCC" pair; for
///      each cp try Hangul composition then table composition with
///      the live starter, honouring the UAX #15 blocking rule, and
///      collapse the pair into the starter slot when a match is found.
///   4. Encode: re-emit each `cp_buf` entry as UTF-8 into `out_buf`.
///
/// ### Locals
///
///   * 0  `s_len:   I32` - input byte count.
///   * 1  `cp_base: I32` - cp_buf pointer.
///   * 2  `out_base: I32` - out_buf pointer.
///   * 3  `cp_len:  I32` - cp_buf logical length.
///   * 4  `i:       I32` - input read cursor.
///   * 5  `j:       I32` - cp_buf write cursor (in bytes).
///   * 6  `cp:      I32` - decoded codepoint.
///   * 7  `cp_bytes: I32` - byte count of the decoded codepoint.
///   * 8  `b0:      I32` - leading byte buffer.
///   * 9  `b_tmp:   I32` - continuation byte buffer.
///   * 10 `sink:    I32` - drop slot for If-arm i32 placeholders.
///   * 11 `tmp:     I32` - general-purpose loop scratch.
///   * 12 `lookup:  I32` - packed decomp lookup result.
///   * 13 `tmp2:    I32` - second general-purpose loop scratch.
///   * 14 `out_j:   I32` - out_buf write cursor (in bytes).
///   * 15 `folded:  I32` - cp slot being encoded.
///   * 16 `k:       I32` - reorder pass cursor.
///   * 17 `run_end: I32` - end of current non-starter run during reorder.
#[allow(clippy::vec_init_then_push, clippy::too_many_lines)]
fn normalize_body_ops(form: NormForm) -> Vec<TaggedOp> {
    const S_LEN: u32 = 0;
    const CP_BASE: u32 = 1;
    const OUT_BASE: u32 = 2;
    const CP_LEN: u32 = 3;
    const I: u32 = 4;
    const J: u32 = 5;
    const CP: u32 = 6;
    const CP_BYTES: u32 = 7;
    const B0: u32 = 8;
    const B_TMP: u32 = 9;
    const SINK: u32 = 10;
    const TMP: u32 = 11;
    const LOOKUP: u32 = 12;
    const TMP2: u32 = 13;
    const OUT_J: u32 = 14;
    const FOLDED: u32 = 15;
    const K: u32 = 16;
    const RUN_END: u32 = 17;
    // Dedicated slot for `append_cp_from_stack` so the helper doesn't
    // stomp on TMP / TMP2 between Hangul-decompose phases.
    const APPEND_TMP: u32 = 18;

    let decomp_idx = DECOMP_LOOKUP_INDEX;
    let ccc_idx = CCC_LOOKUP_INDEX;
    let compose_idx = COMPOSE_LOOKUP_INDEX;
    let compat = form.use_compatibility();

    // Helper: load the `i + off`-th input byte. Pushes one i32.
    let load_input_byte = |off: i32| -> Vec<TaggedOp> {
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

    let trap_invalid_utf8 = || -> Vec<TaggedOp> {
        vec![
            tt(Op::Trap {
                kind: TrapKind::InvalidUtf8,
            }),
            tt(Op::ConstI32(0)),
        ]
    };

    let load_continuation = |n: i32| -> Vec<TaggedOp> {
        let mut out = load_input_byte(n);
        out.push(tt(Op::ConstI32(0x3F)));
        out.push(tt(Op::BitAnd(IrType::I32)));
        out.push(tt(Op::LetSet {
            idx: B_TMP,
            ty: IrType::I32,
        }));
        out
    };

    // Helper: append `cp` (i32 on stack top) as a u32 LE at
    // `cp_base + 4 + j`; advance j by 4. Uses APPEND_TMP so the helper
    // never stomps on TMP / TMP2 / other state slots.
    let append_cp_from_stack = || -> Vec<TaggedOp> {
        let mut v = Vec::new();
        v.push(tt(Op::LetSet {
            idx: APPEND_TMP,
            ty: IrType::I32,
        }));
        v.push(tt(Op::LetGet {
            idx: CP_BASE,
            ty: IrType::I32,
        }));
        v.push(tt(Op::ConstI32(4)));
        v.push(tt(Op::Add(IrType::I32)));
        v.push(tt(Op::LetGet {
            idx: J,
            ty: IrType::I32,
        }));
        v.push(tt(Op::Add(IrType::I32)));
        v.push(tt(Op::LetGet {
            idx: APPEND_TMP,
            ty: IrType::I32,
        }));
        v.push(tt(Op::StoreI32AtAbsolute { offset: 0 }));
        v.push(tt(Op::LetGet {
            idx: J,
            ty: IrType::I32,
        }));
        v.push(tt(Op::ConstI32(4)));
        v.push(tt(Op::Add(IrType::I32)));
        v.push(tt(Op::LetSet {
            idx: J,
            ty: IrType::I32,
        }));
        v
    };

    // ----- UTF-8 decode (CP, CP_BYTES) -----
    let mut decode_seq: Vec<TaggedOp> = Vec::new();
    decode_seq.extend(load_input_byte(0));
    decode_seq.push(tt(Op::LetSet {
        idx: B0,
        ty: IrType::I32,
    }));
    decode_seq.push(tt(Op::LetGet {
        idx: B0,
        ty: IrType::I32,
    }));
    decode_seq.push(tt(Op::ConstI32(0x80)));
    decode_seq.push(tt(Op::Lt(IrType::I32)));
    decode_seq.push(tt(Op::If {
        result_ty: IrType::I32,
        then_body: {
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
            let mut v = Vec::new();
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
                                    t.push(tt(Op::LetGet {
                                        idx: B0,
                                        ty: IrType::I32,
                                    }));
                                    t.push(tt(Op::ConstI32(0x0F)));
                                    t.push(tt(Op::BitAnd(IrType::I32)));
                                    t.push(tt(Op::ConstI32(4096)));
                                    t.push(tt(Op::Mul(IrType::I32)));
                                    t.extend(load_continuation(1));
                                    t.push(tt(Op::LetGet {
                                        idx: B_TMP,
                                        ty: IrType::I32,
                                    }));
                                    t.push(tt(Op::ConstI32(64)));
                                    t.push(tt(Op::Mul(IrType::I32)));
                                    t.push(tt(Op::Add(IrType::I32)));
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
                                    e2.push(tt(Op::LetGet {
                                        idx: B0,
                                        ty: IrType::I32,
                                    }));
                                    e2.push(tt(Op::ConstI32(0x07)));
                                    e2.push(tt(Op::BitAnd(IrType::I32)));
                                    e2.push(tt(Op::ConstI32(262144)));
                                    e2.push(tt(Op::Mul(IrType::I32)));
                                    e2.extend(load_continuation(1));
                                    e2.push(tt(Op::LetGet {
                                        idx: B_TMP,
                                        ty: IrType::I32,
                                    }));
                                    e2.push(tt(Op::ConstI32(4096)));
                                    e2.push(tt(Op::Mul(IrType::I32)));
                                    e2.push(tt(Op::Add(IrType::I32)));
                                    e2.extend(load_continuation(2));
                                    e2.push(tt(Op::LetGet {
                                        idx: B_TMP,
                                        ty: IrType::I32,
                                    }));
                                    e2.push(tt(Op::ConstI32(64)));
                                    e2.push(tt(Op::Mul(IrType::I32)));
                                    e2.push(tt(Op::Add(IrType::I32)));
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

    // ----- decompose dispatch -----
    // Compute decomposition into cp_buf for the current cp:
    //   1. Try Hangul algorithmic decomposition.
    //   2. Otherwise call __decomp_lookup; on hit write the pool slice;
    //      on miss write cp as-is.
    let mut decompose_seq: Vec<TaggedOp> = Vec::new();
    // Hangul fast path: s_index = cp - S_BASE; if 0 <= s_index <
    // S_COUNT, write L / V / (T) and skip table lookup.
    decompose_seq.push(tt(Op::LetGet {
        idx: CP,
        ty: IrType::I32,
    }));
    decompose_seq.push(tt(Op::ConstI32(crate::normalization::HANGUL_S_BASE as i32)));
    decompose_seq.push(tt(Op::Ge(IrType::I32)));
    decompose_seq.push(tt(Op::If {
        result_ty: IrType::I32,
        then_body: {
            // cp - S_BASE
            let mut t = Vec::new();
            t.push(tt(Op::LetGet {
                idx: CP,
                ty: IrType::I32,
            }));
            t.push(tt(Op::ConstI32(crate::normalization::HANGUL_S_BASE as i32)));
            t.push(tt(Op::Sub(IrType::I32)));
            t.push(tt(Op::LetSet {
                idx: TMP,
                ty: IrType::I32,
            }));
            // if tmp < S_COUNT { hangul decompose } else { table }
            t.push(tt(Op::LetGet {
                idx: TMP,
                ty: IrType::I32,
            }));
            t.push(tt(Op::ConstI32(
                crate::normalization::HANGUL_S_COUNT as i32,
            )));
            t.push(tt(Op::Lt(IrType::I32)));
            t.push(tt(Op::If {
                result_ty: IrType::I32,
                then_body: {
                    let mut h = Vec::new();
                    // l = L_BASE + tmp / N_COUNT
                    h.push(tt(Op::ConstI32(crate::normalization::HANGUL_L_BASE as i32)));
                    h.push(tt(Op::LetGet {
                        idx: TMP,
                        ty: IrType::I32,
                    }));
                    h.push(tt(Op::ConstI32(
                        crate::normalization::HANGUL_N_COUNT as i32,
                    )));
                    h.push(tt(Op::Div(IrType::I32)));
                    h.push(tt(Op::Add(IrType::I32)));
                    h.extend(append_cp_from_stack());
                    // v = V_BASE + (tmp % N_COUNT) / T_COUNT
                    h.push(tt(Op::ConstI32(crate::normalization::HANGUL_V_BASE as i32)));
                    h.push(tt(Op::LetGet {
                        idx: TMP,
                        ty: IrType::I32,
                    }));
                    h.push(tt(Op::ConstI32(
                        crate::normalization::HANGUL_N_COUNT as i32,
                    )));
                    h.push(tt(Op::Mod(IrType::I32)));
                    h.push(tt(Op::ConstI32(
                        crate::normalization::HANGUL_T_COUNT as i32,
                    )));
                    h.push(tt(Op::Div(IrType::I32)));
                    h.push(tt(Op::Add(IrType::I32)));
                    h.extend(append_cp_from_stack());
                    // t_offset = tmp % T_COUNT; if != 0 write T_BASE + t_offset
                    h.push(tt(Op::LetGet {
                        idx: TMP,
                        ty: IrType::I32,
                    }));
                    h.push(tt(Op::ConstI32(
                        crate::normalization::HANGUL_T_COUNT as i32,
                    )));
                    h.push(tt(Op::Mod(IrType::I32)));
                    h.push(tt(Op::LetSet {
                        idx: TMP2,
                        ty: IrType::I32,
                    }));
                    h.push(tt(Op::LetGet {
                        idx: TMP2,
                        ty: IrType::I32,
                    }));
                    h.push(tt(Op::ConstI32(0)));
                    h.push(tt(Op::Ne(IrType::I32)));
                    h.push(tt(Op::If {
                        result_ty: IrType::I32,
                        then_body: {
                            let mut tb = Vec::new();
                            tb.push(tt(Op::ConstI32(crate::normalization::HANGUL_T_BASE as i32)));
                            tb.push(tt(Op::LetGet {
                                idx: TMP2,
                                ty: IrType::I32,
                            }));
                            tb.push(tt(Op::Add(IrType::I32)));
                            tb.extend(append_cp_from_stack());
                            tb.push(tt(Op::ConstI32(0)));
                            tb
                        },
                        else_body: vec![tt(Op::ConstI32(0))],
                    }));
                    h.push(tt(Op::LetSet {
                        idx: SINK,
                        ty: IrType::I32,
                    }));
                    // 1 = "took hangul path"
                    h.push(tt(Op::ConstI32(1)));
                    h
                },
                else_body: vec![tt(Op::ConstI32(0))],
            }));
            t
        },
        else_body: vec![tt(Op::ConstI32(0))],
    }));
    // top of stack = 1 if Hangul handled, else 0
    decompose_seq.push(tt(Op::LetSet {
        idx: TMP,
        ty: IrType::I32,
    }));
    // if !hangul_handled: try table lookup, else cp passthrough
    decompose_seq.push(tt(Op::LetGet {
        idx: TMP,
        ty: IrType::I32,
    }));
    decompose_seq.push(tt(Op::ConstI32(0)));
    decompose_seq.push(tt(Op::Eq(IrType::I32)));
    decompose_seq.push(tt(Op::If {
        result_ty: IrType::I32,
        then_body: {
            let mut v = Vec::new();
            // lookup = __decomp_lookup(cp, table_addr)
            v.push(tt(Op::LetGet {
                idx: CP,
                ty: IrType::I32,
            }));
            v.push(tt(Op::DecompTableAddr {
                compatibility: compat,
            }));
            v.push(tt(Op::Call {
                fn_index: decomp_idx,
                arg_count: 2,
                param_tys: vec![IrType::I32, IrType::I32],
                ret_ty: IrType::I32,
            }));
            v.push(tt(Op::LetSet {
                idx: LOOKUP,
                ty: IrType::I32,
            }));
            // if lookup == 0: append cp
            v.push(tt(Op::LetGet {
                idx: LOOKUP,
                ty: IrType::I32,
            }));
            v.push(tt(Op::ConstI32(0)));
            v.push(tt(Op::Eq(IrType::I32)));
            v.push(tt(Op::If {
                result_ty: IrType::I32,
                then_body: {
                    let mut t = Vec::new();
                    t.push(tt(Op::LetGet {
                        idx: CP,
                        ty: IrType::I32,
                    }));
                    t.extend(append_cp_from_stack());
                    t.push(tt(Op::ConstI32(0)));
                    t
                },
                else_body: {
                    // off = lookup / 256; len = lookup % 256
                    let mut t = Vec::new();
                    t.push(tt(Op::LetGet {
                        idx: LOOKUP,
                        ty: IrType::I32,
                    }));
                    t.push(tt(Op::ConstI32(256)));
                    t.push(tt(Op::Div(IrType::I32)));
                    t.push(tt(Op::LetSet {
                        idx: TMP,
                        ty: IrType::I32,
                    })); // off
                    t.push(tt(Op::LetGet {
                        idx: LOOKUP,
                        ty: IrType::I32,
                    }));
                    t.push(tt(Op::ConstI32(0xFF)));
                    t.push(tt(Op::BitAnd(IrType::I32)));
                    t.push(tt(Op::LetSet {
                        idx: TMP2,
                        ty: IrType::I32,
                    })); // len
                         // pool_base = table_addr + 4 + index_count * 12 + 4
                         // We compute pool_base on the fly:
                         //   pool_header = table_addr + 4 + index_count * 12
                         //   pool_base   = pool_header + 4
                         // index_count = load(table_addr, 0)
                    t.push(tt(Op::DecompTableAddr {
                        compatibility: compat,
                    }));
                    t.push(tt(Op::LoadI32AtAbsolute { offset: 0 }));
                    t.push(tt(Op::ConstI32(12)));
                    t.push(tt(Op::Mul(IrType::I32)));
                    t.push(tt(Op::ConstI32(8))); // +4 header +4 pool count
                    t.push(tt(Op::Add(IrType::I32)));
                    t.push(tt(Op::DecompTableAddr {
                        compatibility: compat,
                    }));
                    t.push(tt(Op::Add(IrType::I32)));
                    // pool_base now on stack. Stash.
                    t.push(tt(Op::LetSet {
                        idx: LOOKUP,
                        ty: IrType::I32,
                    })); // repurpose: pool_base
                         // for (k=0; k<len; k++) append load_i32(pool_base + (off+k)*4)
                    t.push(tt(Op::ConstI32(0)));
                    t.push(tt(Op::LetSet {
                        idx: K,
                        ty: IrType::I32,
                    }));
                    t.push(tt(Op::Block {
                        result_ty: None,
                        body: vec![tt(Op::Loop {
                            result_ty: None,
                            body: {
                                let mut lb = Vec::new();
                                lb.push(tt(Op::LetGet {
                                    idx: K,
                                    ty: IrType::I32,
                                }));
                                lb.push(tt(Op::LetGet {
                                    idx: TMP2,
                                    ty: IrType::I32,
                                }));
                                lb.push(tt(Op::Ge(IrType::I32)));
                                lb.push(tt(Op::BrIf { label_depth: 1 }));
                                // addr = pool_base + (off + k) * 4
                                lb.push(tt(Op::LetGet {
                                    idx: LOOKUP,
                                    ty: IrType::I32,
                                }));
                                lb.push(tt(Op::LetGet {
                                    idx: TMP,
                                    ty: IrType::I32,
                                }));
                                lb.push(tt(Op::LetGet {
                                    idx: K,
                                    ty: IrType::I32,
                                }));
                                lb.push(tt(Op::Add(IrType::I32)));
                                lb.push(tt(Op::ConstI32(4)));
                                lb.push(tt(Op::Mul(IrType::I32)));
                                lb.push(tt(Op::Add(IrType::I32)));
                                lb.push(tt(Op::LoadI32AtAbsolute { offset: 0 }));
                                lb.extend(append_cp_from_stack());
                                lb.push(tt(Op::LetGet {
                                    idx: K,
                                    ty: IrType::I32,
                                }));
                                lb.push(tt(Op::ConstI32(1)));
                                lb.push(tt(Op::Add(IrType::I32)));
                                lb.push(tt(Op::LetSet {
                                    idx: K,
                                    ty: IrType::I32,
                                }));
                                lb.push(tt(Op::Br { label_depth: 0 }));
                                lb
                            },
                        })],
                    }));
                    t.push(tt(Op::ConstI32(0)));
                    t
                },
            }));
            v.push(tt(Op::LetSet {
                idx: SINK,
                ty: IrType::I32,
            }));
            v.push(tt(Op::ConstI32(0)));
            v
        },
        else_body: vec![tt(Op::ConstI32(0))],
    }));
    decompose_seq.push(tt(Op::LetSet {
        idx: SINK,
        ty: IrType::I32,
    }));

    // ----- canonical reorder (insertion sort within non-starter runs) -----
    // cp_buf has cp_len u32 entries starting at cp_base + 4. Walk via
    // k. Each non-starter run [start, end) is bubble-sorted by CCC.
    let reorder_seq: Vec<TaggedOp> = {
        let mut v: Vec<TaggedOp> = Vec::new();
        // Compute cp_len = j / 4 (j is byte cursor, 4 bytes per entry)
        v.push(tt(Op::LetGet {
            idx: J,
            ty: IrType::I32,
        }));
        v.push(tt(Op::ConstI32(4)));
        v.push(tt(Op::Div(IrType::I32)));
        v.push(tt(Op::LetSet {
            idx: CP_LEN,
            ty: IrType::I32,
        }));
        // k = 0
        v.push(tt(Op::ConstI32(0)));
        v.push(tt(Op::LetSet {
            idx: K,
            ty: IrType::I32,
        }));
        // Outer block + loop: walk k from 0..cp_len
        v.push(tt(Op::Block {
            result_ty: None,
            body: vec![tt(Op::Loop {
                result_ty: None,
                body: {
                    let mut lb: Vec<TaggedOp> = Vec::new();
                    // exit if k >= cp_len
                    lb.push(tt(Op::LetGet {
                        idx: K,
                        ty: IrType::I32,
                    }));
                    lb.push(tt(Op::LetGet {
                        idx: CP_LEN,
                        ty: IrType::I32,
                    }));
                    lb.push(tt(Op::Ge(IrType::I32)));
                    lb.push(tt(Op::BrIf { label_depth: 1 }));
                    // entry_addr = cp_base + 4 + k*4
                    // tmp_cp = load(entry_addr)
                    lb.push(tt(Op::LetGet {
                        idx: CP_BASE,
                        ty: IrType::I32,
                    }));
                    lb.push(tt(Op::ConstI32(4)));
                    lb.push(tt(Op::Add(IrType::I32)));
                    lb.push(tt(Op::LetGet {
                        idx: K,
                        ty: IrType::I32,
                    }));
                    lb.push(tt(Op::ConstI32(4)));
                    lb.push(tt(Op::Mul(IrType::I32)));
                    lb.push(tt(Op::Add(IrType::I32)));
                    lb.push(tt(Op::LoadI32AtAbsolute { offset: 0 }));
                    lb.push(tt(Op::LetSet {
                        idx: CP,
                        ty: IrType::I32,
                    }));
                    // ccc = __ccc_lookup(cp, ccc_table)
                    lb.push(tt(Op::LetGet {
                        idx: CP,
                        ty: IrType::I32,
                    }));
                    lb.push(tt(Op::CccTableAddr));
                    lb.push(tt(Op::Call {
                        fn_index: ccc_idx,
                        arg_count: 2,
                        param_tys: vec![IrType::I32, IrType::I32],
                        ret_ty: IrType::I32,
                    }));
                    lb.push(tt(Op::LetSet {
                        idx: TMP,
                        ty: IrType::I32,
                    }));
                    // if ccc == 0: k++; continue
                    lb.push(tt(Op::LetGet {
                        idx: TMP,
                        ty: IrType::I32,
                    }));
                    lb.push(tt(Op::ConstI32(0)));
                    lb.push(tt(Op::Eq(IrType::I32)));
                    lb.push(tt(Op::If {
                        result_ty: IrType::I32,
                        then_body: {
                            let mut t = Vec::new();
                            t.push(tt(Op::LetGet {
                                idx: K,
                                ty: IrType::I32,
                            }));
                            t.push(tt(Op::ConstI32(1)));
                            t.push(tt(Op::Add(IrType::I32)));
                            t.push(tt(Op::LetSet {
                                idx: K,
                                ty: IrType::I32,
                            }));
                            // depth 1 = the enclosing reorder Loop -
                            // jumping there continues the outer walk.
                            t.push(tt(Op::Br { label_depth: 1 }));
                            t.push(tt(Op::ConstI32(0)));
                            t
                        },
                        else_body: vec![tt(Op::ConstI32(0))],
                    }));
                    lb.push(tt(Op::LetSet {
                        idx: SINK,
                        ty: IrType::I32,
                    }));
                    // Find run end: run_end = k; while run_end < cp_len
                    // and ccc(buf[run_end]) != 0: run_end++
                    lb.push(tt(Op::LetGet {
                        idx: K,
                        ty: IrType::I32,
                    }));
                    lb.push(tt(Op::LetSet {
                        idx: RUN_END,
                        ty: IrType::I32,
                    }));
                    lb.push(tt(Op::Block {
                        result_ty: None,
                        body: vec![tt(Op::Loop {
                            result_ty: None,
                            body: {
                                let mut rl: Vec<TaggedOp> = Vec::new();
                                rl.push(tt(Op::LetGet {
                                    idx: RUN_END,
                                    ty: IrType::I32,
                                }));
                                rl.push(tt(Op::LetGet {
                                    idx: CP_LEN,
                                    ty: IrType::I32,
                                }));
                                rl.push(tt(Op::Ge(IrType::I32)));
                                rl.push(tt(Op::BrIf { label_depth: 1 }));
                                // load buf[run_end]
                                rl.push(tt(Op::LetGet {
                                    idx: CP_BASE,
                                    ty: IrType::I32,
                                }));
                                rl.push(tt(Op::ConstI32(4)));
                                rl.push(tt(Op::Add(IrType::I32)));
                                rl.push(tt(Op::LetGet {
                                    idx: RUN_END,
                                    ty: IrType::I32,
                                }));
                                rl.push(tt(Op::ConstI32(4)));
                                rl.push(tt(Op::Mul(IrType::I32)));
                                rl.push(tt(Op::Add(IrType::I32)));
                                rl.push(tt(Op::LoadI32AtAbsolute { offset: 0 }));
                                rl.push(tt(Op::CccTableAddr));
                                rl.push(tt(Op::Call {
                                    fn_index: ccc_idx,
                                    arg_count: 2,
                                    param_tys: vec![IrType::I32, IrType::I32],
                                    ret_ty: IrType::I32,
                                }));
                                rl.push(tt(Op::ConstI32(0)));
                                rl.push(tt(Op::Eq(IrType::I32)));
                                rl.push(tt(Op::BrIf { label_depth: 1 }));
                                rl.push(tt(Op::LetGet {
                                    idx: RUN_END,
                                    ty: IrType::I32,
                                }));
                                rl.push(tt(Op::ConstI32(1)));
                                rl.push(tt(Op::Add(IrType::I32)));
                                rl.push(tt(Op::LetSet {
                                    idx: RUN_END,
                                    ty: IrType::I32,
                                }));
                                rl.push(tt(Op::Br { label_depth: 0 }));
                                rl
                            },
                        })],
                    }));
                    // Bubble sort the run [k, run_end) by CCC, stably.
                    // Outer i = k+1; while i < run_end:
                    //   j_inner = i; while j_inner > k and ccc(buf[j_inner-1]) > ccc(buf[j_inner]): swap; j_inner--
                    // We reuse I (clobbered by reorder pass, decode is done)
                    // and J (clobbered similarly). Save K so the outer loop
                    // continues. We'll use I for inner i, B0 for j_inner.
                    lb.push(tt(Op::LetGet {
                        idx: K,
                        ty: IrType::I32,
                    }));
                    lb.push(tt(Op::ConstI32(1)));
                    lb.push(tt(Op::Add(IrType::I32)));
                    lb.push(tt(Op::LetSet {
                        idx: I,
                        ty: IrType::I32,
                    }));
                    lb.push(tt(Op::Block {
                        result_ty: None,
                        body: vec![tt(Op::Loop {
                            result_ty: None,
                            body: {
                                let mut ol: Vec<TaggedOp> = Vec::new();
                                ol.push(tt(Op::LetGet {
                                    idx: I,
                                    ty: IrType::I32,
                                }));
                                ol.push(tt(Op::LetGet {
                                    idx: RUN_END,
                                    ty: IrType::I32,
                                }));
                                ol.push(tt(Op::Ge(IrType::I32)));
                                ol.push(tt(Op::BrIf { label_depth: 1 }));
                                // j_inner = i
                                ol.push(tt(Op::LetGet {
                                    idx: I,
                                    ty: IrType::I32,
                                }));
                                ol.push(tt(Op::LetSet {
                                    idx: B0,
                                    ty: IrType::I32,
                                }));
                                ol.push(tt(Op::Block {
                                    result_ty: None,
                                    body: vec![tt(Op::Loop {
                                        result_ty: None,
                                        body: {
                                            let mut il: Vec<TaggedOp> = Vec::new();
                                            // exit if j_inner <= k
                                            il.push(tt(Op::LetGet {
                                                idx: B0,
                                                ty: IrType::I32,
                                            }));
                                            il.push(tt(Op::LetGet {
                                                idx: K,
                                                ty: IrType::I32,
                                            }));
                                            il.push(tt(Op::Le(IrType::I32)));
                                            il.push(tt(Op::BrIf { label_depth: 1 }));
                                            // a_addr = cp_base + 4 + (j_inner-1)*4
                                            il.push(tt(Op::LetGet {
                                                idx: CP_BASE,
                                                ty: IrType::I32,
                                            }));
                                            il.push(tt(Op::ConstI32(4)));
                                            il.push(tt(Op::Add(IrType::I32)));
                                            il.push(tt(Op::LetGet {
                                                idx: B0,
                                                ty: IrType::I32,
                                            }));
                                            il.push(tt(Op::ConstI32(1)));
                                            il.push(tt(Op::Sub(IrType::I32)));
                                            il.push(tt(Op::ConstI32(4)));
                                            il.push(tt(Op::Mul(IrType::I32)));
                                            il.push(tt(Op::Add(IrType::I32)));
                                            il.push(tt(Op::LetSet {
                                                idx: B_TMP,
                                                ty: IrType::I32,
                                            }));
                                            // b_addr = cp_base + 4 + j_inner*4
                                            il.push(tt(Op::LetGet {
                                                idx: CP_BASE,
                                                ty: IrType::I32,
                                            }));
                                            il.push(tt(Op::ConstI32(4)));
                                            il.push(tt(Op::Add(IrType::I32)));
                                            il.push(tt(Op::LetGet {
                                                idx: B0,
                                                ty: IrType::I32,
                                            }));
                                            il.push(tt(Op::ConstI32(4)));
                                            il.push(tt(Op::Mul(IrType::I32)));
                                            il.push(tt(Op::Add(IrType::I32)));
                                            il.push(tt(Op::LetSet {
                                                idx: LOOKUP,
                                                ty: IrType::I32,
                                            }));
                                            // a = load(a_addr); b = load(b_addr)
                                            il.push(tt(Op::LetGet {
                                                idx: B_TMP,
                                                ty: IrType::I32,
                                            }));
                                            il.push(tt(Op::LoadI32AtAbsolute { offset: 0 }));
                                            il.push(tt(Op::LetSet {
                                                idx: TMP,
                                                ty: IrType::I32,
                                            }));
                                            il.push(tt(Op::LetGet {
                                                idx: LOOKUP,
                                                ty: IrType::I32,
                                            }));
                                            il.push(tt(Op::LoadI32AtAbsolute { offset: 0 }));
                                            il.push(tt(Op::LetSet {
                                                idx: TMP2,
                                                ty: IrType::I32,
                                            }));
                                            // ccc_a / ccc_b
                                            il.push(tt(Op::LetGet {
                                                idx: TMP,
                                                ty: IrType::I32,
                                            }));
                                            il.push(tt(Op::CccTableAddr));
                                            il.push(tt(Op::Call {
                                                fn_index: ccc_idx,
                                                arg_count: 2,
                                                param_tys: vec![IrType::I32, IrType::I32],
                                                ret_ty: IrType::I32,
                                            }));
                                            il.push(tt(Op::LetGet {
                                                idx: TMP2,
                                                ty: IrType::I32,
                                            }));
                                            il.push(tt(Op::CccTableAddr));
                                            il.push(tt(Op::Call {
                                                fn_index: ccc_idx,
                                                arg_count: 2,
                                                param_tys: vec![IrType::I32, IrType::I32],
                                                ret_ty: IrType::I32,
                                            }));
                                            // if ccc_a > ccc_b: swap
                                            il.push(tt(Op::Gt(IrType::I32)));
                                            il.push(tt(Op::If {
                                                result_ty: IrType::I32,
                                                then_body: {
                                                    let mut sb = Vec::new();
                                                    // store(a_addr, b)
                                                    sb.push(tt(Op::LetGet {
                                                        idx: B_TMP,
                                                        ty: IrType::I32,
                                                    }));
                                                    sb.push(tt(Op::LetGet {
                                                        idx: TMP2,
                                                        ty: IrType::I32,
                                                    }));
                                                    sb.push(tt(Op::StoreI32AtAbsolute {
                                                        offset: 0,
                                                    }));
                                                    // store(b_addr, a)
                                                    sb.push(tt(Op::LetGet {
                                                        idx: LOOKUP,
                                                        ty: IrType::I32,
                                                    }));
                                                    sb.push(tt(Op::LetGet {
                                                        idx: TMP,
                                                        ty: IrType::I32,
                                                    }));
                                                    sb.push(tt(Op::StoreI32AtAbsolute {
                                                        offset: 0,
                                                    }));
                                                    // j_inner--
                                                    sb.push(tt(Op::LetGet {
                                                        idx: B0,
                                                        ty: IrType::I32,
                                                    }));
                                                    sb.push(tt(Op::ConstI32(1)));
                                                    sb.push(tt(Op::Sub(IrType::I32)));
                                                    sb.push(tt(Op::LetSet {
                                                        idx: B0,
                                                        ty: IrType::I32,
                                                    }));
                                                    sb.push(tt(Op::ConstI32(0)));
                                                    sb
                                                },
                                                else_body: vec![
                                                    // break out: br 2 exits the inner block.
                                                    tt(Op::Br { label_depth: 2 }),
                                                    tt(Op::ConstI32(0)),
                                                ],
                                            }));
                                            il.push(tt(Op::LetSet {
                                                idx: SINK,
                                                ty: IrType::I32,
                                            }));
                                            il.push(tt(Op::Br { label_depth: 0 }));
                                            il
                                        },
                                    })],
                                }));
                                ol.push(tt(Op::LetGet {
                                    idx: I,
                                    ty: IrType::I32,
                                }));
                                ol.push(tt(Op::ConstI32(1)));
                                ol.push(tt(Op::Add(IrType::I32)));
                                ol.push(tt(Op::LetSet {
                                    idx: I,
                                    ty: IrType::I32,
                                }));
                                ol.push(tt(Op::Br { label_depth: 0 }));
                                ol
                            },
                        })],
                    }));
                    // k = run_end
                    lb.push(tt(Op::LetGet {
                        idx: RUN_END,
                        ty: IrType::I32,
                    }));
                    lb.push(tt(Op::LetSet {
                        idx: K,
                        ty: IrType::I32,
                    }));
                    lb.push(tt(Op::Br { label_depth: 0 }));
                    lb
                },
            })],
        }));
        v
    };

    // ----- canonical composition pass (NFC/NFKC only) -----
    // In-place over cp_buf. last_starter: index in buf (or -1).
    // last_ccc: int. write_idx: cp_buf write position (0..cp_len).
    // For each read_idx in 0..cp_len:
    //   cur = buf[read_idx]; cur_ccc = ccc(cur)
    //   if last_starter != -1:
    //     starter = buf[last_starter]
    //     composed = hangul_compose(starter, cur) or compose_lookup(starter, cur)
    //     if composed != -1 and !(cur_ccc != 0 and last_ccc >= cur_ccc):
    //       buf[last_starter] = composed; continue (do NOT emit cur)
    //   buf[write_idx++] = cur
    //   if cur_ccc == 0: last_starter = write_idx-1; last_ccc = 0
    //   else: last_ccc = cur_ccc
    // After: cp_len = write_idx
    let compose_seq: Vec<TaggedOp> = if form.composes() {
        // Locals reused for compose state:
        //   I        - read_idx
        //   J        - write_idx
        //   B0       - last_starter (-1 sentinel)
        //   B_TMP    - last_ccc
        //   TMP      - cur cp
        //   TMP2     - cur ccc
        //   LOOKUP   - starter cp / composed cp
        //   FOLDED   - hangul/compose result
        let mut v: Vec<TaggedOp> = Vec::new();
        v.push(tt(Op::ConstI32(0)));
        v.push(tt(Op::LetSet {
            idx: I,
            ty: IrType::I32,
        }));
        v.push(tt(Op::ConstI32(0)));
        v.push(tt(Op::LetSet {
            idx: J,
            ty: IrType::I32,
        }));
        v.push(tt(Op::ConstI32(-1)));
        v.push(tt(Op::LetSet {
            idx: B0,
            ty: IrType::I32,
        }));
        v.push(tt(Op::ConstI32(0)));
        v.push(tt(Op::LetSet {
            idx: B_TMP,
            ty: IrType::I32,
        }));
        v.push(tt(Op::Block {
            result_ty: None,
            body: vec![tt(Op::Loop {
                result_ty: None,
                body: {
                    let mut lb: Vec<TaggedOp> = Vec::new();
                    lb.push(tt(Op::LetGet {
                        idx: I,
                        ty: IrType::I32,
                    }));
                    lb.push(tt(Op::LetGet {
                        idx: CP_LEN,
                        ty: IrType::I32,
                    }));
                    lb.push(tt(Op::Ge(IrType::I32)));
                    lb.push(tt(Op::BrIf { label_depth: 1 }));
                    // cur = buf[i]
                    lb.push(tt(Op::LetGet {
                        idx: CP_BASE,
                        ty: IrType::I32,
                    }));
                    lb.push(tt(Op::ConstI32(4)));
                    lb.push(tt(Op::Add(IrType::I32)));
                    lb.push(tt(Op::LetGet {
                        idx: I,
                        ty: IrType::I32,
                    }));
                    lb.push(tt(Op::ConstI32(4)));
                    lb.push(tt(Op::Mul(IrType::I32)));
                    lb.push(tt(Op::Add(IrType::I32)));
                    lb.push(tt(Op::LoadI32AtAbsolute { offset: 0 }));
                    lb.push(tt(Op::LetSet {
                        idx: TMP,
                        ty: IrType::I32,
                    }));
                    // cur_ccc
                    lb.push(tt(Op::LetGet {
                        idx: TMP,
                        ty: IrType::I32,
                    }));
                    lb.push(tt(Op::CccTableAddr));
                    lb.push(tt(Op::Call {
                        fn_index: ccc_idx,
                        arg_count: 2,
                        param_tys: vec![IrType::I32, IrType::I32],
                        ret_ty: IrType::I32,
                    }));
                    lb.push(tt(Op::LetSet {
                        idx: TMP2,
                        ty: IrType::I32,
                    }));
                    // if last_starter != -1: try compose
                    lb.push(tt(Op::LetGet {
                        idx: B0,
                        ty: IrType::I32,
                    }));
                    lb.push(tt(Op::ConstI32(-1)));
                    lb.push(tt(Op::Ne(IrType::I32)));
                    lb.push(tt(Op::If {
                        result_ty: IrType::I32,
                        then_body: {
                            let mut t: Vec<TaggedOp> = Vec::new();
                            // starter = buf[last_starter]
                            t.push(tt(Op::LetGet {
                                idx: CP_BASE,
                                ty: IrType::I32,
                            }));
                            t.push(tt(Op::ConstI32(4)));
                            t.push(tt(Op::Add(IrType::I32)));
                            t.push(tt(Op::LetGet {
                                idx: B0,
                                ty: IrType::I32,
                            }));
                            t.push(tt(Op::ConstI32(4)));
                            t.push(tt(Op::Mul(IrType::I32)));
                            t.push(tt(Op::Add(IrType::I32)));
                            t.push(tt(Op::LoadI32AtAbsolute { offset: 0 }));
                            t.push(tt(Op::LetSet {
                                idx: LOOKUP,
                                ty: IrType::I32,
                            }));
                            // hangul_compose: try L+V or LV+T inline.
                            // Initialise FOLDED = -1 (no composition yet).
                            t.push(tt(Op::ConstI32(-1)));
                            t.push(tt(Op::LetSet {
                                idx: FOLDED,
                                ty: IrType::I32,
                            }));
                            // L+V case: if L_BASE <= starter < L_BASE+L_COUNT
                            //           and V_BASE <= cur < V_BASE+V_COUNT:
                            //   FOLDED = S_BASE + (starter - L_BASE) * V_COUNT * T_COUNT + (cur - V_BASE) * T_COUNT
                            let l_base = crate::normalization::HANGUL_L_BASE as i32;
                            let l_count = crate::normalization::HANGUL_L_COUNT as i32;
                            let v_base = crate::normalization::HANGUL_V_BASE as i32;
                            let v_count = crate::normalization::HANGUL_V_COUNT as i32;
                            let t_base = crate::normalization::HANGUL_T_BASE as i32;
                            let t_count = crate::normalization::HANGUL_T_COUNT as i32;
                            let s_base = crate::normalization::HANGUL_S_BASE as i32;
                            let s_count = crate::normalization::HANGUL_S_COUNT as i32;
                            // is_L = starter >= L_BASE && starter < L_BASE+L_COUNT
                            t.push(tt(Op::LetGet {
                                idx: LOOKUP,
                                ty: IrType::I32,
                            }));
                            t.push(tt(Op::ConstI32(l_base)));
                            t.push(tt(Op::Ge(IrType::I32)));
                            t.push(tt(Op::If {
                                result_ty: IrType::I32,
                                then_body: vec![
                                    tt(Op::LetGet {
                                        idx: LOOKUP,
                                        ty: IrType::I32,
                                    }),
                                    tt(Op::ConstI32(l_base + l_count)),
                                    tt(Op::Lt(IrType::I32)),
                                ],
                                else_body: vec![tt(Op::ConstI32(0))],
                            }));
                            t.push(tt(Op::If {
                                result_ty: IrType::I32,
                                then_body: {
                                    let mut lv = Vec::new();
                                    lv.push(tt(Op::LetGet {
                                        idx: TMP,
                                        ty: IrType::I32,
                                    }));
                                    lv.push(tt(Op::ConstI32(v_base)));
                                    lv.push(tt(Op::Ge(IrType::I32)));
                                    lv.push(tt(Op::If {
                                        result_ty: IrType::I32,
                                        then_body: vec![
                                            tt(Op::LetGet {
                                                idx: TMP,
                                                ty: IrType::I32,
                                            }),
                                            tt(Op::ConstI32(v_base + v_count)),
                                            tt(Op::Lt(IrType::I32)),
                                        ],
                                        else_body: vec![tt(Op::ConstI32(0))],
                                    }));
                                    lv.push(tt(Op::If {
                                        result_ty: IrType::I32,
                                        then_body: {
                                            let mut comp = Vec::new();
                                            // FOLDED = S_BASE + (starter - L_BASE) * (V_COUNT*T_COUNT) + (cur - V_BASE) * T_COUNT
                                            comp.push(tt(Op::ConstI32(s_base)));
                                            comp.push(tt(Op::LetGet {
                                                idx: LOOKUP,
                                                ty: IrType::I32,
                                            }));
                                            comp.push(tt(Op::ConstI32(l_base)));
                                            comp.push(tt(Op::Sub(IrType::I32)));
                                            comp.push(tt(Op::ConstI32(v_count * t_count)));
                                            comp.push(tt(Op::Mul(IrType::I32)));
                                            comp.push(tt(Op::Add(IrType::I32)));
                                            comp.push(tt(Op::LetGet {
                                                idx: TMP,
                                                ty: IrType::I32,
                                            }));
                                            comp.push(tt(Op::ConstI32(v_base)));
                                            comp.push(tt(Op::Sub(IrType::I32)));
                                            comp.push(tt(Op::ConstI32(t_count)));
                                            comp.push(tt(Op::Mul(IrType::I32)));
                                            comp.push(tt(Op::Add(IrType::I32)));
                                            comp.push(tt(Op::LetSet {
                                                idx: FOLDED,
                                                ty: IrType::I32,
                                            }));
                                            comp.push(tt(Op::ConstI32(0)));
                                            comp
                                        },
                                        else_body: vec![tt(Op::ConstI32(0))],
                                    }));
                                    lv.push(tt(Op::LetSet {
                                        idx: SINK,
                                        ty: IrType::I32,
                                    }));
                                    lv.push(tt(Op::ConstI32(0)));
                                    lv
                                },
                                else_body: vec![tt(Op::ConstI32(0))],
                            }));
                            t.push(tt(Op::LetSet {
                                idx: SINK,
                                ty: IrType::I32,
                            }));
                            // LV+T: if FOLDED still -1 and starter is LV-shaped + cur is T-jamo > T_BASE
                            t.push(tt(Op::LetGet {
                                idx: FOLDED,
                                ty: IrType::I32,
                            }));
                            t.push(tt(Op::ConstI32(-1)));
                            t.push(tt(Op::Eq(IrType::I32)));
                            t.push(tt(Op::If {
                                result_ty: IrType::I32,
                                then_body: {
                                    let mut lv = Vec::new();
                                    // s_index = starter - S_BASE
                                    // if 0 <= s_index < S_COUNT and s_index % T_COUNT == 0
                                    // and T_BASE+1 <= cur < T_BASE+T_COUNT:
                                    //   FOLDED = starter + (cur - T_BASE)
                                    lv.push(tt(Op::LetGet {
                                        idx: LOOKUP,
                                        ty: IrType::I32,
                                    }));
                                    lv.push(tt(Op::ConstI32(s_base)));
                                    lv.push(tt(Op::Ge(IrType::I32)));
                                    lv.push(tt(Op::If {
                                        result_ty: IrType::I32,
                                        then_body: vec![
                                            tt(Op::LetGet {
                                                idx: LOOKUP,
                                                ty: IrType::I32,
                                            }),
                                            tt(Op::ConstI32(s_base + s_count)),
                                            tt(Op::Lt(IrType::I32)),
                                        ],
                                        else_body: vec![tt(Op::ConstI32(0))],
                                    }));
                                    lv.push(tt(Op::If {
                                        result_ty: IrType::I32,
                                        then_body: {
                                            let mut g = Vec::new();
                                            // (starter - S_BASE) % T_COUNT == 0?
                                            g.push(tt(Op::LetGet {
                                                idx: LOOKUP,
                                                ty: IrType::I32,
                                            }));
                                            g.push(tt(Op::ConstI32(s_base)));
                                            g.push(tt(Op::Sub(IrType::I32)));
                                            g.push(tt(Op::ConstI32(t_count)));
                                            g.push(tt(Op::Mod(IrType::I32)));
                                            g.push(tt(Op::ConstI32(0)));
                                            g.push(tt(Op::Eq(IrType::I32)));
                                            g.push(tt(Op::If {
                                                result_ty: IrType::I32,
                                                then_body: vec![
                                                    tt(Op::LetGet {
                                                        idx: TMP,
                                                        ty: IrType::I32,
                                                    }),
                                                    tt(Op::ConstI32(t_base + 1)),
                                                    tt(Op::Ge(IrType::I32)),
                                                    tt(Op::If {
                                                        result_ty: IrType::I32,
                                                        then_body: vec![
                                                            tt(Op::LetGet {
                                                                idx: TMP,
                                                                ty: IrType::I32,
                                                            }),
                                                            tt(Op::ConstI32(t_base + t_count)),
                                                            tt(Op::Lt(IrType::I32)),
                                                        ],
                                                        else_body: vec![tt(Op::ConstI32(0))],
                                                    }),
                                                ],
                                                else_body: vec![tt(Op::ConstI32(0))],
                                            }));
                                            g.push(tt(Op::If {
                                                result_ty: IrType::I32,
                                                then_body: {
                                                    let mut comp = Vec::new();
                                                    comp.push(tt(Op::LetGet {
                                                        idx: LOOKUP,
                                                        ty: IrType::I32,
                                                    }));
                                                    comp.push(tt(Op::LetGet {
                                                        idx: TMP,
                                                        ty: IrType::I32,
                                                    }));
                                                    comp.push(tt(Op::ConstI32(t_base)));
                                                    comp.push(tt(Op::Sub(IrType::I32)));
                                                    comp.push(tt(Op::Add(IrType::I32)));
                                                    comp.push(tt(Op::LetSet {
                                                        idx: FOLDED,
                                                        ty: IrType::I32,
                                                    }));
                                                    comp.push(tt(Op::ConstI32(0)));
                                                    comp
                                                },
                                                else_body: vec![tt(Op::ConstI32(0))],
                                            }));
                                            g.push(tt(Op::LetSet {
                                                idx: SINK,
                                                ty: IrType::I32,
                                            }));
                                            g.push(tt(Op::ConstI32(0)));
                                            g
                                        },
                                        else_body: vec![tt(Op::ConstI32(0))],
                                    }));
                                    lv.push(tt(Op::LetSet {
                                        idx: SINK,
                                        ty: IrType::I32,
                                    }));
                                    lv.push(tt(Op::ConstI32(0)));
                                    lv
                                },
                                else_body: vec![tt(Op::ConstI32(0))],
                            }));
                            t.push(tt(Op::LetSet {
                                idx: SINK,
                                ty: IrType::I32,
                            }));
                            // if FOLDED == -1: try table compose
                            t.push(tt(Op::LetGet {
                                idx: FOLDED,
                                ty: IrType::I32,
                            }));
                            t.push(tt(Op::ConstI32(-1)));
                            t.push(tt(Op::Eq(IrType::I32)));
                            t.push(tt(Op::If {
                                result_ty: IrType::I32,
                                then_body: {
                                    let mut tc = Vec::new();
                                    tc.push(tt(Op::LetGet {
                                        idx: LOOKUP,
                                        ty: IrType::I32,
                                    }));
                                    tc.push(tt(Op::LetGet {
                                        idx: TMP,
                                        ty: IrType::I32,
                                    }));
                                    tc.push(tt(Op::CompositionTableAddr));
                                    tc.push(tt(Op::Call {
                                        fn_index: compose_idx,
                                        arg_count: 3,
                                        param_tys: vec![IrType::I32, IrType::I32, IrType::I32],
                                        ret_ty: IrType::I32,
                                    }));
                                    tc.push(tt(Op::LetSet {
                                        idx: FOLDED,
                                        ty: IrType::I32,
                                    }));
                                    tc.push(tt(Op::ConstI32(0)));
                                    tc
                                },
                                else_body: vec![tt(Op::ConstI32(0))],
                            }));
                            t.push(tt(Op::LetSet {
                                idx: SINK,
                                ty: IrType::I32,
                            }));
                            // if FOLDED != -1 AND !(cur_ccc != 0 AND last_ccc >= cur_ccc):
                            //   buf[last_starter] = FOLDED; advance i and continue
                            // Compute blocked:
                            //   blocked = (cur_ccc != 0) AND (last_ccc >= cur_ccc)
                            t.push(tt(Op::LetGet {
                                idx: TMP2,
                                ty: IrType::I32,
                            }));
                            t.push(tt(Op::ConstI32(0)));
                            t.push(tt(Op::Ne(IrType::I32)));
                            t.push(tt(Op::If {
                                result_ty: IrType::I32,
                                then_body: vec![
                                    tt(Op::LetGet {
                                        idx: B_TMP,
                                        ty: IrType::I32,
                                    }),
                                    tt(Op::LetGet {
                                        idx: TMP2,
                                        ty: IrType::I32,
                                    }),
                                    tt(Op::Ge(IrType::I32)),
                                ],
                                else_body: vec![tt(Op::ConstI32(0))],
                            }));
                            // top = blocked
                            t.push(tt(Op::LetSet {
                                idx: SINK,
                                ty: IrType::I32,
                            }));
                            // Now check: FOLDED != -1 AND !blocked
                            t.push(tt(Op::LetGet {
                                idx: FOLDED,
                                ty: IrType::I32,
                            }));
                            t.push(tt(Op::ConstI32(-1)));
                            t.push(tt(Op::Ne(IrType::I32)));
                            t.push(tt(Op::If {
                                result_ty: IrType::I32,
                                then_body: vec![
                                    tt(Op::LetGet {
                                        idx: TMP2,
                                        ty: IrType::I32,
                                    }),
                                    tt(Op::ConstI32(0)),
                                    tt(Op::Ne(IrType::I32)),
                                    tt(Op::If {
                                        result_ty: IrType::I32,
                                        then_body: vec![
                                            tt(Op::LetGet {
                                                idx: B_TMP,
                                                ty: IrType::I32,
                                            }),
                                            tt(Op::LetGet {
                                                idx: TMP2,
                                                ty: IrType::I32,
                                            }),
                                            tt(Op::Lt(IrType::I32)),
                                        ],
                                        else_body: vec![tt(Op::ConstI32(1))],
                                    }),
                                ],
                                else_body: vec![tt(Op::ConstI32(0))],
                            }));
                            // top = can_compose
                            t.push(tt(Op::If {
                                result_ty: IrType::I32,
                                then_body: {
                                    let mut cc = Vec::new();
                                    // store FOLDED into buf[last_starter]
                                    cc.push(tt(Op::LetGet {
                                        idx: CP_BASE,
                                        ty: IrType::I32,
                                    }));
                                    cc.push(tt(Op::ConstI32(4)));
                                    cc.push(tt(Op::Add(IrType::I32)));
                                    cc.push(tt(Op::LetGet {
                                        idx: B0,
                                        ty: IrType::I32,
                                    }));
                                    cc.push(tt(Op::ConstI32(4)));
                                    cc.push(tt(Op::Mul(IrType::I32)));
                                    cc.push(tt(Op::Add(IrType::I32)));
                                    cc.push(tt(Op::LetGet {
                                        idx: FOLDED,
                                        ty: IrType::I32,
                                    }));
                                    cc.push(tt(Op::StoreI32AtAbsolute { offset: 0 }));
                                    // i++
                                    cc.push(tt(Op::LetGet {
                                        idx: I,
                                        ty: IrType::I32,
                                    }));
                                    cc.push(tt(Op::ConstI32(1)));
                                    cc.push(tt(Op::Add(IrType::I32)));
                                    cc.push(tt(Op::LetSet {
                                        idx: I,
                                        ty: IrType::I32,
                                    }));
                                    // Br 2: depth 0 = inner If, 1 =
                                    // outer If, 2 = Loop. Jumping to
                                    // the Loop label continues the
                                    // outer compose walk without
                                    // writing the current cp.
                                    cc.push(tt(Op::Br { label_depth: 2 }));
                                    cc.push(tt(Op::ConstI32(0)));
                                    cc
                                },
                                else_body: vec![tt(Op::ConstI32(0))],
                            }));
                            t.push(tt(Op::LetSet {
                                idx: SINK,
                                ty: IrType::I32,
                            }));
                            t.push(tt(Op::ConstI32(0)));
                            t
                        },
                        else_body: vec![tt(Op::ConstI32(0))],
                    }));
                    lb.push(tt(Op::LetSet {
                        idx: SINK,
                        ty: IrType::I32,
                    }));
                    // buf[write_idx] = cur
                    lb.push(tt(Op::LetGet {
                        idx: CP_BASE,
                        ty: IrType::I32,
                    }));
                    lb.push(tt(Op::ConstI32(4)));
                    lb.push(tt(Op::Add(IrType::I32)));
                    lb.push(tt(Op::LetGet {
                        idx: J,
                        ty: IrType::I32,
                    }));
                    lb.push(tt(Op::ConstI32(4)));
                    lb.push(tt(Op::Mul(IrType::I32)));
                    lb.push(tt(Op::Add(IrType::I32)));
                    lb.push(tt(Op::LetGet {
                        idx: TMP,
                        ty: IrType::I32,
                    }));
                    lb.push(tt(Op::StoreI32AtAbsolute { offset: 0 }));
                    // if cur_ccc == 0: last_starter = write_idx; last_ccc = 0
                    // else: last_ccc = cur_ccc
                    lb.push(tt(Op::LetGet {
                        idx: TMP2,
                        ty: IrType::I32,
                    }));
                    lb.push(tt(Op::ConstI32(0)));
                    lb.push(tt(Op::Eq(IrType::I32)));
                    lb.push(tt(Op::If {
                        result_ty: IrType::I32,
                        then_body: vec![
                            tt(Op::LetGet {
                                idx: J,
                                ty: IrType::I32,
                            }),
                            tt(Op::LetSet {
                                idx: B0,
                                ty: IrType::I32,
                            }),
                            tt(Op::ConstI32(0)),
                            tt(Op::LetSet {
                                idx: B_TMP,
                                ty: IrType::I32,
                            }),
                            tt(Op::ConstI32(0)),
                        ],
                        else_body: vec![
                            tt(Op::LetGet {
                                idx: TMP2,
                                ty: IrType::I32,
                            }),
                            tt(Op::LetSet {
                                idx: B_TMP,
                                ty: IrType::I32,
                            }),
                            tt(Op::ConstI32(0)),
                        ],
                    }));
                    lb.push(tt(Op::LetSet {
                        idx: SINK,
                        ty: IrType::I32,
                    }));
                    // write_idx++
                    lb.push(tt(Op::LetGet {
                        idx: J,
                        ty: IrType::I32,
                    }));
                    lb.push(tt(Op::ConstI32(1)));
                    lb.push(tt(Op::Add(IrType::I32)));
                    lb.push(tt(Op::LetSet {
                        idx: J,
                        ty: IrType::I32,
                    }));
                    // i++
                    lb.push(tt(Op::LetGet {
                        idx: I,
                        ty: IrType::I32,
                    }));
                    lb.push(tt(Op::ConstI32(1)));
                    lb.push(tt(Op::Add(IrType::I32)));
                    lb.push(tt(Op::LetSet {
                        idx: I,
                        ty: IrType::I32,
                    }));
                    lb.push(tt(Op::Br { label_depth: 0 }));
                    lb
                },
            })],
        }));
        // cp_len = write_idx
        v.push(tt(Op::LetGet {
            idx: J,
            ty: IrType::I32,
        }));
        v.push(tt(Op::LetSet {
            idx: CP_LEN,
            ty: IrType::I32,
        }));
        v
    } else {
        Vec::new()
    };

    // ----- encode each cp in cp_buf back to UTF-8 in out_buf -----
    let prefix_plus_shifted = |prefix: i32, shift_div: i32, mask: bool| {
        let mut v = vec![tt(Op::ConstI32(prefix))];
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
    let store_byte = |off: i32, byte_expr: Vec<TaggedOp>| {
        let mut v = Vec::new();
        v.push(tt(Op::LetGet {
            idx: OUT_BASE,
            ty: IrType::I32,
        }));
        v.push(tt(Op::ConstI32(4 + off)));
        v.push(tt(Op::Add(IrType::I32)));
        v.push(tt(Op::LetGet {
            idx: OUT_J,
            ty: IrType::I32,
        }));
        v.push(tt(Op::Add(IrType::I32)));
        v.extend(byte_expr);
        v.push(tt(Op::StoreI8AtAbsolute { offset: 0 }));
        v
    };

    let encode_seq: Vec<TaggedOp> = {
        let mut v: Vec<TaggedOp> = Vec::new();
        v.push(tt(Op::ConstI32(0)));
        v.push(tt(Op::LetSet {
            idx: OUT_J,
            ty: IrType::I32,
        }));
        v.push(tt(Op::ConstI32(0)));
        v.push(tt(Op::LetSet {
            idx: K,
            ty: IrType::I32,
        }));
        v.push(tt(Op::Block {
            result_ty: None,
            body: vec![tt(Op::Loop {
                result_ty: None,
                body: {
                    let mut lb: Vec<TaggedOp> = Vec::new();
                    lb.push(tt(Op::LetGet {
                        idx: K,
                        ty: IrType::I32,
                    }));
                    lb.push(tt(Op::LetGet {
                        idx: CP_LEN,
                        ty: IrType::I32,
                    }));
                    lb.push(tt(Op::Ge(IrType::I32)));
                    lb.push(tt(Op::BrIf { label_depth: 1 }));
                    // FOLDED = buf[k]
                    lb.push(tt(Op::LetGet {
                        idx: CP_BASE,
                        ty: IrType::I32,
                    }));
                    lb.push(tt(Op::ConstI32(4)));
                    lb.push(tt(Op::Add(IrType::I32)));
                    lb.push(tt(Op::LetGet {
                        idx: K,
                        ty: IrType::I32,
                    }));
                    lb.push(tt(Op::ConstI32(4)));
                    lb.push(tt(Op::Mul(IrType::I32)));
                    lb.push(tt(Op::Add(IrType::I32)));
                    lb.push(tt(Op::LoadI32AtAbsolute { offset: 0 }));
                    lb.push(tt(Op::LetSet {
                        idx: FOLDED,
                        ty: IrType::I32,
                    }));
                    // Encode FOLDED to UTF-8 (same 4-arm split as case_fold_body)
                    lb.push(tt(Op::LetGet {
                        idx: FOLDED,
                        ty: IrType::I32,
                    }));
                    lb.push(tt(Op::ConstI32(0x80)));
                    lb.push(tt(Op::Lt(IrType::I32)));
                    lb.push(tt(Op::If {
                        result_ty: IrType::I32,
                        then_body: {
                            let mut t = Vec::new();
                            t.extend(store_byte(
                                0,
                                vec![tt(Op::LetGet {
                                    idx: FOLDED,
                                    ty: IrType::I32,
                                })],
                            ));
                            t.push(tt(Op::LetGet {
                                idx: OUT_J,
                                ty: IrType::I32,
                            }));
                            t.push(tt(Op::ConstI32(1)));
                            t.push(tt(Op::Add(IrType::I32)));
                            t.push(tt(Op::LetSet {
                                idx: OUT_J,
                                ty: IrType::I32,
                            }));
                            t.push(tt(Op::ConstI32(0)));
                            t
                        },
                        else_body: {
                            let mut e = Vec::new();
                            e.push(tt(Op::LetGet {
                                idx: FOLDED,
                                ty: IrType::I32,
                            }));
                            e.push(tt(Op::ConstI32(0x800)));
                            e.push(tt(Op::Lt(IrType::I32)));
                            e.push(tt(Op::If {
                                result_ty: IrType::I32,
                                then_body: {
                                    let mut t = Vec::new();
                                    t.extend(store_byte(0, prefix_plus_shifted(0xC0, 64, false)));
                                    t.extend(store_byte(1, {
                                        let mut x = vec![tt(Op::ConstI32(0x80))];
                                        x.push(tt(Op::LetGet {
                                            idx: FOLDED,
                                            ty: IrType::I32,
                                        }));
                                        x.push(tt(Op::ConstI32(0x3F)));
                                        x.push(tt(Op::BitAnd(IrType::I32)));
                                        x.push(tt(Op::Add(IrType::I32)));
                                        x
                                    }));
                                    t.push(tt(Op::LetGet {
                                        idx: OUT_J,
                                        ty: IrType::I32,
                                    }));
                                    t.push(tt(Op::ConstI32(2)));
                                    t.push(tt(Op::Add(IrType::I32)));
                                    t.push(tt(Op::LetSet {
                                        idx: OUT_J,
                                        ty: IrType::I32,
                                    }));
                                    t.push(tt(Op::ConstI32(0)));
                                    t
                                },
                                else_body: {
                                    let mut e2 = Vec::new();
                                    e2.push(tt(Op::LetGet {
                                        idx: FOLDED,
                                        ty: IrType::I32,
                                    }));
                                    e2.push(tt(Op::ConstI32(0x10000)));
                                    e2.push(tt(Op::Lt(IrType::I32)));
                                    e2.push(tt(Op::If {
                                        result_ty: IrType::I32,
                                        then_body: {
                                            let mut t = Vec::new();
                                            t.extend(store_byte(
                                                0,
                                                prefix_plus_shifted(0xE0, 4096, false),
                                            ));
                                            t.extend(store_byte(
                                                1,
                                                prefix_plus_shifted(0x80, 64, true),
                                            ));
                                            t.extend(store_byte(2, {
                                                let mut x = vec![tt(Op::ConstI32(0x80))];
                                                x.push(tt(Op::LetGet {
                                                    idx: FOLDED,
                                                    ty: IrType::I32,
                                                }));
                                                x.push(tt(Op::ConstI32(0x3F)));
                                                x.push(tt(Op::BitAnd(IrType::I32)));
                                                x.push(tt(Op::Add(IrType::I32)));
                                                x
                                            }));
                                            t.push(tt(Op::LetGet {
                                                idx: OUT_J,
                                                ty: IrType::I32,
                                            }));
                                            t.push(tt(Op::ConstI32(3)));
                                            t.push(tt(Op::Add(IrType::I32)));
                                            t.push(tt(Op::LetSet {
                                                idx: OUT_J,
                                                ty: IrType::I32,
                                            }));
                                            t.push(tt(Op::ConstI32(0)));
                                            t
                                        },
                                        else_body: {
                                            let mut t = Vec::new();
                                            t.extend(store_byte(
                                                0,
                                                prefix_plus_shifted(0xF0, 262144, false),
                                            ));
                                            t.extend(store_byte(
                                                1,
                                                prefix_plus_shifted(0x80, 4096, true),
                                            ));
                                            t.extend(store_byte(
                                                2,
                                                prefix_plus_shifted(0x80, 64, true),
                                            ));
                                            t.extend(store_byte(3, {
                                                let mut x = vec![tt(Op::ConstI32(0x80))];
                                                x.push(tt(Op::LetGet {
                                                    idx: FOLDED,
                                                    ty: IrType::I32,
                                                }));
                                                x.push(tt(Op::ConstI32(0x3F)));
                                                x.push(tt(Op::BitAnd(IrType::I32)));
                                                x.push(tt(Op::Add(IrType::I32)));
                                                x
                                            }));
                                            t.push(tt(Op::LetGet {
                                                idx: OUT_J,
                                                ty: IrType::I32,
                                            }));
                                            t.push(tt(Op::ConstI32(4)));
                                            t.push(tt(Op::Add(IrType::I32)));
                                            t.push(tt(Op::LetSet {
                                                idx: OUT_J,
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
                    lb.push(tt(Op::LetSet {
                        idx: SINK,
                        ty: IrType::I32,
                    }));
                    // k++
                    lb.push(tt(Op::LetGet {
                        idx: K,
                        ty: IrType::I32,
                    }));
                    lb.push(tt(Op::ConstI32(1)));
                    lb.push(tt(Op::Add(IrType::I32)));
                    lb.push(tt(Op::LetSet {
                        idx: K,
                        ty: IrType::I32,
                    }));
                    lb.push(tt(Op::Br { label_depth: 0 }));
                    lb
                },
            })],
        }));
        v
    };

    // ----- assemble the full body -----
    let mut body: Vec<TaggedOp> = Vec::new();
    // s_len = i32.load(s, 0)
    body.push(tt(Op::LocalGet(0)));
    body.push(tt(Op::LoadI32AtAbsolute { offset: 0 }));
    body.push(tt(Op::LetSet {
        idx: S_LEN,
        ty: IrType::I32,
    }));
    // cp_base = alloc_scratch_dyn(4 + s_len * 18 * 4)
    body.push(tt(Op::ConstI32(4)));
    body.push(tt(Op::LetGet {
        idx: S_LEN,
        ty: IrType::I32,
    }));
    body.push(tt(Op::ConstI32(18 * 4)));
    body.push(tt(Op::Mul(IrType::I32)));
    body.push(tt(Op::Add(IrType::I32)));
    body.push(tt(Op::AllocScratchDyn));
    body.push(tt(Op::LetSet {
        idx: CP_BASE,
        ty: IrType::I32,
    }));
    // out_base = alloc_scratch_dyn(4 + s_len * 18 * 4)
    // worst case mirrors cp_buf: each cp is up to 4 bytes UTF-8.
    body.push(tt(Op::ConstI32(4)));
    body.push(tt(Op::LetGet {
        idx: S_LEN,
        ty: IrType::I32,
    }));
    body.push(tt(Op::ConstI32(18 * 4)));
    body.push(tt(Op::Mul(IrType::I32)));
    body.push(tt(Op::Add(IrType::I32)));
    body.push(tt(Op::AllocScratchDyn));
    body.push(tt(Op::LetSet {
        idx: OUT_BASE,
        ty: IrType::I32,
    }));
    // i = 0; j = 0
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
    // Phase 1: decompose loop
    let mut decompose_loop: Vec<TaggedOp> = Vec::new();
    decompose_loop.push(tt(Op::LetGet {
        idx: I,
        ty: IrType::I32,
    }));
    decompose_loop.push(tt(Op::LetGet {
        idx: S_LEN,
        ty: IrType::I32,
    }));
    decompose_loop.push(tt(Op::Ge(IrType::I32)));
    decompose_loop.push(tt(Op::BrIf { label_depth: 1 }));
    decompose_loop.extend(decode_seq);
    decompose_loop.extend(decompose_seq);
    // i += cp_bytes
    decompose_loop.push(tt(Op::LetGet {
        idx: I,
        ty: IrType::I32,
    }));
    decompose_loop.push(tt(Op::LetGet {
        idx: CP_BYTES,
        ty: IrType::I32,
    }));
    decompose_loop.push(tt(Op::Add(IrType::I32)));
    decompose_loop.push(tt(Op::LetSet {
        idx: I,
        ty: IrType::I32,
    }));
    decompose_loop.push(tt(Op::Br { label_depth: 0 }));
    body.push(tt(Op::Block {
        result_ty: None,
        body: vec![tt(Op::Loop {
            result_ty: None,
            body: decompose_loop,
        })],
    }));
    // Phase 2: canonical reorder
    body.extend(reorder_seq);
    // Phase 3: composition (NFC/NFKC only)
    body.extend(compose_seq);
    // Phase 4: encode
    body.extend(encode_seq);
    // Write out_buf header: store(out_base + 0, out_j)
    body.push(tt(Op::LetGet {
        idx: OUT_BASE,
        ty: IrType::I32,
    }));
    body.push(tt(Op::LetGet {
        idx: OUT_J,
        ty: IrType::I32,
    }));
    body.push(tt(Op::StoreI32AtAbsolute { offset: 0 }));
    // Return out_base
    body.push(tt(Op::LetGet {
        idx: OUT_BASE,
        ty: IrType::I32,
    }));
    body.push(tt(Op::Return));

    // F-D2-G: name / params / ret tuple is wired by the caller's
    // `StdlibFunction::new` invocation; this helper only emits the
    // op stream for the given normalization form.
    body
}
