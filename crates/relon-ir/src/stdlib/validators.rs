//! Wave R9 body builders for Bool-returning `is_*` validator stdlib
//! functions that previously capped on the compiled backends.
//!
//! Bodies in this file:
//!   * `is_uuid(s: String) -> Bool` — RFC 4122 canonical text form,
//!     case-insensitive. Byte-exact with the tree-walk oracle
//!     (`relon-evaluator/src/stdlib.rs::is_uuid_str`): length must be
//!     exactly 36, positions 8/13/18/23 must be `-` (0x2D), every other
//!     position must be an ASCII hex digit (`0-9` / `A-F` / `a-f`).
//!   * `multiple_of(n: Int, d: Int) -> Bool` — Int form of the JSON
//!     Schema `multipleOf` predicate. Byte-exact with the tree-walk
//!     `MultipleOf` oracle for the `(Int, Int)` arm: `d == 0` returns
//!     `false`, else `n % d == 0`. The `d == 0` guard lives in an
//!     `Op::If` so the `Op::Mod(I64)` never executes on a zero divisor
//!     (which would trap on cranelift / wasm). The Float arms
//!     (`(Float, Float)` / `(Int, Float)` / `(Float, Int)`) stay capped:
//!     `Op::Mod(F64)` has no native cranelift / wasm remainder, and the
//!     oracle's `fract().abs() < 1e-9` tolerance has no four-way body.
//!   * `in_range(n, lo, hi) -> Bool` — JSON Schema `minimum` / `maximum`
//!     inclusive bound check. The tree-walk oracle widens every argument
//!     to `f64` (`to_f64_val`) and returns `n >= lo && n <= hi`, so the
//!     body is fully `F64` (`Ge` / `Le` / `BitAnd`); the lowering peephole
//!     widens any `Int` argument with `ConvertI64ToF64` first, matching
//!     the oracle's coercion. Four-way clean.
//!   * `size_in_range(recv, lo, hi) -> Bool` /
//!     `dict_size_in_range(recv, lo, hi) -> Bool` — JSON Schema
//!     `minItems` / `maxItems` (List) and `minProperties` /
//!     `maxProperties` (Dict). The element / entry count comes from the
//!     `[len: u32 LE]` record-header prefix (`ReadStringLen`) every list
//!     and dict record shares, then `len >= lo && len <= hi` over `I64`.
//!     The List and Dict forms share the op-stream; the lowering peephole
//!     picks the body by the receiver's IR type. The String form stays
//!     capped: the oracle counts Unicode code points (`chars().count()`),
//!     which needs the UTF-8 decode seam the LLVM-native / wasm backends
//!     do not lower (the same seam that keeps `upper` / `title` / `nfd`
//!     at tree-walk + cranelift). Unlock this once the decode seam lands.
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

/// `multiple_of(n: Int, d: Int) -> Bool` — Int arm of JSON Schema
/// `multipleOf`. Mirrors the `(Int, Int)` arm of the tree-walk
/// `MultipleOf` oracle:
/// ```text
/// if d == 0 { false } else { n % d == 0 }
/// ```
/// The `d == 0` test gates an `Op::If` so the `Op::Mod(I64)` (cranelift /
/// wasm `srem`, which traps on a zero divisor) only ever runs on the
/// non-zero arm — there is no division when `d == 0`, exactly as the
/// oracle short-circuits. Float arms stay capped (no four-way
/// `Op::Mod(F64)`).
pub(super) fn multiple_of_int() -> StdlibFunction {
    StdlibFunction::new(
        "multiple_of",
        vec![IrType::I64, IrType::I64],
        IrType::Bool,
        multiple_of_int_body,
    )
}

fn multiple_of_int_body() -> Vec<TaggedOp> {
    // d == 0 ?
    let cond = vec![
        tt(Op::LocalGet(1)),
        tt(Op::ConstI64(0)),
        tt(Op::Eq(IrType::I64)),
    ];
    // else branch: (n % d) == 0
    let else_body = vec![
        tt(Op::LocalGet(0)),
        tt(Op::LocalGet(1)),
        tt(Op::Mod(IrType::I64)),
        tt(Op::ConstI64(0)),
        tt(Op::Eq(IrType::I64)),
    ];
    let mut body = cond;
    body.push(tt(Op::If {
        result_ty: IrType::Bool,
        then_body: vec![tt(Op::ConstBool(false))],
        else_body,
    }));
    body.push(tt(Op::Return));
    body
}

/// `in_range(n, lo, hi) -> Bool` — JSON Schema `minimum` / `maximum`
/// inclusive bound check. The tree-walk oracle widens every argument to
/// `f64` (`to_f64_val`) before comparing, so the body is all-`F64`:
/// ```text
/// (n >= lo) & (n <= hi)
/// ```
/// `Op::Ge(F64)` / `Op::Le(F64)` push `0`/`1` i32s; `Op::BitAnd(I32)`
/// folds them into the Bool result. The lowering peephole widens any
/// `Int` argument with `Op::ConvertI64ToF64` before the call, matching
/// the oracle's coercion, so the body itself only ever sees `F64`
/// operands. Four-way clean (no UTF-8 decode, no trap, no remainder).
pub(super) fn in_range_float() -> StdlibFunction {
    StdlibFunction::new(
        "in_range",
        vec![IrType::F64, IrType::F64, IrType::F64],
        IrType::Bool,
        in_range_float_body,
    )
}

fn in_range_float_body() -> Vec<TaggedOp> {
    vec![
        // n >= lo
        tt(Op::LocalGet(0)),
        tt(Op::LocalGet(1)),
        tt(Op::Ge(IrType::F64)),
        // n <= hi
        tt(Op::LocalGet(0)),
        tt(Op::LocalGet(2)),
        tt(Op::Le(IrType::F64)),
        // &
        tt(Op::BitAnd(IrType::I32)),
        tt(Op::Return),
    ]
}

/// Shared body for `size_in_range` (List receiver) and
/// `dict_size_in_range` (Dict receiver). Every list and dict record
/// carries the same `[len: u32 LE]` count prefix at offset 0, so
/// `ReadStringLen` recovers the element / entry count for both. The
/// bounds are inclusive `Int`s:
/// ```text
/// (len >= lo) & (len <= hi)
/// ```
/// where `len` is read from the receiver and `lo` / `hi` are the two
/// `I64` parameters. `Op::Ge(I64)` / `Op::Le(I64)` push `0`/`1` i32s and
/// `Op::BitAnd(I32)` folds them into the Bool result. The String form of
/// the oracle (`chars().count()`) stays capped — it needs the UTF-8
/// decode seam the LLVM-native / wasm backends do not lower.
fn size_in_range_record_header_body() -> Vec<TaggedOp> {
    vec![
        // len = read_u32_le(recv, 0) widened to i64
        tt(Op::LocalGet(0)),
        tt(Op::ReadStringLen),
        // len >= lo
        tt(Op::LocalGet(1)),
        tt(Op::Ge(IrType::I64)),
        // len <= hi  (re-read len)
        tt(Op::LocalGet(0)),
        tt(Op::ReadStringLen),
        tt(Op::LocalGet(2)),
        tt(Op::Le(IrType::I64)),
        // &
        tt(Op::BitAnd(IrType::I32)),
        tt(Op::Return),
    ]
}

/// `size_in_range(xs: List<Int>, lo: Int, hi: Int) -> Bool` — JSON
/// Schema `minItems` / `maxItems`. Registered with a `ListInt` receiver
/// param, but the body only reads the shared `[len: u32 LE]` header, so
/// the lowering peephole routes every `List<_>` receiver here (all list
/// pointers share the same i32 wasm slot + count-prefix layout).
pub(super) fn size_in_range_list() -> StdlibFunction {
    StdlibFunction::new(
        "size_in_range",
        vec![IrType::ListInt, IrType::I64, IrType::I64],
        IrType::Bool,
        size_in_range_record_header_body,
    )
}

/// `dict_size_in_range(d: Dict, lo: Int, hi: Int) -> Bool` — JSON Schema
/// `minProperties` / `maxProperties`. Shares the record-header body with
/// [`size_in_range_list`]; only the receiver param tag differs, which
/// drives the peephole dispatch (a Dict receiver routes here).
pub(super) fn dict_size_in_range() -> StdlibFunction {
    StdlibFunction::new(
        "dict_size_in_range",
        vec![IrType::Dict, IrType::I64, IrType::I64],
        IrType::Bool,
        size_in_range_record_header_body,
    )
}

// ---------------------------------------------------------------------
// `is_email` / `is_uri` — ASCII-structured string validators.
// ---------------------------------------------------------------------
//
// Both oracles (`is_email_str` / `is_uri_str`) accept a codepoint only
// when it is ASCII (`is_ascii_alphanumeric` / a fixed ASCII punctuation
// set / `is_ascii_alphabetic`); every non-ASCII codepoint fails the
// `.chars().all(...)` predicate. A multi-byte UTF-8 sequence is made
// entirely of bytes `>= 0x80`, none of which satisfy any ASCII class, so
// a byte-level scan reaches the same accept/reject decision as the
// codepoint-level oracle WITHOUT decoding UTF-8: the only structural
// bytes (`@`, `.`, `-`, `:`) are ASCII and never appear inside a
// multi-byte sequence, and all length checks in the oracle are byte
// lengths. The bodies are therefore purely byte-level (record-header read
// + byte loads + integer compares + `BitAnd` / `Add` / `Sub` / `Mul`) —
// no UTF-8 decode, no `Op::Trap` — so they lower four-way (tree-walk ==
// cranelift == llvm-native == llvm-wasm), the same discipline `is_uuid`
// uses.

/// Push `(byte_local == code)` as a `0`/`1` i32, reading the byte from
/// `byte_local`.
fn eq_byte(byte: u32, code: i32) -> Vec<TaggedOp> {
    vec![
        tt(Op::LetGet {
            idx: byte,
            ty: IrType::I32,
        }),
        tt(Op::ConstI32(code)),
        tt(Op::Eq(IrType::I32)),
    ]
}

/// Push the per-byte ASCII-alphanumeric predicate (`1` if `byte_local` is
/// `0-9` / `A-Z` / `a-z`, else `0`). The three ranges are disjoint, so
/// summing the three `range_pred`s keeps the result in `{0, 1}`.
fn push_is_alnum(byte: u32) -> Vec<TaggedOp> {
    let mut ops = range_pred(byte, b'0' as i32, b'9' as i32);
    ops.extend(range_pred(byte, b'A' as i32, b'Z' as i32));
    ops.push(tt(Op::Add(IrType::I32)));
    ops.extend(range_pred(byte, b'a' as i32, b'z' as i32));
    ops.push(tt(Op::Add(IrType::I32)));
    ops
}

/// Push the per-byte ASCII-alphabetic predicate (`1` if `byte_local` is
/// `A-Z` / `a-z`, else `0`).
fn push_is_alpha(byte: u32) -> Vec<TaggedOp> {
    let mut ops = range_pred(byte, b'A' as i32, b'Z' as i32);
    ops.extend(range_pred(byte, b'a' as i32, b'z' as i32));
    ops.push(tt(Op::Add(IrType::I32)));
    ops
}

/// `is_email(s: String) -> Bool`.
///
/// Mirrors `relon-evaluator/src/stdlib.rs::is_email_str`:
/// ```text
/// let at = s.find('@')?;             // first '@'
/// let local = &s[..at];
/// let domain = &s[at + 1..];
/// if local.is_empty()  || local.len()  > 64  { return false }
/// if domain.is_empty() || domain.len() > 255 { return false }
/// if !local.chars().all(|c| c.is_ascii_alphanumeric()
///                           || ".!#$%&'*+/=?^_`{|}~-".contains(c)) { return false }
/// if local.starts_with('.') || local.ends_with('.')
///                           || local.contains("..") { return false }
/// let labels: Vec<&str> = domain.split('.').collect();
/// if labels.len() < 2 { return false }
/// for label in &labels {
///     if label.is_empty() || label.len() > 63 { return false }
///     if !label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') { return false }
///     if label.starts_with('-') || label.ends_with('-') { return false }
/// }
/// true
/// ```
/// Every accepted codepoint is ASCII and every structural byte (`@`,
/// `.`, `-`) is ASCII, so the body scans bytes directly: a non-ASCII
/// byte fails the local / label char class exactly as the codepoint-level
/// `.chars().all(...)` rejects a non-ASCII codepoint. `local.len()` /
/// `domain.len()` / `label.len()` are byte lengths. The label structure
/// is recovered with a single forward scan over the domain that closes
/// each label on a `.` separator (and once more at the end). Branch-free
/// accumulation into `acc` / `label_acc` (no `Br` crosses an `If`).
pub(super) fn is_email_string() -> StdlibFunction {
    StdlibFunction::new(
        "is_email",
        vec![IrType::String],
        IrType::Bool,
        is_email_string_body,
    )
}

/// ASCII codes of the local-part punctuation set
/// `".!#$%&'*+/=?^_`{|}~-"`. Disjoint from the alphanumeric ranges and
/// from each other, so the per-byte predicate sums them with `Add` and
/// stays in `{0, 1}` (the same disjoint-`Add` discipline `is_uuid` uses).
const EMAIL_LOCAL_PUNCT: &[i32] = &[
    b'.' as i32,
    b'!' as i32,
    b'#' as i32,
    b'$' as i32,
    b'%' as i32,
    b'&' as i32,
    b'\'' as i32,
    b'*' as i32,
    b'+' as i32,
    b'/' as i32,
    b'=' as i32,
    b'?' as i32,
    b'^' as i32,
    b'_' as i32,
    b'`' as i32,
    b'{' as i32,
    b'|' as i32,
    b'}' as i32,
    b'~' as i32,
    b'-' as i32,
];

/// Push `label_ok = (len >= 1) & (len <= 63) & !start_dash & !end_dash`
/// for the domain label whose state is held in the three given locals.
fn push_label_ok(len: u32, start_dash: u32, end_dash: u32) -> Vec<TaggedOp> {
    vec![
        // len >= 1
        tt(Op::LetGet {
            idx: len,
            ty: IrType::I32,
        }),
        tt(Op::ConstI32(1)),
        tt(Op::Ge(IrType::I32)),
        // & len <= 63
        tt(Op::LetGet {
            idx: len,
            ty: IrType::I32,
        }),
        tt(Op::ConstI32(63)),
        tt(Op::Le(IrType::I32)),
        tt(Op::BitAnd(IrType::I32)),
        // & (1 - start_dash)
        tt(Op::ConstI32(1)),
        tt(Op::LetGet {
            idx: start_dash,
            ty: IrType::I32,
        }),
        tt(Op::Sub(IrType::I32)),
        tt(Op::BitAnd(IrType::I32)),
        // & (1 - end_dash)
        tt(Op::ConstI32(1)),
        tt(Op::LetGet {
            idx: end_dash,
            ty: IrType::I32,
        }),
        tt(Op::Sub(IrType::I32)),
        tt(Op::BitAnd(IrType::I32)),
    ]
}

#[allow(clippy::vec_init_then_push, clippy::too_many_lines)]
fn is_email_string_body() -> Vec<TaggedOp> {
    const S_LEN: u32 = 0;
    const I: u32 = 1;
    const ACC: u32 = 2;
    const BYTE: u32 = 3;
    const AT: u32 = 4; // index of first '@'
    const HAS_AT: u32 = 5;
    const LOCAL_LEN: u32 = 6;
    const DOMAIN_LEN: u32 = 7;
    const PREV_DOT: u32 = 8; // local scan: previous byte was '.'
    const DLABEL_LEN: u32 = 9;
    const DSTART_DASH: u32 = 10;
    const DEND_DASH: u32 = 11;
    const DOT_COUNT: u32 = 12;
    const LABEL_ACC: u32 = 13;
    const OK: u32 = 14; // scratch per-byte predicate
    const IS_DOT: u32 = 15; // domain scan: current byte is '.'

    let load_byte = |idx_local: u32| {
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
    // Push the local-part char-class predicate for the byte in `BYTE`:
    // alnum(byte) + sum(byte == punct) over the 20 disjoint punctuation
    // codes — stays in {0, 1}.
    let push_local_charclass = || {
        let mut ops = push_is_alnum(BYTE);
        for &code in EMAIL_LOCAL_PUNCT {
            ops.extend(eq_byte(BYTE, code));
            ops.push(tt(Op::Add(IrType::I32)));
        }
        ops
    };

    let mut body: Vec<TaggedOp> = Vec::new();
    // s_len = load_i32(s, 0)
    body.push(tt(Op::LocalGet(0)));
    body.push(tt(Op::LoadI32AtAbsolute { offset: 0 }));
    body.push(tt(Op::LetSet {
        idx: S_LEN,
        ty: IrType::I32,
    }));

    // ----- find first '@' -----
    body.push(tt(Op::ConstI32(0)));
    body.push(tt(Op::LetSet {
        idx: HAS_AT,
        ty: IrType::I32,
    }));
    body.push(tt(Op::ConstI32(0)));
    body.push(tt(Op::LetSet {
        idx: AT,
        ty: IrType::I32,
    }));
    body.push(tt(Op::ConstI32(0)));
    body.push(tt(Op::LetSet {
        idx: I,
        ty: IrType::I32,
    }));
    let mut find_loop: Vec<TaggedOp> = Vec::new();
    find_loop.push(tt(Op::LetGet {
        idx: I,
        ty: IrType::I32,
    }));
    find_loop.push(tt(Op::LetGet {
        idx: S_LEN,
        ty: IrType::I32,
    }));
    find_loop.push(tt(Op::Ge(IrType::I32)));
    find_loop.push(tt(Op::BrIf { label_depth: 1 }));
    find_loop.push(tt(Op::LetGet {
        idx: HAS_AT,
        ty: IrType::I32,
    }));
    find_loop.push(tt(Op::BrIf { label_depth: 1 }));
    find_loop.extend(load_byte(I));
    find_loop.push(tt(Op::LetSet {
        idx: BYTE,
        ty: IrType::I32,
    }));
    find_loop.push(tt(Op::LetGet {
        idx: BYTE,
        ty: IrType::I32,
    }));
    find_loop.push(tt(Op::ConstI32(b'@' as i32)));
    find_loop.push(tt(Op::Eq(IrType::I32)));
    find_loop.push(tt(Op::If {
        result_ty: IrType::I32,
        then_body: vec![
            tt(Op::LetGet {
                idx: I,
                ty: IrType::I32,
            }),
            tt(Op::LetSet {
                idx: AT,
                ty: IrType::I32,
            }),
            tt(Op::ConstI32(1)),
            tt(Op::LetSet {
                idx: HAS_AT,
                ty: IrType::I32,
            }),
            tt(Op::ConstI32(0)),
        ],
        else_body: vec![tt(Op::ConstI32(0))],
    }));
    find_loop.push(tt(Op::LetSet {
        idx: BYTE,
        ty: IrType::I32,
    }));
    find_loop.push(tt(Op::LetGet {
        idx: I,
        ty: IrType::I32,
    }));
    find_loop.push(tt(Op::ConstI32(1)));
    find_loop.push(tt(Op::Add(IrType::I32)));
    find_loop.push(tt(Op::LetSet {
        idx: I,
        ty: IrType::I32,
    }));
    find_loop.push(tt(Op::Br { label_depth: 0 }));
    body.push(tt(Op::Block {
        result_ty: None,
        body: vec![tt(Op::Loop {
            result_ty: None,
            body: find_loop,
        })],
    }));

    // acc = has_at
    body.push(tt(Op::LetGet {
        idx: HAS_AT,
        ty: IrType::I32,
    }));
    body.push(tt(Op::LetSet {
        idx: ACC,
        ty: IrType::I32,
    }));
    // local_len = at; domain_len = s_len - at - 1
    body.push(tt(Op::LetGet {
        idx: AT,
        ty: IrType::I32,
    }));
    body.push(tt(Op::LetSet {
        idx: LOCAL_LEN,
        ty: IrType::I32,
    }));
    body.push(tt(Op::LetGet {
        idx: S_LEN,
        ty: IrType::I32,
    }));
    body.push(tt(Op::LetGet {
        idx: AT,
        ty: IrType::I32,
    }));
    body.push(tt(Op::Sub(IrType::I32)));
    body.push(tt(Op::ConstI32(1)));
    body.push(tt(Op::Sub(IrType::I32)));
    body.push(tt(Op::LetSet {
        idx: DOMAIN_LEN,
        ty: IrType::I32,
    }));

    // acc &= (local_len >= 1) & (local_len <= 64)
    body.push(tt(Op::LetGet {
        idx: ACC,
        ty: IrType::I32,
    }));
    body.push(tt(Op::LetGet {
        idx: LOCAL_LEN,
        ty: IrType::I32,
    }));
    body.push(tt(Op::ConstI32(1)));
    body.push(tt(Op::Ge(IrType::I32)));
    body.push(tt(Op::BitAnd(IrType::I32)));
    body.push(tt(Op::LetGet {
        idx: LOCAL_LEN,
        ty: IrType::I32,
    }));
    body.push(tt(Op::ConstI32(64)));
    body.push(tt(Op::Le(IrType::I32)));
    body.push(tt(Op::BitAnd(IrType::I32)));
    // acc &= (domain_len >= 1) & (domain_len <= 255)
    body.push(tt(Op::LetGet {
        idx: DOMAIN_LEN,
        ty: IrType::I32,
    }));
    body.push(tt(Op::ConstI32(1)));
    body.push(tt(Op::Ge(IrType::I32)));
    body.push(tt(Op::BitAnd(IrType::I32)));
    body.push(tt(Op::LetGet {
        idx: DOMAIN_LEN,
        ty: IrType::I32,
    }));
    body.push(tt(Op::ConstI32(255)));
    body.push(tt(Op::Le(IrType::I32)));
    body.push(tt(Op::BitAnd(IrType::I32)));
    body.push(tt(Op::LetSet {
        idx: ACC,
        ty: IrType::I32,
    }));

    // ----- local scan [0, at): char class + dot rules -----
    // prev_dot = 0; i = 0
    body.push(tt(Op::ConstI32(0)));
    body.push(tt(Op::LetSet {
        idx: PREV_DOT,
        ty: IrType::I32,
    }));
    body.push(tt(Op::ConstI32(0)));
    body.push(tt(Op::LetSet {
        idx: I,
        ty: IrType::I32,
    }));
    let mut local_loop: Vec<TaggedOp> = Vec::new();
    // exit when i >= at
    local_loop.push(tt(Op::LetGet {
        idx: I,
        ty: IrType::I32,
    }));
    local_loop.push(tt(Op::LetGet {
        idx: AT,
        ty: IrType::I32,
    }));
    local_loop.push(tt(Op::Ge(IrType::I32)));
    local_loop.push(tt(Op::BrIf { label_depth: 1 }));
    // byte = s[i]
    local_loop.extend(load_byte(I));
    local_loop.push(tt(Op::LetSet {
        idx: BYTE,
        ty: IrType::I32,
    }));
    // acc &= local_charclass(byte)
    local_loop.push(tt(Op::LetGet {
        idx: ACC,
        ty: IrType::I32,
    }));
    local_loop.extend(push_local_charclass());
    local_loop.push(tt(Op::BitAnd(IrType::I32)));
    local_loop.push(tt(Op::LetSet {
        idx: ACC,
        ty: IrType::I32,
    }));
    // is_dot = (byte == '.')
    local_loop.extend(eq_byte(BYTE, b'.' as i32));
    local_loop.push(tt(Op::LetSet {
        idx: OK,
        ty: IrType::I32,
    }));
    // starts_with('.'): if i == 0 { acc &= !is_dot }
    local_loop.push(tt(Op::LetGet {
        idx: I,
        ty: IrType::I32,
    }));
    local_loop.push(tt(Op::ConstI32(0)));
    local_loop.push(tt(Op::Eq(IrType::I32)));
    local_loop.push(tt(Op::If {
        result_ty: IrType::I32,
        then_body: vec![
            tt(Op::LetGet {
                idx: ACC,
                ty: IrType::I32,
            }),
            tt(Op::ConstI32(1)),
            tt(Op::LetGet {
                idx: OK,
                ty: IrType::I32,
            }),
            tt(Op::Sub(IrType::I32)),
            tt(Op::BitAnd(IrType::I32)),
            tt(Op::LetSet {
                idx: ACC,
                ty: IrType::I32,
            }),
            tt(Op::ConstI32(0)),
        ],
        else_body: vec![tt(Op::ConstI32(0))],
    }));
    local_loop.push(tt(Op::LetSet {
        idx: BYTE,
        ty: IrType::I32,
    }));
    // ends_with('.'): if i == at - 1 { acc &= !is_dot }
    local_loop.push(tt(Op::LetGet {
        idx: I,
        ty: IrType::I32,
    }));
    local_loop.push(tt(Op::LetGet {
        idx: AT,
        ty: IrType::I32,
    }));
    local_loop.push(tt(Op::ConstI32(1)));
    local_loop.push(tt(Op::Sub(IrType::I32)));
    local_loop.push(tt(Op::Eq(IrType::I32)));
    local_loop.push(tt(Op::If {
        result_ty: IrType::I32,
        then_body: vec![
            tt(Op::LetGet {
                idx: ACC,
                ty: IrType::I32,
            }),
            tt(Op::ConstI32(1)),
            tt(Op::LetGet {
                idx: OK,
                ty: IrType::I32,
            }),
            tt(Op::Sub(IrType::I32)),
            tt(Op::BitAnd(IrType::I32)),
            tt(Op::LetSet {
                idx: ACC,
                ty: IrType::I32,
            }),
            tt(Op::ConstI32(0)),
        ],
        else_body: vec![tt(Op::ConstI32(0))],
    }));
    local_loop.push(tt(Op::LetSet {
        idx: BYTE,
        ty: IrType::I32,
    }));
    // contains(".."): acc &= !(prev_dot & is_dot)
    local_loop.push(tt(Op::LetGet {
        idx: ACC,
        ty: IrType::I32,
    }));
    local_loop.push(tt(Op::ConstI32(1)));
    local_loop.push(tt(Op::LetGet {
        idx: PREV_DOT,
        ty: IrType::I32,
    }));
    local_loop.push(tt(Op::LetGet {
        idx: OK,
        ty: IrType::I32,
    }));
    local_loop.push(tt(Op::BitAnd(IrType::I32)));
    local_loop.push(tt(Op::Sub(IrType::I32)));
    local_loop.push(tt(Op::BitAnd(IrType::I32)));
    local_loop.push(tt(Op::LetSet {
        idx: ACC,
        ty: IrType::I32,
    }));
    // prev_dot = is_dot
    local_loop.push(tt(Op::LetGet {
        idx: OK,
        ty: IrType::I32,
    }));
    local_loop.push(tt(Op::LetSet {
        idx: PREV_DOT,
        ty: IrType::I32,
    }));
    // i = i + 1
    local_loop.push(tt(Op::LetGet {
        idx: I,
        ty: IrType::I32,
    }));
    local_loop.push(tt(Op::ConstI32(1)));
    local_loop.push(tt(Op::Add(IrType::I32)));
    local_loop.push(tt(Op::LetSet {
        idx: I,
        ty: IrType::I32,
    }));
    local_loop.push(tt(Op::Br { label_depth: 0 }));
    body.push(tt(Op::Block {
        result_ty: None,
        body: vec![tt(Op::Loop {
            result_ty: None,
            body: local_loop,
        })],
    }));

    // ----- domain scan [at + 1, s_len): label structure + char class -----
    // dlabel_len = 0; dstart_dash = 0; dend_dash = 0; dot_count = 0;
    // label_acc = 1; i = at + 1
    for (slot, val) in [
        (DLABEL_LEN, 0),
        (DSTART_DASH, 0),
        (DEND_DASH, 0),
        (DOT_COUNT, 0),
        (LABEL_ACC, 1),
    ] {
        body.push(tt(Op::ConstI32(val)));
        body.push(tt(Op::LetSet {
            idx: slot,
            ty: IrType::I32,
        }));
    }
    body.push(tt(Op::LetGet {
        idx: AT,
        ty: IrType::I32,
    }));
    body.push(tt(Op::ConstI32(1)));
    body.push(tt(Op::Add(IrType::I32)));
    body.push(tt(Op::LetSet {
        idx: I,
        ty: IrType::I32,
    }));
    let mut domain_loop: Vec<TaggedOp> = Vec::new();
    // exit when i >= s_len
    domain_loop.push(tt(Op::LetGet {
        idx: I,
        ty: IrType::I32,
    }));
    domain_loop.push(tt(Op::LetGet {
        idx: S_LEN,
        ty: IrType::I32,
    }));
    domain_loop.push(tt(Op::Ge(IrType::I32)));
    domain_loop.push(tt(Op::BrIf { label_depth: 1 }));
    // byte = s[i]
    domain_loop.extend(load_byte(I));
    domain_loop.push(tt(Op::LetSet {
        idx: BYTE,
        ty: IrType::I32,
    }));
    // is_dot = (byte == '.')
    domain_loop.extend(eq_byte(BYTE, b'.' as i32));
    domain_loop.push(tt(Op::LetSet {
        idx: IS_DOT,
        ty: IrType::I32,
    }));
    // if is_dot { close label } else { char class + label bookkeeping }
    domain_loop.push(tt(Op::LetGet {
        idx: IS_DOT,
        ty: IrType::I32,
    }));
    domain_loop.push(tt(Op::If {
        result_ty: IrType::I32,
        then_body: {
            let mut t: Vec<TaggedOp> = Vec::new();
            // label_acc &= label_ok(...)
            t.push(tt(Op::LetGet {
                idx: LABEL_ACC,
                ty: IrType::I32,
            }));
            t.extend(push_label_ok(DLABEL_LEN, DSTART_DASH, DEND_DASH));
            t.push(tt(Op::BitAnd(IrType::I32)));
            t.push(tt(Op::LetSet {
                idx: LABEL_ACC,
                ty: IrType::I32,
            }));
            // dot_count += 1
            t.push(tt(Op::LetGet {
                idx: DOT_COUNT,
                ty: IrType::I32,
            }));
            t.push(tt(Op::ConstI32(1)));
            t.push(tt(Op::Add(IrType::I32)));
            t.push(tt(Op::LetSet {
                idx: DOT_COUNT,
                ty: IrType::I32,
            }));
            // reset label state
            for slot in [DLABEL_LEN, DSTART_DASH, DEND_DASH] {
                t.push(tt(Op::ConstI32(0)));
                t.push(tt(Op::LetSet {
                    idx: slot,
                    ty: IrType::I32,
                }));
            }
            t.push(tt(Op::ConstI32(0)));
            t
        },
        else_body: {
            let mut e: Vec<TaggedOp> = Vec::new();
            // acc &= (alnum(byte) || byte == '-')  (disjoint -> Add)
            e.push(tt(Op::LetGet {
                idx: ACC,
                ty: IrType::I32,
            }));
            e.extend(push_is_alnum(BYTE));
            e.extend(eq_byte(BYTE, b'-' as i32));
            e.push(tt(Op::Add(IrType::I32)));
            e.push(tt(Op::BitAnd(IrType::I32)));
            e.push(tt(Op::LetSet {
                idx: ACC,
                ty: IrType::I32,
            }));
            // dash = (byte == '-')
            e.extend(eq_byte(BYTE, b'-' as i32));
            e.push(tt(Op::LetSet {
                idx: OK,
                ty: IrType::I32,
            }));
            // if dlabel_len == 0 { dstart_dash = dash }
            e.push(tt(Op::LetGet {
                idx: DLABEL_LEN,
                ty: IrType::I32,
            }));
            e.push(tt(Op::ConstI32(0)));
            e.push(tt(Op::Eq(IrType::I32)));
            e.push(tt(Op::If {
                result_ty: IrType::I32,
                then_body: vec![
                    tt(Op::LetGet {
                        idx: OK,
                        ty: IrType::I32,
                    }),
                    tt(Op::LetSet {
                        idx: DSTART_DASH,
                        ty: IrType::I32,
                    }),
                    tt(Op::ConstI32(0)),
                ],
                else_body: vec![tt(Op::ConstI32(0))],
            }));
            e.push(tt(Op::LetSet {
                idx: BYTE,
                ty: IrType::I32,
            }));
            // dend_dash = dash
            e.push(tt(Op::LetGet {
                idx: OK,
                ty: IrType::I32,
            }));
            e.push(tt(Op::LetSet {
                idx: DEND_DASH,
                ty: IrType::I32,
            }));
            // dlabel_len += 1
            e.push(tt(Op::LetGet {
                idx: DLABEL_LEN,
                ty: IrType::I32,
            }));
            e.push(tt(Op::ConstI32(1)));
            e.push(tt(Op::Add(IrType::I32)));
            e.push(tt(Op::LetSet {
                idx: DLABEL_LEN,
                ty: IrType::I32,
            }));
            e.push(tt(Op::ConstI32(0)));
            e
        },
    }));
    domain_loop.push(tt(Op::LetSet {
        idx: BYTE,
        ty: IrType::I32,
    }));
    // i = i + 1
    domain_loop.push(tt(Op::LetGet {
        idx: I,
        ty: IrType::I32,
    }));
    domain_loop.push(tt(Op::ConstI32(1)));
    domain_loop.push(tt(Op::Add(IrType::I32)));
    domain_loop.push(tt(Op::LetSet {
        idx: I,
        ty: IrType::I32,
    }));
    domain_loop.push(tt(Op::Br { label_depth: 0 }));
    body.push(tt(Op::Block {
        result_ty: None,
        body: vec![tt(Op::Loop {
            result_ty: None,
            body: domain_loop,
        })],
    }));

    // close the final label: label_acc &= label_ok(...)
    body.push(tt(Op::LetGet {
        idx: LABEL_ACC,
        ty: IrType::I32,
    }));
    body.extend(push_label_ok(DLABEL_LEN, DSTART_DASH, DEND_DASH));
    body.push(tt(Op::BitAnd(IrType::I32)));
    body.push(tt(Op::LetSet {
        idx: LABEL_ACC,
        ty: IrType::I32,
    }));

    // acc &= label_acc; acc &= (dot_count >= 1)  (labels.len() >= 2)
    body.push(tt(Op::LetGet {
        idx: ACC,
        ty: IrType::I32,
    }));
    body.push(tt(Op::LetGet {
        idx: LABEL_ACC,
        ty: IrType::I32,
    }));
    body.push(tt(Op::BitAnd(IrType::I32)));
    body.push(tt(Op::LetGet {
        idx: DOT_COUNT,
        ty: IrType::I32,
    }));
    body.push(tt(Op::ConstI32(1)));
    body.push(tt(Op::Ge(IrType::I32)));
    body.push(tt(Op::BitAnd(IrType::I32)));
    body.push(tt(Op::LetSet {
        idx: ACC,
        ty: IrType::I32,
    }));

    // result = acc != 0
    body.push(tt(Op::LetGet {
        idx: ACC,
        ty: IrType::I32,
    }));
    body.push(tt(Op::ConstI32(0)));
    body.push(tt(Op::Ne(IrType::I32)));
    body.push(tt(Op::Return));
    body
}

/// `is_uri(s: String) -> Bool`.
///
/// Mirrors `relon-evaluator/src/stdlib.rs::is_uri_str`:
/// ```text
/// let colon = s.find(':')?;          // first ':'
/// let scheme = &s[..colon];
/// let rest   = &s[colon + 1..];
/// if scheme.is_empty() || rest.is_empty() { return false }
/// let first = scheme.chars().next()?;
/// if !first.is_ascii_alphabetic() { return false }
/// scheme[1..].chars().all(|c| c.is_ascii_alphanumeric() || c == '+'
///                              || c == '-' || c == '.')
/// ```
/// All structural bytes are ASCII, so the body scans bytes directly
/// (`find(':')` is the first `0x3A`). Branch-free accumulation into
/// `acc` (no `Br` crosses an `If`).
pub(super) fn is_uri_string() -> StdlibFunction {
    StdlibFunction::new(
        "is_uri",
        vec![IrType::String],
        IrType::Bool,
        is_uri_string_body,
    )
}

#[allow(clippy::vec_init_then_push)]
fn is_uri_string_body() -> Vec<TaggedOp> {
    const S_LEN: u32 = 0;
    const I: u32 = 1;
    const ACC: u32 = 2;
    const BYTE: u32 = 3;
    const COLON: u32 = 4; // index of first ':'
    const HAS_COLON: u32 = 5;

    let load_byte = |idx_local: u32| {
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

    let mut body: Vec<TaggedOp> = Vec::new();
    // s_len = load_i32(s, 0)
    body.push(tt(Op::LocalGet(0)));
    body.push(tt(Op::LoadI32AtAbsolute { offset: 0 }));
    body.push(tt(Op::LetSet {
        idx: S_LEN,
        ty: IrType::I32,
    }));
    // has_colon = 0; colon = 0; i = 0
    body.push(tt(Op::ConstI32(0)));
    body.push(tt(Op::LetSet {
        idx: HAS_COLON,
        ty: IrType::I32,
    }));
    body.push(tt(Op::ConstI32(0)));
    body.push(tt(Op::LetSet {
        idx: COLON,
        ty: IrType::I32,
    }));
    body.push(tt(Op::ConstI32(0)));
    body.push(tt(Op::LetSet {
        idx: I,
        ty: IrType::I32,
    }));

    // ----- find first ':' -----
    let mut find_loop: Vec<TaggedOp> = Vec::new();
    // exit when i >= s_len OR has_colon == 1
    find_loop.push(tt(Op::LetGet {
        idx: I,
        ty: IrType::I32,
    }));
    find_loop.push(tt(Op::LetGet {
        idx: S_LEN,
        ty: IrType::I32,
    }));
    find_loop.push(tt(Op::Ge(IrType::I32)));
    find_loop.push(tt(Op::BrIf { label_depth: 1 }));
    find_loop.push(tt(Op::LetGet {
        idx: HAS_COLON,
        ty: IrType::I32,
    }));
    find_loop.push(tt(Op::BrIf { label_depth: 1 }));
    // byte = s[i]
    find_loop.extend(load_byte(I));
    find_loop.push(tt(Op::LetSet {
        idx: BYTE,
        ty: IrType::I32,
    }));
    // if byte == ':' { colon = i; has_colon = 1 }
    find_loop.push(tt(Op::LetGet {
        idx: BYTE,
        ty: IrType::I32,
    }));
    find_loop.push(tt(Op::ConstI32(b':' as i32)));
    find_loop.push(tt(Op::Eq(IrType::I32)));
    find_loop.push(tt(Op::If {
        result_ty: IrType::I32,
        then_body: vec![
            tt(Op::LetGet {
                idx: I,
                ty: IrType::I32,
            }),
            tt(Op::LetSet {
                idx: COLON,
                ty: IrType::I32,
            }),
            tt(Op::ConstI32(1)),
            tt(Op::LetSet {
                idx: HAS_COLON,
                ty: IrType::I32,
            }),
            tt(Op::ConstI32(0)),
        ],
        else_body: vec![tt(Op::ConstI32(0))],
    }));
    find_loop.push(tt(Op::LetSet {
        idx: BYTE,
        ty: IrType::I32,
    }));
    // i = i + 1
    find_loop.push(tt(Op::LetGet {
        idx: I,
        ty: IrType::I32,
    }));
    find_loop.push(tt(Op::ConstI32(1)));
    find_loop.push(tt(Op::Add(IrType::I32)));
    find_loop.push(tt(Op::LetSet {
        idx: I,
        ty: IrType::I32,
    }));
    find_loop.push(tt(Op::Br { label_depth: 0 }));
    body.push(tt(Op::Block {
        result_ty: None,
        body: vec![tt(Op::Loop {
            result_ty: None,
            body: find_loop,
        })],
    }));

    // acc = has_colon  (no colon -> false, matching `s.find(':')?`)
    body.push(tt(Op::LetGet {
        idx: HAS_COLON,
        ty: IrType::I32,
    }));
    body.push(tt(Op::LetSet {
        idx: ACC,
        ty: IrType::I32,
    }));

    // acc &= (colon >= 1)        scheme non-empty
    body.push(tt(Op::LetGet {
        idx: ACC,
        ty: IrType::I32,
    }));
    body.push(tt(Op::LetGet {
        idx: COLON,
        ty: IrType::I32,
    }));
    body.push(tt(Op::ConstI32(1)));
    body.push(tt(Op::Ge(IrType::I32)));
    body.push(tt(Op::BitAnd(IrType::I32)));
    // acc &= (s_len - colon - 1 >= 1)  rest non-empty
    body.push(tt(Op::LetGet {
        idx: S_LEN,
        ty: IrType::I32,
    }));
    body.push(tt(Op::LetGet {
        idx: COLON,
        ty: IrType::I32,
    }));
    body.push(tt(Op::Sub(IrType::I32)));
    body.push(tt(Op::ConstI32(1)));
    body.push(tt(Op::Sub(IrType::I32)));
    body.push(tt(Op::ConstI32(1)));
    body.push(tt(Op::Ge(IrType::I32)));
    body.push(tt(Op::BitAnd(IrType::I32)));
    body.push(tt(Op::LetSet {
        idx: ACC,
        ty: IrType::I32,
    }));

    // ----- scheme char class over [0, colon) -----
    // i == 0   : require is_ascii_alphabetic (the first scheme char).
    // i  > 0   : require alnum || '+' || '-' || '.'.
    // Scanning over `[0, colon)` (rather than reading s[0] separately)
    // keeps every byte load inside the string payload: `colon` is the
    // index of a ':' that exists, so every `i < colon` is in bounds.
    // When colon == 0 the loop body never runs (scheme empty), and acc
    // is already 0 from the scheme-non-empty check above.
    body.push(tt(Op::ConstI32(0)));
    body.push(tt(Op::LetSet {
        idx: I,
        ty: IrType::I32,
    }));
    let mut scheme_loop: Vec<TaggedOp> = Vec::new();
    // exit when i >= colon
    scheme_loop.push(tt(Op::LetGet {
        idx: I,
        ty: IrType::I32,
    }));
    scheme_loop.push(tt(Op::LetGet {
        idx: COLON,
        ty: IrType::I32,
    }));
    scheme_loop.push(tt(Op::Ge(IrType::I32)));
    scheme_loop.push(tt(Op::BrIf { label_depth: 1 }));
    // byte = s[i]
    scheme_loop.extend(load_byte(I));
    scheme_loop.push(tt(Op::LetSet {
        idx: BYTE,
        ty: IrType::I32,
    }));
    // ok = (i == 0) ? alpha(byte)
    //               : alnum(byte) + (byte=='+') + (byte=='-') + (byte=='.')
    //   (the four non-first cases are disjoint, so the sum stays in {0,1}).
    scheme_loop.push(tt(Op::LetGet {
        idx: I,
        ty: IrType::I32,
    }));
    scheme_loop.push(tt(Op::ConstI32(0)));
    scheme_loop.push(tt(Op::Eq(IrType::I32)));
    scheme_loop.push(tt(Op::If {
        result_ty: IrType::I32,
        then_body: push_is_alpha(BYTE),
        else_body: {
            let mut e = push_is_alnum(BYTE);
            e.extend(eq_byte(BYTE, b'+' as i32));
            e.push(tt(Op::Add(IrType::I32)));
            e.extend(eq_byte(BYTE, b'-' as i32));
            e.push(tt(Op::Add(IrType::I32)));
            e.extend(eq_byte(BYTE, b'.' as i32));
            e.push(tt(Op::Add(IrType::I32)));
            e
        },
    }));
    // acc &= ok
    scheme_loop.push(tt(Op::LetGet {
        idx: ACC,
        ty: IrType::I32,
    }));
    scheme_loop.push(tt(Op::BitAnd(IrType::I32)));
    scheme_loop.push(tt(Op::LetSet {
        idx: ACC,
        ty: IrType::I32,
    }));
    // i = i + 1
    scheme_loop.push(tt(Op::LetGet {
        idx: I,
        ty: IrType::I32,
    }));
    scheme_loop.push(tt(Op::ConstI32(1)));
    scheme_loop.push(tt(Op::Add(IrType::I32)));
    scheme_loop.push(tt(Op::LetSet {
        idx: I,
        ty: IrType::I32,
    }));
    scheme_loop.push(tt(Op::Br { label_depth: 0 }));
    body.push(tt(Op::Block {
        result_ty: None,
        body: vec![tt(Op::Loop {
            result_ty: None,
            body: scheme_loop,
        })],
    }));

    // result = acc != 0
    body.push(tt(Op::LetGet {
        idx: ACC,
        ty: IrType::I32,
    }));
    body.push(tt(Op::ConstI32(0)));
    body.push(tt(Op::Ne(IrType::I32)));
    body.push(tt(Op::Return));
    body
}
