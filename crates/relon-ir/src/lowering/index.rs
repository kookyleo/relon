//! Lowering sub-module: list / dict index-expression lowering.
//!
//! Inline payload addressing for `xs[i]` on `List<Int>` /
//! `List<Float>` / typed list receivers, string-key indexing on
//! `List<String>`, and `dict[key]` string-key lookup — all mirroring
//! the record layout the bundled stdlib bodies write.

use super::*;

/// AOT-4 (W16 slice): lower a 1D `xs[i]` index on a `List<Int>`
/// receiver whose arena handle is already on top of the vstack (pushed
/// by [`lower_variable`]'s head load, tagged `IrType::ListInt`).
///
/// Emits the inline payload addressing that mirrors the record layout
/// the bundled `list_int_*` bodies write
/// (`stdlib::defs::list_filter_body_typed`): `[len: u32 LE][pad: u32]
/// [i64 elements...]`, payload aligned at `(base + 4 + 7) & -8`,
/// element `i` at `payload + i*8`:
///
/// ```text
/// base    = <handle on vstack>             ; i32, stashed in a let
/// idx     = <index expr>                   ; i64 -> truncated to i32
/// payload = (base + 11) & -8               ; i32
/// addr    = payload + idx*8                 ; i32
/// push i64.load(addr)                       ; LoadI64AtAbsolute{0}
/// ```
///
/// No bounds branch is emitted — see the caller doc-comment for the
/// in-bounds rationale. The element is left on the vstack tagged
/// `IrType::I64`.
pub(super) fn lower_list_int_index(
    index_node: &Node,
    range: TokenRange,
    ctx: &mut LowerCtx<'_>,
) -> Result<(), LoweringError> {
    lower_list_index_typed(index_node, IrType::ListInt, range, ctx)
}

/// #359 (W20): generalised 1D list index — the `List<Int>` body
/// (`elem_ty = I64`, `LoadI64AtAbsolute`) and the `List<Float>` body
/// (`elem_ty = F64`, `LoadF64AtAbsolute`) share the identical record
/// layout (`[len:u32][pad:u32][8-byte elements...]`, payload at
/// `(base + 11) & -8`, element `i` at `payload + i*8`) — only the
/// element-load op and the pushed element type differ. Pops the
/// receiver list handle (tagged `recv_ty`), pushes the element scalar
/// (`I64` for `ListInt`, `F64` for `ListFloat`).
pub(super) fn lower_list_index_typed(
    index_node: &Node,
    recv_ty: IrType,
    range: TokenRange,
    ctx: &mut LowerCtx<'_>,
) -> Result<(), LoweringError> {
    debug_assert!(matches!(recv_ty, IrType::ListInt | IrType::ListFloat));
    let (elem_ty, load_op) = match recv_ty {
        IrType::ListFloat => (IrType::F64, Op::LoadF64AtAbsolute { offset: 0 }),
        _ => (IrType::I64, Op::LoadI64AtAbsolute { offset: 0 }),
    };
    // Reserve let slots: `base` (i32 handle) + `idx` (i32 element
    // index). Each slot is single-typed for its lifetime so the LLVM
    // emitter's `ensure_let_slot` aliasing guard stays happy.
    let base_i = ctx.next_let_idx;
    let idx_i = ctx.next_let_idx + 1;
    ctx.next_let_idx += 2;

    // Stash the receiver handle (already on the vstack as the list ty).
    let top = ctx.tstack.pop().ok_or(cap!(
        "lower_list_index_typed.unsupported_expr",
        LoweringError::UnsupportedExpr {
            kind: "Variable(list-index-empty-stack)".to_string(),
            range,
        }
    ))?;
    debug_assert_eq!(top, recv_ty);
    ctx.out.push(TaggedOp {
        op: Op::LetSet {
            idx: base_i,
            ty: IrType::I32,
        },
        range,
    });

    // Lower the index expression; it must be an Int (I64). Truncate
    // into the i32 `idx` slot via the type-narrowing `LetSet` (the
    // emitter's `coerce_to_let_ty` truncates I64 -> I32).
    lower_expr(&index_node.expr, index_node.range, ctx)?;
    expect_int_top(ctx, range)?;
    ctx.out.push(TaggedOp {
        op: Op::LetSet {
            idx: idx_i,
            ty: IrType::I32,
        },
        range,
    });
    ctx.tstack.pop();

    // payload = (base + 11) & -8
    ctx.out.push(TaggedOp {
        op: Op::LetGet {
            idx: base_i,
            ty: IrType::I32,
        },
        range,
    });
    ctx.tstack.push(IrType::I32);
    ctx.out.push(TaggedOp {
        op: Op::ConstI32(4 + 7),
        range,
    });
    ctx.tstack.push(IrType::I32);
    ctx.out.push(TaggedOp {
        op: Op::Add(IrType::I32),
        range,
    });
    ctx.tstack.pop();
    ctx.tstack.pop();
    ctx.tstack.push(IrType::I32);
    ctx.out.push(TaggedOp {
        op: Op::ConstI32(-8),
        range,
    });
    ctx.tstack.push(IrType::I32);
    ctx.out.push(TaggedOp {
        op: Op::BitAnd(IrType::I32),
        range,
    });
    ctx.tstack.pop();
    ctx.tstack.pop();
    ctx.tstack.push(IrType::I32);

    // addr = payload + idx*8
    ctx.out.push(TaggedOp {
        op: Op::LetGet {
            idx: idx_i,
            ty: IrType::I32,
        },
        range,
    });
    ctx.tstack.push(IrType::I32);
    ctx.out.push(TaggedOp {
        op: Op::ConstI32(8),
        range,
    });
    ctx.tstack.push(IrType::I32);
    ctx.out.push(TaggedOp {
        op: Op::Mul(IrType::I32),
        range,
    });
    ctx.tstack.pop();
    ctx.tstack.pop();
    ctx.tstack.push(IrType::I32);
    ctx.out.push(TaggedOp {
        op: Op::Add(IrType::I32),
        range,
    });
    ctx.tstack.pop();
    ctx.tstack.pop();
    ctx.tstack.push(IrType::I32);

    // push <i64|f64>.load(addr)
    ctx.out.push(TaggedOp { op: load_op, range });
    ctx.tstack.pop(); // i32 addr
    ctx.tstack.push(elem_ty);
    Ok(())
}

/// W5-P2: lower a 1D `keys[i]` index on a `List<String>` receiver
/// (tagged `IrType::ListString`, pushed by [`lower_variable`]'s head
/// load). A `List<String>` record is a *pointer array*, NOT the inline
/// 8-byte-element shape `lower_list_index_typed` handles:
///
/// ```text
/// [len: u32 LE][off_0: u32 LE][off_1: u32 LE]...[off_{N-1}: u32 LE]
/// ```
///
/// `off_i` is the arena-relative byte offset of the i-th String record
/// (`[slen: u32 LE][utf8]`) — the exact same handle representation
/// `Op::ConstString` pushes. The header has NO 8-byte pad (every slot
/// is u32), so element `i` sits at `base + 4 + i*4`. Indexing therefore
/// loads the `u32` slot and leaves it on the vstack tagged
/// `IrType::String` — a downstream String-return tail-record copy
/// (`EmitTailRecordFromAbsoluteAddr { ty: String }`) then resolves the
/// `[slen][utf8]` record through that handle exactly as it would for a
/// `ConstString`.
///
/// Addressing (mirrors `lower_list_index_typed`'s let-stash discipline
/// but with a 4-byte stride and no align mask):
///
/// ```text
/// base = <ListString handle on vstack>      ; i32, stashed in a let
/// idx  = <index expr>                        ; i64 -> truncated to i32
/// addr = base + 4 + idx*4                     ; i32
/// push i32.load(addr)                         ; LoadI32AtAbsolute{0}
/// ```
///
/// No bounds branch — same in-bounds rationale as the int/float path
/// (`keys[i % 10]` is provably within a 10-element list).
pub(super) fn lower_list_string_index(
    index_node: &Node,
    range: TokenRange,
    ctx: &mut LowerCtx<'_>,
) -> Result<(), LoweringError> {
    let base_i = ctx.next_let_idx;
    let idx_i = ctx.next_let_idx + 1;
    ctx.next_let_idx += 2;

    // Stash the receiver handle (a `ListString` arena offset, i32).
    let top = ctx.tstack.pop().ok_or(cap!(
        "lower_list_string_index.unsupported_expr",
        LoweringError::UnsupportedExpr {
            kind: "Variable(list-string-index-empty-stack)".to_string(),
            range,
        }
    ))?;
    debug_assert_eq!(top, IrType::ListString);
    ctx.out.push(TaggedOp {
        op: Op::LetSet {
            idx: base_i,
            ty: IrType::I32,
        },
        range,
    });

    // Lower the index expression; require an Int (I64), truncate into
    // the i32 `idx` slot.
    lower_expr(&index_node.expr, index_node.range, ctx)?;
    expect_int_top(ctx, range)?;
    ctx.out.push(TaggedOp {
        op: Op::LetSet {
            idx: idx_i,
            ty: IrType::I32,
        },
        range,
    });
    ctx.tstack.pop();

    // addr = base + 4 + idx*4
    ctx.out.push(TaggedOp {
        op: Op::LetGet {
            idx: base_i,
            ty: IrType::I32,
        },
        range,
    });
    ctx.tstack.push(IrType::I32);
    ctx.out.push(TaggedOp {
        op: Op::ConstI32(4),
        range,
    });
    ctx.tstack.push(IrType::I32);
    ctx.out.push(TaggedOp {
        op: Op::Add(IrType::I32),
        range,
    });
    ctx.tstack.pop();
    ctx.tstack.pop();
    ctx.tstack.push(IrType::I32);

    ctx.out.push(TaggedOp {
        op: Op::LetGet {
            idx: idx_i,
            ty: IrType::I32,
        },
        range,
    });
    ctx.tstack.push(IrType::I32);
    ctx.out.push(TaggedOp {
        op: Op::ConstI32(4),
        range,
    });
    ctx.tstack.push(IrType::I32);
    ctx.out.push(TaggedOp {
        op: Op::Mul(IrType::I32),
        range,
    });
    ctx.tstack.pop();
    ctx.tstack.pop();
    ctx.tstack.push(IrType::I32);
    ctx.out.push(TaggedOp {
        op: Op::Add(IrType::I32),
        range,
    });
    ctx.tstack.pop();
    ctx.tstack.pop();
    ctx.tstack.push(IrType::I32);

    // push i32.load(addr) -> the String handle (arena offset).
    ctx.out.push(TaggedOp {
        op: Op::LoadI32AtAbsolute { offset: 0 },
        range,
    });
    ctx.tstack.pop(); // i32 addr
    ctx.tstack.push(IrType::String);
    Ok(())
}

/// W5-P3: lower `d[k]` where `d` is a materialised `{String -> Int}`
/// dict (an `IrType::Dict` arena handle pushed by [`lower_variable`]'s
/// head load via `Op::ConstDict`) and `k` is a runtime `String` handle
/// (e.g. `keys[i]`, the same `[slen: u32][utf8]` record `ConstString`
/// pushes). The lookup runs entirely as IR-lowered primitive ops
/// (`Block`/`Loop`/`BrIf`/`LoadI32AtAbsolute`/`LoadI8UAtAbsolute`/…),
/// so it needs **no new runtime helper / wasm import**: every backend
/// (cranelift AOT, LLVM AOT, wasm32) already lowers these ops, exactly
/// as the bundled `starts_with` stdlib body does its byte compare.
///
/// ## Arena dict record (record-relative offsets — see
/// `relon-codegen-*::const_pool::visit_const_dict`)
///
/// ```text
/// [entry_count: u32 @0][pad: u32 @4][shape_hash: u64 @8]      ; 16-byte header
/// entry_count × [key_off: u32][key_len: u32][value: i64]      ; 16 bytes each,
///                                                             ;   sorted by key
/// concatenated UTF-8 key bytes                                ; key_off is
///                                                             ;   record-relative
/// ```
///
/// ## Probe (linear scan + byte compare)
///
/// ```text
/// dict_base = <Dict handle>          ; i32 arena-relative
/// kh        = <String handle from k> ; i32 arena-relative
/// klen      = load_u32(kh + 0)       ; key byte length
/// n         = load_u32(dict_base+0)  ; entry_count
/// i = 0; found = 0; result = 0
/// loop over entries:
///   if found != 0 || i >= n: break
///   eoff      = dict_base + 16 + i*16
///   ekey_len  = load_u32(eoff + 4)
///   if ekey_len == klen:
///     ekey_addr = dict_base + load_u32(eoff + 0)   ; record-relative key_off
///     j = 0; mismatch = 0
///     inner byte loop until j == klen or a byte differs
///     if mismatch == 0:               ; full match
///       result = load_i64(eoff + 8); found = 1
///   i += 1
/// if found == 0: trap(IndexOutOfBounds)   ; honest miss — no silent wrong value
/// push result (Int)
/// ```
///
/// The scan is linear (the entry table being key-sorted is not
/// required for correctness; a binary search is a future optimisation).
/// No bounds branch is needed beyond `i < n` — the probe stays within
/// the record by construction. The not-found path traps rather than
/// returning a sentinel so a miss is never silently mis-read.
pub(super) fn lower_dict_string_index(
    index_node: &Node,
    range: TokenRange,
    ctx: &mut LowerCtx<'_>,
) -> Result<(), LoweringError> {
    // Pop the `Dict` receiver handle into an i32 let-local.
    let top = ctx.tstack.pop().ok_or(cap!(
        "lower_dict_string_index.unsupported_expr.1",
        LoweringError::UnsupportedExpr {
            kind: "Variable(dict-index-empty-stack)".to_string(),
            range,
        }
    ))?;
    debug_assert_eq!(top, IrType::Dict);
    let dict_base = ctx.next_let_idx;
    ctx.next_let_idx += 1;
    ctx.out.push(TaggedOp {
        op: Op::LetSet {
            idx: dict_base,
            ty: IrType::I32,
        },
        range,
    });

    // Lower the key expression; it must produce a `String` handle.
    lower_expr(&index_node.expr, index_node.range, ctx)?;
    let key_ty = ctx.tstack.pop().ok_or(cap!(
        "lower_dict_string_index.unsupported_expr.2",
        LoweringError::UnsupportedExpr {
            kind: "Variable(dict-index-key-empty-stack)".to_string(),
            range,
        }
    ))?;
    if key_ty != IrType::String {
        return Err(cap!(
            "lower_dict_string_index.unsupported_expr.3",
            LoweringError::UnsupportedExpr {
                kind: format!("Variable(dict-index key must be String, got {key_ty:?})"),
                range,
            }
        ));
    }
    let kh = ctx.next_let_idx;
    ctx.next_let_idx += 1;
    ctx.out.push(TaggedOp {
        op: Op::LetSet {
            idx: kh,
            ty: IrType::I32,
        },
        range,
    });

    // Fresh scratch let-locals for the probe state. All i32 except the
    // i64 `result` accumulator.
    let klen = ctx.next_let_idx;
    let n = ctx.next_let_idx + 1;
    let i = ctx.next_let_idx + 2;
    let found = ctx.next_let_idx + 3;
    let ekey_len = ctx.next_let_idx + 4;
    let ekey_addr = ctx.next_let_idx + 5;
    let eoff = ctx.next_let_idx + 6;
    let j = ctx.next_let_idx + 7;
    let mismatch = ctx.next_let_idx + 8;
    let result = ctx.next_let_idx + 9;
    ctx.next_let_idx += 10;

    let i32_get = |idx: u32| TaggedOp {
        op: Op::LetGet {
            idx,
            ty: IrType::I32,
        },
        range,
    };
    let i32_set = |idx: u32| TaggedOp {
        op: Op::LetSet {
            idx,
            ty: IrType::I32,
        },
        range,
    };
    let ci32 = |v: i32| TaggedOp {
        op: Op::ConstI32(v),
        range,
    };
    let add = || TaggedOp {
        op: Op::Add(IrType::I32),
        range,
    };
    let load_u32 = |off: u32| TaggedOp {
        op: Op::LoadI32AtAbsolute { offset: off },
        range,
    };

    // klen = load_u32(kh + 0)
    ctx.out.push(i32_get(kh));
    ctx.out.push(load_u32(0));
    ctx.out.push(i32_set(klen));

    // n = load_u32(dict_base + 0)
    ctx.out.push(i32_get(dict_base));
    ctx.out.push(load_u32(0));
    ctx.out.push(i32_set(n));

    // i = 0; found = 0; result = 0
    ctx.out.push(ci32(0));
    ctx.out.push(i32_set(i));
    ctx.out.push(ci32(0));
    ctx.out.push(i32_set(found));
    ctx.out.push(TaggedOp {
        op: Op::ConstI64(0),
        range,
    });
    ctx.out.push(TaggedOp {
        op: Op::LetSet {
            idx: result,
            ty: IrType::I64,
        },
        range,
    });

    // Inner byte-compare loop (mismatch := first differing byte / 0 on
    // full window match). Built once and embedded into the entry body.
    let inner_compare = TaggedOp {
        op: Op::Block {
            result_ty: None,
            body: vec![TaggedOp {
                op: Op::Loop {
                    result_ty: None,
                    body: vec![
                        // j >= klen ? br 1 (fully matched; mismatch stays 0)
                        i32_get(j),
                        i32_get(klen),
                        TaggedOp {
                            op: Op::Ge(IrType::I32),
                            range,
                        },
                        TaggedOp {
                            op: Op::BrIf { label_depth: 1 },
                            range,
                        },
                        // kb = load_i8(kh + 4 + j)
                        i32_get(kh),
                        ci32(4),
                        add(),
                        i32_get(j),
                        add(),
                        TaggedOp {
                            op: Op::LoadI8UAtAbsolute { offset: 0 },
                            range,
                        },
                        // eb = load_i8(ekey_addr + j)
                        i32_get(ekey_addr),
                        i32_get(j),
                        add(),
                        TaggedOp {
                            op: Op::LoadI8UAtAbsolute { offset: 0 },
                            range,
                        },
                        // mismatch = (kb != eb)
                        TaggedOp {
                            op: Op::Ne(IrType::I32),
                            range,
                        },
                        i32_set(mismatch),
                        // mismatch != 0 ? br 1
                        i32_get(mismatch),
                        ci32(0),
                        TaggedOp {
                            op: Op::Ne(IrType::I32),
                            range,
                        },
                        TaggedOp {
                            op: Op::BrIf { label_depth: 1 },
                            range,
                        },
                        // j = j + 1
                        i32_get(j),
                        ci32(1),
                        add(),
                        i32_set(j),
                        TaggedOp {
                            op: Op::Br { label_depth: 0 },
                            range,
                        },
                    ],
                },
                range,
            }],
        },
        range,
    };

    // Per-entry body: compute eoff, compare key length, then bytes.
    let entry_body = vec![
        // found != 0 ? br 1
        i32_get(found),
        ci32(0),
        TaggedOp {
            op: Op::Ne(IrType::I32),
            range,
        },
        TaggedOp {
            op: Op::BrIf { label_depth: 1 },
            range,
        },
        // i >= n ? br 1
        i32_get(i),
        i32_get(n),
        TaggedOp {
            op: Op::Ge(IrType::I32),
            range,
        },
        TaggedOp {
            op: Op::BrIf { label_depth: 1 },
            range,
        },
        // eoff = dict_base + 16 + i*16
        i32_get(dict_base),
        ci32(16),
        add(),
        i32_get(i),
        ci32(16),
        TaggedOp {
            op: Op::Mul(IrType::I32),
            range,
        },
        add(),
        i32_set(eoff),
        // ekey_len = load_u32(eoff + 4)
        i32_get(eoff),
        load_u32(4),
        i32_set(ekey_len),
        // `if ekey_len == klen { compare bytes }`, expressed as a
        // stack-neutral skip-block: branch past the body when the key
        // lengths differ (`Op::If` requires both branches to push a
        // value, so a structured `Block { BrIf 0; body }` is the clean
        // void-conditional form here).
        TaggedOp {
            op: Op::Block {
                result_ty: None,
                body: vec![
                    // ekey_len != klen ? br 0 (skip — length mismatch)
                    i32_get(ekey_len),
                    i32_get(klen),
                    TaggedOp {
                        op: Op::Ne(IrType::I32),
                        range,
                    },
                    TaggedOp {
                        op: Op::BrIf { label_depth: 0 },
                        range,
                    },
                    // ekey_addr = dict_base + load_u32(eoff + 0)
                    i32_get(dict_base),
                    i32_get(eoff),
                    load_u32(0),
                    add(),
                    i32_set(ekey_addr),
                    // j = 0; mismatch = 0
                    ci32(0),
                    i32_set(j),
                    ci32(0),
                    i32_set(mismatch),
                    inner_compare,
                    // mismatch != 0 ? br 0 (skip — bytes differ)
                    i32_get(mismatch),
                    ci32(0),
                    TaggedOp {
                        op: Op::Ne(IrType::I32),
                        range,
                    },
                    TaggedOp {
                        op: Op::BrIf { label_depth: 0 },
                        range,
                    },
                    // full match: result = load_i64(eoff+8); found = 1
                    i32_get(eoff),
                    TaggedOp {
                        op: Op::LoadI64AtAbsolute { offset: 8 },
                        range,
                    },
                    TaggedOp {
                        op: Op::LetSet {
                            idx: result,
                            ty: IrType::I64,
                        },
                        range,
                    },
                    ci32(1),
                    i32_set(found),
                ],
            },
            range,
        },
        // i = i + 1
        i32_get(i),
        ci32(1),
        add(),
        i32_set(i),
        TaggedOp {
            op: Op::Br { label_depth: 0 },
            range,
        },
    ];

    // Outer scan: Block { Loop { entry_body } }. Stack-neutral.
    ctx.out.push(TaggedOp {
        op: Op::Block {
            result_ty: None,
            body: vec![TaggedOp {
                op: Op::Loop {
                    result_ty: None,
                    body: entry_body,
                },
                range,
            }],
        },
        range,
    });

    // Honest miss: trap if no entry matched. w5 always hits, but a
    // missing key must never surface a silent wrong value. Expressed
    // as a skip-block: branch past the trap when `found != 0`.
    ctx.out.push(TaggedOp {
        op: Op::Block {
            result_ty: None,
            body: vec![
                // found != 0 ? br 0 (hit — skip the trap)
                i32_get(found),
                ci32(0),
                TaggedOp {
                    op: Op::Ne(IrType::I32),
                    range,
                },
                TaggedOp {
                    op: Op::BrIf { label_depth: 0 },
                    range,
                },
                TaggedOp {
                    op: Op::Trap {
                        kind: TrapKind::IndexOutOfBounds,
                    },
                    range,
                },
            ],
        },
        range,
    });

    // Push the i64 value result as an Int.
    ctx.out.push(TaggedOp {
        op: Op::LetGet {
            idx: result,
            ty: IrType::I64,
        },
        range,
    });
    ctx.tstack.push(IrType::I64);
    Ok(())
}
