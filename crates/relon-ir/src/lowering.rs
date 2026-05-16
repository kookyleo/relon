//! `AnalyzedTree` -> [`Module`] lowering for Phase 2.b.
//!
//! Surface accepted (Phase 2.b widens v1.beta slightly):
//!
//! * `#main(<scalar> x [, ...]) -> <scalar>` on the entry module.
//!   `<scalar>` ∈ { `Int`, `Float`, `Bool` }. The pass packages every
//!   `#main` parameter into a 1-record schema `MainParams` (canonical
//!   form) and the return type into a 1-field schema `Ret` so codegen
//!   can apply the binary handshake uniformly.
//! * Root expression is the function body. Allowed shapes:
//!   - `Expr::Int(i)`           -> [`Op::ConstI64`]
//!   - `Expr::Float(f)`         -> [`Op::ConstF64`]
//!   - `Expr::Variable(path)`   -> [`Op::LoadField`] reading from the
//!     `in_buf` at the offset declared by the `MainParams` schema
//!   - `Expr::Binary(op, l, r)` with `op` in `{Add, Sub, Mul, Div, Mod}`
//!     -> recursive lower of `l`, `r`, then the matching [`Op`] tagged
//!     with the operands' [`IrType`]
//!
//! The wasm-level function signature emitted by codegen is
//! `(in_ptr i32, in_len i32, out_ptr i32, out_cap i32) -> i64`; the
//! IR records these wasm params on `Func::params`. User-declared
//! `#main` params are surfaced via `LoadField` operations, not as
//! `LocalGet` of wasm function locals.

use ordered_float::OrderedFloat;
use relon_analyzer::main_sig::MainSignature;
use relon_analyzer::tree::AnalyzedTree;
use relon_analyzer::workspace::WorkspaceTree;
use relon_eval_api::layout::{OffsetTable, SchemaLayout};
use relon_eval_api::schema_canonical::{Field, Schema, TypeRepr};
use relon_parser::{Expr, Node, Operator, TokenKey, TokenRange, TypeNode};

use crate::error::LoweringError;
use crate::ir::{Func, IrType, Module, Op, TaggedOp};

/// Wasm-side handshake parameter index — `in_ptr` is local 0.
pub const WASM_LOCAL_IN_PTR: u32 = 0;
/// Wasm-side handshake parameter index — `in_len` is local 1.
pub const WASM_LOCAL_IN_LEN: u32 = 1;
/// Wasm-side handshake parameter index — `out_ptr` is local 2.
pub const WASM_LOCAL_OUT_PTR: u32 = 2;
/// Wasm-side handshake parameter index — `out_cap` is local 3.
pub const WASM_LOCAL_OUT_CAP: u32 = 3;

/// Canonical name used for the synthesised `#main` parameter schema.
/// Phase 2.b packages the parameter list into a single record under
/// this name so the canonical hash + layout pass treat it uniformly.
pub const MAIN_PARAMS_SCHEMA_NAME: &str = "MainParams";
/// Canonical name used for the synthesised `#main` return schema.
/// Phase 2.b wraps the scalar return type in a 1-field record named
/// `"value"` under this schema so codegen can write through the
/// generic `BufferBuilder` path.
pub const MAIN_RETURN_SCHEMA_NAME: &str = "Ret";
/// Field name for the synthesised return-value slot inside the
/// `Ret` schema. Kept as a constant so codegen and host-side test
/// fixtures agree on the spelling.
pub const RETURN_VALUE_FIELD_NAME: &str = "value";

/// Result of lowering an entry module: the IR plus the canonical
/// shapes of the `#main` parameter pack and return value.
#[derive(Debug, Clone, PartialEq)]
pub struct LoweredEntry {
    /// IR module ready to hand to the codegen pass.
    pub module: Module,
    /// Canonical schema describing the `in_buf` layout. Phase 2.b
    /// always synthesises a single record named [`MAIN_PARAMS_SCHEMA_NAME`].
    pub main_schema: Schema,
    /// Canonical schema describing the `out_buf` layout. Phase 2.b
    /// always synthesises a 1-field record named
    /// [`MAIN_RETURN_SCHEMA_NAME`] with a single `value` field.
    pub return_schema: Schema,
}

/// Lower the entry module of a workspace.
///
/// Looks up `entry_module` in `ws.modules` and the matching root
/// node in `ws.nodes`, then delegates to [`lower_workspace_single`].
/// Phase 2.b still only handles single-entry workspaces.
pub fn lower_workspace(
    ws: &WorkspaceTree,
    entry_module: &str,
) -> Result<LoweredEntry, LoweringError> {
    let tree = ws
        .modules
        .get(entry_module)
        .ok_or_else(|| LoweringError::EntryModuleNotFound {
            module: entry_module.to_string(),
        })?;
    let root = ws
        .nodes
        .get(entry_module)
        .ok_or_else(|| LoweringError::EntryModuleNotFound {
            module: entry_module.to_string(),
        })?;
    lower_workspace_single_with_module(tree.as_ref(), root.as_ref(), entry_module)
}

/// Single-file lowering convenience. Treats the supplied `(tree,
/// root)` pair as a one-module workspace with id `"main"`.
pub fn lower_workspace_single(
    tree: &AnalyzedTree,
    root: &Node,
) -> Result<LoweredEntry, LoweringError> {
    lower_workspace_single_with_module(tree, root, "main")
}

fn lower_workspace_single_with_module(
    tree: &AnalyzedTree,
    root: &Node,
    module_id: &str,
) -> Result<LoweredEntry, LoweringError> {
    let sig = tree
        .main_signature
        .as_ref()
        .ok_or_else(|| LoweringError::MissingMain {
            module: module_id.to_string(),
        })?;

    // Build the canonical-form schemas for in_buf and out_buf, then
    // compute the offset table for the param schema so each
    // `Variable(x)` reference can be lowered to a typed LoadField.
    let main_schema = build_main_params_schema(sig)?;
    let return_schema = build_main_return_schema(sig)?;
    let main_layout = SchemaLayout::offsets_for(&main_schema)?;
    let return_layout = SchemaLayout::offsets_for(&return_schema)?;

    // Bind each parameter name to its (offset, IR type) so the body
    // walk can lower bare-identifier references to a typed LoadField
    // without a second pass over the layout pass.
    let locals = build_local_index(sig, &main_schema, &main_layout)?;

    // Virtual stack mirroring the wasm value stack at lowering time.
    // Each entry records the IR type that op produced.
    let mut body = Vec::new();
    let mut tstack: Vec<IrType> = Vec::new();
    lower_expr(&root.expr, root.range, &locals, &mut body, &mut tstack)?;

    // Trailing StoreField for the single root return value. Pops the
    // top stack entry — codegen will translate this to `local.get
    // $out_ptr; <value>; <store>.offset=N`.
    let ret_offset = return_layout
        .fields
        .first()
        .map(|f| f.offset as u32)
        .unwrap_or(0);
    let ret_ir_ty = type_repr_to_ir_type(&return_schema.fields[0].ty)?;
    body.push(TaggedOp {
        op: Op::StoreField {
            offset: ret_offset,
            ty: ret_ir_ty,
        },
        range: sig.range,
    });
    tstack.pop();

    // `Op::Return` keeps its v1.beta meaning: end of function. The
    // codegen pass synthesises the actual wasm `return` (it pushes
    // `bytes_written` and emits the implicit `end`).
    body.push(TaggedOp {
        op: Op::Return,
        range: sig.range,
    });

    let func = Func {
        name: "run_main".to_string(),
        // Wasm-level binary handshake signature: four i32 slots
        // (in_ptr, in_len, out_ptr, out_cap). User-declared params
        // reach the body through `LoadField`.
        params: vec![IrType::I32, IrType::I32, IrType::I32, IrType::I32],
        // `bytes_written` returned as i32. Phase 2.b never returns
        // anything else from the wasm function itself; user-side
        // return values live in `out_buf`.
        ret: IrType::I32,
        body,
        range: sig.range,
    };

    Ok(LoweredEntry {
        module: Module {
            funcs: vec![func],
            entry_func_index: Some(0),
        },
        main_schema,
        return_schema,
    })
}

/// Synthesise the [`MAIN_PARAMS_SCHEMA_NAME`] canonical schema from
/// the `#main` parameter list. Rejects any non-scalar parameter type.
fn build_main_params_schema(sig: &MainSignature) -> Result<Schema, LoweringError> {
    let mut fields = Vec::with_capacity(sig.params.len());
    for p in &sig.params {
        let ty = type_node_to_canonical(&p.type_node).ok_or_else(|| {
            LoweringError::UnsupportedTypeInMain {
                type_name: type_head_for_display(&p.type_node),
                range: p.type_node.range,
            }
        })?;
        fields.push(Field {
            name: p.name.clone(),
            ty,
            default: None,
        });
    }
    Ok(Schema {
        name: MAIN_PARAMS_SCHEMA_NAME.to_string(),
        generics: vec![],
        fields,
    })
}

/// Synthesise the [`MAIN_RETURN_SCHEMA_NAME`] canonical schema with a
/// single `value` field carrying the declared return type.
fn build_main_return_schema(sig: &MainSignature) -> Result<Schema, LoweringError> {
    let rt = sig
        .return_type
        .as_ref()
        .ok_or_else(|| LoweringError::UnsupportedTypeInMain {
            type_name: "<missing>".to_string(),
            range: sig.range,
        })?;
    let ty = type_node_to_canonical(rt).ok_or_else(|| LoweringError::UnsupportedTypeInMain {
        type_name: type_head_for_display(rt),
        range: rt.range,
    })?;
    Ok(Schema {
        name: MAIN_RETURN_SCHEMA_NAME.to_string(),
        generics: vec![],
        fields: vec![Field {
            name: RETURN_VALUE_FIELD_NAME.to_string(),
            ty,
            default: None,
        }],
    })
}

/// Map a parser [`TypeNode`] to a canonical [`TypeRepr`]. Phase 2.b
/// only supports the four scalar leaves.
fn type_node_to_canonical(t: &TypeNode) -> Option<TypeRepr> {
    if t.path.len() != 1 || !t.generics.is_empty() || t.variant_fields.is_some() {
        return None;
    }
    match t.path[0].as_str() {
        "Int" => Some(TypeRepr::Int),
        "Float" => Some(TypeRepr::Float),
        "Bool" => Some(TypeRepr::Bool),
        "Null" => Some(TypeRepr::Null),
        _ => None,
    }
}

/// Map a canonical [`TypeRepr`] to the matching [`IrType`]. Used both
/// when building the local index (so `Variable` references know their
/// type) and when synthesising the trailing `StoreField`.
fn type_repr_to_ir_type(t: &TypeRepr) -> Result<IrType, LoweringError> {
    match t {
        TypeRepr::Int => Ok(IrType::I64),
        TypeRepr::Float => Ok(IrType::F64),
        TypeRepr::Bool => Ok(IrType::Bool),
        TypeRepr::Null => Ok(IrType::Null),
        // Composite types are rejected upstream during schema build;
        // this branch fires only for a directly hand-crafted IR.
        _ => Err(LoweringError::UnsupportedTypeInMain {
            type_name: format!("{t:?}"),
            range: TokenRange::default(),
        }),
    }
}

/// One entry in the `#main` parameter index: `(name, ir_type,
/// field_offset)`. The body walk uses this to lower `Variable(x)`
/// into a typed [`Op::LoadField`].
#[derive(Debug, Clone)]
struct LocalBinding {
    name: String,
    ty: IrType,
    offset: u32,
}

fn build_local_index(
    sig: &MainSignature,
    main_schema: &Schema,
    layout: &OffsetTable,
) -> Result<Vec<LocalBinding>, LoweringError> {
    // The schema and layout are built side-by-side from the same
    // `MainSignature`, so their `fields` vectors line up index-for-
    // index. We zip them once here so the body walk does a O(N) lookup
    // by name without re-walking the offset table per reference.
    debug_assert_eq!(main_schema.fields.len(), layout.fields.len());
    debug_assert_eq!(main_schema.fields.len(), sig.params.len());
    let mut out = Vec::with_capacity(sig.params.len());
    for (field, slot) in main_schema.fields.iter().zip(layout.fields.iter()) {
        let ir_ty = type_repr_to_ir_type(&field.ty)?;
        out.push(LocalBinding {
            name: field.name.clone(),
            ty: ir_ty,
            offset: slot.offset as u32,
        });
    }
    Ok(out)
}

/// Format a `TypeNode`'s head + generics for the error message
/// without dragging the analyzer's full `format_type` in.
fn type_head_for_display(t: &TypeNode) -> String {
    if t.path.is_empty() {
        return "<empty>".to_string();
    }
    let mut s = t.path.join(".");
    if !t.generics.is_empty() {
        s.push('<');
        for (i, g) in t.generics.iter().enumerate() {
            if i > 0 {
                s.push_str(", ");
            }
            s.push_str(&type_head_for_display(g));
        }
        s.push('>');
    }
    s
}

/// Recursive expression lowering. Appends ops to `out` in postfix /
/// stack order.
fn lower_expr(
    expr: &Expr,
    range: TokenRange,
    locals: &[LocalBinding],
    out: &mut Vec<TaggedOp>,
    tstack: &mut Vec<IrType>,
) -> Result<(), LoweringError> {
    match expr {
        Expr::Int(i) => {
            out.push(TaggedOp {
                op: Op::ConstI64(*i),
                range,
            });
            tstack.push(IrType::I64);
            Ok(())
        }
        Expr::Float(f) => {
            out.push(TaggedOp {
                op: Op::ConstF64(OrderedFloat::from(f.into_inner())),
                range,
            });
            tstack.push(IrType::F64);
            Ok(())
        }
        Expr::Variable(path) => lower_variable(path, range, locals, out, tstack),
        Expr::Binary(op, lhs, rhs) => {
            let ir_op_ctor = arithmetic_op_ctor(*op)
                .ok_or(LoweringError::UnsupportedOperator { op: *op, range })?;
            lower_expr(&lhs.expr, lhs.range, locals, out, tstack)?;
            lower_expr(&rhs.expr, rhs.range, locals, out, tstack)?;
            let rhs_ty = tstack
                .pop()
                .ok_or(LoweringError::UnsupportedOperator { op: *op, range })?;
            let lhs_ty = tstack
                .pop()
                .ok_or(LoweringError::UnsupportedOperator { op: *op, range })?;
            if lhs_ty != rhs_ty {
                return Err(LoweringError::UnsupportedOperator { op: *op, range });
            }
            // Only Int / Float pairs support arithmetic. Bool / Null
            // / I32 here would be a stage upstream rejecting a body
            // that the analyzer should have caught; we double-check
            // so a hand-crafted IR can't sneak through.
            if !matches!(lhs_ty, IrType::I64 | IrType::F64) {
                return Err(LoweringError::UnsupportedOperator { op: *op, range });
            }
            // Mod on F64 is unsupported (wasm has no `f64.rem`).
            if lhs_ty == IrType::F64 && matches!(op, Operator::Mod) {
                return Err(LoweringError::UnsupportedOperator { op: *op, range });
            }
            out.push(TaggedOp {
                op: ir_op_ctor(lhs_ty),
                range,
            });
            tstack.push(lhs_ty);
            Ok(())
        }
        _ => Err(LoweringError::UnsupportedExpr {
            kind: expr.kind().to_string(),
            range,
        }),
    }
}

/// Map a parser `Operator` to the matching IR op constructor.
fn arithmetic_op_ctor(op: Operator) -> Option<fn(IrType) -> Op> {
    match op {
        Operator::Add => Some(Op::Add),
        Operator::Sub => Some(Op::Sub),
        Operator::Mul => Some(Op::Mul),
        Operator::Div => Some(Op::Div),
        Operator::Mod => Some(Op::Mod),
        _ => None,
    }
}

/// Lower a bare-identifier reference. Phase 2.b looks the name up in
/// the `#main` parameter index and emits a typed [`Op::LoadField`]
/// reading from the `in_buf`.
fn lower_variable(
    path: &[TokenKey],
    range: TokenRange,
    locals: &[LocalBinding],
    out: &mut Vec<TaggedOp>,
    tstack: &mut Vec<IrType>,
) -> Result<(), LoweringError> {
    if path.len() != 1 {
        return Err(LoweringError::UnsupportedExpr {
            kind: "Variable(multi-segment)".to_string(),
            range,
        });
    }
    let name = match &path[0] {
        TokenKey::String(s, _, _) => s.as_str(),
        TokenKey::Index(_, _) | TokenKey::Dummy | TokenKey::Spread(_) | TokenKey::Dynamic(_, _) => {
            return Err(LoweringError::UnsupportedExpr {
                kind: "Variable(non-string-key)".to_string(),
                range,
            });
        }
    };
    let binding = locals.iter().find(|b| b.name == name).ok_or_else(|| {
        LoweringError::UnresolvedVariable {
            name: name.to_string(),
            range,
        }
    })?;
    out.push(TaggedOp {
        op: Op::LoadField {
            offset: binding.offset,
            ty: binding.ty,
        },
        range,
    });
    tstack.push(binding.ty);
    Ok(())
}
