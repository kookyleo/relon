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
use relon_analyzer::schema::SchemaDef;
use relon_analyzer::tree::AnalyzedTree;
use relon_analyzer::workspace::WorkspaceTree;
use relon_eval_api::layout::{FieldKind, OffsetTable, SchemaLayout};
use relon_eval_api::schema_canonical::{Field, Schema, TypeRepr};
use relon_parser::{Expr, Node, Operator, TokenKey, TokenRange, TypeNode};
use std::collections::HashMap;

use crate::error::LoweringError;
use crate::ir::{Func, IrType, Module, Op, TaggedOp};

/// Per-function lowering state shared across the recursive walk.
///
/// Phase 3.a introduces user-let bindings (`where { name: value }`)
/// and inline const literals (`true` / `"hello"` / `[1, 2, 3]`); each
/// of those needs a per-function counter the recursive walker hands
/// back to codegen. Phase 3.b extends the context with a record-local
/// counter (one per dict literal currently being constructed) and a
/// schema resolver so nested branded dict fields find their canonical
/// shape without re-walking the analyzer table.
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
    /// Next per-function record-local index. Each
    /// [`Op::AllocRootRecord`] / [`Op::AllocSubRecord`] hands out a
    /// fresh local so nested dicts under construction don't clobber
    /// their parent's base offset.
    next_record_idx: u32,
    /// Output op stream. Appended to in postfix / stack order.
    out: Vec<TaggedOp>,
    /// Virtual operand stack tracking the IR type each pushed value
    /// has. Lets us validate arithmetic / store tags without a
    /// separate analysis pass.
    tstack: Vec<IrType>,
    /// Schema-name → analyzer-side `SchemaDef` resolver. Phase 3.b
    /// dict literal lowering consults it when a field's declared
    /// type-hint names a user-declared schema (the nested dict
    /// case).
    schema_resolver: SchemaResolver<'a>,
}

/// Name → `SchemaDef` lookup built once per `lower_workspace_*` call
/// from the analyzer's `tree.root_schemas` + `tree.schemas`. Cheap to
/// construct — only the schema declarations participate, not every
/// node in the source tree.
#[derive(Debug, Clone)]
struct SchemaResolver<'a> {
    by_name: HashMap<&'a str, &'a SchemaDef>,
}

impl<'a> SchemaResolver<'a> {
    fn new(tree: &'a AnalyzedTree) -> Self {
        let mut by_name: HashMap<&'a str, &'a SchemaDef> = HashMap::new();
        // Root-level `#schema X ...` directives are the standard
        // surface for top-level brand declarations; the schema body
        // lives in `tree.schemas` keyed by the body node id. We pick
        // the SchemaDef out of `tree.schemas` to get the analyzed
        // field shape.
        for decl in &tree.root_schemas {
            if let Some(def) = tree.schemas.get(&decl.schema_node.id) {
                by_name.insert(decl.name.as_str(), def);
            }
        }
        // Dict-field `#schema X { ... }` declarations also surface in
        // `tree.schemas`. Walk every entry that has a non-None name
        // and add it to the map (later declarations of the same name
        // are kept earliest — analyzer-level diagnostics already
        // catch duplicates).
        for def in tree.schemas.values() {
            if let Some(name) = &def.name {
                by_name.entry(name.as_str()).or_insert(def);
            }
        }
        Self { by_name }
    }

    fn resolve(&self, name: &str) -> Option<&'a SchemaDef> {
        self.by_name.get(name).copied()
    }
}

#[derive(Debug, Clone)]
struct LetBinding {
    name: String,
    idx: u32,
    ty: IrType,
}

impl<'a> LowerCtx<'a> {
    fn new(params: &'a [LocalBinding], schema_resolver: SchemaResolver<'a>) -> Self {
        Self {
            params,
            lets: Vec::new(),
            next_let_idx: 0,
            next_string_idx: 0,
            next_list_int_idx: 0,
            next_record_idx: 0,
            out: Vec::new(),
            tstack: Vec::new(),
            schema_resolver,
        }
    }

    /// Allocate a fresh per-function record-local index used by
    /// [`Op::AllocRootRecord`] / [`Op::AllocSubRecord`] /
    /// [`Op::StoreFieldAtRecord`] / [`Op::PushRecordBase`].
    fn alloc_record_local(&mut self) -> u32 {
        let idx = self.next_record_idx;
        self.next_record_idx += 1;
        idx
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

    // Detect whether the return type names a user-declared schema.
    // When it does, the body must evaluate to a (possibly defaulted)
    // dict literal whose canonical shape comes from the schema; the
    // synthesised `Ret` schema in that case is structurally
    // equivalent to a 1-field record whose `value` is the user
    // schema, but the wasm-level layout pads the *user schema* into
    // the root return area directly (no extra pointer slot).
    let resolver = SchemaResolver::new(tree);
    let user_return_schema =
        resolve_return_user_schema(sig.return_type.as_ref(), &resolver)?;

    // Build the canonical-form schemas for in_buf and out_buf, then
    // compute the offset table for the param schema so each
    // `Variable(x)` reference can be lowered to a typed LoadField.
    let main_schema = build_main_params_schema(sig)?;
    let return_schema = if let Some(ref user_schema) = user_return_schema {
        // The dict-return path lays the user schema directly into the
        // fixed area — the host reads it back with the same
        // `BufferReader::new(...)` it would use for a hand-built dict
        // input. No `value` wrapping.
        user_schema.clone()
    } else {
        build_main_return_schema(sig)?
    };
    let main_layout = SchemaLayout::offsets_for(&main_schema)?;
    let return_layout = SchemaLayout::offsets_for(&return_schema)?;

    // Bind each parameter name to its (offset, IR type) so the body
    // walk can lower bare-identifier references to a typed LoadField
    // without a second pass over the layout pass.
    let locals = build_local_index(sig, &main_schema, &main_layout)?;

    // Walk the body into a single op stream + virtual stack via the
    // per-function lowering context. Phase 3.a's let-bindings + const
    // literals piggy-back on `LowerCtx` for their counters.
    let mut ctx = LowerCtx::new(&locals, resolver);

    if let Some(ref user_schema) = user_return_schema {
        // Branded dict-return path: emit `AllocRootRecord` + the
        // per-field stores into the root record, then `Return`.
        // Top-level dict expression must be a `Expr::Dict(...)` (the
        // brand is contributed by the return type).
        let dict_pairs = match &*root.expr {
            Expr::Dict(pairs) => pairs.as_slice(),
            _ => {
                return Err(LoweringError::UnsupportedExpr {
                    kind: format!(
                        "Body-of-branded-#main must be a dict literal, got `{}`",
                        root.expr.kind()
                    ),
                    range: root.range,
                });
            }
        };
        let record_local = ctx.alloc_record_local();
        ctx.out.push(TaggedOp {
            op: Op::AllocRootRecord {
                record_local_idx: record_local,
            },
            range: root.range,
        });
        lower_dict_into_record(
            user_schema,
            &return_layout,
            dict_pairs,
            root.range,
            record_local,
            &mut ctx,
        )?;
    } else {
        // Scalar-return path: existing v1 shape.
        lower_expr(&root.expr, root.range, &mut ctx)?;

        // Trailing StoreField for the single root return value. Pops
        // the top stack entry — codegen will translate this to
        // `local.get $out_ptr; <value>; <store>.offset=N`.
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
    }

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

// =====================================================================
// Phase 3.b: dict-literal lowering helpers.
// =====================================================================

/// If `return_type` names a user-declared schema (single-segment
/// TypeNode with no generics), return its canonical-form `Schema`
/// recursively flattened. Returns `Ok(None)` when the return type is
/// not a user schema (the v1 scalar-return path stays in effect).
fn resolve_return_user_schema(
    return_type: Option<&TypeNode>,
    resolver: &SchemaResolver<'_>,
) -> Result<Option<Schema>, LoweringError> {
    let Some(t) = return_type else {
        return Ok(None);
    };
    if t.path.len() != 1 || !t.generics.is_empty() || t.variant_fields.is_some() {
        return Ok(None);
    }
    let name = &t.path[0];
    // Built-in scalar / wrapper heads stay on the scalar path even
    // though they would also fail the user-schema lookup below.
    if matches!(
        name.as_str(),
        "Int" | "Float" | "Bool" | "Null" | "String" | "List" | "Option" | "Result"
    ) {
        return Ok(None);
    }
    let Some(def) = resolver.resolve(name) else {
        return Ok(None);
    };
    let mut stack: Vec<&str> = Vec::new();
    let schema = canonical_schema_from_def(def, resolver, &mut stack, t.range)?;
    Ok(Some(schema))
}

/// Recursively build a canonical [`Schema`] from a [`SchemaDef`].
///
/// `stack` carries the in-progress schema names so a cycle in nested
/// declarations (`#schema A { B b: * }`, `#schema B { A a: * }`)
/// surfaces as [`LoweringError::CyclicFieldDependency`] rather than
/// infinite recursion. Cycles in nested-schema *types* are
/// independent of the per-schema field-default cycle the topological
/// emit pass detects later — both surface the same error variant so
/// users get a uniform diagnostic for either shape.
fn canonical_schema_from_def<'a>(
    def: &'a SchemaDef,
    resolver: &SchemaResolver<'a>,
    stack: &mut Vec<&'a str>,
    range: TokenRange,
) -> Result<Schema, LoweringError> {
    let name = def
        .name
        .as_deref()
        .ok_or_else(|| LoweringError::UnsupportedExpr {
            kind: "anonymous-nested-schema".to_string(),
            range,
        })?;
    if stack.iter().any(|n| *n == name) {
        let mut cycle: Vec<String> = stack.iter().map(|s| s.to_string()).collect();
        cycle.push(name.to_string());
        return Err(LoweringError::CyclicFieldDependency {
            schema: name.to_string(),
            cycle,
            range,
        });
    }
    stack.push(name);
    let mut fields = Vec::with_capacity(def.fields.len());
    for f in &def.fields {
        let ty_node = f
            .type_hint
            .as_ref()
            .ok_or_else(|| LoweringError::UnsupportedFieldType {
                schema: name.to_string(),
                field: f.name.clone(),
                ty: "<untyped>".to_string(),
                range: f.value_range,
            })?;
        let ty = canonical_type_repr(ty_node, resolver, stack, f.value_range)?;
        fields.push(Field {
            name: f.name.clone(),
            ty,
            default: None,
        });
    }
    stack.pop();
    Ok(Schema {
        name: name.to_string(),
        generics: def.generics.clone(),
        fields,
    })
}

/// Convert a `TypeNode` into the canonical `TypeRepr`. Supports the
/// Phase 3.b surface (scalar leaves, `String`, `List<Int>`, nested
/// user schemas); everything else surfaces
/// [`LoweringError::UnsupportedFieldType`] at the call site that
/// owns the field name.
fn canonical_type_repr<'a>(
    ty: &TypeNode,
    resolver: &SchemaResolver<'a>,
    stack: &mut Vec<&'a str>,
    range: TokenRange,
) -> Result<TypeRepr, LoweringError> {
    if ty.path.len() != 1 || ty.variant_fields.is_some() {
        return Err(LoweringError::UnsupportedFieldType {
            schema: stack.last().copied().unwrap_or("?").to_string(),
            field: "?".to_string(),
            ty: type_head_for_display(ty),
            range,
        });
    }
    let head = ty.path[0].as_str();
    match (head, ty.generics.as_slice()) {
        ("Int", []) => Ok(TypeRepr::Int),
        ("Float", []) => Ok(TypeRepr::Float),
        ("Bool", []) => Ok(TypeRepr::Bool),
        ("Null", []) => Ok(TypeRepr::Null),
        ("String", []) => Ok(TypeRepr::String),
        ("List", [elem]) => {
            let inner = canonical_type_repr(elem, resolver, stack, range)?;
            if matches!(inner, TypeRepr::Int) {
                Ok(TypeRepr::List {
                    element: Box::new(inner),
                })
            } else {
                Err(LoweringError::UnsupportedFieldType {
                    schema: stack.last().copied().unwrap_or("?").to_string(),
                    field: "?".to_string(),
                    ty: type_head_for_display(ty),
                    range,
                })
            }
        }
        _ => {
            // Treat any single-segment head as a user-schema reference.
            let Some(def) = resolver.resolve(head) else {
                return Err(LoweringError::UnsupportedFieldType {
                    schema: stack.last().copied().unwrap_or("?").to_string(),
                    field: "?".to_string(),
                    ty: head.to_string(),
                    range,
                });
            };
            let sub = canonical_schema_from_def(def, resolver, stack, range)?;
            Ok(TypeRepr::Schema {
                schema: Box::new(sub),
            })
        }
    }
}

/// Decide topological order in which a schema's fields must be
/// emitted, given the set of user-provided field names. A field
/// that's user-provided stops dependency tracking for itself (the
/// user value wins and is independent of the schema default).
/// Otherwise the default expression's referenced sibling fields
/// become incoming edges.
///
/// Returns `Err(CyclicFieldDependency)` when the dependency graph on
/// the **needs-defaults** subset has a cycle. User-provided values
/// can break a cycle: a schema `{ Int a: b, Int b: a }` constructed
/// as `{ a: 1 }` is fine — only `b` needs defaulting and its
/// reference to `a` is satisfied by the user value.
fn topo_order_fields(
    schema_name: &str,
    def: &SchemaDef,
    user_provided: &std::collections::HashSet<&str>,
    range: TokenRange,
) -> Result<Vec<usize>, LoweringError> {
    // Collect per-field referenced sibling field names. Only fields
    // we'll evaluate via their default expression need this — others
    // get the user-supplied value and contribute no edges.
    let mut deps: Vec<Vec<usize>> = vec![Vec::new(); def.fields.len()];
    let name_to_idx: HashMap<&str, usize> = def
        .fields
        .iter()
        .enumerate()
        .map(|(i, f)| (f.name.as_str(), i))
        .collect();
    for (i, field) in def.fields.iter().enumerate() {
        if user_provided.contains(field.name.as_str()) {
            // User-supplied: ignore its default expression.
            continue;
        }
        if field.is_wildcard {
            // `Int x: *` declares the field with no default value.
            // The dict literal must provide it.
            return Err(LoweringError::MissingFieldNoDefault {
                schema: schema_name.to_string(),
                field: field.name.clone(),
                range,
            });
        }
        collect_field_refs(&field.value_node.expr, &name_to_idx, &mut deps[i]);
        // Sanity: every reference must resolve to a sibling field.
        // We can't know yet which references are unresolved at this
        // step — `collect_field_refs` only walks bare-identifier
        // references; an unresolved one was already a diagnostic at
        // analyzer time. We still surface the case where a default
        // expression names a sibling that doesn't exist as
        // `UnknownFieldReferenceInDefault`. The walk runs the same
        // resolution again and reports the first miss.
        check_field_default_refs_resolvable(
            schema_name,
            &field.name,
            &field.value_node.expr,
            &name_to_idx,
        )?;
    }
    // Kahn-style topological sort. `incoming[i]` = number of edges
    // pointing into i. A field `j` evaluated from a default that
    // references `i` requires `i` ready first → edge `i → j`. We
    // build the graph from `deps[i] = list of i's prerequisite
    // field indices` ⇒ for every `r ∈ deps[i]` add edge `r → i`,
    // i.e. incoming[i] += 1 for each ref.
    let n = def.fields.len();
    let mut incoming = vec![0usize; n];
    let mut outgoing: Vec<Vec<usize>> = vec![Vec::new(); n];
    for (i, refs) in deps.iter().enumerate() {
        for &r in refs {
            outgoing[r].push(i);
            incoming[i] += 1;
        }
    }
    let mut order: Vec<usize> = Vec::with_capacity(n);
    let mut ready: std::collections::VecDeque<usize> = std::collections::VecDeque::new();
    for i in 0..n {
        if incoming[i] == 0 {
            ready.push_back(i);
        }
    }
    while let Some(i) = ready.pop_front() {
        order.push(i);
        for &j in &outgoing[i] {
            incoming[j] -= 1;
            if incoming[j] == 0 {
                ready.push_back(j);
            }
        }
    }
    if order.len() != n {
        // Find one cycle path for the error message via DFS.
        let cycle = find_cycle_path(&outgoing, def, &incoming);
        return Err(LoweringError::CyclicFieldDependency {
            schema: schema_name.to_string(),
            cycle,
            range,
        });
    }
    Ok(order)
}

/// DFS through the remaining (non-zero-incoming) field-default graph
/// looking for a cycle path. The caller has already established at
/// least one cycle exists (Kahn's algorithm couldn't drain the
/// graph); we build a representative path so the user sees the field
/// chain that participates.
fn find_cycle_path(
    outgoing: &[Vec<usize>],
    def: &SchemaDef,
    incoming: &[usize],
) -> Vec<String> {
    let n = outgoing.len();
    let mut visited = vec![false; n];
    let mut on_stack = vec![false; n];
    let mut stack: Vec<usize> = Vec::new();
    for start in 0..n {
        if visited[start] || incoming[start] == 0 {
            continue;
        }
        if let Some(cycle) = dfs_find_cycle(start, outgoing, &mut visited, &mut on_stack, &mut stack)
        {
            return cycle
                .iter()
                .map(|&i| def.fields[i].name.clone())
                .collect();
        }
    }
    // Fallback: should never happen given the caller's invariant.
    Vec::new()
}

fn dfs_find_cycle(
    start: usize,
    outgoing: &[Vec<usize>],
    visited: &mut [bool],
    on_stack: &mut [bool],
    stack: &mut Vec<usize>,
) -> Option<Vec<usize>> {
    visited[start] = true;
    on_stack[start] = true;
    stack.push(start);
    for &next in &outgoing[start] {
        if on_stack[next] {
            // Cycle: emit the portion of the stack from `next` to the
            // current node, plus `next` repeated at the end for a
            // readable arrow chain.
            let entry = stack.iter().position(|&i| i == next).unwrap_or(0);
            let mut cycle = stack[entry..].to_vec();
            cycle.push(next);
            on_stack[start] = false;
            stack.pop();
            return Some(cycle);
        }
        if !visited[next] {
            if let Some(c) = dfs_find_cycle(next, outgoing, visited, on_stack, stack) {
                on_stack[start] = false;
                stack.pop();
                return Some(c);
            }
        }
    }
    on_stack[start] = false;
    stack.pop();
    None
}

/// Walk a default expression and record any bare-identifier
/// references whose head matches a sibling field. Multi-segment
/// references (`a.b.c`) only contribute the head segment — if the
/// head resolves to a sibling, the rest of the path is treated as a
/// post-access we don't track.
fn collect_field_refs(expr: &Expr, name_to_idx: &HashMap<&str, usize>, out: &mut Vec<usize>) {
    match expr {
        Expr::Variable(path) | Expr::Reference { path, .. } => {
            if let Some(TokenKey::String(name, _, _)) = path.first() {
                if let Some(&idx) = name_to_idx.get(name.as_str()) {
                    if !out.contains(&idx) {
                        out.push(idx);
                    }
                }
            }
        }
        Expr::Binary(_, a, b) => {
            collect_field_refs(&a.expr, name_to_idx, out);
            collect_field_refs(&b.expr, name_to_idx, out);
        }
        Expr::Unary(_, inner) => collect_field_refs(&inner.expr, name_to_idx, out),
        Expr::Ternary { cond, then, els } => {
            collect_field_refs(&cond.expr, name_to_idx, out);
            collect_field_refs(&then.expr, name_to_idx, out);
            collect_field_refs(&els.expr, name_to_idx, out);
        }
        Expr::List(items) => {
            for n in items {
                collect_field_refs(&n.expr, name_to_idx, out);
            }
        }
        Expr::Dict(pairs) => {
            for (_, v) in pairs {
                collect_field_refs(&v.expr, name_to_idx, out);
            }
        }
        Expr::Where { expr, bindings } => {
            collect_field_refs(&bindings.expr, name_to_idx, out);
            collect_field_refs(&expr.expr, name_to_idx, out);
        }
        Expr::FnCall { args, .. } => {
            for a in args {
                collect_field_refs(&a.value.expr, name_to_idx, out);
            }
        }
        // Other shapes don't matter for the Phase 3.b surface (they
        // either fail to lower upstream or don't reference siblings).
        _ => {}
    }
}

/// Recursive walker mirroring [`collect_field_refs`] that reports the
/// first bare-identifier reference whose head doesn't resolve to a
/// sibling field. Lowering uses this to surface
/// `UnknownFieldReferenceInDefault` instead of letting the inner
/// `lower_expr` fall through with an `UnresolvedVariable` (which the
/// user would see as a confusing diagnostic about `#main` params).
fn check_field_default_refs_resolvable(
    schema: &str,
    field: &str,
    expr: &Expr,
    name_to_idx: &HashMap<&str, usize>,
) -> Result<(), LoweringError> {
    let mut stack: Vec<&Expr> = vec![expr];
    while let Some(e) = stack.pop() {
        match e {
            Expr::Variable(path) | Expr::Reference { path, .. } => {
                if let Some(TokenKey::String(name, range, _)) = path.first() {
                    if !name_to_idx.contains_key(name.as_str()) {
                        return Err(LoweringError::UnknownFieldReferenceInDefault {
                            schema: schema.to_string(),
                            field: field.to_string(),
                            referenced: name.clone(),
                            range: *range,
                        });
                    }
                }
            }
            Expr::Binary(_, a, b) => {
                stack.push(&a.expr);
                stack.push(&b.expr);
            }
            Expr::Unary(_, inner) => stack.push(&inner.expr),
            Expr::Ternary { cond, then, els } => {
                stack.push(&cond.expr);
                stack.push(&then.expr);
                stack.push(&els.expr);
            }
            Expr::List(items) => {
                for n in items {
                    stack.push(&n.expr);
                }
            }
            Expr::Dict(pairs) => {
                for (_, v) in pairs {
                    stack.push(&v.expr);
                }
            }
            Expr::Where { expr, bindings } => {
                stack.push(&expr.expr);
                stack.push(&bindings.expr);
            }
            Expr::FnCall { args, .. } => {
                for a in args {
                    stack.push(&a.value.expr);
                }
            }
            _ => {}
        }
    }
    Ok(())
}

/// Map a Phase 3.b `TypeRepr` to its corresponding `IrType`. Distinct
/// from [`type_repr_to_ir_type`] above only in that it accepts the
/// `Schema { ... }` variant (treated as a pointer-indirect i32).
fn type_repr_to_ir_type_dict(t: &TypeRepr) -> IrType {
    match t {
        TypeRepr::Int => IrType::I64,
        TypeRepr::Float => IrType::F64,
        TypeRepr::Bool => IrType::Bool,
        TypeRepr::Null => IrType::Null,
        TypeRepr::String => IrType::String,
        TypeRepr::List { .. } => IrType::ListInt,
        // Nested branded schema rides a pointer slot — same wasm
        // representation as String / ListInt.
        TypeRepr::Schema { .. } => IrType::I32,
        TypeRepr::Option { .. } | TypeRepr::Result { .. } => IrType::I32,
    }
}

/// Lower a dict literal into the in-construction record at
/// `record_local`. The schema describes the record's shape; the
/// `OffsetTable` carries field offsets; `dict_pairs` are the user-
/// supplied fields.
///
/// Steps:
///   1. Resolve user pairs to a (name, expr) map.
///   2. Compute topological emit order from the schema defaults.
///   3. For each field in topo order, lower the value expression
///      (either user-provided or schema default) and emit the
///      matching `StoreFieldAtRecord` op.
///
/// Nested branded dicts recurse via the same helper after allocating
/// a fresh sub-record.
fn lower_dict_into_record(
    schema: &Schema,
    layout: &OffsetTable,
    dict_pairs: &[(TokenKey, Node)],
    range: TokenRange,
    record_local: u32,
    ctx: &mut LowerCtx<'_>,
) -> Result<(), LoweringError> {
    // Resolve the analyzer-side SchemaDef so default expressions can
    // be lowered. The canonical Schema we have here only carries
    // field name + type — defaults live on the SchemaDef.
    let def = ctx.schema_resolver.resolve(&schema.name).ok_or_else(|| {
        LoweringError::UnknownSchemaBrand {
            name: schema.name.clone(),
            range,
        }
    })?;

    // Build name → user-expr map. Reject duplicate keys.
    let mut user_values: HashMap<String, &Node> = HashMap::new();
    for (key, value) in dict_pairs {
        let TokenKey::String(name, _, _) = key else {
            return Err(LoweringError::UnsupportedExpr {
                kind: format!("Dict(non-string-key in branded dict for `{}`)", schema.name),
                range,
            });
        };
        // Schema must declare this field.
        if !schema.fields.iter().any(|f| &f.name == name) {
            return Err(LoweringError::UnsupportedFieldType {
                schema: schema.name.clone(),
                field: name.clone(),
                ty: format!("(unknown field, not declared on `{}`)", schema.name),
                range,
            });
        }
        user_values.insert(name.clone(), value);
    }

    let user_set: std::collections::HashSet<&str> =
        user_values.keys().map(|s| s.as_str()).collect();
    let order = topo_order_fields(&schema.name, def, &user_set, range)?;

    for idx in order {
        let canonical_field = &schema.fields[idx];
        let layout_field = layout.fields.iter().find(|fo| fo.name == canonical_field.name).ok_or_else(|| {
            LoweringError::UnsupportedFieldType {
                schema: schema.name.clone(),
                field: canonical_field.name.clone(),
                ty: "<layout-miss>".to_string(),
                range,
            }
        })?;
        let field_range = def.fields[idx].value_range;
        // Lower the value expression (user-supplied or schema default).
        if let Some(user_value) = user_values.get(canonical_field.name.as_str()) {
            lower_dict_field_value(
                schema,
                idx,
                user_value,
                user_value.range,
                ctx,
            )?;
        } else {
            // Schema default. Re-bind `#main` params; let-scope is
            // shared with the surrounding body (defaults sit at the
            // schema-instantiation site, not inside an isolated
            // scope, so referenced field names already resolved
            // through the topo-ordered store ops above are reachable
            // via `LetGet` over the per-field default-local — see
            // below for the sibling lookup mechanism).
            //
            // For Phase 3.b sibling field references are resolved
            // through the lowered value expression directly: the
            // default expression `a + 1` lowers to `LetGet { idx:
            // sibling_let_of_a }`. That trick requires us to keep a
            // per-record map from field name → let-local index when
            // a field's value is consumed by a later default. The
            // simpler shape: emit a `LetSet` for every default-
            // evaluated field so the wasm side caches the value and
            // a later default can read it back via `LetGet`.
            lower_dict_default(
                &schema.name,
                idx,
                def,
                ctx,
                field_range,
            )?;
        }
        // Stack now holds the field's value (with type matching the
        // canonical Field). Emit the StoreFieldAtRecord.
        let ir_ty = type_repr_to_ir_type_dict(&canonical_field.ty);
        let store_ty = match layout_field.kind {
            FieldKind::Inline { .. } => ir_ty,
            // Pointer-indirect fields all store as an i32 pointer.
            FieldKind::PointerIndirect { .. } => IrType::I32,
        };
        let top = ctx.tstack.pop().ok_or_else(|| LoweringError::UnsupportedExpr {
            kind: format!(
                "Dict field `{}` of `{}` produced no value",
                canonical_field.name, schema.name
            ),
            range,
        })?;
        if top.wasm_slot() != store_ty.wasm_slot() {
            return Err(LoweringError::UnsupportedFieldType {
                schema: schema.name.clone(),
                field: canonical_field.name.clone(),
                ty: format!("got {:?}, expected {:?}", top, store_ty),
                range,
            });
        }
        ctx.out.push(TaggedOp {
            op: Op::StoreFieldAtRecord {
                record_local_idx: record_local,
                offset: layout_field.offset as u32,
                ty: store_ty,
            },
            range: field_range,
        });

        // Cache the freshly-stored value into a let-local so later
        // sibling defaults can `LetGet` it. We only do this for
        // fields the schema's defaults actually reference — but
        // computing that subset requires a second pass. For the
        // Phase 3.b surface we cache *every* field, accepting the
        // unused-local overhead in exchange for simpler bookkeeping.
        // The wasm engine drops unused locals at JIT time.
        //
        // The cache mechanism: re-lower the value into a `LetSet` so
        // the value lives in a wasm local, then map the field name
        // to that let-idx. Because the value has already been
        // consumed by `StoreFieldAtRecord`, we re-emit a `LetGet`
        // that pulls the *stored slot* back through `LoadField`-like
        // semantics — but that's expensive. Simpler: stash the
        // value in a let *before* the StoreFieldAtRecord.
        //
        // Reorder: emit value → LetSet (cache) → LetGet (push back)
        // → StoreFieldAtRecord. The implementation does this by
        // splicing the LetSet/Get pair just before the store.
        //
        // We thread the cache via `ctx`'s let-binding stack so the
        // existing `Variable(name)` lookup resolves to the cached
        // value when a later default emits a reference.
        //
        // Performed below.
        let bound_ty = top;
        let let_idx = ctx.next_let_idx;
        ctx.next_let_idx += 1;
        // Reach into the just-emitted op stream: splice
        // [LetSet, LetGet] right before the trailing
        // StoreFieldAtRecord. The current top-of-`out` is that
        // StoreFieldAtRecord (we pushed it just above) — pop, push
        // the cache pair, push it back. Cheaper than re-walking.
        let store_op = ctx.out.pop().expect("StoreFieldAtRecord just pushed");
        ctx.out.push(TaggedOp {
            op: Op::LetSet {
                idx: let_idx,
                ty: bound_ty,
            },
            range: field_range,
        });
        ctx.out.push(TaggedOp {
            op: Op::LetGet {
                idx: let_idx,
                ty: bound_ty,
            },
            range: field_range,
        });
        ctx.out.push(store_op);
        ctx.lets.push(LetBinding {
            name: canonical_field.name.clone(),
            idx: let_idx,
            ty: bound_ty,
        });
    }

    // Pop the field-name let bindings we pushed so the surrounding
    // scope sees its original let stack.
    let drop_count = schema.fields.len();
    let new_len = ctx.lets.len().saturating_sub(drop_count);
    ctx.lets.truncate(new_len);

    Ok(())
}

/// Lower one user-supplied dict-literal field value. Field `idx`
/// describes the schema-side canonical type; the value's source-side
/// expression decides which lowering arm to take.
fn lower_dict_field_value(
    schema: &Schema,
    field_idx: usize,
    value: &Node,
    range: TokenRange,
    ctx: &mut LowerCtx<'_>,
) -> Result<(), LoweringError> {
    let canonical = &schema.fields[field_idx];
    match (&canonical.ty, &*value.expr) {
        (TypeRepr::Schema { schema: sub_schema }, Expr::Dict(pairs)) => {
            // Nested branded dict. Allocate a sub-record, recurse,
            // then push the sub-record's base offset for the parent's
            // pointer slot.
            let sub_layout = SchemaLayout::offsets_for(sub_schema)?;
            let record_local = ctx.alloc_record_local();
            ctx.out.push(TaggedOp {
                op: Op::AllocSubRecord {
                    record_local_idx: record_local,
                    root_size: sub_layout.root_size as u32,
                    root_align: sub_layout.root_align as u32,
                },
                range,
            });
            lower_dict_into_record(
                sub_schema,
                &sub_layout,
                pairs,
                range,
                record_local,
                ctx,
            )?;
            // Push the sub-record base so the parent's pointer-slot
            // store can consume it.
            ctx.out.push(TaggedOp {
                op: Op::PushRecordBase {
                    record_local_idx: record_local,
                },
                range,
            });
            ctx.tstack.push(IrType::I32);
            Ok(())
        }
        (TypeRepr::String, _) | (TypeRepr::List { .. }, _) => {
            // Recursively lower the value to produce an absolute
            // pointer (ConstString / ConstListInt / LoadStringPtr /
            // ...). Then copy the record into the parent's tail
            // area and push the buffer-relative offset.
            lower_expr(&value.expr, range, ctx)?;
            // Top of stack is an absolute address. Emit the tail-
            // record memcpy.
            let popped = ctx.tstack.pop().ok_or(LoweringError::UnsupportedExpr {
                kind: "Dict(field-value-stack-empty)".to_string(),
                range,
            })?;
            // Cross-check the IR type against the declared field
            // type — saves a confusing codegen-time failure when the
            // dict field expects a String but the value lowered to
            // List<Int>.
            let expected_ir = match &canonical.ty {
                TypeRepr::String => IrType::String,
                TypeRepr::List { .. } => IrType::ListInt,
                _ => unreachable!(),
            };
            if popped != expected_ir {
                return Err(LoweringError::UnsupportedFieldType {
                    schema: schema.name.clone(),
                    field: canonical.name.clone(),
                    ty: format!("expected {expected_ir:?}, got {popped:?}"),
                    range,
                });
            }
            ctx.out.push(TaggedOp {
                op: Op::EmitTailRecordFromAbsoluteAddr { ty: expected_ir },
                range,
            });
            ctx.tstack.push(IrType::I32);
            Ok(())
        }
        // Scalar leaves: just lower the value. The
        // StoreFieldAtRecord ranges already align.
        _ => lower_expr(&value.expr, range, ctx),
    }
}

/// Lower a schema-default expression for field `field_idx`. The
/// default's body lives on the analyzer-side `SchemaFieldDef::value_node`;
/// we re-route the existing `lower_expr` recursion at that body so
/// references to sibling fields hit the just-pushed let-bindings
/// (we cache each evaluated field into a let-local in
/// [`lower_dict_into_record`]).
fn lower_dict_default(
    schema_name: &str,
    field_idx: usize,
    def: &SchemaDef,
    ctx: &mut LowerCtx<'_>,
    range: TokenRange,
) -> Result<(), LoweringError> {
    let field = &def.fields[field_idx];
    if field.is_wildcard {
        return Err(LoweringError::MissingFieldNoDefault {
            schema: schema_name.to_string(),
            field: field.name.clone(),
            range,
        });
    }
    // Lower the default expression with the surrounding lets in
    // scope. The let-stack already carries `<prior-field-name> →
    // value` bindings because the topological order placed
    // dependencies first.
    let value_node = &field.value_node;
    lower_expr(&value_node.expr, value_node.range, ctx)?;
    Ok(())
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
