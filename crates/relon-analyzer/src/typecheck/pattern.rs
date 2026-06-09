//! Type-check sub-module: match / closure-return / pattern checks.
//!
//! Four methods extend [`super::Walker`]:
//!
//! * `check_match_arm_types` — Stage 1.6 reduce-by-join across arm
//!   bodies; if the result collapses to `Any` while none of the
//!   inputs were `Any`, the arms are statically heterogeneous and we
//!   emit `MatchArmTypeMismatch`.
//! * `check_closure_return` — push `StaticTypeMismatch` when a
//!   closure's body inference disagrees with its declared `-> Type`.
//! * `check_match_exhaustiveness` — conservative exhaustiveness +
//!   unknown-variant + duplicate-arm checks against the
//!   `enum_index` populated by the index sub-module.
//! * `infer_enum_type` — small helper that looks up the static
//!   enum-name of a scrutinee node via the resolver's reference table
//!   (typed binding or VariantCtor literal).
//!
//! Co-located because they all interact with `enum_index`, `references`,
//! and the closure-param scope frame in the same way.

use super::helpers::{closest_variant, format_type};
use super::Walker;
use crate::diagnostic::{span_of, Diagnostic};
use crate::infer::{self, infer_type, InferredType, TypeScope};
use relon_parser::{Expr, Node, TokenKey, TokenRange, TypeNode};
use std::collections::{HashMap, HashSet};

fn enum_name_from_type_node(
    t: &TypeNode,
    enum_index: &HashMap<String, Vec<String>>,
) -> Option<String> {
    if t.is_optional {
        return Some("Option".to_string());
    }
    if t.path.len() == 1 && enum_index.contains_key(&t.path[0]) {
        Some(t.path[0].clone())
    } else {
        None
    }
}

fn normalize_optional_type_node(t: &TypeNode) -> TypeNode {
    if !t.is_optional {
        return t.clone();
    }
    let mut inner = t.clone();
    inner.is_optional = false;
    TypeNode {
        path: vec!["Option".to_string()],
        generics: vec![inner],
        is_optional: false,
        range: t.range,
        variant_fields: None,
        doc_comment: None,
    }
}

impl<'a> Walker<'a> {
    /// Stage 1.6: report `MatchArmTypeMismatch` when arm bodies
    /// produce statically-incompatible types. We only fire when *every*
    /// non-wildcard arm body is inferrable; if any arm relies on a
    /// runtime-only computation, we punt (runtime keeps owning the
    /// verdict). Wildcard arms are excluded from the join because their
    /// body type is unconstrained by exhaustiveness rules.
    pub(super) fn check_match_arm_types(
        &mut self,
        match_range: TokenRange,
        scrutinee: &Node,
        arms: &[(Node, Node)],
    ) {
        let scope = self.build_type_scope();
        let mut arm_types: Vec<InferredType> = Vec::new();
        let strict = self.tree.strict_mode;
        for (pat, body) in arms {
            if matches!(pat.expr.as_ref(), Expr::Wildcard) {
                continue;
            }
            let scoped;
            let body_scope = if let Some(locals) = self.match_arm_locals(scrutinee, pat) {
                scoped = scope.child_with_locals(locals);
                &scoped
            } else {
                &scope
            };
            // If any non-wildcard arm body is uninferrable, defer to
            // runtime — we'd be guessing.
            let Some(t) = infer_type(body, body_scope) else {
                // v1.4: strict mode demands every arm have a static
                // type. Pin the diagnostic on the failing arm body so
                // the user knows where to add an annotation.
                if strict {
                    self.tree
                        .diagnostics
                        .push(Diagnostic::ExpressionTypeUnknown {
                            reason: "match arm body type is not statically derivable".to_string(),
                            range: span_of(body.range),
                        });
                }
                return;
            };
            arm_types.push(t);
        }
        if arm_types.len() < 2 {
            return;
        }
        // Reduce-by-join across all arm types; if the result collapses
        // to `Any` while none of the inputs were `Any`, the arms are
        // statically heterogeneous.
        let any_input_was_any = arm_types.iter().any(|t| matches!(t, InferredType::Any));
        let mut joined = arm_types[0].clone();
        for t in &arm_types[1..] {
            joined = InferredType::join(&joined, t);
        }
        if matches!(joined, InferredType::Any) && !any_input_was_any {
            let enum_name = self.infer_enum_type(scrutinee);
            self.tree
                .diagnostics
                .push(Diagnostic::MatchArmTypeMismatch {
                    enum_name,
                    arm_types: arm_types.iter().map(|t| t.name()).collect(),
                    range: span_of(match_range),
                });
        }
    }

    /// Push a `StaticTypeMismatch` when a closure's body inference
    /// disagrees with its declared `-> Type`. Closures whose body
    /// remains uninferrable (FnCall, dynamic refs) silently pass.
    pub(super) fn check_closure_return(
        &mut self,
        params: &[relon_parser::ClosureParam],
        body: &Node,
        declared_return: &TypeNode,
        field_name: Option<&str>,
    ) {
        // Build a fresh inference scope: dict frames inherited from
        // the active stack, locals seeded with the closure's typed
        // params (untyped params default to `Any` so they don't gate
        // analysis).
        let mut locals = HashMap::new();
        let imports = self.tree.workspace_import_index.as_ref();
        for param in params {
            let ty = param
                .type_hint
                .as_ref()
                .map(|t| infer::infer_from_type_node_with_imports(t, imports))
                .unwrap_or(InferredType::Any);
            locals.insert(param.name.clone(), ty);
        }
        let scope = TypeScope {
            locals,
            parent_locals: Vec::new(),
            schemas: Some(&self.schema_index),
            frames: self.scope_stack.iter().collect(),
            tree: Some(self.tree),
            resolving: Vec::new(),
        };
        let Some(body_ty) = infer_type(body, &scope) else {
            return;
        };
        if body_ty.subsumes_with_imports(declared_return, Some(&self.base_index), imports) {
            return;
        }
        self.tree.diagnostics.push(Diagnostic::StaticTypeMismatch {
            field: field_name.unwrap_or("_").to_string(),
            expected: format_type(declared_return),
            found: body_ty.name(),
            range: span_of(body.range),
        });
    }

    /// Conservative exhaustiveness check: only fires when we can statically
    /// determine the matched expression's type to be a sum-type Enum.
    /// Otherwise we silently fall through to the runtime mismatch path.
    pub(super) fn check_match_exhaustiveness(
        &mut self,
        match_range: TokenRange,
        expr: &Node,
        arms: &[(Node, Node)],
    ) {
        let Some(enum_name) = self.infer_enum_type(expr) else {
            return;
        };
        // Borrow the variant list for the whole arm walk; we route diagnostic
        // pushes through a local buffer so the read-only borrow of
        // `enum_index` doesn't collide with `&mut self.tree.diagnostics`.
        let Some(variants) = self.enum_index.get(&enum_name) else {
            return;
        };

        let mut seen = HashSet::new();
        let mut has_wildcard = false;
        let mut diags: Vec<Diagnostic> = Vec::new();
        for (pat, _) in arms {
            match pat.expr.as_ref() {
                Expr::Wildcard => {
                    has_wildcard = true;
                }
                Expr::Type(t) if t.path.len() == 1 => {
                    let arm_name = &t.path[0];
                    if !variants.contains(arm_name) {
                        let suggestion = closest_variant(arm_name, variants);
                        diags.push(Diagnostic::UnknownVariant {
                            enum_name: enum_name.clone(),
                            variant_name: arm_name.clone(),
                            suggestion,
                            range: span_of(pat.range),
                        });
                        continue;
                    }
                    if !seen.insert(arm_name.clone()) {
                        diags.push(Diagnostic::DuplicateMatchArm {
                            enum_name: enum_name.clone(),
                            variant_name: arm_name.clone(),
                            range: span_of(pat.range),
                        });
                    }
                }
                Expr::VariantPattern { variant, .. } => {
                    let arm_name = variant;
                    if !variants.contains(arm_name) {
                        let suggestion = closest_variant(arm_name, variants);
                        diags.push(Diagnostic::UnknownVariant {
                            enum_name: enum_name.clone(),
                            variant_name: arm_name.clone(),
                            suggestion,
                            range: span_of(pat.range),
                        });
                        continue;
                    }
                    if !seen.insert(arm_name.clone()) {
                        diags.push(Diagnostic::DuplicateMatchArm {
                            enum_name: enum_name.clone(),
                            variant_name: arm_name.clone(),
                            range: span_of(pat.range),
                        });
                    }
                }
                _ => {}
            }
        }
        if !has_wildcard {
            let missing: Vec<String> = variants
                .iter()
                .filter(|v| !seen.contains(*v))
                .cloned()
                .collect();
            if !missing.is_empty() {
                diags.push(Diagnostic::NonExhaustiveMatch {
                    enum_name,
                    missing_variants: missing,
                    range: span_of(match_range),
                });
            }
        }
        self.tree.diagnostics.extend(diags);
    }

    /// Try to determine the static enum-name of `node`. Prefer the resolver's
    /// target when available, then fall back to the current type scope so
    /// `#main(EnumName value)` parameters are handled too.
    pub(super) fn infer_enum_type(&self, node: &Node) -> Option<String> {
        if let Some(resolved) = self.tree.references.get(&node.id) {
            if let Some(target_node) = self.tree.node_index.get(&resolved.target).cloned() {
                if let Some(t) = &target_node.type_hint {
                    if let Some(name) = enum_name_from_type_node(t, &self.enum_index) {
                        return Some(name);
                    }
                }
                if let Expr::VariantCtor { enum_path, .. } = target_node.expr.as_ref() {
                    if !enum_path.is_empty() && self.enum_index.contains_key(&enum_path[0]) {
                        return Some(enum_path[0].clone());
                    }
                }
            }
        }
        let scope = self.build_type_scope();
        match infer_type(node, &scope)? {
            InferredType::Schema(name)
            | InferredType::Variant(name, _)
            | InferredType::VariantPayload(name, _, _)
                if self.enum_index.contains_key(&name) =>
            {
                Some(name)
            }
            InferredType::Optional(_) if self.enum_index.contains_key("Option") => {
                Some("Option".to_string())
            }
            _ => None,
        }
    }

    pub(super) fn match_arm_locals(
        &self,
        scrutinee: &Node,
        pat: &Node,
    ) -> Option<HashMap<String, InferredType>> {
        let enum_name = self.infer_enum_type(scrutinee)?;
        let variant_name = pattern_variant_name(pat)?;
        let variants = self.variant_field_index.get(&enum_name)?;
        let (generic_params, fields) = variants.get(&variant_name)?;
        let binding = single_variable_name(scrutinee);
        let fields = if let Some(scrutinee_ty) = binding
            .as_deref()
            .and_then(|binding| self.scrutinee_type_node(binding, scrutinee))
        {
            let scrutinee_ty = normalize_optional_type_node(&scrutinee_ty);
            let mut subst = HashMap::new();
            for (i, param) in generic_params.iter().enumerate() {
                if let Some(actual) = scrutinee_ty.generics.get(i) {
                    subst.insert(param.clone(), actual.clone());
                }
            }
            if subst.is_empty() {
                fields.clone()
            } else {
                fields
                    .iter()
                    .map(|(name, ty)| {
                        (
                            name.clone(),
                            crate::typecheck::substitute_generics_in_typenode(ty, &subst),
                        )
                    })
                    .collect()
            }
        } else {
            fields.clone()
        };

        let mut locals = HashMap::new();
        if let Some(binding) = binding {
            locals.insert(
                binding,
                InferredType::VariantPayload(
                    enum_name.clone(),
                    variant_name.clone(),
                    fields.clone(),
                ),
            );
        }
        if let Expr::VariantPattern { bindings, .. } = pat.expr.as_ref() {
            let imports = self.tree.workspace_import_index.as_ref();
            for (idx, binding) in bindings.iter().enumerate() {
                let Some(name) = binding.binding.as_ref() else {
                    continue;
                };
                let field_name = binding
                    .field
                    .clone()
                    .or_else(|| positional_field_name(&fields, idx))
                    .unwrap_or_else(|| idx.to_string());
                let ty = fields
                    .get(&field_name)
                    .map(|t| infer::infer_from_type_node_with_imports(t, imports))
                    .unwrap_or(InferredType::Any);
                locals.insert(name.clone(), ty);
            }
        }
        Some(locals)
    }

    fn scrutinee_type_node(&self, binding: &str, scrutinee: &Node) -> Option<TypeNode> {
        for frame in self.scope_stack.iter().rev() {
            if let Some(ty) = frame.closure_param_types.get(binding) {
                return Some(ty.clone());
            }
        }
        if let Some(sig) = self.tree.main_signature.as_ref() {
            if let Some(param) = sig.params.iter().find(|p| p.name == binding) {
                return Some(param.type_node.clone());
            }
        }
        let resolved = self.tree.references.get(&scrutinee.id)?;
        let target_node = self.tree.node_index.get(&resolved.target)?;
        target_node.type_hint.clone()
    }
}

fn positional_field_name(fields: &HashMap<String, TypeNode>, idx: usize) -> Option<String> {
    let numeric = idx.to_string();
    if fields.contains_key(&numeric) {
        return Some(numeric);
    }
    if fields.len() == 1 {
        return fields.keys().next().cloned();
    }
    None
}

fn single_variable_name(node: &Node) -> Option<String> {
    match node.expr.as_ref() {
        Expr::Variable(path) => match path.as_slice() {
            [TokenKey::String(name, _, _)] => Some(name.clone()),
            _ => None,
        },
        _ => None,
    }
}

fn pattern_variant_name(node: &Node) -> Option<String> {
    match node.expr.as_ref() {
        Expr::Type(t) if t.path.len() == 1 => Some(t.path[0].clone()),
        Expr::VariantPattern { variant, .. } => Some(variant.clone()),
        _ => None,
    }
}
