//! Wave R8 body builders for scalar / Bool / String-returning string
//! stdlib functions that previously capped on the compiled backends.
//!
//! Bodies in this file:
//!   * `len(s: String) -> Int` — free-call byte length (the method
//!     forms `s.len()` / `s.length()` already route to the bundled
//!     `length` body; this slot lets the free-call surface lower too).
//!   * `ends_with(s: String, suffix: String) -> Bool` — short-circuit
//!     suffix predicate (sibling to `starts_with`).
//!   * `replace(s: String, from: String, to: String) -> String` —
//!     non-overlapping left-to-right substring replace, byte-identical
//!     to Rust `str::replace`. The empty-`from` case inserts `to` at
//!     every UTF-8 char boundary (and once at the end), matching the
//!     oracle. The body is purely byte-level (loads / stores / `BitAnd`
//!     for the char-boundary test) — no UTF-8 decode or `Op::Trap`, so
//!     it lowers on every backend including LLVM-native / wasm.
//!
//! `trim` / `trim_start` / `trim_end` are intentionally NOT here: a
//! `char::is_whitespace()`-exact trim needs the UTF-8 decoder +
//! `__is_whitespace` helper + `Op::Trap { InvalidUtf8 }` seam that the
//! LLVM-native / wasm backends do not yet lower (the same seam that keeps
//! `upper` / `lower` / `title` / `nfd` at tree-walk + cranelift only —
//! see `relon-codegen-llvm/tests/phase0b_unicode.rs`). Lowering trim
//! there would break the four-way byte-equality contract, so it stays
//! capped (ledger / corpus note).
//!
//! Every body uses only existing `Op`s; the entries are appended at the
//! tail of [`super::registry::builtin_stdlib`] so no position-pinned
//! index moves and the cranelift/llvm byte output of existing
//! constructs is unchanged (GENERATOR_VERSION stays put).

use crate::ir::{IrType, Op, TaggedOp};

use super::defs::tt;
use super::signatures::StdlibFunction;

/// `len(s: String) -> Int` free-call body. Identical op-stream to the
/// bundled `length` body — reads the record's `[len: u32 LE]` header.
/// A distinct registry slot named `len` lets the free-call surface
/// (`len(s)`) resolve through `stdlib_function_index` while the method
/// forms keep routing through `stdlib_method_index`.
pub(super) fn len_string_to_int() -> StdlibFunction {
    StdlibFunction::new("len", vec![IrType::String], IrType::I64, || {
        vec![tt(Op::LocalGet(0)), tt(Op::ReadStringLen), tt(Op::Return)]
    })
}

/// `ends_with(s: String, suffix: String) -> Bool`.
///
/// Mirrors [`super::defs::starts_with_string`] but anchors the compare
/// at the tail: aligns the suffix against `s[s_len - p_len ..]` and
/// compares byte-by-byte. `p_len > s_len` returns `false`; empty suffix
/// returns `true` (the loop never runs, `acc` stays 1). Byte-exact with
/// Rust `str::ends_with(&str)` over valid UTF-8.
pub(super) fn ends_with_string() -> StdlibFunction {
    StdlibFunction::new(
        "ends_with",
        vec![IrType::String, IrType::String],
        IrType::Bool,
        ends_with_string_body,
    )
}

fn ends_with_string_body() -> Vec<TaggedOp> {
    const S_LEN: u32 = 0;
    const P_LEN: u32 = 1;
    const I: u32 = 2;
    const ACC: u32 = 3;
    const BASE_OFF: u32 = 4; // s_len - p_len (suffix start offset in s)
    vec![
        // s_len = load_i32(s, 0)
        tt(Op::LocalGet(0)),
        tt(Op::LoadI32AtAbsolute { offset: 0 }),
        tt(Op::LetSet {
            idx: S_LEN,
            ty: IrType::I32,
        }),
        // p_len = load_i32(p, 0)
        tt(Op::LocalGet(1)),
        tt(Op::LoadI32AtAbsolute { offset: 0 }),
        tt(Op::LetSet {
            idx: P_LEN,
            ty: IrType::I32,
        }),
        // if p_len > s_len { false } else { scan } -> Bool
        tt(Op::LetGet {
            idx: P_LEN,
            ty: IrType::I32,
        }),
        tt(Op::LetGet {
            idx: S_LEN,
            ty: IrType::I32,
        }),
        tt(Op::Gt(IrType::I32)),
        tt(Op::If {
            result_ty: IrType::Bool,
            then_body: vec![tt(Op::ConstBool(false))],
            else_body: vec![
                // base_off = s_len - p_len
                tt(Op::LetGet {
                    idx: S_LEN,
                    ty: IrType::I32,
                }),
                tt(Op::LetGet {
                    idx: P_LEN,
                    ty: IrType::I32,
                }),
                tt(Op::Sub(IrType::I32)),
                tt(Op::LetSet {
                    idx: BASE_OFF,
                    ty: IrType::I32,
                }),
                // acc = 1
                tt(Op::ConstI32(1)),
                tt(Op::LetSet {
                    idx: ACC,
                    ty: IrType::I32,
                }),
                // i = 0
                tt(Op::ConstI32(0)),
                tt(Op::LetSet {
                    idx: I,
                    ty: IrType::I32,
                }),
                tt(Op::Block {
                    result_ty: None,
                    body: vec![tt(Op::Loop {
                        result_ty: None,
                        body: vec![
                            // exit when i >= p_len
                            tt(Op::LetGet {
                                idx: I,
                                ty: IrType::I32,
                            }),
                            tt(Op::LetGet {
                                idx: P_LEN,
                                ty: IrType::I32,
                            }),
                            tt(Op::Ge(IrType::I32)),
                            tt(Op::BrIf { label_depth: 1 }),
                            // sb = i8u(s + 4 + base_off + i)
                            tt(Op::LocalGet(0)),
                            tt(Op::ConstI32(4)),
                            tt(Op::Add(IrType::I32)),
                            tt(Op::LetGet {
                                idx: BASE_OFF,
                                ty: IrType::I32,
                            }),
                            tt(Op::Add(IrType::I32)),
                            tt(Op::LetGet {
                                idx: I,
                                ty: IrType::I32,
                            }),
                            tt(Op::Add(IrType::I32)),
                            tt(Op::LoadI8UAtAbsolute { offset: 0 }),
                            // pb = i8u(p + 4 + i)
                            tt(Op::LocalGet(1)),
                            tt(Op::ConstI32(4)),
                            tt(Op::Add(IrType::I32)),
                            tt(Op::LetGet {
                                idx: I,
                                ty: IrType::I32,
                            }),
                            tt(Op::Add(IrType::I32)),
                            tt(Op::LoadI8UAtAbsolute { offset: 0 }),
                            // acc = acc & (sb == pb)
                            //   (structurally branch-free so no `Br` ever
                            //    crosses an `If` boundary — LLVM's branch
                            //    validator counts only Block/Loop labels.)
                            tt(Op::Eq(IrType::I32)),
                            tt(Op::LetGet {
                                idx: ACC,
                                ty: IrType::I32,
                            }),
                            tt(Op::BitAnd(IrType::I32)),
                            tt(Op::LetSet {
                                idx: ACC,
                                ty: IrType::I32,
                            }),
                            // early-out: if acc == 0, exit the scan (Br to
                            // the enclosing Block, depth 1 from the Loop).
                            tt(Op::LetGet {
                                idx: ACC,
                                ty: IrType::I32,
                            }),
                            tt(Op::ConstI32(0)),
                            tt(Op::Eq(IrType::I32)),
                            tt(Op::BrIf { label_depth: 1 }),
                            // i = i + 1
                            tt(Op::LetGet {
                                idx: I,
                                ty: IrType::I32,
                            }),
                            tt(Op::ConstI32(1)),
                            tt(Op::Add(IrType::I32)),
                            tt(Op::LetSet {
                                idx: I,
                                ty: IrType::I32,
                            }),
                            tt(Op::Br { label_depth: 0 }),
                        ],
                    })],
                }),
                // result = acc != 0
                tt(Op::LetGet {
                    idx: ACC,
                    ty: IrType::I32,
                }),
                tt(Op::ConstI32(0)),
                tt(Op::Ne(IrType::I32)),
            ],
        }),
        tt(Op::Return),
    ]
}

/// `replace(s: String, from: String, to: String) -> String`.
///
/// Byte-exact with Rust `str::replace`:
///   * Non-empty `from`: scan `s` left-to-right; at each position test
///     whether `from` matches; on a match, emit `to` and advance the
///     read cursor by `from.len()` (non-overlapping); otherwise emit
///     `s[i]` and advance by 1.
///   * Empty `from`: emit `to` before every UTF-8 char boundary in `s`
///     and once more at the very end (so `"ab".replace("", "-")` →
///     `"-a-b-"`). A byte is a char boundary iff `(b & 0xC0) != 0x80`.
///
/// Two passes: the first computes the exact output byte length (so the
/// scratch record is sized correctly), the second writes the bytes.
#[allow(clippy::vec_init_then_push)]
pub(super) fn replace_string() -> StdlibFunction {
    StdlibFunction::new(
        "replace",
        vec![IrType::String, IrType::String, IrType::String],
        IrType::String,
        replace_string_body,
    )
}

#[allow(clippy::vec_init_then_push)]
fn replace_string_body() -> Vec<TaggedOp> {
    const S_LEN: u32 = 0;
    const F_LEN: u32 = 1;
    const T_LEN: u32 = 2;
    const OUT_LEN: u32 = 3;
    const BASE: u32 = 4;
    const I: u32 = 5; // read cursor into s payload
    const W: u32 = 6; // write cursor into output payload
    const MATCH: u32 = 7; // 1 if `from` matches at i
    const J: u32 = 8; // inner compare / copy cursor
    const SINK: u32 = 9;
    const SB: u32 = 10;
    const BYTE: u32 = 11;

    let mut body: Vec<TaggedOp> = Vec::new();

    // Lengths.
    body.push(tt(Op::LocalGet(0)));
    body.push(tt(Op::LoadI32AtAbsolute { offset: 0 }));
    body.push(tt(Op::LetSet {
        idx: S_LEN,
        ty: IrType::I32,
    }));
    body.push(tt(Op::LocalGet(1)));
    body.push(tt(Op::LoadI32AtAbsolute { offset: 0 }));
    body.push(tt(Op::LetSet {
        idx: F_LEN,
        ty: IrType::I32,
    }));
    body.push(tt(Op::LocalGet(2)));
    body.push(tt(Op::LoadI32AtAbsolute { offset: 0 }));
    body.push(tt(Op::LetSet {
        idx: T_LEN,
        ty: IrType::I32,
    }));

    // ===== Pass 1: compute OUT_LEN =====
    body.push(tt(Op::ConstI32(0)));
    body.push(tt(Op::LetSet {
        idx: OUT_LEN,
        ty: IrType::I32,
    }));
    body.push(tt(Op::ConstI32(0)));
    body.push(tt(Op::LetSet {
        idx: I,
        ty: IrType::I32,
    }));

    // if F_LEN == 0 { empty-from sizing } else { scan sizing }
    body.push(tt(Op::LetGet {
        idx: F_LEN,
        ty: IrType::I32,
    }));
    body.push(tt(Op::ConstI32(0)));
    body.push(tt(Op::Eq(IrType::I32)));
    body.push(tt(Op::If {
        result_ty: IrType::I32,
        then_body: empty_from_size(S_LEN, T_LEN, OUT_LEN, I, SB),
        else_body: nonempty_scan_size(S_LEN, F_LEN, T_LEN, OUT_LEN, I, MATCH, J, SINK, SB, BYTE),
    }));
    body.push(tt(Op::LetSet {
        idx: SINK,
        ty: IrType::I32,
    }));

    // ===== Allocate output =====
    body.push(tt(Op::LetGet {
        idx: OUT_LEN,
        ty: IrType::I32,
    }));
    body.push(tt(Op::ConstI32(4)));
    body.push(tt(Op::Add(IrType::I32)));
    body.push(tt(Op::AllocScratchDyn));
    body.push(tt(Op::LetSet {
        idx: BASE,
        ty: IrType::I32,
    }));
    // header
    body.push(tt(Op::LetGet {
        idx: BASE,
        ty: IrType::I32,
    }));
    body.push(tt(Op::LetGet {
        idx: OUT_LEN,
        ty: IrType::I32,
    }));
    body.push(tt(Op::StoreI32AtAbsolute { offset: 0 }));

    // ===== Pass 2: write bytes =====
    body.push(tt(Op::ConstI32(0)));
    body.push(tt(Op::LetSet {
        idx: I,
        ty: IrType::I32,
    }));
    body.push(tt(Op::ConstI32(0)));
    body.push(tt(Op::LetSet {
        idx: W,
        ty: IrType::I32,
    }));
    body.push(tt(Op::LetGet {
        idx: F_LEN,
        ty: IrType::I32,
    }));
    body.push(tt(Op::ConstI32(0)));
    body.push(tt(Op::Eq(IrType::I32)));
    body.push(tt(Op::If {
        result_ty: IrType::I32,
        then_body: empty_from_write(S_LEN, T_LEN, BASE, I, W, J, SINK, SB, BYTE),
        else_body: nonempty_scan_write(S_LEN, F_LEN, T_LEN, BASE, I, W, MATCH, J, SINK, SB, BYTE),
    }));
    body.push(tt(Op::LetSet {
        idx: SINK,
        ty: IrType::I32,
    }));

    // return base
    body.push(tt(Op::LetGet {
        idx: BASE,
        ty: IrType::I32,
    }));
    body.push(tt(Op::Return));
    body
}

// ---------------------------------------------------------------------
// Shared op-stream helpers.
// ---------------------------------------------------------------------

// ---- replace pass-1 helpers (sizing) ----

/// Empty-`from` sizing: `out_len = s_len + (char_count + 1) * t_len`,
/// where char_count = number of UTF-8 char-boundary bytes in s. We add
/// `t_len` once per char-start byte and once more at the end.
fn empty_from_size(s_len: u32, t_len: u32, out_len: u32, i: u32, sb: u32) -> Vec<TaggedOp> {
    vec![
        // out_len = s_len (all original bytes survive)
        tt(Op::LetGet {
            idx: s_len,
            ty: IrType::I32,
        }),
        tt(Op::LetSet {
            idx: out_len,
            ty: IrType::I32,
        }),
        // i = 0
        tt(Op::ConstI32(0)),
        tt(Op::LetSet {
            idx: i,
            ty: IrType::I32,
        }),
        tt(Op::Block {
            result_ty: None,
            body: vec![tt(Op::Loop {
                result_ty: None,
                body: vec![
                    tt(Op::LetGet {
                        idx: i,
                        ty: IrType::I32,
                    }),
                    tt(Op::LetGet {
                        idx: s_len,
                        ty: IrType::I32,
                    }),
                    tt(Op::Ge(IrType::I32)),
                    tt(Op::BrIf { label_depth: 1 }),
                    // sb = i8u(s + 4 + i)
                    tt(Op::LocalGet(0)),
                    tt(Op::ConstI32(4)),
                    tt(Op::Add(IrType::I32)),
                    tt(Op::LetGet {
                        idx: i,
                        ty: IrType::I32,
                    }),
                    tt(Op::Add(IrType::I32)),
                    tt(Op::LoadI8UAtAbsolute { offset: 0 }),
                    tt(Op::LetSet {
                        idx: sb,
                        ty: IrType::I32,
                    }),
                    // if (sb & 0xC0) != 0x80 { out_len += t_len }
                    tt(Op::LetGet {
                        idx: sb,
                        ty: IrType::I32,
                    }),
                    tt(Op::ConstI32(0xC0)),
                    tt(Op::BitAnd(IrType::I32)),
                    tt(Op::ConstI32(0x80)),
                    tt(Op::Ne(IrType::I32)),
                    tt(Op::If {
                        result_ty: IrType::I32,
                        then_body: vec![
                            tt(Op::LetGet {
                                idx: out_len,
                                ty: IrType::I32,
                            }),
                            tt(Op::LetGet {
                                idx: t_len,
                                ty: IrType::I32,
                            }),
                            tt(Op::Add(IrType::I32)),
                            tt(Op::LetSet {
                                idx: out_len,
                                ty: IrType::I32,
                            }),
                            tt(Op::ConstI32(0)),
                        ],
                        else_body: vec![tt(Op::ConstI32(0))],
                    }),
                    tt(Op::LetSet {
                        idx: sb,
                        ty: IrType::I32,
                    }),
                    // i += 1
                    tt(Op::LetGet {
                        idx: i,
                        ty: IrType::I32,
                    }),
                    tt(Op::ConstI32(1)),
                    tt(Op::Add(IrType::I32)),
                    tt(Op::LetSet {
                        idx: i,
                        ty: IrType::I32,
                    }),
                    tt(Op::Br { label_depth: 0 }),
                ],
            })],
        }),
        // out_len += t_len (trailing boundary)
        tt(Op::LetGet {
            idx: out_len,
            ty: IrType::I32,
        }),
        tt(Op::LetGet {
            idx: t_len,
            ty: IrType::I32,
        }),
        tt(Op::Add(IrType::I32)),
        tt(Op::LetSet {
            idx: out_len,
            ty: IrType::I32,
        }),
        tt(Op::ConstI32(0)),
    ]
}

/// Non-empty `from` sizing: scan and, at each match, add `t_len` and
/// skip `f_len`; otherwise add 1 and skip 1.
#[allow(clippy::too_many_arguments, clippy::vec_init_then_push)]
fn nonempty_scan_size(
    s_len: u32,
    f_len: u32,
    t_len: u32,
    out_len: u32,
    i: u32,
    match_flag: u32,
    j: u32,
    sink: u32,
    sb: u32,
    byte: u32,
) -> Vec<TaggedOp> {
    vec![
        tt(Op::ConstI32(0)),
        tt(Op::LetSet {
            idx: i,
            ty: IrType::I32,
        }),
        tt(Op::Block {
            result_ty: None,
            body: vec![tt(Op::Loop {
                result_ty: None,
                body: {
                    let mut l = Vec::new();
                    l.push(tt(Op::LetGet {
                        idx: i,
                        ty: IrType::I32,
                    }));
                    l.push(tt(Op::LetGet {
                        idx: s_len,
                        ty: IrType::I32,
                    }));
                    l.push(tt(Op::Ge(IrType::I32)));
                    l.push(tt(Op::BrIf { label_depth: 1 }));
                    l.extend(match_from_at_i(
                        s_len, f_len, i, match_flag, j, sink, sb, byte,
                    ));
                    // if match { out_len += t_len; i += f_len }
                    //      else { out_len += 1;     i += 1 }
                    l.push(tt(Op::LetGet {
                        idx: match_flag,
                        ty: IrType::I32,
                    }));
                    l.push(tt(Op::If {
                        result_ty: IrType::I32,
                        then_body: vec![
                            tt(Op::LetGet {
                                idx: out_len,
                                ty: IrType::I32,
                            }),
                            tt(Op::LetGet {
                                idx: t_len,
                                ty: IrType::I32,
                            }),
                            tt(Op::Add(IrType::I32)),
                            tt(Op::LetSet {
                                idx: out_len,
                                ty: IrType::I32,
                            }),
                            tt(Op::LetGet {
                                idx: i,
                                ty: IrType::I32,
                            }),
                            tt(Op::LetGet {
                                idx: f_len,
                                ty: IrType::I32,
                            }),
                            tt(Op::Add(IrType::I32)),
                            tt(Op::LetSet {
                                idx: i,
                                ty: IrType::I32,
                            }),
                            tt(Op::ConstI32(0)),
                        ],
                        else_body: vec![
                            tt(Op::LetGet {
                                idx: out_len,
                                ty: IrType::I32,
                            }),
                            tt(Op::ConstI32(1)),
                            tt(Op::Add(IrType::I32)),
                            tt(Op::LetSet {
                                idx: out_len,
                                ty: IrType::I32,
                            }),
                            tt(Op::LetGet {
                                idx: i,
                                ty: IrType::I32,
                            }),
                            tt(Op::ConstI32(1)),
                            tt(Op::Add(IrType::I32)),
                            tt(Op::LetSet {
                                idx: i,
                                ty: IrType::I32,
                            }),
                            tt(Op::ConstI32(0)),
                        ],
                    }));
                    l.push(tt(Op::LetSet {
                        idx: sink,
                        ty: IrType::I32,
                    }));
                    l.push(tt(Op::Br { label_depth: 0 }));
                    l
                },
            })],
        }),
        tt(Op::ConstI32(0)),
    ]
}

/// Set `match_flag = 1` iff the `from` record matches `s` starting at
/// payload offset `i`. Requires `i + f_len <= s_len`; otherwise 0.
/// Stack-neutral; uses `j` / `sb` / `byte` scratch locals.
#[allow(clippy::too_many_arguments, clippy::vec_init_then_push)]
fn match_from_at_i(
    s_len: u32,
    f_len: u32,
    i: u32,
    match_flag: u32,
    j: u32,
    sink: u32,
    sb: u32,
    byte: u32,
) -> Vec<TaggedOp> {
    vec![
        // if i + f_len > s_len { match = 0 } else { compare loop }
        tt(Op::LetGet {
            idx: i,
            ty: IrType::I32,
        }),
        tt(Op::LetGet {
            idx: f_len,
            ty: IrType::I32,
        }),
        tt(Op::Add(IrType::I32)),
        tt(Op::LetGet {
            idx: s_len,
            ty: IrType::I32,
        }),
        tt(Op::Gt(IrType::I32)),
        tt(Op::If {
            result_ty: IrType::I32,
            then_body: vec![
                tt(Op::ConstI32(0)),
                tt(Op::LetSet {
                    idx: match_flag,
                    ty: IrType::I32,
                }),
                tt(Op::ConstI32(0)),
            ],
            else_body: {
                let mut e = Vec::new();
                // match = 1
                e.push(tt(Op::ConstI32(1)));
                e.push(tt(Op::LetSet {
                    idx: match_flag,
                    ty: IrType::I32,
                }));
                // j = 0
                e.push(tt(Op::ConstI32(0)));
                e.push(tt(Op::LetSet {
                    idx: j,
                    ty: IrType::I32,
                }));
                e.push(tt(Op::Block {
                    result_ty: None,
                    body: vec![tt(Op::Loop {
                        result_ty: None,
                        body: vec![
                            tt(Op::LetGet {
                                idx: j,
                                ty: IrType::I32,
                            }),
                            tt(Op::LetGet {
                                idx: f_len,
                                ty: IrType::I32,
                            }),
                            tt(Op::Ge(IrType::I32)),
                            tt(Op::BrIf { label_depth: 1 }),
                            // sb = i8u(s + 4 + i + j)
                            tt(Op::LocalGet(0)),
                            tt(Op::ConstI32(4)),
                            tt(Op::Add(IrType::I32)),
                            tt(Op::LetGet {
                                idx: i,
                                ty: IrType::I32,
                            }),
                            tt(Op::Add(IrType::I32)),
                            tt(Op::LetGet {
                                idx: j,
                                ty: IrType::I32,
                            }),
                            tt(Op::Add(IrType::I32)),
                            tt(Op::LoadI8UAtAbsolute { offset: 0 }),
                            tt(Op::LetSet {
                                idx: sb,
                                ty: IrType::I32,
                            }),
                            // byte = i8u(f + 4 + j)
                            tt(Op::LocalGet(1)),
                            tt(Op::ConstI32(4)),
                            tt(Op::Add(IrType::I32)),
                            tt(Op::LetGet {
                                idx: j,
                                ty: IrType::I32,
                            }),
                            tt(Op::Add(IrType::I32)),
                            tt(Op::LoadI8UAtAbsolute { offset: 0 }),
                            tt(Op::LetSet {
                                idx: byte,
                                ty: IrType::I32,
                            }),
                            // match_flag = match_flag & (sb == byte)
                            //   (branch-free, so no `Br` crosses an `If`.)
                            tt(Op::LetGet {
                                idx: sb,
                                ty: IrType::I32,
                            }),
                            tt(Op::LetGet {
                                idx: byte,
                                ty: IrType::I32,
                            }),
                            tt(Op::Eq(IrType::I32)),
                            tt(Op::LetGet {
                                idx: match_flag,
                                ty: IrType::I32,
                            }),
                            tt(Op::BitAnd(IrType::I32)),
                            tt(Op::LetSet {
                                idx: match_flag,
                                ty: IrType::I32,
                            }),
                            // early-out: if match_flag == 0, exit compare
                            // loop (Br to enclosing Block, depth 1).
                            tt(Op::LetGet {
                                idx: match_flag,
                                ty: IrType::I32,
                            }),
                            tt(Op::ConstI32(0)),
                            tt(Op::Eq(IrType::I32)),
                            tt(Op::BrIf { label_depth: 1 }),
                            // j += 1
                            tt(Op::LetGet {
                                idx: j,
                                ty: IrType::I32,
                            }),
                            tt(Op::ConstI32(1)),
                            tt(Op::Add(IrType::I32)),
                            tt(Op::LetSet {
                                idx: j,
                                ty: IrType::I32,
                            }),
                            tt(Op::Br { label_depth: 0 }),
                        ],
                    })],
                }));
                e.push(tt(Op::ConstI32(0)));
                e
            },
        }),
        tt(Op::LetSet {
            idx: sink,
            ty: IrType::I32,
        }),
    ]
}

// ---- replace pass-2 helpers (writing) ----

/// Copy `to`'s payload (`t_len` bytes) into `base + 4 + w`, advancing
/// `w` by `t_len`. Stack-neutral.
fn emit_to(t_len: u32, base: u32, w: u32) -> Vec<TaggedOp> {
    vec![
        // memcpy(base + 4 + w, to + 4, t_len)
        tt(Op::LetGet {
            idx: base,
            ty: IrType::I32,
        }),
        tt(Op::ConstI32(4)),
        tt(Op::Add(IrType::I32)),
        tt(Op::LetGet {
            idx: w,
            ty: IrType::I32,
        }),
        tt(Op::Add(IrType::I32)),
        tt(Op::LocalGet(2)),
        tt(Op::ConstI32(4)),
        tt(Op::Add(IrType::I32)),
        tt(Op::LetGet {
            idx: t_len,
            ty: IrType::I32,
        }),
        tt(Op::MemcpyAtAbsolute),
        // w += t_len
        tt(Op::LetGet {
            idx: w,
            ty: IrType::I32,
        }),
        tt(Op::LetGet {
            idx: t_len,
            ty: IrType::I32,
        }),
        tt(Op::Add(IrType::I32)),
        tt(Op::LetSet {
            idx: w,
            ty: IrType::I32,
        }),
    ]
}

/// Empty-`from` write: emit `to`, then each source byte preceded by a
/// `to` insertion at every char boundary, then a final `to`.
/// Equivalent: before copying a source byte that is a char start, emit
/// `to`; after the loop, emit `to` once. The very first char start at
/// i==0 yields the leading `to`.
#[allow(clippy::too_many_arguments, clippy::vec_init_then_push)]
fn empty_from_write(
    s_len: u32,
    t_len: u32,
    base: u32,
    i: u32,
    w: u32,
    j: u32,
    sink: u32,
    sb: u32,
    byte: u32,
) -> Vec<TaggedOp> {
    let _ = (j, byte);
    vec![
        tt(Op::ConstI32(0)),
        tt(Op::LetSet {
            idx: i,
            ty: IrType::I32,
        }),
        tt(Op::Block {
            result_ty: None,
            body: vec![tt(Op::Loop {
                result_ty: None,
                body: {
                    let mut l = Vec::new();
                    l.push(tt(Op::LetGet {
                        idx: i,
                        ty: IrType::I32,
                    }));
                    l.push(tt(Op::LetGet {
                        idx: s_len,
                        ty: IrType::I32,
                    }));
                    l.push(tt(Op::Ge(IrType::I32)));
                    l.push(tt(Op::BrIf { label_depth: 1 }));
                    // sb = i8u(s + 4 + i)
                    l.push(tt(Op::LocalGet(0)));
                    l.push(tt(Op::ConstI32(4)));
                    l.push(tt(Op::Add(IrType::I32)));
                    l.push(tt(Op::LetGet {
                        idx: i,
                        ty: IrType::I32,
                    }));
                    l.push(tt(Op::Add(IrType::I32)));
                    l.push(tt(Op::LoadI8UAtAbsolute { offset: 0 }));
                    l.push(tt(Op::LetSet {
                        idx: sb,
                        ty: IrType::I32,
                    }));
                    // if (sb & 0xC0) != 0x80 { emit to }
                    l.push(tt(Op::LetGet {
                        idx: sb,
                        ty: IrType::I32,
                    }));
                    l.push(tt(Op::ConstI32(0xC0)));
                    l.push(tt(Op::BitAnd(IrType::I32)));
                    l.push(tt(Op::ConstI32(0x80)));
                    l.push(tt(Op::Ne(IrType::I32)));
                    l.push(tt(Op::If {
                        result_ty: IrType::I32,
                        then_body: {
                            let mut t = emit_to(t_len, base, w);
                            t.push(tt(Op::ConstI32(0)));
                            t
                        },
                        else_body: vec![tt(Op::ConstI32(0))],
                    }));
                    l.push(tt(Op::LetSet {
                        idx: sink,
                        ty: IrType::I32,
                    }));
                    // store source byte: i8.store(base + 4 + w, sb); w += 1
                    l.push(tt(Op::LetGet {
                        idx: base,
                        ty: IrType::I32,
                    }));
                    l.push(tt(Op::ConstI32(4)));
                    l.push(tt(Op::Add(IrType::I32)));
                    l.push(tt(Op::LetGet {
                        idx: w,
                        ty: IrType::I32,
                    }));
                    l.push(tt(Op::Add(IrType::I32)));
                    l.push(tt(Op::LetGet {
                        idx: sb,
                        ty: IrType::I32,
                    }));
                    l.push(tt(Op::StoreI8AtAbsolute { offset: 0 }));
                    l.push(tt(Op::LetGet {
                        idx: w,
                        ty: IrType::I32,
                    }));
                    l.push(tt(Op::ConstI32(1)));
                    l.push(tt(Op::Add(IrType::I32)));
                    l.push(tt(Op::LetSet {
                        idx: w,
                        ty: IrType::I32,
                    }));
                    // i += 1
                    l.push(tt(Op::LetGet {
                        idx: i,
                        ty: IrType::I32,
                    }));
                    l.push(tt(Op::ConstI32(1)));
                    l.push(tt(Op::Add(IrType::I32)));
                    l.push(tt(Op::LetSet {
                        idx: i,
                        ty: IrType::I32,
                    }));
                    l.push(tt(Op::Br { label_depth: 0 }));
                    l
                },
            })],
        }),
        // trailing to
        {
            let seq = emit_to(t_len, base, w);
            // wrap: emit_to is stack-neutral; append then push a 0 result
            tt(Op::Block {
                result_ty: None,
                body: seq,
            })
        },
        tt(Op::ConstI32(0)),
    ]
}

/// Non-empty `from` write: scan; on match emit `to` and skip `f_len`,
/// otherwise copy one source byte and advance 1.
#[allow(clippy::too_many_arguments, clippy::vec_init_then_push)]
fn nonempty_scan_write(
    s_len: u32,
    f_len: u32,
    t_len: u32,
    base: u32,
    i: u32,
    w: u32,
    match_flag: u32,
    j: u32,
    sink: u32,
    sb: u32,
    byte: u32,
) -> Vec<TaggedOp> {
    vec![
        tt(Op::ConstI32(0)),
        tt(Op::LetSet {
            idx: i,
            ty: IrType::I32,
        }),
        tt(Op::Block {
            result_ty: None,
            body: vec![tt(Op::Loop {
                result_ty: None,
                body: {
                    let mut l = Vec::new();
                    l.push(tt(Op::LetGet {
                        idx: i,
                        ty: IrType::I32,
                    }));
                    l.push(tt(Op::LetGet {
                        idx: s_len,
                        ty: IrType::I32,
                    }));
                    l.push(tt(Op::Ge(IrType::I32)));
                    l.push(tt(Op::BrIf { label_depth: 1 }));
                    l.extend(match_from_at_i(
                        s_len, f_len, i, match_flag, j, sink, sb, byte,
                    ));
                    l.push(tt(Op::LetGet {
                        idx: match_flag,
                        ty: IrType::I32,
                    }));
                    l.push(tt(Op::If {
                        result_ty: IrType::I32,
                        then_body: {
                            let mut t = emit_to(t_len, base, w);
                            // i += f_len
                            t.push(tt(Op::LetGet {
                                idx: i,
                                ty: IrType::I32,
                            }));
                            t.push(tt(Op::LetGet {
                                idx: f_len,
                                ty: IrType::I32,
                            }));
                            t.push(tt(Op::Add(IrType::I32)));
                            t.push(tt(Op::LetSet {
                                idx: i,
                                ty: IrType::I32,
                            }));
                            t.push(tt(Op::ConstI32(0)));
                            t
                        },
                        else_body: {
                            let mut e = Vec::new();
                            // sb = i8u(s + 4 + i)
                            e.push(tt(Op::LocalGet(0)));
                            e.push(tt(Op::ConstI32(4)));
                            e.push(tt(Op::Add(IrType::I32)));
                            e.push(tt(Op::LetGet {
                                idx: i,
                                ty: IrType::I32,
                            }));
                            e.push(tt(Op::Add(IrType::I32)));
                            e.push(tt(Op::LoadI8UAtAbsolute { offset: 0 }));
                            e.push(tt(Op::LetSet {
                                idx: sb,
                                ty: IrType::I32,
                            }));
                            // store: i8.store(base + 4 + w, sb)
                            e.push(tt(Op::LetGet {
                                idx: base,
                                ty: IrType::I32,
                            }));
                            e.push(tt(Op::ConstI32(4)));
                            e.push(tt(Op::Add(IrType::I32)));
                            e.push(tt(Op::LetGet {
                                idx: w,
                                ty: IrType::I32,
                            }));
                            e.push(tt(Op::Add(IrType::I32)));
                            e.push(tt(Op::LetGet {
                                idx: sb,
                                ty: IrType::I32,
                            }));
                            e.push(tt(Op::StoreI8AtAbsolute { offset: 0 }));
                            // w += 1
                            e.push(tt(Op::LetGet {
                                idx: w,
                                ty: IrType::I32,
                            }));
                            e.push(tt(Op::ConstI32(1)));
                            e.push(tt(Op::Add(IrType::I32)));
                            e.push(tt(Op::LetSet {
                                idx: w,
                                ty: IrType::I32,
                            }));
                            // i += 1
                            e.push(tt(Op::LetGet {
                                idx: i,
                                ty: IrType::I32,
                            }));
                            e.push(tt(Op::ConstI32(1)));
                            e.push(tt(Op::Add(IrType::I32)));
                            e.push(tt(Op::LetSet {
                                idx: i,
                                ty: IrType::I32,
                            }));
                            e.push(tt(Op::ConstI32(0)));
                            e
                        },
                    }));
                    l.push(tt(Op::LetSet {
                        idx: sink,
                        ty: IrType::I32,
                    }));
                    l.push(tt(Op::Br { label_depth: 0 }));
                    l
                },
            })],
        }),
        tt(Op::ConstI32(0)),
    ]
}

// ---------------------------------------------------------------------
// Wave R15: `split(s: String, sep: String) -> List<String>`.
// ---------------------------------------------------------------------

/// `split(s: String, sep: String) -> List<String>`.
///
/// Byte-exact with the tree-walk oracle (`str::split(&str)` over a
/// **non-empty** substring separator): splits `s` at every
/// non-overlapping left-to-right occurrence of `sep` and returns the
/// `N+1` segments between cuts (where `N` is the match count). Leading,
/// trailing, and consecutive-delimiter cuts each yield an empty segment,
/// and an empty input yields the single empty segment `[""]`; a no-match
/// input returns the whole string as one element. The empty-separator
/// case is rejected by the tree-walk oracle (it returns a loud
/// `UnsupportedOperator` error rather than a value), so it never reaches
/// a lowered body — the lowering keeps it capped (it cannot be proven
/// byte-equal to a *value* the oracle never produces). See the cap note
/// in `super::registry::builtin_stdlib`.
///
/// The result is a `List<String>` pointer-array record
/// `[count: u32][off_0: u32]…[off_{N}: u32]` whose `off_i` are
/// arena-relative offsets to per-segment String records `[len: u32][utf8]`.
/// This matches the `write_list_string` layout the return ABI / verifier
/// walk byte-for-byte (same shape Wave R3c's `list_map_to_string_body`
/// produces, but with a **data-dependent** element count). Every segment
/// record is independently arena-allocated and self-contained under the
/// single global arena-relative pointer convention, so the result returns
/// in place through the shared `inplace_return` decoder (no rigid-block
/// copy / relocation — same invariant as the R3c String-result HOF
/// results and a param-sourced pointer array).
///
/// Two passes over the input. Pass 1 counts the segment count `N` (match
/// count + 1) so the header size is known; pass 2 re-scans, and at every
/// cut emits one segment String record + writes its offset into the next
/// header slot. The body is purely byte-level (record-header read, byte
/// loads/stores, `Memcpy`, integer compares / `BitAnd`) — no UTF-8 decode
/// or `Op::Trap` — so it lowers four-way (tree-walk == cranelift ==
/// llvm-native == llvm-wasm).
pub(super) fn split_string() -> StdlibFunction {
    StdlibFunction::new(
        "split",
        vec![IrType::String, IrType::String],
        IrType::ListString,
        split_string_body,
    )
}

#[allow(clippy::vec_init_then_push)]
fn split_string_body() -> Vec<TaggedOp> {
    const S_LEN: u32 = 0;
    const F_LEN: u32 = 1;
    const N: u32 = 2; // segment count (= match count + 1)
    const NEW_BASE: u32 = 3; // result header base (4-aligned)
    const SLOT: u32 = 4; // next header slot offset (NEW_BASE + 4 + 4*p)
    const SEG_START: u32 = 5; // current segment start in s payload
    const I: u32 = 6; // outer scan cursor into s payload
    const MATCH: u32 = 7; // 1 if `sep` matches at i
    const J: u32 = 8; // inner compare cursor (match_from_at_i scratch)
    const SINK: u32 = 9; // value sink for stack-balancing If/helpers
    const SB: u32 = 10; // scratch byte (s side)
    const BYTE: u32 = 11; // scratch byte (sep side)

    let mut body: Vec<TaggedOp> = Vec::new();

    // s_len = i32.load(s, 0); f_len = i32.load(sep, 0).
    body.push(tt(Op::LocalGet(0)));
    body.push(tt(Op::LoadI32AtAbsolute { offset: 0 }));
    body.push(tt(Op::LetSet {
        idx: S_LEN,
        ty: IrType::I32,
    }));
    body.push(tt(Op::LocalGet(1)));
    body.push(tt(Op::LoadI32AtAbsolute { offset: 0 }));
    body.push(tt(Op::LetSet {
        idx: F_LEN,
        ty: IrType::I32,
    }));

    // ===== Pass 1: N = match_count + 1 =====
    body.push(tt(Op::ConstI32(1)));
    body.push(tt(Op::LetSet {
        idx: N,
        ty: IrType::I32,
    }));
    body.push(tt(Op::ConstI32(0)));
    body.push(tt(Op::LetSet {
        idx: I,
        ty: IrType::I32,
    }));
    body.push(tt(Op::Block {
        result_ty: None,
        body: vec![tt(Op::Loop {
            result_ty: None,
            body: {
                let mut l = Vec::new();
                // exit when i >= s_len (so the tail where i+f_len > s_len
                // can never false-match — match_from_at_i returns 0 there).
                l.push(tt(Op::LetGet {
                    idx: I,
                    ty: IrType::I32,
                }));
                l.push(tt(Op::LetGet {
                    idx: S_LEN,
                    ty: IrType::I32,
                }));
                l.push(tt(Op::Ge(IrType::I32)));
                l.push(tt(Op::BrIf { label_depth: 1 }));
                l.extend(match_from_at_i(S_LEN, F_LEN, I, MATCH, J, SINK, SB, BYTE));
                // if match { N += 1; i += f_len } else { i += 1 }
                l.push(tt(Op::LetGet {
                    idx: MATCH,
                    ty: IrType::I32,
                }));
                l.push(tt(Op::If {
                    result_ty: IrType::I32,
                    then_body: vec![
                        tt(Op::LetGet {
                            idx: N,
                            ty: IrType::I32,
                        }),
                        tt(Op::ConstI32(1)),
                        tt(Op::Add(IrType::I32)),
                        tt(Op::LetSet {
                            idx: N,
                            ty: IrType::I32,
                        }),
                        tt(Op::LetGet {
                            idx: I,
                            ty: IrType::I32,
                        }),
                        tt(Op::LetGet {
                            idx: F_LEN,
                            ty: IrType::I32,
                        }),
                        tt(Op::Add(IrType::I32)),
                        tt(Op::LetSet {
                            idx: I,
                            ty: IrType::I32,
                        }),
                        tt(Op::ConstI32(0)),
                    ],
                    else_body: vec![
                        tt(Op::LetGet {
                            idx: I,
                            ty: IrType::I32,
                        }),
                        tt(Op::ConstI32(1)),
                        tt(Op::Add(IrType::I32)),
                        tt(Op::LetSet {
                            idx: I,
                            ty: IrType::I32,
                        }),
                        tt(Op::ConstI32(0)),
                    ],
                }));
                l.push(tt(Op::LetSet {
                    idx: SINK,
                    ty: IrType::I32,
                }));
                l.push(tt(Op::Br { label_depth: 0 }));
                l
            },
        })],
    }));

    // ===== Allocate result header [count][off_0]…[off_{N-1}] =====
    // record_size = 4 + 4*N + 4 (trailing +4 is align slop so the
    // 4-aligned header fits regardless of the raw scratch base — mirrors
    // `list_map_to_string_body`).
    body.push(tt(Op::ConstI32(8)));
    body.push(tt(Op::LetGet {
        idx: N,
        ty: IrType::I32,
    }));
    body.push(tt(Op::ConstI32(4)));
    body.push(tt(Op::Mul(IrType::I32)));
    body.push(tt(Op::Add(IrType::I32)));
    body.push(tt(Op::AllocScratchDyn));
    // new_base = (raw_base + 3) & -4
    body.push(tt(Op::ConstI32(3)));
    body.push(tt(Op::Add(IrType::I32)));
    body.push(tt(Op::ConstI32(-4)));
    body.push(tt(Op::BitAnd(IrType::I32)));
    body.push(tt(Op::LetSet {
        idx: NEW_BASE,
        ty: IrType::I32,
    }));
    // store header count: i32.store(new_base, N)
    body.push(tt(Op::LetGet {
        idx: NEW_BASE,
        ty: IrType::I32,
    }));
    body.push(tt(Op::LetGet {
        idx: N,
        ty: IrType::I32,
    }));
    body.push(tt(Op::StoreI32AtAbsolute { offset: 0 }));

    // slot = new_base + 4 (first off_i slot); seg_start = 0; i = 0.
    body.push(tt(Op::LetGet {
        idx: NEW_BASE,
        ty: IrType::I32,
    }));
    body.push(tt(Op::ConstI32(4)));
    body.push(tt(Op::Add(IrType::I32)));
    body.push(tt(Op::LetSet {
        idx: SLOT,
        ty: IrType::I32,
    }));
    body.push(tt(Op::ConstI32(0)));
    body.push(tt(Op::LetSet {
        idx: SEG_START,
        ty: IrType::I32,
    }));
    body.push(tt(Op::ConstI32(0)));
    body.push(tt(Op::LetSet {
        idx: I,
        ty: IrType::I32,
    }));

    // ===== Pass 2: emit a segment record at every cut =====
    body.push(tt(Op::Block {
        result_ty: None,
        body: vec![tt(Op::Loop {
            result_ty: None,
            body: {
                let mut l = Vec::new();
                l.push(tt(Op::LetGet {
                    idx: I,
                    ty: IrType::I32,
                }));
                l.push(tt(Op::LetGet {
                    idx: S_LEN,
                    ty: IrType::I32,
                }));
                l.push(tt(Op::Ge(IrType::I32)));
                l.push(tt(Op::BrIf { label_depth: 1 }));
                l.extend(match_from_at_i(S_LEN, F_LEN, I, MATCH, J, SINK, SB, BYTE));
                l.push(tt(Op::LetGet {
                    idx: MATCH,
                    ty: IrType::I32,
                }));
                l.push(tt(Op::If {
                    result_ty: IrType::I32,
                    then_body: {
                        // emit segment s[seg_start .. i] -> slot; advance.
                        let mut t = emit_segment_record(SEG_START, I, SLOT, SB, J, SINK);
                        // seg_start = i + f_len; i += f_len.
                        t.push(tt(Op::LetGet {
                            idx: I,
                            ty: IrType::I32,
                        }));
                        t.push(tt(Op::LetGet {
                            idx: F_LEN,
                            ty: IrType::I32,
                        }));
                        t.push(tt(Op::Add(IrType::I32)));
                        t.push(tt(Op::LetSet {
                            idx: SEG_START,
                            ty: IrType::I32,
                        }));
                        t.push(tt(Op::LetGet {
                            idx: I,
                            ty: IrType::I32,
                        }));
                        t.push(tt(Op::LetGet {
                            idx: F_LEN,
                            ty: IrType::I32,
                        }));
                        t.push(tt(Op::Add(IrType::I32)));
                        t.push(tt(Op::LetSet {
                            idx: I,
                            ty: IrType::I32,
                        }));
                        t.push(tt(Op::ConstI32(0)));
                        t
                    },
                    else_body: vec![
                        tt(Op::LetGet {
                            idx: I,
                            ty: IrType::I32,
                        }),
                        tt(Op::ConstI32(1)),
                        tt(Op::Add(IrType::I32)),
                        tt(Op::LetSet {
                            idx: I,
                            ty: IrType::I32,
                        }),
                        tt(Op::ConstI32(0)),
                    ],
                }));
                l.push(tt(Op::LetSet {
                    idx: SINK,
                    ty: IrType::I32,
                }));
                l.push(tt(Op::Br { label_depth: 0 }));
                l
            },
        })],
    }));
    // Final (or only) segment: s[seg_start .. s_len] -> last slot.
    body.extend(emit_segment_record(SEG_START, S_LEN, SLOT, SB, J, SINK));

    // return new_base
    body.push(tt(Op::LetGet {
        idx: NEW_BASE,
        ty: IrType::I32,
    }));
    body.push(tt(Op::Return));
    body
}

/// Emit a String record for `s[seg_start .. seg_end]` (param 0 is `s`),
/// store its arena offset into the header slot at local `slot`, and
/// advance `slot` by 4. Stack-neutral. Uses `len`/`rec`/`sink` scratch
/// locals (the caller passes `sb`/`j`/`sink`, all dead across this call
/// site, reused here as `len`/`rec`/`sink`).
fn emit_segment_record(
    seg_start: u32,
    seg_end: u32,
    slot: u32,
    len: u32,
    rec: u32,
    sink: u32,
) -> Vec<TaggedOp> {
    vec![
        // len = seg_end - seg_start
        tt(Op::LetGet {
            idx: seg_end,
            ty: IrType::I32,
        }),
        tt(Op::LetGet {
            idx: seg_start,
            ty: IrType::I32,
        }),
        tt(Op::Sub(IrType::I32)),
        tt(Op::LetSet {
            idx: len,
            ty: IrType::I32,
        }),
        // rec = AllocScratchDyn(4 + len); store len header.
        tt(Op::ConstI32(4)),
        tt(Op::LetGet {
            idx: len,
            ty: IrType::I32,
        }),
        tt(Op::Add(IrType::I32)),
        tt(Op::AllocScratchDyn),
        tt(Op::LetSet {
            idx: rec,
            ty: IrType::I32,
        }),
        tt(Op::LetGet {
            idx: rec,
            ty: IrType::I32,
        }),
        tt(Op::LetGet {
            idx: len,
            ty: IrType::I32,
        }),
        tt(Op::StoreI32AtAbsolute { offset: 0 }),
        // memcpy(rec + 4, s + 4 + seg_start, len)
        tt(Op::LetGet {
            idx: rec,
            ty: IrType::I32,
        }),
        tt(Op::ConstI32(4)),
        tt(Op::Add(IrType::I32)),
        tt(Op::LocalGet(0)),
        tt(Op::ConstI32(4)),
        tt(Op::Add(IrType::I32)),
        tt(Op::LetGet {
            idx: seg_start,
            ty: IrType::I32,
        }),
        tt(Op::Add(IrType::I32)),
        tt(Op::LetGet {
            idx: len,
            ty: IrType::I32,
        }),
        tt(Op::MemcpyAtAbsolute),
        // i32.store(slot, rec) — header slot holds the segment's offset.
        tt(Op::LetGet {
            idx: slot,
            ty: IrType::I32,
        }),
        tt(Op::LetGet {
            idx: rec,
            ty: IrType::I32,
        }),
        tt(Op::StoreI32AtAbsolute { offset: 0 }),
        // slot += 4
        tt(Op::LetGet {
            idx: slot,
            ty: IrType::I32,
        }),
        tt(Op::ConstI32(4)),
        tt(Op::Add(IrType::I32)),
        tt(Op::LetSet {
            idx: slot,
            ty: IrType::I32,
        }),
        // The caller treats this helper as stack-neutral; every op above
        // is a statement, so we leave the stack untouched (sink unused —
        // kept in the signature to mirror the other pass-2 helpers).
        tt(Op::LetGet {
            idx: sink,
            ty: IrType::I32,
        }),
        tt(Op::LetSet {
            idx: sink,
            ty: IrType::I32,
        }),
    ]
}
