//! Type-check sub-module: dict v1.3 + spread checks.
//!
//! Two related concerns live here:
//!
//! * `check_dict_v1_3` — the master dict-pair walker that enforces
//!   strict-mode typed-spread / typed-dynamic-key rules and the
//!   cross-mode `DuplicateField` invariant.
//! * `schema_known` + `spread_source_schema` +
//!   `spread_source_known_non_dict` + `spread_source_is_dict` +
//!   `spread_contributed_keys` — the five-stage classifier the dict
//!   walker queries to decide whether a `...spread` source is
//!   acceptable, what schema it resolves to (for `UnresolvedSchema`
//!   diagnostics), and which keys it statically contributes (for
//!   `DuplicateField` detection).
//!
//! Pulled out as one block because the five spread helpers are
//! tightly coupled — they share the same `infer::walk_path` /
//! `resolve_call_signature` / `schema_index` lookups, and only the
//! dict walker consumes them.

use super::Walker;
use crate::diagnostic::{span_of, Diagnostic};
use crate::infer::{self, InferredType};
use relon_parser::{Expr, TokenKey, TypeNode};
use std::collections::HashSet;

impl<'a> Walker<'a> {
    /// v1.3: validate every entry of a dict literal for typed-spread,
    /// typed-dynamic-key, and DuplicateField rules. The latter fires
    /// regardless of strict mode (the language always treated spread-
    /// induced collisions as ambiguous; v1.2 silently picked one);
    /// the typed-hint rules only fire under strict mode because a
    /// non-strict file is allowed to defer to runtime.
    pub(super) fn check_dict_v1_3(&mut self, pairs: &[(TokenKey, relon_parser::Node)]) {
        let strict = self.tree.strict_mode;
        // Track every named key declared so far so we can report the
        // first collision against the dict's named entries. Spread-
        // contributed keys are also folded in via the schema index when
        // a typed spread's source schema is statically known.
        let mut named: HashSet<String> = HashSet::new();
        // v1.8+ fix: dynamic-key inner expressions whose typehint we
        // need to validate after the per-pair loop completes (the loop
        // body holds an immutable borrow of `pairs`, so we can't call
        // `&mut self` methods like `check_typed_binding` directly).
        let mut post_dynkey_checks: Vec<(TypeNode, relon_parser::Node)> = Vec::new();
        for (key, _) in pairs {
            if let TokenKey::String(name, _, _) = key {
                named.insert(name.clone());
            }
        }
        let mut to_emit: Vec<Diagnostic> = Vec::new();
        // Track keys contributed by earlier spreads so two spreads that
        // overlap also produce DuplicateField.
        let mut spread_keys: HashSet<String> = HashSet::new();
        for (key, value) in pairs {
            match key {
                TokenKey::Spread(range) => {
                    let has_hint = value.type_hint.is_some();
                    let is_dict_literal = matches!(&*value.expr, Expr::Dict(_));
                    // Resolve a Variable / Reference head to its
                    // sibling target; if that target is itself a
                    // typed binding to a known schema we treat the
                    // spread as having a derivable shape.
                    let derived_schema = self.spread_source_schema(value);
                    let resolves_to_schema = derived_schema.is_some();
                    // v1.4: a `Dict<K, V>`-typed spread source is
                    // also acceptable under strict mode — the value
                    // type is fully classified even though the keys
                    // are dynamic. Detect via the inference walker
                    // so path chains and FnCall returns work the
                    // same as the schema case.
                    let resolves_to_dict = self.spread_source_is_dict(value);
                    let is_spreadable_shape =
                        is_dict_literal || has_hint || resolves_to_schema || resolves_to_dict;

                    // Cross-mode: if the source has a known static
                    // type but that type isn't dict-shaped (Int,
                    // List<T>, Bool, ...) the program is wrong in
                    // every mode — no `<T>` hint can rescue it.
                    if !is_spreadable_shape {
                        if let Some(known_ty) = self.spread_source_known_non_dict(value) {
                            to_emit.push(Diagnostic::NonSpreadableSource {
                                source_type: known_ty,
                                range: span_of(*range),
                            });
                        } else if strict {
                            // Strict-only: the source's type is
                            // genuinely unknown — adding a hint or
                            // typing the binding is the literal fix.
                            to_emit.push(Diagnostic::SpreadSourceTypeUnknown {
                                range: span_of(*range),
                            });
                        }
                    }
                    // Cross-mode: an explicit `<Schema>` hint (or a
                    // resolved-source schema name) that isn't in the
                    // workspace's schema set is broken regardless of
                    // strict mode — the runtime would error too.
                    if let Some(name) = &derived_schema {
                        if !self.schema_known(name) {
                            to_emit.push(Diagnostic::UnresolvedSchema {
                                name: name.clone(),
                                range: span_of(*range),
                            });
                        }
                    }
                    // DuplicateField: collect the spread's contributed
                    // keys when we can derive them statically (typed
                    // spread → schema index, dict literal → its keys).
                    let contributed = self.spread_contributed_keys(value);
                    for k in &contributed {
                        if named.contains(k) || spread_keys.contains(k) {
                            to_emit.push(Diagnostic::DuplicateField {
                                field: k.clone(),
                                range: span_of(*range),
                            });
                        }
                    }
                    spread_keys.extend(contributed);
                }
                TokenKey::Dynamic(inner, _) => {
                    if strict && inner.type_hint.is_none() {
                        to_emit.push(Diagnostic::DynamicKeyTypeUnknown {
                            range: span_of(inner.range),
                        });
                    }
                    // v1.8+ fix: when a typehint *is* present, validate
                    // the key expression against it. Pre-fix the dict
                    // walker only visited `value`, never `inner`, so
                    // `[<String> 1]: v` parsed as well-typed and the
                    // mismatch only surfaced at runtime. Defer to
                    // `check_typed_binding`'s subsumption machinery so
                    // the diagnostic shape matches the rest of the
                    // typed-binding cases.
                    if let Some(hint) = &inner.type_hint {
                        let hint_clone = hint.clone();
                        let inner_clone = inner.clone();
                        // Avoid double-reporting on already-bogus types
                        // — `ban_unsafe_types`/UnknownTypeName already
                        // owns malformed-hint reports.
                        post_dynkey_checks.push((hint_clone, inner_clone));
                    }
                }
                _ => {}
            }
        }
        self.tree.diagnostics.extend(to_emit);
        // v1.8+ fix: now that the immutable borrow of `pairs` is
        // released, validate every typed dynamic-key inner expression
        // against its hint. We share `check_typed_binding`'s machinery
        // so the diagnostic shape matches every other typed-binding
        // case (literal-vs-hint mismatch, schema field check, etc.).
        for (hint, inner) in post_dynkey_checks {
            self.check_typed_binding(&hint, &inner, "<dynamic key>");
        }
    }

    /// True when `name` corresponds to a declared schema reachable
    /// from this analysis pass: dict-field `#schema X ...`, root-level
    /// `#schema X Body`, prelude (`Result`, `Option`), or an import
    /// brought in via spread / destructure / alias.
    pub(super) fn schema_known(&self, name: &str) -> bool {
        if matches!(name, "Result" | "Option") {
            return true;
        }
        if self.schema_index.contains_key(name) {
            return true;
        }
        if self.tree.root_schemas.iter().any(|d| d.name == name) {
            return true;
        }
        if let Some(idx) = &self.tree.workspace_import_index {
            if idx.spread.contains(name)
                || idx.destructured.contains_key(name)
                || idx.aliased.contains_key(name)
            {
                return true;
            }
        }
        false
    }

    /// Try to derive the schema name a spread source references. v1.4
    /// recognises four static cases:
    ///
    /// 1. An inline `<T>` typehint on the spread itself.
    /// 2. A resolved sibling field with an explicit `T:` type binding.
    /// 3. A path expression (`...x.y.z`) whose tail-walk produces a
    ///    `Schema` head — covers `...o.extras` against
    ///    `Order { Extras extras: * }` and friends.
    /// 4. A `FnCall` (`...load_extras()`) whose static signature
    ///    declares a single-segment `Schema` return type.
    ///
    /// Returns `None` when none of those apply, leaving strict mode to
    /// demand an explicit hint.
    pub(super) fn spread_source_schema(&self, value: &relon_parser::Node) -> Option<String> {
        if let Some(t) = &value.type_hint {
            if t.path.len() == 1 {
                let head = &t.path[0];
                // v1.8+ fix: a builtin head (`Dict`, `List`, `Tuple`,
                // `Closure`, primitives, ...) is *not* a schema name,
                // even when it carries generics. The companion
                // `spread_source_is_dict` covers `Dict<K, V>`-typed
                // spreads; returning the head as a schema name here
                // would push a bogus `UnresolvedSchema("Dict")`.
                if relon_parser::is_builtin_type_name(head) {
                    return None;
                }
                if matches!(head.as_str(), "Result" | "Option") {
                    return None;
                }
                return Some(head.clone());
            }
        }
        // Resolved reference to a sibling whose head carries a type
        // hint we can lift directly. Same as v1.3 — kept for the
        // single-segment `...e` case where path-tail walking would
        // collapse to the head's type anyway.
        if let Some(resolved) = self.tree.references.get(&value.id) {
            if let Some(target) = self.tree.node_index.get(&resolved.target) {
                if let Some(hint) = target.type_hint.as_ref() {
                    if hint.path.len() == 1 {
                        return Some(hint.path[0].clone());
                    }
                }
            }
        }
        // v1.4 case 3: the spread source is a `Variable` / `Reference`
        // path whose tail-walk lands on a `Schema(name)` (a multi-hop
        // chain like `...o.extras` or a single-segment param like
        // `...o`). The walker reuses the same machinery as the
        // expression-level inference engine, so multi-segment chains
        // through schema fields and dict generics work identically.
        if matches!(&*value.expr, Expr::Variable(_) | Expr::Reference { .. }) {
            let scope = self.build_type_scope();
            let path = match &*value.expr {
                Expr::Variable(p) => p.as_slice(),
                Expr::Reference { path, .. } => path.as_slice(),
                _ => unreachable!(),
            };
            if let infer::PathTailOutcome::Resolved(InferredType::Schema(name)) =
                infer::walk_path(path, &scope)
            {
                return Some(name);
            }
        }
        // v1.4 case 4: the spread source is a static FnCall whose
        // signature has a single-segment Schema return type. Generics
        // aren't unified here because spread-source signatures we'd
        // accept under strict mode never carry placeholders today; if
        // that changes, route through `instantiate` like the FnCall
        // arm of `infer_type` does.
        if let Expr::FnCall { path, .. } = &*value.expr {
            if let Some(sig) = self.resolve_call_signature(path) {
                if sig.generics.is_empty() && sig.return_type.path.len() == 1 {
                    return Some(sig.return_type.path[0].clone());
                }
            }
        }
        None
    }

    /// v1.4: companion to [`Self::spread_source_schema`] for sources
    /// whose static type is `Dict<K, V>` rather than a named schema.
    /// Strict mode treats these as fully classified — the value type
    /// is known, even if the keys are dynamic — so they don't need a
    /// `<T>` typehint. Returns `true` when the spread source's static
    /// type is some `Dict<...>` variant.
    /// Return the printable name of the spread source's static type
    /// when that type is **known** and **isn't** dict-shaped — the
    /// signal for `NonSpreadableSource`. Returns `None` for dict /
    /// schema / `Dict<K,V>` sources (the spread is fine) or for
    /// genuinely unknown / `Any` sources (the strict-only
    /// `SpreadSourceTypeUnknown` covers them).
    ///
    /// Only inspects expression forms whose type the inference layer
    /// is confident about today: literal scalars / lists, references
    /// to typed bindings whose declared type is a non-dict primitive,
    /// and path-tail walks resolving to a non-dict / non-schema head.
    pub(super) fn spread_source_known_non_dict(
        &self,
        value: &relon_parser::Node,
    ) -> Option<String> {
        // Literal scalars and lists are obvious offenders.
        match &*value.expr {
            Expr::Int(_) => return Some("Int".to_string()),
            Expr::Float(_) => return Some("Float".to_string()),
            Expr::Bool(_) => return Some("Bool".to_string()),
            Expr::String(_) => return Some("String".to_string()),
            Expr::List(_) => return Some("List".to_string()),
            Expr::Missing => return None,
            _ => {}
        }
        // Fall through to the inference walker for everything else
        // (binops like `1 + 2`, identifiers, path chains).
        let scope = self.build_type_scope();
        let inferred = infer::infer_type(value, &scope).unwrap_or(InferredType::Any);
        match inferred {
            // Dict-shaped — handled by `spread_source_is_dict`.
            InferredType::Dict(_) | InferredType::Schema(_) => None,
            // Genuinely unknown — `SpreadSourceTypeUnknown` covers it.
            InferredType::Any => None,
            // Anything else has a known, non-spreadable shape.
            InferredType::Bool => Some("Bool".to_string()),
            InferredType::Int => Some("Int".to_string()),
            InferredType::Float => Some("Float".to_string()),
            InferredType::Number => Some("Number".to_string()),
            InferredType::String => Some("String".to_string()),
            InferredType::List(_) => Some("List".to_string()),
            InferredType::Variant(_, _) | InferredType::VariantPayload(_, _, _) => {
                Some("Variant".to_string())
            }
            InferredType::Optional(_) => Some("Optional".to_string()),
            InferredType::Fn(_, _) => Some("Fn".to_string()),
            InferredType::Tuple(_) => Some("Tuple".to_string()),
        }
    }

    pub(super) fn spread_source_is_dict(&self, value: &relon_parser::Node) -> bool {
        // Inline `<Dict<K, V>>` typehint.
        if let Some(t) = &value.type_hint {
            if t.path.len() == 1 && t.path[0] == "Dict" {
                return true;
            }
        }
        // `...x` where `x` is bound to a sibling whose declared type is
        // some `Dict<...>` — same lift the schema case does, but for
        // the dict head.
        if let Some(resolved) = self.tree.references.get(&value.id) {
            if let Some(target) = self.tree.node_index.get(&resolved.target) {
                if let Some(hint) = target.type_hint.as_ref() {
                    if hint.path.len() == 1 && hint.path[0] == "Dict" {
                        return true;
                    }
                }
            }
        }
        // Path chain (`...o.kv`) or FnCall (`...load_kv()`) whose
        // inference produces a `Dict(_)` — reuse the central walker.
        match &*value.expr {
            Expr::Variable(p) | Expr::Reference { path: p, .. } => {
                let scope = self.build_type_scope();
                if let infer::PathTailOutcome::Resolved(InferredType::Dict(_)) =
                    infer::walk_path(p, &scope)
                {
                    return true;
                }
            }
            Expr::FnCall { path, .. } => {
                if let Some(sig) = self.resolve_call_signature(path) {
                    if sig.return_type.path.len() == 1 && sig.return_type.path[0] == "Dict" {
                        return true;
                    }
                }
            }
            _ => {}
        }
        false
    }

    /// Best-effort enumeration of the keys a spread source statically
    /// contributes. Returns an empty `Vec` when the source is a typed
    /// spread whose schema isn't visible, or a non-literal expression
    /// without a hint — DuplicateField only fires on contributions we
    /// can prove.
    pub(super) fn spread_contributed_keys(&self, value: &relon_parser::Node) -> Vec<String> {
        let mut out = Vec::new();
        if let Expr::Dict(inner_pairs) = &*value.expr {
            for (k, _) in inner_pairs {
                if let TokenKey::String(name, _, _) = k {
                    out.push(name.clone());
                }
            }
            return out;
        }
        if let Some(name) = self.spread_source_schema(value) {
            if let Some(fields) = self.schema_index.get(&name) {
                out.extend(fields.keys().cloned());
            }
        }
        out
    }
}
