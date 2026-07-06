//! Type-check sub-module: typed-binding / custom-schema / generics checks.
//!
//! Four methods extend [`super::Walker`]:
//!
//! * `check_typed_binding` — the central `Type field: value`
//!   subsumption gate. Routes to `check_generics` for matching outer
//!   containers, falls through to `StaticTypeMismatch` otherwise, and
//!   under strict mode escalates uninferrable values to
//!   `ExpressionTypeUnknown`.
//! * `check_generics` — structural walker that descends into List /
//!   Dict / Tuple / variant-constructor literals to validate inner
//!   types against the slot's generic parameters. Calls back into
//!   `check_typed_binding` per element so the diagnostic shape matches
//!   non-generic slots.
//! * `check_against_custom_schema` — when `expected` names a custom
//!   schema and `value` is a dict literal, validate each dict field
//!   against the schema's declared field types (with generic
//!   substitution for `Box<Int>` → `T -> Int`).
//! * `build_generic_subst` — collect the `param_name → arg_TypeNode`
//!   map implied by a generic schema's typehint, shared by the custom
//!   schema walker.

use super::helpers::{format_type, same_outer_container};
use super::index::substitute_generics_in_typenode;
use super::Walker;
use crate::diagnostic::{span_of, Diagnostic};
use crate::infer::infer_type;
use relon_parser::{Expr, Node, TokenKey, TypeNode};
use std::collections::HashMap;

impl<'a> Walker<'a> {
    /// Validate that `value` plausibly satisfies `expected`. Only
    /// fires when the value's shape is statically classifiable; calls
    /// to functions, references, etc. are deferred to the runtime
    /// check.
    pub(super) fn check_typed_binding(
        &mut self,
        expected: &TypeNode,
        value: &Node,
        field_name: &str,
    ) {
        // Run the inference engine against the active scope so
        // `Variable`/`Reference` heads reach back to their dict
        // siblings and pick up the declared type-hint. Falls back to
        // the legacy name-string path for the diagnostic shape.
        let scope = self.build_type_scope();
        let inferred = infer_type(value, &scope);
        if self.check_tuple_schema_literal(expected, value, field_name) {
            return;
        }
        if let Some(t) = &inferred {
            // Value-binding slots (`Type field: value`, schema fields)
            // are fail-closed against `Any` in a strict module, mirroring
            // the function-argument boundary (`fn_call.rs`): an internal
            // `Any` (chiefly the return of an untyped `#relaxed` closure)
            // must not silently whitewash a concrete typed slot. The
            // strictness is per-module — a `#relaxed` file clears
            // `strict_mode` and keeps the permissive `Any` pass, so the
            // escape hatch stays open where the author opted out. (Empty
            // collection literals infer `List(Never)` / `Dict(Never)`,
            // a polymorphic bottom that still satisfies any element slot,
            // so `List<Int> xs: []` is unaffected by this tightening.)
            if t.subsumes_with_imports(
                expected,
                Some(&self.base_index),
                self.tree.workspace_import_index.as_ref(),
                self.tree.strict_mode,
            ) {
                self.check_generics(expected, value, field_name);
                return;
            }
            // v1.1: when the failure is at the *element* level of a
            // matching outer shape (`List<...>` against `List<...>`,
            // `Dict<...>` against `Dict<...>`) and the value is a
            // literal, defer to the structural `check_generics`
            // walker so the diagnostic carries the precise inner
            // path (`matrix[1][0]: Int / String`) instead of the
            // coarser `matrix[1]: List<Int> / List<String>`. The
            // outer head must still match — an Int landing in a
            // List slot stays a coarse mismatch.
            if same_outer_container(t, expected)
                && matches!(&*value.expr, Expr::List(_) | Expr::Dict(_))
            {
                self.check_generics(expected, value, field_name);
                return;
            }
            self.tree.diagnostics.push(Diagnostic::StaticTypeMismatch {
                field: field_name.to_string(),
                expected: format_type(expected),
                found: t.name(),
                range: span_of(value.range),
            });
            return;
        }

        // If we couldn't infer a single static type, check if it's a
        // structured expression whose branches we can partially validate.
        if let Expr::Ternary { then, els, .. } = &*value.expr {
            self.check_typed_binding(expected, then, field_name);
            self.check_typed_binding(expected, els, field_name);
            return;
        }

        // v1.4 / v1.5: under strict mode, a typed binding whose value
        // can't produce *any* inferred type leaks `Any` into the
        // typed slot — strict mode demands a derivable type, so emit
        // `ExpressionTypeUnknown` describing the precise reason. v1.5 made
        // comprehensions / where / spread / closures all inferable
        // out-of-the-box, so the only paths that still hit this branch
        // are FnCall without a signature and a handful of edge cases
        // (e.g. unary on a leaf type).
        if self.tree.strict_mode {
            let reason = match &*value.expr {
                Expr::FnCall { path, .. } => match path.first() {
                    Some(TokenKey::String(name, _, _)) => {
                        format!("call to `{name}` has no static return type")
                    }
                    _ => "call result has no static return type".to_string(),
                },
                _ => "value type is not statically derivable".to_string(),
            };
            self.tree
                .diagnostics
                .push(Diagnostic::ExpressionTypeUnknown {
                    reason,
                    range: span_of(value.range),
                });
        }
    }

    /// Recursively check the contents of List and Dict literals against
    /// expected generic parameters.
    pub(super) fn check_generics(&mut self, expected: &TypeNode, value: &Node, field_name: &str) {
        if expected.generics.is_empty() && expected.path != vec!["Tuple"] {
            return;
        }
        // v1.8 C3: variant-constructor literal landing in a generic
        // sum-type slot. Look up the variant's declared field types,
        // substitute the slot's generic args (`Result<Int, String>` →
        // `T -> Int, E -> String`), then recurse into each body field.
        if expected.path.len() == 1 {
            let slot_name = &expected.path[0];
            if let Expr::VariantCtor {
                enum_path,
                variant,
                body,
            } = &*value.expr
            {
                if enum_path.first().map(|s| s.as_str()) == Some(slot_name.as_str()) {
                    if let Some(variants) = self.variant_field_index.get(slot_name).cloned() {
                        if let Some((generic_params, fields)) = variants.get(variant).cloned() {
                            let mut subst = HashMap::new();
                            for (i, gname) in generic_params.iter().enumerate() {
                                if let Some(arg) = expected.generics.get(i) {
                                    subst.insert(gname.clone(), arg.clone());
                                }
                            }
                            if let Expr::Dict(pairs) = &*body.expr {
                                for (key, inner) in pairs {
                                    if let TokenKey::String(k, _, _) = key {
                                        if let Some(field_ty) = fields.get(k) {
                                            let resolved =
                                                substitute_generics_in_typenode(field_ty, &subst);
                                            self.check_typed_binding(
                                                &resolved,
                                                inner,
                                                &format!("{field_name}.{k}"),
                                            );
                                        }
                                    }
                                }
                            }
                            return;
                        }
                    }
                }
            }

            if let Expr::FnCall { path, args } = &*value.expr {
                let variant_and_field =
                    match (slot_name.as_str(), path.as_slice()) {
                        ("Option", [TokenKey::String(name, _, _)]) if name == "Some" => {
                            Some(("Some", "value"))
                        }
                        (
                            "Option",
                            [TokenKey::String(head, _, _), TokenKey::String(name, _, _)],
                        ) if head == "Option" && name == "Some" => Some(("Some", "value")),
                        ("Result", [TokenKey::String(name, _, _)]) if name == "Ok" => {
                            Some(("Ok", "value"))
                        }
                        (
                            "Result",
                            [TokenKey::String(head, _, _), TokenKey::String(name, _, _)],
                        ) if head == "Result" && name == "Ok" => Some(("Ok", "value")),
                        ("Result", [TokenKey::String(name, _, _)]) if name == "Err" => {
                            Some(("Err", "error"))
                        }
                        (
                            "Result",
                            [TokenKey::String(head, _, _), TokenKey::String(name, _, _)],
                        ) if head == "Result" && name == "Err" => Some(("Err", "error")),
                        _ => None,
                    };
                if let Some((variant, field_name_in_variant)) = variant_and_field {
                    if let Some(arg) = args.first().filter(|_| args.len() == 1) {
                        if arg.name.is_none() {
                            if let Some(variants) = self.variant_field_index.get(slot_name).cloned()
                            {
                                if let Some((generic_params, fields)) =
                                    variants.get(variant).cloned()
                                {
                                    let mut subst = HashMap::new();
                                    for (i, gname) in generic_params.iter().enumerate() {
                                        if let Some(arg_ty) = expected.generics.get(i) {
                                            subst.insert(gname.clone(), arg_ty.clone());
                                        }
                                    }
                                    if let Some(field_ty) = fields.get(field_name_in_variant) {
                                        let resolved =
                                            substitute_generics_in_typenode(field_ty, &subst);
                                        self.check_typed_binding(
                                            &resolved,
                                            &arg.value,
                                            &format!("{field_name}.{field_name_in_variant}"),
                                        );
                                    }
                                }
                            }
                        }
                    }
                    return;
                }
            }
        }
        match &*value.expr {
            Expr::List(items) if expected.path == vec!["List"] && expected.generics.len() == 1 => {
                let inner_expected = &expected.generics[0];
                let spread_expected = TypeNode {
                    path: vec!["List".to_string()],
                    generics: vec![inner_expected.clone()],
                    is_optional: false,
                    range: expected.range,
                    variant_fields: None,
                    doc_comment: None,
                };
                for (i, item) in items.iter().enumerate() {
                    if let Expr::Spread(inner) = &*item.expr {
                        self.check_typed_binding(
                            &spread_expected,
                            inner,
                            &format!("{}[{}]", field_name, i),
                        );
                    } else {
                        self.check_typed_binding(
                            inner_expected,
                            item,
                            &format!("{}[{}]", field_name, i),
                        );
                    }
                }
            }
            // A `(...)` tuple value landing in a typed slot. The common
            // case is a `Tuple<T1, T2, ...>` slot: check arity, then
            // recurse position-by-position. It is not a List: tuple
            // values only satisfy tuple-typed slots.
            Expr::Tuple(items) if expected.path == vec!["Tuple"] => {
                if items.len() != expected.generics.len() {
                    self.tree.diagnostics.push(Diagnostic::StaticTypeMismatch {
                        field: field_name.to_string(),
                        expected: format_type(expected),
                        found: format!("tuple of {} element(s)", items.len()),
                        range: span_of(value.range),
                    });
                    return;
                }
                for (i, (item, slot)) in items.iter().zip(expected.generics.iter()).enumerate() {
                    self.check_typed_binding(slot, item, &format!("{}[{}]", field_name, i));
                }
            }
            // Only the common `Dict<String, V>` shape is descended into;
            // other key types are deferred to the runtime checker.
            Expr::Dict(pairs)
                if expected.path == vec!["Dict"]
                    && expected.generics.len() == 2
                    && expected.generics[0].path == vec!["String"] =>
            {
                let inner_expected = &expected.generics[1];
                for (key, val) in pairs {
                    if let TokenKey::String(k, _, _) = key {
                        self.check_typed_binding(
                            inner_expected,
                            val,
                            &format!("{}.{}", field_name, k),
                        );
                    }
                }
            }
            _ => {}
        }
    }

    /// When `expected` names a custom schema and `value` is a literal
    /// dict, walk the dict's fields and report any that disagree with
    /// the schema's declared field types.
    pub(super) fn check_against_custom_schema(&mut self, expected: &TypeNode, value: &Node) {
        if expected.path.len() != 1 {
            return;
        }
        let schema_name = &expected.path[0];
        let Expr::Dict(pairs) = &*value.expr else {
            return;
        };
        // Collect the (field_name, expected_type) lookups up front so
        // `check_typed_binding`'s `&mut self.tree` borrow doesn't conflict
        // with the read of `self.schema_index`. Stay lazy: only clone the
        // TypeNode for fields we'll actually check, instead of cloning the
        // whole `field_types` HashMap once per Dict.
        //
        // v1.8+ fix (issue 4): when `expected` carries generic args
        // (`Box<Int>`), substitute the schema's generic param names
        // with the supplied args before recursing — otherwise a field
        // declared as `T value: *` checks the value against the
        // literal type `T` and reports a static mismatch. We pull the
        // schema's generic param names from `tree.schemas` /
        // `root_schemas` (the schema_index only carries field types,
        // not the param list).
        let subst_map = self.build_generic_subst(schema_name, expected);
        let mut to_check: Vec<(String, TypeNode, &Node)> = Vec::new();
        if let Some(field_types) = self.schema_index.get(schema_name) {
            for (key, inner) in pairs {
                if let TokenKey::String(field_name, _, _) = key {
                    if let Some(field_type) = field_types.get(field_name) {
                        let substituted = if subst_map.is_empty() {
                            field_type.clone()
                        } else {
                            substitute_generics_in_typenode(field_type, &subst_map)
                        };
                        to_check.push((field_name.clone(), substituted, inner));
                    }
                }
            }
        }
        for (field_name, field_type, inner) in to_check {
            self.check_typed_binding(&field_type, inner, &field_name);
        }
    }

    fn check_tuple_schema_literal(
        &mut self,
        expected: &TypeNode,
        value: &Node,
        field_name: &str,
    ) -> bool {
        let Some((schema_name, elements)) =
            crate::schema::tuple_elements_for_schema_type(self.tree, expected)
        else {
            return false;
        };
        let Expr::Tuple(items) = &*value.expr else {
            return false;
        };
        if items.len() != elements.len() {
            self.tree.diagnostics.push(Diagnostic::StaticTypeMismatch {
                field: field_name.to_string(),
                expected: format_type(expected),
                found: format!("tuple of {} element(s)", items.len()),
                range: span_of(value.range),
            });
            return true;
        }
        let subst_map = self.build_generic_subst(&schema_name, expected);
        for (i, (item, slot)) in items.iter().zip(elements.iter()).enumerate() {
            let resolved = if subst_map.is_empty() {
                slot.clone()
            } else {
                substitute_generics_in_typenode(slot, &subst_map)
            };
            self.check_typed_binding(&resolved, item, &format!("{}[{}]", field_name, i));
        }
        true
    }

    /// v1.8+ fix (issue 4): collect the `param_name → arg_TypeNode`
    /// substitution implied by `expected` (e.g. `Box<Int>` against
    /// `#schema Box<T> { ... }` produces `{T → Int}`). Returns an
    /// empty map when the schema isn't generic or no `<...>` was
    /// supplied.
    pub(super) fn build_generic_subst(
        &self,
        schema_name: &str,
        expected: &TypeNode,
    ) -> HashMap<String, TypeNode> {
        let mut out = HashMap::new();
        if expected.generics.is_empty() {
            return out;
        }
        // Locate the schema's declared generic param names. Both
        // dict-form (`tree.schemas`) and root-form (`tree.root_schemas`)
        // can declare generics; check both.
        let params: Vec<String> = self
            .tree
            .schemas
            .values()
            .find(|def| def.name.as_deref() == Some(schema_name))
            .map(|def| def.generics.clone())
            .or_else(|| {
                self.tree
                    .root_schemas
                    .iter()
                    .find(|d| d.name == schema_name)
                    .map(|d| d.generics.clone())
            })
            .or_else(|| {
                self.tree
                    .workspace_import_index
                    .as_ref()
                    .and_then(|idx| idx.imported_schema_generics.get(schema_name).cloned())
            })
            .unwrap_or_default();
        for (i, p) in params.iter().enumerate() {
            if let Some(arg) = expected.generics.get(i) {
                out.insert(p.clone(), arg.clone());
            }
        }
        out
    }
}
