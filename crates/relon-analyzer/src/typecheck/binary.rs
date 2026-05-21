//! Type-check sub-module: binary-mismatch / const-fold / strict-fn-call
//! checks.
//!
//! Three small but discrete checks share this file:
//!
//! * `check_binary_mismatch` — Stage 1 inference-driven
//!   `StaticTypeMismatch` for binary operators whose operands have
//!   statically-incompatible types.
//! * `check_const_fold` — Stage 5 literal-arithmetic folder that
//!   surfaces `ConstDivisionByZero` and `ConstNumericOverflow`. The
//!   caller uses the return value to stop recursing past a folded
//!   subtree, avoiding duplicate diagnostics on overlapping nodes.
//! * `check_strict_fn_call` — v1.3 `NativeFnSignatureMissing`: under
//!   strict mode, an FnCall whose name resolves *only* through the
//!   host's native fn allowlist (no static signature) leaks `Any`
//!   into the type flow, so we ask the user to install a signature.
//!
//! Co-located because they all run during the `visit_internal`
//! Binary/Unary/FnCall arms and share the same `tree.diagnostics`
//! push pattern.

use super::Walker;
use crate::diagnostic::{span_of, Diagnostic};
use crate::infer::{self, infer_type};
use crate::sig::lookup_signature;
use relon_parser::{Node, TokenKey};

impl<'a> Walker<'a> {
    /// Push a `StaticTypeMismatch` when a binary operator is applied to
    /// statically-incompatible operand types (`1 + "hello"`, `true * 3`,
    /// …). Unknown operands (`Any`, `None`) silently pass — runtime
    /// keeps owning the authoritative call.
    pub(super) fn check_binary_mismatch(
        &mut self,
        node: &Node,
        op: relon_parser::Operator,
        left: &Node,
        right: &Node,
        field_name: Option<&str>,
    ) {
        let scope = self.build_type_scope();
        let Some(lt) = infer_type(left, &scope) else {
            return;
        };
        let Some(rt) = infer_type(right, &scope) else {
            return;
        };
        if !infer::binary_known_invalid(op, &lt, &rt) {
            return;
        }
        self.tree.diagnostics.push(Diagnostic::StaticTypeMismatch {
            field: field_name.unwrap_or("_").to_string(),
            expected: format!("{op:?} operands compatible"),
            found: format!("{} {op:?} {}", lt.name(), rt.name()),
            range: span_of(node.range),
        });
    }

    /// Stage 5: try to fold `node` as a literal arithmetic expression.
    /// Pushes `ConstDivisionByZero` or `ConstNumericOverflow` when the
    /// fold trips, and returns `true` so the caller can stop recursing
    /// (avoiding duplicate diagnostics on overlapping subtrees).
    pub(super) fn check_const_fold(&mut self, node: &Node) -> bool {
        match crate::const_fold::try_fold(node) {
            Err(crate::const_fold::FoldError::DivByZero(range)) => {
                self.tree.diagnostics.push(Diagnostic::ConstDivisionByZero {
                    range: span_of(range),
                });
                true
            }
            Err(crate::const_fold::FoldError::Overflow { op, range }) => {
                self.tree
                    .diagnostics
                    .push(Diagnostic::ConstNumericOverflow {
                        op: format!("{op:?}"),
                        range: span_of(range),
                    });
                true
            }
            // Whole subtree folds cleanly to a constant — nothing to
            // diagnose. Fully-folded nodes still get walked normally
            // (caller decides) so any sibling diagnostics stay live.
            Ok(_) => false,
        }
    }

    /// v1.3: under strict mode, an FnCall whose name resolves *only*
    /// through the host's native fn allowlist (no static signature
    /// describing its return) leaks `Any` into the surrounding type
    /// flow. Surface a `NativeFnSignatureMissing` so the user adds a
    /// signature or stops calling the unknown native fn.
    pub(super) fn check_strict_fn_call(&mut self, node: &relon_parser::Node, path: &[TokenKey]) {
        if !self.tree.strict_mode {
            return;
        }
        // Pipe RHS: same suppression as `check_fn_call` — the static
        // arity is intentionally one short and we already validated the
        // pipe's operands.
        if self.pipe_target_calls.contains(&node.id) {
            return;
        }
        let TokenKey::String(name, _, _) = path.first().unwrap_or(&TokenKey::Dummy) else {
            return;
        };
        // Single-segment only — multi-segment paths go through
        // `resolve_call_signature`, which already returns `None` for
        // anything we can't classify (and that's covered by the host
        // signature check above).
        if path.len() != 1 {
            return;
        }
        // If we have a static signature, the return type is known —
        // strict mode is satisfied.
        if lookup_signature(name, self.tree, &self.tree.host_fn_signatures).is_some() {
            return;
        }
        // The fn is *registered* (allowlisted) but lacks a signature.
        // That's the precise "native fn whose return we can't see"
        // shape strict mode forbids.
        if self.tree.host_fn_names.contains(name) {
            self.tree
                .diagnostics
                .push(Diagnostic::NativeFnSignatureMissing {
                    fn_name: name.clone(),
                    range: span_of(node.range),
                });
        }
    }
}
