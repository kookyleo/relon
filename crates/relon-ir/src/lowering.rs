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

/// Per-function lowering state shared across the recursive walk.
///
/// Phase 3.a introduces user-let bindings (`where { name: value }`)
/// and inline const literals (`true` / `"hello"` / `[1, 2, 3]`); each
/// of those needs a per-function counter the recursive walker hands
/// back to codegen. The context bundles them together with the
/// virtual operand stack so the `lower_expr` family of functions keep
/// a single tail-call shape.
#[derive(Debug)]
struct LowerCtx<'a> {
    /// `#main` parameter bindings (offset + IR type) used to resolve
    /// bare identifier references.
    params: &'a [LocalBinding],
    /// Stack of in-scope let bindings. Pushed when entering a
    /// `where { ... }` block and popped after the inner expression
    /// lowers — gives us lexical scoping for free.
    lets: Vec<LetBinding>,
    /// Next per-function let-local index. Stable across `where`
    /// blocks so a shadowed name still picks up a fresh wasm local.
    next_let_idx: u32,
    /// Next per-module constant index for [`Op::ConstString`].
    next_string_idx: u32,
    /// Next per-module constant index for [`Op::ConstListInt`].
    next_list_int_idx: u32,
    /// Output op stream. Appended to in postfix / stack order.
    out: Vec<TaggedOp>,
    /// Virtual operand stack tracking the IR type each pushed value
    /// has. Lets us validate arithmetic / store tags without a
    /// separate analysis pass.
    tstack: Vec<IrType>,
}

#[derive(Debug, Clone)]
struct LetBinding {
    name: String,
    idx: u32,
    ty: IrType,
}

impl<'a> LowerCtx<'a> {
    fn new(params: &'a [LocalBinding]) -> Self {
        Self {
            params,
            lets: Vec::new(),
            next_let_idx: 0,
            next_string_idx: 0,
            next_list_int_idx: 0,
            out: Vec::new(),
            tstack: Vec::new(),
        }
    }
}

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

    // Walk the body into a single op stream + virtual stack via the
    // per-function lowering context. Phase 3.a's let-bindings + const
    // literals piggy-back on `LowerCtx` for their counters.
    let mut ctx = LowerCtx::new(&locals);
    lower_expr(&root.expr, root.range, &mut ctx)?;

    // Trailing StoreField for the single root return value. Pops the
    // top stack entry — codegen will translate this to `local.get
    // $out_ptr; <value>; <store>.offset=N`.
    let ret_offset = return_layout
        .fields
        .first()
        .map(|f| f.offset as u32)
        .unwrap_or(0);
    let ret_ir_ty = type_repr_to_ir_type(&return_schema.fields[0].ty)?;
    ctx.out.push(TaggedOp {
        op: Op::StoreField {
            offset: ret_offset,
            ty: ret_ir_ty,
        },
        range: sig.range,
    });
    ctx.tstack.pop();

    // `Op::Return` keeps its v1.beta meaning: end of function. The
    // codegen pass synthesises the actual wasm `return` (it pushes
    // `bytes_written` and emits the implicit `end`).
    ctx.out.push(TaggedOp {
        op: Op::Return,
        range: sig.range,
    });
    let body = ctx.out;

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
///
/// Phase 3.a widens the return surface to `String` / `List<Int>`
/// alongside the v1 scalars. The codegen pass copies the tail-area
/// record bytes into `out_buf` at a `$tail_cursor` past the fixed
/// area; the fixed-area pointer slot stores a buffer-relative
/// offset so the host's `BufferReader` can decode it uniformly.
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

/// Map a parser [`TypeNode`] to a canonical [`TypeRepr`].
///
/// Phase 2.c surface:
///   * `Int` / `Float` / `Bool` / `Null` — the v1 scalar leaves.
///   * `String` — pointer-indirect leaf.
///   * `List<Int>` — pointer-indirect leaf with i64 elements. Other
///     list element types still return `None` so the schema build
///     rejects them with `UnsupportedTypeInMain`.
fn type_node_to_canonical(t: &TypeNode) -> Option<TypeRepr> {
    if t.path.len() != 1 || t.variant_fields.is_some() {
        return None;
    }
    let head = t.path[0].as_str();
    match (head, t.generics.as_slice()) {
        ("Int", []) => Some(TypeRepr::Int),
        ("Float", []) => Some(TypeRepr::Float),
        ("Bool", []) => Some(TypeRepr::Bool),
        ("Null", []) => Some(TypeRepr::Null),
        ("String", []) => Some(TypeRepr::String),
        ("List", [elem]) => {
            // Phase 2.c only opens `List<Int>`; everything else
            // stays out of the surface so the layout pass doesn't
            // have to model String / Float / Bool element tail areas.
            let inner = type_node_to_canonical(elem)?;
            if matches!(inner, TypeRepr::Int) {
                Some(TypeRepr::List {
                    element: Box::new(inner),
                })
            } else {
                None
            }
        }
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
        TypeRepr::String => Ok(IrType::String),
        TypeRepr::List { element } if matches!(element.as_ref(), TypeRepr::Int) => {
            Ok(IrType::ListInt)
        }
        // Composite / list-of-other types are rejected upstream
        // during schema build; this branch fires only for a directly
        // hand-crafted IR.
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

/// Recursive expression lowering. Appends ops to `ctx.out` in postfix /
/// stack order.
fn lower_expr(expr: &Expr, range: TokenRange, ctx: &mut LowerCtx<'_>) -> Result<(), LoweringError> {
    match expr {
        Expr::Bool(b) => {
            ctx.out.push(TaggedOp {
                op: Op::ConstBool(*b),
                range,
            });
            ctx.tstack.push(IrType::Bool);
            Ok(())
        }
        Expr::Int(i) => {
            ctx.out.push(TaggedOp {
                op: Op::ConstI64(*i),
                range,
            });
            ctx.tstack.push(IrType::I64);
            Ok(())
        }
        Expr::Float(f) => {
            ctx.out.push(TaggedOp {
                op: Op::ConstF64(OrderedFloat::from(f.into_inner())),
                range,
            });
            ctx.tstack.push(IrType::F64);
            Ok(())
        }
        Expr::String(s) => {
            // Each ConstString gets a fresh module-unique index the
            // codegen layout pass uses to look up its absolute
            // memory offset. The bytes ride along on the op so a
            // future cross-function dedup pass can hash them; for
            // Phase 3.a we keep it simple and let codegen materialise
            // every occurrence.
            let idx = ctx.next_string_idx;
            ctx.next_string_idx += 1;
            ctx.out.push(TaggedOp {
                op: Op::ConstString {
                    idx,
                    value: s.clone(),
                },
                range,
            });
            ctx.tstack.push(IrType::String);
            Ok(())
        }
        Expr::List(items) => {
            // Phase 3.a only opens List<Int> literals — everything
            // else falls through to the generic "unsupported" branch.
            // We inspect each element's expression up front; any
            // non-Int literal rejects the list, so a hand-written
            // `[1, 2.0, 3]` surfaces as a lowering error rather than
            // as a confusing type mismatch at codegen.
            let mut elements: Vec<i64> = Vec::with_capacity(items.len());
            for node in items {
                match &*node.expr {
                    Expr::Int(v) => elements.push(*v),
                    _ => {
                        return Err(LoweringError::UnsupportedExpr {
                            kind: format!("List(non-Int element `{}`)", node.expr.kind()),
                            range: node.range,
                        });
                    }
                }
            }
            let idx = ctx.next_list_int_idx;
            ctx.next_list_int_idx += 1;
            ctx.out.push(TaggedOp {
                op: Op::ConstListInt { idx, elements },
                range,
            });
            ctx.tstack.push(IrType::ListInt);
            Ok(())
        }
        Expr::Variable(path) => lower_variable(path, range, ctx),
        Expr::Binary(op, lhs, rhs) => lower_binary(*op, lhs, rhs, range, ctx),
        Expr::Ternary { cond, then, els } => lower_ternary(cond, then, els, range, ctx),
        Expr::Where { expr, bindings } => lower_where(expr, bindings, range, ctx),
        _ => Err(LoweringError::UnsupportedExpr {
            kind: expr.kind().to_string(),
            range,
        }),
    }
}

/// Lower a `<expr> where { name: value, ... }` form by emitting one
/// `LetSet` per binding (in declaration order) and then lowering
/// `expr` with the names in scope.
///
/// Each binding picks up a fresh per-function let-local index;
/// shadowing is supported (a re-declared name uses a new index, and
/// the outer binding becomes unreachable inside the inner expression
/// but stays valid after). We restore the outer scope after the
/// inner body lowers so the trailing `StoreField` sees a clean
/// virtual stack.
fn lower_where(
    expr: &Node,
    bindings: &Node,
    range: TokenRange,
    ctx: &mut LowerCtx<'_>,
) -> Result<(), LoweringError> {
    let pairs = match &*bindings.expr {
        Expr::Dict(pairs) => pairs,
        _ => {
            return Err(LoweringError::UnsupportedExpr {
                kind: format!("Where(bindings={})", bindings.expr.kind()),
                range,
            });
        }
    };
    let saved_lets_len = ctx.lets.len();
    for (key, value) in pairs {
        let name = match key {
            TokenKey::String(s, _, _) => s.clone(),
            _ => {
                return Err(LoweringError::UnsupportedExpr {
                    kind: "Where(non-string-binding-key)".to_string(),
                    range,
                });
            }
        };
        lower_expr(&value.expr, value.range, ctx)?;
        let value_ty = ctx.tstack.pop().ok_or(LoweringError::UnsupportedExpr {
            kind: "Where(binding-empty-stack)".to_string(),
            range: value.range,
        })?;
        let idx = ctx.next_let_idx;
        ctx.next_let_idx += 1;
        ctx.out.push(TaggedOp {
            op: Op::LetSet { idx, ty: value_ty },
            range: value.range,
        });
        ctx.lets.push(LetBinding {
            name,
            idx,
            ty: value_ty,
        });
    }
    lower_expr(&expr.expr, expr.range, ctx)?;
    // Pop only the bindings we pushed in this frame — preserves
    // outer-scope lets for sibling expressions.
    ctx.lets.truncate(saved_lets_len);
    Ok(())
}

/// Lower one binary expression. Splits the arithmetic + comparison
/// paths so each surface keeps its rejection rules explicit.
fn lower_binary(
    op: Operator,
    lhs: &Node,
    rhs: &Node,
    range: TokenRange,
    ctx: &mut LowerCtx<'_>,
) -> Result<(), LoweringError> {
    if let Some(ir_op_ctor) = arithmetic_op_ctor(op) {
        lower_expr(&lhs.expr, lhs.range, ctx)?;
        lower_expr(&rhs.expr, rhs.range, ctx)?;
        let rhs_ty = ctx
            .tstack
            .pop()
            .ok_or(LoweringError::UnsupportedOperator { op, range })?;
        let lhs_ty = ctx
            .tstack
            .pop()
            .ok_or(LoweringError::UnsupportedOperator { op, range })?;
        if lhs_ty != rhs_ty {
            return Err(LoweringError::UnsupportedOperator { op, range });
        }
        // Only Int / Float pairs support arithmetic.
        if !matches!(lhs_ty, IrType::I64 | IrType::F64) {
            return Err(LoweringError::UnsupportedOperator { op, range });
        }
        // Mod on F64 is unsupported (wasm has no `f64.rem`).
        if lhs_ty == IrType::F64 && matches!(op, Operator::Mod) {
            return Err(LoweringError::UnsupportedOperator { op, range });
        }
        ctx.out.push(TaggedOp {
            op: ir_op_ctor(lhs_ty),
            range,
        });
        ctx.tstack.push(lhs_ty);
        return Ok(());
    }
    if let Some(cmp_ctor) = comparison_op_ctor(op) {
        lower_expr(&lhs.expr, lhs.range, ctx)?;
        lower_expr(&rhs.expr, rhs.range, ctx)?;
        let rhs_ty = ctx
            .tstack
            .pop()
            .ok_or(LoweringError::UnsupportedOperator { op, range })?;
        let lhs_ty = ctx
            .tstack
            .pop()
            .ok_or(LoweringError::UnsupportedOperator { op, range })?;
        if lhs_ty != rhs_ty {
            return Err(LoweringError::UnsupportedOperator { op, range });
        }
        // Phase 2.c supports comparisons on Int / Float / Bool /
        // Null. Bool / Null only support `==` / `!=`; ordering
        // (`<`, `<=`, `>`, `>=`) is rejected at the comparison
        // codegen layer too, but we surface it here as a lowering
        // error so the message stays user-facing.
        match (lhs_ty, op) {
            (IrType::I64 | IrType::F64, _) => {}
            (IrType::Bool, Operator::Eq | Operator::Ne) => {}
            (IrType::Null, Operator::Eq | Operator::Ne) => {}
            _ => return Err(LoweringError::UnsupportedOperator { op, range }),
        }
        ctx.out.push(TaggedOp {
            op: cmp_ctor(lhs_ty),
            range,
        });
        ctx.tstack.push(IrType::Bool);
        return Ok(());
    }
    Err(LoweringError::UnsupportedOperator { op, range })
}

/// Lower a ternary `cond ? then : els` into `Op::If`. The branches
/// must agree on the IR type they push; the condition must lower to
/// `IrType::Bool`.
fn lower_ternary(
    cond: &Node,
    then: &Node,
    els: &Node,
    range: TokenRange,
    ctx: &mut LowerCtx<'_>,
) -> Result<(), LoweringError> {
    // Lower the condition in the outer tstack so a body like
    // `(a > 0) ? ... : ...` accurately reports its Bool result.
    lower_expr(&cond.expr, cond.range, ctx)?;
    let cond_ty = ctx.tstack.pop().ok_or(LoweringError::UnsupportedExpr {
        kind: "Ternary(cond)".to_string(),
        range,
    })?;
    if cond_ty != IrType::Bool {
        return Err(LoweringError::IfConditionNotBool {
            got: cond_ty,
            range,
        });
    }
    // Lower each branch into its own sub-stream + isolated tstack so
    // an inner expression spilling extra values onto the stack is
    // caught here rather than leaking into the outer body. The
    // branch sub-ctx inherits the outer `lets` scope + counters so
    // a `let`-bound name remains visible inside the arm and any
    // const-literal index issued by the arm doesn't collide with
    // the outer module.
    let then_ops = lower_branch(then, range, ctx)?;
    let then_ty = then_ops.1;
    let then_body = then_ops.0;

    let else_ops = lower_branch(els, range, ctx)?;
    let else_ty = else_ops.1;
    let else_body = else_ops.0;

    if then_ty != else_ty {
        return Err(LoweringError::IfBranchTypeMismatch {
            then_ty,
            else_ty,
            range,
        });
    }
    let result_ty = then_ty;
    ctx.out.push(TaggedOp {
        op: Op::If {
            result_ty,
            then_body,
            else_body,
        },
        range,
    });
    ctx.tstack.push(result_ty);
    Ok(())
}

/// Lower one branch of a ternary into a self-contained op stream +
/// the type it leaves on top of its private virtual stack.
///
/// The branch shares the outer `LowerCtx`'s let-local counter / const
/// indices so a const literal inside the branch picks up a unique
/// per-module index. Only the `out` stream is rerouted; the `tstack`
/// is replaced with a fresh one so the branch can be checked in
/// isolation.
fn lower_branch(
    node: &Node,
    range: TokenRange,
    parent: &mut LowerCtx<'_>,
) -> Result<(Vec<TaggedOp>, IrType), LoweringError> {
    let saved_out = std::mem::take(&mut parent.out);
    let saved_stack = std::mem::take(&mut parent.tstack);
    lower_expr(&node.expr, node.range, parent)?;
    let branch_ops = std::mem::replace(&mut parent.out, saved_out);
    let branch_stack = std::mem::replace(&mut parent.tstack, saved_stack);
    if branch_stack.len() != 1 {
        return Err(LoweringError::UnsupportedExpr {
            kind: format!("Ternary(branch-stack={})", branch_stack.len()),
            range,
        });
    }
    Ok((branch_ops, branch_stack[0]))
}

/// Map a parser comparison `Operator` to the matching IR op
/// constructor. Returns `None` for non-comparison ops.
fn comparison_op_ctor(op: Operator) -> Option<fn(IrType) -> Op> {
    match op {
        Operator::Eq => Some(Op::Eq),
        Operator::Ne => Some(Op::Ne),
        Operator::Lt => Some(Op::Lt),
        Operator::Le => Some(Op::Le),
        Operator::Gt => Some(Op::Gt),
        Operator::Ge => Some(Op::Ge),
        _ => None,
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

/// Lower a bare-identifier reference. Phase 3.a checks the user-let
/// scope first (innermost shadow wins) and falls back to the `#main`
/// parameter index. The let-binding hit emits an [`Op::LetGet`]; the
/// param hit emits a typed [`Op::LoadField`] reading from the `in_buf`.
fn lower_variable(
    path: &[TokenKey],
    range: TokenRange,
    ctx: &mut LowerCtx<'_>,
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
    // Let-binding lookup walks innermost-first (`rev`) so a shadowed
    // name resolves to the most recently pushed binding.
    if let Some(b) = ctx.lets.iter().rev().find(|b| b.name == name) {
        ctx.out.push(TaggedOp {
            op: Op::LetGet {
                idx: b.idx,
                ty: b.ty,
            },
            range,
        });
        ctx.tstack.push(b.ty);
        return Ok(());
    }
    let binding = ctx.params.iter().find(|b| b.name == name).ok_or_else(|| {
        LoweringError::UnresolvedVariable {
            name: name.to_string(),
            range,
        }
    })?;
    // Pointer-indirect leaves (`String` / `ListInt`) get their own op
    // tag so a later phase can hang String / List operations off them
    // without re-deriving the type from the slot.
    let op = match binding.ty {
        IrType::String => Op::LoadStringPtr {
            offset: binding.offset,
        },
        IrType::ListInt => Op::LoadListIntPtr {
            offset: binding.offset,
        },
        _ => Op::LoadField {
            offset: binding.offset,
            ty: binding.ty,
        },
    };
    ctx.out.push(TaggedOp { op, range });
    ctx.tstack.push(binding.ty);
    Ok(())
}
