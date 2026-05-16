//! `AnalyzedTree` -> [`Module`] lowering for Phase 1.beta.
//!
//! Surface accepted (v1.beta intentionally narrow):
//!
//! * `#main(Int x [, ...]) -> Int` or `#main(Float x [, ...]) -> Float`
//!   on the entry module. Mixed-type signatures are rejected at
//!   codegen (this pass only validates each scalar slot individually).
//! * Root expression is the function body. Allowed shapes:
//!   - `Expr::Int(i)`           -> [`Op::ConstI64`]
//!   - `Expr::Float(f)`         -> [`Op::ConstF64`]
//!   - `Expr::Variable(path)`   -> [`Op::LocalGet`] when the
//!     single-segment head names a declared `#main` parameter
//!   - `Expr::Binary(op, l, r)` with `op` in `{Add, Sub, Mul, Div, Mod}`
//!     -> recursive lower of `l`, `r`, then the matching [`Op`] tagged
//!     with the operands' [`IrType`]
//!
//! Everything else fails the lowering with the appropriate
//! [`LoweringError`] variant. The IR is deliberately tighter than
//! the analyzer's accepted surface so that codegen can stay
//! mechanical — Phase 1.gamma / 2 / 3 widen the accepted shapes one
//! variant at a time.

use ordered_float::OrderedFloat;
use relon_analyzer::main_sig::MainSignature;
use relon_analyzer::tree::AnalyzedTree;
use relon_analyzer::workspace::WorkspaceTree;
use relon_parser::{Expr, Node, Operator, TokenKey, TokenRange, TypeNode};

use crate::error::LoweringError;
use crate::ir::{Func, IrType, Module, Op, TaggedOp};

/// Lower the entry module of a workspace to a v1.beta [`Module`].
///
/// Looks up `entry_module` in `ws.modules` and the matching root
/// node in `ws.nodes`, then delegates to [`lower_workspace_single`].
/// Phase 1.beta only handles single-entry workspaces; multi-module
/// import chains are not yet plumbed through the IR (no cross-module
/// function symbols exist in v1.beta).
pub fn lower_workspace(ws: &WorkspaceTree, entry_module: &str) -> Result<Module, LoweringError> {
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

/// Single-file lowering convenience used by the smoke test and by
/// callers that haven't built a full [`WorkspaceTree`]. Treats the
/// supplied `(tree, root)` pair as a one-module workspace with id
/// `"main"` for diagnostic messages.
///
/// This is the Phase 1.beta primary entry point. Once multi-module
/// support lands the smoke test will switch to [`lower_workspace`];
/// the single-module helper stays for unit-test use and host
/// scenarios where the workspace machinery is overkill.
pub fn lower_workspace_single(tree: &AnalyzedTree, root: &Node) -> Result<Module, LoweringError> {
    lower_workspace_single_with_module(tree, root, "main")
}

fn lower_workspace_single_with_module(
    tree: &AnalyzedTree,
    root: &Node,
    module_id: &str,
) -> Result<Module, LoweringError> {
    let sig = tree
        .main_signature
        .as_ref()
        .ok_or_else(|| LoweringError::MissingMain {
            module: module_id.to_string(),
        })?;

    let param_types = lower_main_params(sig)?;
    let ret_type = lower_main_return(sig)?;

    // Bind each parameter name to its declaration-order local index
    // and IR type. v1.beta has no other scope, so this single table
    // suffices for the whole body walk.
    let mut local_idx: Vec<(&str, u32, IrType)> = Vec::with_capacity(sig.params.len());
    for (i, p) in sig.params.iter().enumerate() {
        let ty = param_types[i];
        local_idx.push((p.name.as_str(), i as u32, ty));
    }

    // Virtual stack mirroring the wasm value stack at lowering time.
    // Each entry records the IR type that op produced, so the
    // binary-arithmetic path can pick the right `IrType` tag for
    // `Add` / `Sub` / `Mul` / `Div` / `Mod` without re-walking
    // children. Built once, popped/pushed in lock-step with `body`.
    let mut body = Vec::new();
    let mut tstack: Vec<IrType> = Vec::new();
    lower_expr(&root.expr, root.range, &local_idx, &mut body, &mut tstack)?;
    body.push(TaggedOp {
        op: Op::Return,
        range: sig.range,
    });
    tstack.pop();

    let func = Func {
        name: "run_main".to_string(),
        params: param_types,
        ret: ret_type,
        body,
        range: sig.range,
    };

    Ok(Module {
        funcs: vec![func],
        entry_func_index: Some(0),
    })
}

/// Map each `#main` parameter's declared type to an [`IrType`].
/// Rejects any type head other than `Int` / `Float`.
fn lower_main_params(sig: &MainSignature) -> Result<Vec<IrType>, LoweringError> {
    sig.params
        .iter()
        .map(|p| ir_type_of(&p.type_node))
        .collect()
}

/// Map the optional `#main -> Type` return annotation to an
/// [`IrType`]. v1.beta requires the annotation (no inference) so the
/// codegen pass can size the wasm result type up front.
fn lower_main_return(sig: &MainSignature) -> Result<IrType, LoweringError> {
    let rt = sig
        .return_type
        .as_ref()
        .ok_or_else(|| LoweringError::UnsupportedTypeInMain {
            type_name: "<missing>".to_string(),
            range: sig.range,
        })?;
    ir_type_of(rt)
}

/// Map a single [`TypeNode`] to an [`IrType`]. Only single-segment
/// `Int` / `Float` are accepted; everything else is reported as
/// `UnsupportedTypeInMain`.
fn ir_type_of(t: &TypeNode) -> Result<IrType, LoweringError> {
    let bad = || LoweringError::UnsupportedTypeInMain {
        type_name: type_head_for_display(t),
        range: t.range,
    };
    if t.path.len() != 1 || !t.generics.is_empty() || t.variant_fields.is_some() {
        return Err(bad());
    }
    match t.path[0].as_str() {
        "Int" => Ok(IrType::I64),
        "Float" => Ok(IrType::F64),
        _ => Err(bad()),
    }
}

/// Format a `TypeNode`'s head + generics for the error message
/// without dragging the analyzer's full `format_type` in. Enough
/// detail for the user to spot which annotation is the offender.
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
/// stack order. `locals` is the `#main` parameter name -> local
/// index table; `range` is the enclosing node's source range, used
/// when the leaf op doesn't carry its own (e.g. literals).
fn lower_expr(
    expr: &Expr,
    range: TokenRange,
    locals: &[(&str, u32, IrType)],
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
            // Lower LHS, then RHS — wasm stack order matches source
            // order for binary arithmetic (`(a op b)` -> `[a, b, op]`).
            lower_expr(&lhs.expr, lhs.range, locals, out, tstack)?;
            lower_expr(&rhs.expr, rhs.range, locals, out, tstack)?;
            // Pop RHS then LHS off the virtual stack.
            let rhs_ty = tstack
                .pop()
                .ok_or(LoweringError::UnsupportedOperator { op: *op, range })?;
            let lhs_ty = tstack
                .pop()
                .ok_or(LoweringError::UnsupportedOperator { op: *op, range })?;
            // v1.beta requires both operands share the IR type. We
            // record the rejection here (rather than in codegen) so
            // the error names the source expression instead of a
            // post-lowering wasm offset. Mixed-numeric bodies must
            // wait for the implicit-promotion design in Phase 2+.
            if lhs_ty != rhs_ty {
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
        // Everything else is out of v1.beta scope. Report with the
        // parser's stable `kind()` name so the message stays useful
        // even as new variants land.
        _ => Err(LoweringError::UnsupportedExpr {
            kind: expr.kind().to_string(),
            range,
        }),
    }
}

/// Map a parser `Operator` to the matching IR op constructor. The
/// constructor still takes an [`IrType`] tag — the lowering pass
/// supplies it after walking the operands.
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

/// Lower a bare-identifier reference to `LocalGet`. v1.beta only
/// resolves single-segment heads against the `#main` parameter
/// table — multi-segment paths (`a.b`) are out of scope here and
/// surface as `UnsupportedExpr`.
fn lower_variable(
    path: &[TokenKey],
    range: TokenRange,
    locals: &[(&str, u32, IrType)],
    out: &mut Vec<TaggedOp>,
    tstack: &mut Vec<IrType>,
) -> Result<(), LoweringError> {
    if path.len() != 1 {
        // Defer to the generic "unsupported expression" report —
        // dotted paths require analyzer-resolved references which
        // the v1.beta IR doesn't yet model.
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
    let (idx, ty) = locals
        .iter()
        .find_map(|(n, i, t)| (*n == name).then_some((*i, *t)))
        .ok_or_else(|| LoweringError::UnresolvedVariable {
            name: name.to_string(),
            range,
        })?;
    out.push(TaggedOp {
        op: Op::LocalGet(idx),
        range,
    });
    tstack.push(ty);
    Ok(())
}
