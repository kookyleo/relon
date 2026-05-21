//! Type-check sub-module: reference / variable / path-tail / strict-path
//! checks.
//!
//! Four methods extend [`super::Walker`]:
//!
//! * `check_unresolved_ref` — Stage 2.5/2.6/v1.5 entry for `Reference`
//!   nodes. Resolves head, runs path-tail validation, escalates under
//!   strict mode.
//! * `check_unresolved_var` — Same shape for `Variable` nodes, with
//!   the extra host-fn / stdlib name allowlist consulted before
//!   flagging.
//! * `check_strict_path` — v1.4 inference-driven walker that emits
//!   `UnknownReferenceType` for `Resolved(Any)` (strict-only) and
//!   `UnknownStep` (cross-mode) outcomes.
//! * `check_path_tail` — Stage 2.6 field-narrowing walker that
//!   classifies each segment against the running type
//!   (`Schema(_)` → field set lookup; `Dict<V>` → value type lift;
//!   anything else → defer to runtime).
//!
//! All four touch the same `references` / `node_index` / `schema_index`
//! state and share the strict-mode escalation branch shape, so they
//! co-locate cleanly.

use super::Walker;
use crate::diagnostic::{span_of, Diagnostic};
use crate::infer::{self, InferredType};
use crate::resolve::path_head;
use relon_parser::{Expr, Node, RefBase, TokenKey};

impl<'a> Walker<'a> {
    pub(super) fn check_unresolved_ref(&mut self, node: &Node, base: &RefBase, path: &[TokenKey]) {
        if self.tree.references.contains_key(&node.id) {
            // Head resolved — but multi-segment paths still need a tail
            // walk (Stage 2.6) to catch `obj.b` where `obj` exists but
            // `b` doesn't.
            self.check_path_tail(node, path);
            // v1.4: under strict mode, the tail walk additionally
            // surfaces `UnknownReferenceType` whenever the inference
            // walker can prove a step lands on an opaque value (head
            // is `Any` / a non-schema-non-dict head). Run it even for
            // single-segment paths: the head's resolved target may
            // still carry no static type.
            self.check_strict_path(node, path);
            return;
        }
        // Static analyzer skipped this reference. Decide whether to
        // flag it.
        match base {
            // List-context refs depend on iteration state — never flag.
            RefBase::This | RefBase::Prev | RefBase::Next | RefBase::Index => return,
            _ => {}
        }
        let Some(name) = path_head(path) else { return };
        if self.dynamic_save(&name) {
            return;
        }
        self.tree.diagnostics.push(Diagnostic::UnresolvedReference {
            name: name.clone(),
            range: span_of(node.range),
        });
        // v1.5: strict mode escalates the head-unresolved case from a
        // warning-level `UnresolvedReference` to an error-level
        // `UnknownReferenceType { path: [name, ...] }` so the
        // evaluator never reaches a runtime-only "name not found"
        // path. The warning still fires for non-strict analyzers / IDE
        // hints; strict callers see the matching error too.
        if self.tree.strict_mode {
            let segs = infer::path_segments(path);
            self.tree
                .diagnostics
                .push(Diagnostic::UnknownReferenceType {
                    name,
                    path: segs,
                    range: span_of(node.range),
                });
        }
    }

    pub(super) fn check_unresolved_var(&mut self, node: &Node, path: &[TokenKey]) {
        if self.tree.references.contains_key(&node.id) {
            // Same Stage 2.6 tail walk as `check_unresolved_ref`.
            self.check_path_tail(node, path);
            // v1.4 strict: also run the inference-driven tail walk so
            // multi-segment opaque steps (`o.unknown`, `f.x.y`) emit
            // `UnknownReferenceType` rather than silently leaking
            // `Any`.
            self.check_strict_path(node, path);
            return;
        }
        let Some(name) = path_head(path) else { return };
        // Variables also resolve against function names registered
        // by the host (stdlib like `range`, `len`, ...). The analyzer
        // consults the evaluator's hardcoded stdlib name set plus the
        // host-supplied `host_fn_names` (Stage 2.4) before flagging.
        if self.dynamic_save(&name) || self.is_known_fn(&name) {
            return;
        }
        self.tree.diagnostics.push(Diagnostic::UnresolvedReference {
            name: name.clone(),
            range: span_of(node.range),
        });
        // v1.5: strict-mode escalation, same as `check_unresolved_ref`.
        if self.tree.strict_mode {
            let segs = infer::path_segments(path);
            self.tree
                .diagnostics
                .push(Diagnostic::UnknownReferenceType {
                    name,
                    path: segs,
                    range: span_of(node.range),
                });
        }
    }

    /// Run the inference-driven path walker over `path` and report a
    /// [`Diagnostic::UnknownReferenceType`] whenever a segment can't
    /// be classified. Splits the responsibility:
    ///
    /// * `UnknownStep` (e.g. `o.unknown` where `o: Schema` has no
    ///   `unknown` field, or `int_field.something` descending past a
    ///   leaf) is a **static error** — the analyzer has positive
    ///   knowledge the path is broken. Fired cross-mode.
    /// * `Resolved(Any)` (e.g. path runs into an untyped closure
    ///   parameter whose type the analyzer literally can't see) is a
    ///   **strict-only** finding — the analyzer doesn't *know* the
    ///   path is broken, it just refuses to keep going under strict.
    /// * `UnknownHead` is owned by the resolution-side
    ///   `UnresolvedReference` diagnostic, so we don't report here.
    pub(super) fn check_strict_path(&mut self, node: &Node, path: &[TokenKey]) {
        let segs = infer::path_segments(path);
        if segs.is_empty() {
            return;
        }
        // v1.5 deduplication: when the path is a single segment whose
        // head names an *untyped closure parameter* on the active
        // scope stack, the `ClosureParamTypeMissing` walker
        // already pinned a diagnostic on the param's declaration. Don't
        // double-fire `UnknownReferenceType` on every body reference
        // — the user already knows which param to annotate.
        if segs.len() == 1 {
            let head = &segs[0];
            for frame in self.scope_stack.iter().rev() {
                if frame.closure_params.contains_key(head)
                    && !frame.closure_param_types.contains_key(head)
                {
                    return;
                }
            }
        }
        let scope = self.build_type_scope();
        match infer::walk_path(path, &scope) {
            infer::PathTailOutcome::Resolved(InferredType::Any) => {
                // Head / mid-walk landed on `Any`. The analyzer can't
                // verify or refute the path — strict mode refuses the
                // leak, non-strict keeps the silent fallback.
                if !self.tree.strict_mode {
                    return;
                }
                let last = segs.last().cloned().unwrap_or_default();
                self.tree
                    .diagnostics
                    .push(Diagnostic::UnknownReferenceType {
                        name: last,
                        path: segs,
                        range: span_of(node.range),
                    });
            }
            infer::PathTailOutcome::Resolved(_) => {
                // Fully classified — both modes are satisfied.
            }
            infer::PathTailOutcome::UnknownStep { at_segment, .. } => {
                // Cross-mode: the walker has positive knowledge that
                // the path is broken. The runtime would fail too.
                let name = segs.get(at_segment).cloned().unwrap_or_default();
                self.tree
                    .diagnostics
                    .push(Diagnostic::UnknownReferenceType {
                        name,
                        path: segs,
                        range: span_of(node.range),
                    });
            }
            infer::PathTailOutcome::UnknownHead => {
                // The head wasn't visible. Resolution-side diagnostics
                // (UnresolvedReference) own the head case; we don't
                // double-report here.
            }
        }
    }

    /// Stage 2.6: walk the rest of a multi-segment `path` (after the
    /// head bound to a known field / param). For each segment, narrow
    /// the running type:
    ///
    /// * `Schema(name)` → segment must be a declared field of that
    ///   schema; otherwise push `UnresolvedReference("obj.field")`.
    /// * `Dict(value_ty)` / `Optional(...)` → continue with the inner
    ///   type. Without per-key info we can't validate the segment, so
    ///   we just walk past it.
    /// * `Any` / FnCall result / unknown / closure-param-without-type
    ///   → silent fall-back (defer to runtime).
    pub(super) fn check_path_tail(&mut self, node: &Node, path: &[TokenKey]) {
        if path.len() < 2 {
            return;
        }
        let scope = self.build_type_scope();
        let head = match path.first() {
            Some(TokenKey::String(s, _, _)) => s.clone(),
            _ => return,
        };

        // Stage 2.6 fast-path for dict-literal field lookup: if the
        // head resolves to a sibling whose value is a `Dict` literal
        // *without* an explicit schema type hint, we can validate the
        // first tail segment directly against the literal's keys. This
        // catches `{ obj: { a: 1 }, x: obj.b }` where `Dict(Any)` would
        // otherwise be too coarse to flag the missing `b`.
        if let Some(target_node) = self.lookup_field_node(&head) {
            if target_node.type_hint.is_none() {
                if let Expr::Dict(inner_pairs) = &*target_node.expr {
                    if let Some(TokenKey::String(seg_name, _, _)) = path.get(1) {
                        let has_key = inner_pairs
                            .iter()
                            .any(|(k, _)| matches!(k, TokenKey::String(n, _, _) if n == seg_name));
                        // Any non-dict-literal spread inside the inner
                        // dict makes the key-set dynamic — we can't say
                        // for sure that `seg_name` is missing.
                        let has_dynamic = inner_pairs.iter().any(|(k, v)| {
                            matches!(k, TokenKey::Spread(_)) && !matches!(&*v.expr, Expr::Dict(_))
                        });
                        if !has_key && !has_dynamic {
                            self.tree.diagnostics.push(Diagnostic::UnresolvedReference {
                                name: format!("{head}.{seg_name}"),
                                range: span_of(node.range),
                            });
                            return;
                        }
                    }
                }
            }
        }

        let Some(mut current) = scope.lookup(&head) else {
            return;
        };
        let mut accumulated = head.clone();
        // Walk path[1..]: each String segment must exist on the running
        // type (or we punt). We strip Optional wrappers as we walk so
        // `T? . x` is checked against `T`'s field set.
        for seg in &path[1..] {
            let TokenKey::String(name, _, _) = seg else {
                return;
            };
            current = match current {
                InferredType::Optional(inner) => *inner,
                other => other,
            };
            match &current {
                InferredType::Any => return,
                InferredType::Schema(schema_name) => {
                    let Some(fields) = self.schema_index.get(schema_name) else {
                        // Unknown schema body — runtime owns the verdict.
                        return;
                    };
                    let Some(field_ty) = fields.get(name) else {
                        let full = format!("{accumulated}.{name}");
                        self.tree.diagnostics.push(Diagnostic::UnresolvedReference {
                            name: full,
                            range: span_of(node.range),
                        });
                        return;
                    };
                    accumulated = format!("{accumulated}.{name}");
                    current = infer::infer_from_type_node(field_ty);
                }
                InferredType::Dict(value_ty) => {
                    // Homogeneous dict — every key has type `value_ty`,
                    // so the segment is structurally fine. Continue
                    // walking with the value type.
                    accumulated = format!("{accumulated}.{name}");
                    current = (**value_ty).clone();
                }
                _ => return, // Other shapes: defer to runtime.
            }
        }
    }
}
