//! Wave R9 body builders for Bool-returning `is_*` validator stdlib
//! functions that previously capped on the compiled backends.
//!
//! Bodies in this file:
//!   * `is_uuid(s: String) -> Bool` — RFC 4122 canonical text form,
//!     case-insensitive. Byte-exact with the tree-walk oracle
//!     (`relon-evaluator/src/stdlib.rs::is_uuid_str`): length must be
//!     exactly 36, positions 8/13/18/23 must be `-` (0x2D), every other
//!     position must be an ASCII hex digit (`0-9` / `A-F` / `a-f`).
//!
//! The body is purely byte-level (record-header read + byte loads +
//! integer compares + `BitAnd` / `Add` / `Sub` / `Mul`) — no UTF-8
//! decode, no `Op::Trap`, no integer division/remainder — so it lowers
//! four-way (tree-walk == cranelift == llvm-native == llvm-wasm).
//!
//! Sibling validators stay capped:
//!   * `is_email` / `is_uri` iterate `s.chars()` (UTF-8 decode seam that
//!     segfaults on LLVM-native / wasm — same seam that keeps `upper` /
//!     `title` / `nfd` at tree-walk + cranelift only).
//!   * `is_ipv4` / `is_ipv6` route through `core::net::Ipv*Addr::parse`,
//!     which has no wasm-portable body.
//!   * `is_iso_date` needs integer division / remainder (the leap-year
//!     `year % 4 / % 100 / % 400` test); the IR exposes no `DivS` /
//!     `RemS` op, so a byte-exact four-way body is not constructible
//!     without new ops + codegen (out of this wave's scope).
//!
//! Every body uses only existing `Op`s; the entry is appended at the
//! tail of [`super::registry::builtin_stdlib`] so no position-pinned
//! index moves and the cranelift/llvm byte output of existing
//! constructs is unchanged (GENERATOR_VERSION stays put).

use crate::ir::{IrType, Op, TaggedOp};

use super::defs::tt;
use super::signatures::StdlibFunction;

/// `is_uuid(s: String) -> Bool`.
///
/// Mirrors `relon-evaluator/src/stdlib.rs::is_uuid_str`:
/// ```text
/// if s.len() != 36 { return false }
/// for (i, b) in s.bytes().enumerate() {
///     match i {
///         8 | 13 | 18 | 23 => if b != b'-' { return false },
///         _                => if !b.is_ascii_hexdigit() { return false },
///     }
/// }
/// true
/// ```
///
/// The loop is structurally branch-free (it accumulates the per-byte
/// predicate into `acc` via `BitAnd` rather than early-returning) so no
/// `Br` ever crosses an `If` boundary — keeping LLVM's branch validator
/// happy, the same discipline `ends_with` uses.
pub(super) fn is_uuid_string() -> StdlibFunction {
    StdlibFunction::new(
        "is_uuid",
        vec![IrType::String],
        IrType::Bool,
        is_uuid_string_body,
    )
}

/// Push the per-byte hex-digit predicate (`1` if `b` is an ASCII hex
/// digit, else `0`) onto the operand stack. Assumes the byte value is
/// already loaded into local `byte`. Branch-free: the three disjoint
/// ASCII ranges (`0-9`, `A-F`, `a-f`) each contribute a `0`/`1` and are
/// summed (the ranges never overlap, so the sum stays in `{0, 1}`).
fn push_is_hexdigit(byte: u32) -> Vec<TaggedOp> {
    // (b >= '0') & (b <= '9')
    let mut ops = range_pred(byte, b'0' as i32, b'9' as i32);
    // + (b >= 'A') & (b <= 'F')
    ops.extend(range_pred(byte, b'A' as i32, b'F' as i32));
    ops.push(tt(Op::Add(IrType::I32)));
    // + (b >= 'a') & (b <= 'f')
    ops.extend(range_pred(byte, b'a' as i32, b'f' as i32));
    ops.push(tt(Op::Add(IrType::I32)));
    ops
}

/// Push `(byte >= lo) & (byte <= hi)` as a `0`/`1` i32.
fn range_pred(byte: u32, lo: i32, hi: i32) -> Vec<TaggedOp> {
    vec![
        // byte >= lo
        tt(Op::LetGet {
            idx: byte,
            ty: IrType::I32,
        }),
        tt(Op::ConstI32(lo)),
        tt(Op::Ge(IrType::I32)),
        // byte <= hi
        tt(Op::LetGet {
            idx: byte,
            ty: IrType::I32,
        }),
        tt(Op::ConstI32(hi)),
        tt(Op::Le(IrType::I32)),
        // &
        tt(Op::BitAnd(IrType::I32)),
    ]
}

#[allow(clippy::vec_init_then_push)]
fn is_uuid_string_body() -> Vec<TaggedOp> {
    const S_LEN: u32 = 0;
    const I: u32 = 1;
    const ACC: u32 = 2;
    const BYTE: u32 = 3;
    const IS_DASH_POS: u32 = 4;

    let mut body: Vec<TaggedOp> = Vec::new();
    // s_len = load_i32(s, 0)
    body.push(tt(Op::LocalGet(0)));
    body.push(tt(Op::LoadI32AtAbsolute { offset: 0 }));
    body.push(tt(Op::LetSet {
        idx: S_LEN,
        ty: IrType::I32,
    }));
    // if s_len != 36 { false } else { scan } -> Bool
    body.push(tt(Op::LetGet {
        idx: S_LEN,
        ty: IrType::I32,
    }));
    body.push(tt(Op::ConstI32(36)));
    body.push(tt(Op::Ne(IrType::I32)));

    // ---- scan body (else branch) ----
    let mut scan: Vec<TaggedOp> = Vec::new();
    // acc = 1
    scan.push(tt(Op::ConstI32(1)));
    scan.push(tt(Op::LetSet {
        idx: ACC,
        ty: IrType::I32,
    }));
    // i = 0
    scan.push(tt(Op::ConstI32(0)));
    scan.push(tt(Op::LetSet {
        idx: I,
        ty: IrType::I32,
    }));

    // loop body
    let mut loop_body: Vec<TaggedOp> = Vec::new();
    // exit when i >= 36
    loop_body.push(tt(Op::LetGet {
        idx: I,
        ty: IrType::I32,
    }));
    loop_body.push(tt(Op::ConstI32(36)));
    loop_body.push(tt(Op::Ge(IrType::I32)));
    loop_body.push(tt(Op::BrIf { label_depth: 1 }));

    // byte = i8u(s + 4 + i)
    loop_body.push(tt(Op::LocalGet(0)));
    loop_body.push(tt(Op::ConstI32(4)));
    loop_body.push(tt(Op::Add(IrType::I32)));
    loop_body.push(tt(Op::LetGet {
        idx: I,
        ty: IrType::I32,
    }));
    loop_body.push(tt(Op::Add(IrType::I32)));
    loop_body.push(tt(Op::LoadI8UAtAbsolute { offset: 0 }));
    loop_body.push(tt(Op::LetSet {
        idx: BYTE,
        ty: IrType::I32,
    }));

    // is_dash_pos = (i==8) + (i==13) + (i==18) + (i==23)
    // The four positions are distinct, so the sum stays in {0, 1}.
    let eq_pos = |p: i32| -> Vec<TaggedOp> {
        vec![
            tt(Op::LetGet {
                idx: I,
                ty: IrType::I32,
            }),
            tt(Op::ConstI32(p)),
            tt(Op::Eq(IrType::I32)),
        ]
    };
    loop_body.extend(eq_pos(8));
    loop_body.extend(eq_pos(13));
    loop_body.push(tt(Op::Add(IrType::I32)));
    loop_body.extend(eq_pos(18));
    loop_body.push(tt(Op::Add(IrType::I32)));
    loop_body.extend(eq_pos(23));
    loop_body.push(tt(Op::Add(IrType::I32)));
    loop_body.push(tt(Op::LetSet {
        idx: IS_DASH_POS,
        ty: IrType::I32,
    }));

    // dash_ok = (byte == '-')
    let dash_ok = vec![
        tt(Op::LetGet {
            idx: BYTE,
            ty: IrType::I32,
        }),
        tt(Op::ConstI32(b'-' as i32)),
        tt(Op::Eq(IrType::I32)),
    ];
    // hex_ok = is_hexdigit(byte)
    let hex_ok = push_is_hexdigit(BYTE);

    // ok = is_dash_pos*dash_ok + (1 - is_dash_pos)*hex_ok
    //   is_dash_pos ∈ {0,1}, so this selects the right predicate.
    // term_a = is_dash_pos * dash_ok
    loop_body.push(tt(Op::LetGet {
        idx: IS_DASH_POS,
        ty: IrType::I32,
    }));
    loop_body.extend(dash_ok);
    loop_body.push(tt(Op::Mul(IrType::I32)));
    // term_b = (1 - is_dash_pos) * hex_ok
    loop_body.push(tt(Op::ConstI32(1)));
    loop_body.push(tt(Op::LetGet {
        idx: IS_DASH_POS,
        ty: IrType::I32,
    }));
    loop_body.push(tt(Op::Sub(IrType::I32)));
    loop_body.extend(hex_ok);
    loop_body.push(tt(Op::Mul(IrType::I32)));
    // ok = term_a + term_b
    loop_body.push(tt(Op::Add(IrType::I32)));
    // acc = acc & ok
    loop_body.push(tt(Op::LetGet {
        idx: ACC,
        ty: IrType::I32,
    }));
    loop_body.push(tt(Op::BitAnd(IrType::I32)));
    loop_body.push(tt(Op::LetSet {
        idx: ACC,
        ty: IrType::I32,
    }));

    // early-out: if acc == 0, exit the scan (Br to enclosing Block).
    loop_body.push(tt(Op::LetGet {
        idx: ACC,
        ty: IrType::I32,
    }));
    loop_body.push(tt(Op::ConstI32(0)));
    loop_body.push(tt(Op::Eq(IrType::I32)));
    loop_body.push(tt(Op::BrIf { label_depth: 1 }));

    // i = i + 1
    loop_body.push(tt(Op::LetGet {
        idx: I,
        ty: IrType::I32,
    }));
    loop_body.push(tt(Op::ConstI32(1)));
    loop_body.push(tt(Op::Add(IrType::I32)));
    loop_body.push(tt(Op::LetSet {
        idx: I,
        ty: IrType::I32,
    }));
    loop_body.push(tt(Op::Br { label_depth: 0 }));

    scan.push(tt(Op::Block {
        result_ty: None,
        body: vec![tt(Op::Loop {
            result_ty: None,
            body: loop_body,
        })],
    }));
    // result = acc != 0
    scan.push(tt(Op::LetGet {
        idx: ACC,
        ty: IrType::I32,
    }));
    scan.push(tt(Op::ConstI32(0)));
    scan.push(tt(Op::Ne(IrType::I32)));

    body.push(tt(Op::If {
        result_ty: IrType::Bool,
        then_body: vec![tt(Op::ConstBool(false))],
        else_body: scan,
    }));
    body.push(tt(Op::Return));
    body
}
