//! Runtime schema construction and type checking.
//!
//! Schema *desugar* (`@schema Name: { ... }` → `SchemaDef`) lives in
//! [`relon_analyzer::schema`]. This module owns the runtime side:
//!
//! * [`Evaluator::build_schema_from_def`] — turn an analyzer-produced
//!   `SchemaDef` into a `HashMap<String, SchemaField>` by instantiating
//!   predicate closures, resolving base schemas via reference, and
//!   running per-decorator `schema_field_meta` hooks.
//! * [`Evaluator::merge_schema_with_dict_pairs`] — handle the inline
//!   `Schema + Dict_AST` arithmetic case where a literal dict is folded
//!   into an existing schema (typed entries refine fields, untyped
//!   entries set defaults).
//! * [`Evaluator::check_type`] — runtime type validation against a
//!   `Value::Schema` or built-in type name.
//!
//! Hosts that don't attach an `AnalyzedTree` get on-demand desugar
//! through [`relon_analyzer::lower_schema_pure`] (called from the
//! `SchemaDecorator` plugin), so this module never needs an
//! evaluator-internal AST walker.

use crate::error::RuntimeError;
use crate::eval::{decorator_name, Evaluator};
use crate::native_fn::EvaluatedArg;
use crate::scope::Scope;
use crate::value::{SchemaField, Value};
use relon_analyzer::{SchemaDef, SchemaFieldDef};
use relon_parser::{is_builtin_type_name, Expr, Node, TokenKey, TokenRange, TypeNode};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

/// Pretty-print a [`TypeNode`] for use in error messages.
pub(crate) fn format_type_node(node: &TypeNode) -> String {
    let suffix = if node.is_optional { "?" } else { "" };
    let path_str = node.path.join(".");
    if node.generics.is_empty() {
        format!("{path_str}{suffix}")
    } else {
        let generics: Vec<String> = node.generics.iter().map(format_type_node).collect();
        format!("{path_str}<{}>{suffix}", generics.join(", "))
    }
}

impl<'a> Evaluator<'a> {
    /// Validate `value` against `expected`. Mutates `value` in place when
    /// recursing through `List<T>` / `Dict<K, V>` so per-element checks see
    /// the same allocation.
    pub(crate) fn check_type(
        &self,
        value: &mut Value,
        expected: &TypeNode,
        scope: &Arc<Scope>,
        range: TokenRange,
    ) -> Result<(), RuntimeError> {
        self.check_type_internal(value, expected, scope, range, &mut HashSet::new(), 0)
    }

    pub(crate) fn check_type_internal(
        &self,
        value: &mut Value,
        expected: &TypeNode,
        scope: &Arc<Scope>,
        range: TokenRange,
        visited: &mut HashSet<(String, *const Value)>,
        depth: usize,
    ) -> Result<(), RuntimeError> {
        if depth > 20 {
            return Err(RuntimeError::UnsupportedOperator(
                "Type recursion depth exceeded".to_string(),
                range,
            ));
        }

        if expected.is_optional && matches!(value, Value::Null) {
            return Ok(());
        }

        let expected_str = format_type_node(expected);

        // Recursion guard for custom schemas: stop if we'd re-check the same
        // (Schema name, value pointer) pair.
        let tname = expected.path.join(".");
        if !is_builtin_type_name(&tname) {
            let ptr = value as *const Value;
            if !visited.insert((tname.clone(), ptr)) {
                return Ok(());
            }
        }

        let matches = if expected.path.len() == 1 {
            match expected.path[0].as_str() {
                "Any" => true,
                "Int" => matches!(value, Value::Int(_)),
                "Float" => matches!(value, Value::Float(_)),
                "Number" => matches!(value, Value::Int(_) | Value::Float(_)),
                "String" => matches!(value, Value::String(_)),
                "Bool" => matches!(value, Value::Bool(_)),
                "Null" => matches!(value, Value::Null),
                "List" => self.check_list(value, expected, scope, range, visited, depth)?,
                "Dict" => self.check_dict(value, expected, scope, range, visited, depth)?,
                "Closure" | "Fn" => matches!(value, Value::Closure { .. }),
                "Enum" => self.check_enum(value, expected, scope, range, visited, depth)?,
                _ => self.check_custom_schema(
                    value,
                    &expected.path,
                    scope,
                    range,
                    visited,
                    depth + 1,
                )?,
            }
        } else {
            self.check_custom_schema(value, &expected.path, scope, range, visited, depth + 1)?
        };

        if !matches {
            return Err(RuntimeError::TypeMismatch {
                expected: expected_str,
                found: value.type_name().to_string(),
                range,
            });
        }
        Ok(())
    }

    fn check_list(
        &self,
        value: &mut Value,
        expected: &TypeNode,
        scope: &Arc<Scope>,
        range: TokenRange,
        visited: &mut HashSet<(String, *const Value)>,
        depth: usize,
    ) -> Result<bool, RuntimeError> {
        let Value::List(l) = value else {
            return Ok(false);
        };
        if let Some(generic) = expected.generics.first() {
            for item in Arc::make_mut(l).iter_mut() {
                self.check_type_internal(item, generic, scope, range, visited, depth + 1)?;
            }
        }
        Ok(true)
    }

    fn check_dict(
        &self,
        value: &mut Value,
        expected: &TypeNode,
        scope: &Arc<Scope>,
        range: TokenRange,
        visited: &mut HashSet<(String, *const Value)>,
        depth: usize,
    ) -> Result<bool, RuntimeError> {
        let Value::Dict(d) = value else {
            return Ok(false);
        };
        if expected.generics.len() == 2 {
            let val_type = &expected.generics[1];
            for val in Arc::make_mut(d).map.values_mut() {
                self.check_type_internal(val, val_type, scope, range, visited, depth + 1)?;
            }
        }
        Ok(true)
    }

    fn check_enum(
        &self,
        value: &mut Value,
        expected: &TypeNode,
        scope: &Arc<Scope>,
        range: TokenRange,
        visited: &mut HashSet<(String, *const Value)>,
        depth: usize,
    ) -> Result<bool, RuntimeError> {
        for choice in &expected.generics {
            let mut temp = value.clone();
            if self
                .check_type_internal(&mut temp, choice, scope, range, visited, depth + 1)
                .is_ok()
            {
                return Ok(true);
            }
            if let Value::String(s) = value {
                if choice.path.len() == 1 && choice.path[0] == *s {
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }

    pub(crate) fn check_custom_schema(
        &self,
        value: &mut Value,
        path: &[String],
        scope: &Arc<Scope>,
        range: TokenRange,
        visited: &mut HashSet<(String, *const Value)>,
        depth: usize,
    ) -> Result<bool, RuntimeError> {
        let mut current_val = scope
            .get_local(&path[0])
            .ok_or_else(|| RuntimeError::VariableNotFound(path[0].clone(), range))?;

        for part in &path[1..] {
            match current_val {
                Value::Dict(d) => {
                    current_val = d.map.get(part).cloned().ok_or_else(|| {
                        RuntimeError::VariableNotFound(format!("{}.{part}", path[0]), range)
                    })?;
                }
                _ => return Ok(false),
            }
        }

        match current_val {
            Value::Schema { fields, .. } => self.apply_schema(fields, value, scope, range, visited, depth),
            Value::Type(t) => {
                if t.path == path {
                    return Ok(false);
                }
                self.check_type_internal(value, &t, scope, range, visited, depth)
                    .map(|_| true)
            }
            _ => Ok(false),
        }
    }

    fn apply_schema(
        &self,
        fields: HashMap<String, SchemaField>,
        value: &mut Value,
        scope: &Arc<Scope>,
        range: TokenRange,
        visited: &mut HashSet<(String, *const Value)>,
        depth: usize,
    ) -> Result<bool, RuntimeError> {
        let Value::Dict(d) = value else {
            return Ok(false);
        };
        let d = Arc::make_mut(d);
        let mut deferred_closures: Vec<(String, Value)> = Vec::new();
        for (field_name, field) in fields.iter() {
            if let Some(field_val) = d.map.get_mut(field_name) {
                self.check_type_internal(
                    field_val,
                    &field.type_hint,
                    scope,
                    range,
                    visited,
                    depth,
                )?;
                // AND-evaluate every closure predicate; the first failing one
                // short-circuits with `custom_error` (or a generic message).
                // Non-closure predicates (e.g. `Wildcard` from `Type field: *`)
                // are skipped.
                for predicate in &field.predicates {
                    if !matches!(predicate, Value::Closure { .. }) {
                        continue;
                    }
                    let result = self.call_function_by_value(
                        predicate.clone(),
                        vec![EvaluatedArg::positional(field_val.clone())],
                        scope,
                        range,
                    )?;
                    if !result.is_truthy() {
                        let err_msg = field
                            .custom_error
                            .clone()
                            .unwrap_or_else(|| format!("predicate constraint for '{field_name}'"));
                        return Err(RuntimeError::TypeMismatch {
                            expected: err_msg,
                            found: field_val.to_string(),
                            range,
                        });
                    }
                }
            } else if let Some(ref def) = field.default_value {
                if matches!(def, Value::Closure { .. }) {
                    // Computed default: defer to a second pass so the closure
                    // sees explicit + literal-default fields via `self`.
                    // Closure defaults do not observe each other — semantics
                    // stay independent of HashMap iteration order.
                    deferred_closures.push((field_name.clone(), def.clone()));
                } else {
                    d.map.insert(field_name.clone(), def.clone());
                }
            } else if field.type_hint.is_optional {
                continue;
            } else {
                return Err(RuntimeError::TypeMismatch {
                    expected: format!("field '{field_name}'"),
                    found: "missing".to_string(),
                    range,
                });
            }
        }
        if !deferred_closures.is_empty() {
            let self_snapshot = Value::Dict(Arc::new(d.clone()));
            for (field_name, def) in deferred_closures {
                let computed = self.call_function_by_value(
                    def,
                    vec![EvaluatedArg::positional(self_snapshot.clone())],
                    scope,
                    range,
                )?;
                d.map.insert(field_name, computed);
            }
        }
        Ok(true)
    }

    /// Fold a literal-dict AST into an existing schema as the RHS of
    /// `Schema + Dict`.
    ///
    /// Each pair contributes either a typed-field definition (when its
    /// `value_node` carries a `type_hint` or a closure predicate) or a
    /// default-value patch (literal value with no type info). Pure-default
    /// patches preserve the LHS's type/predicate info; typed entries
    /// AND-merge predicates and replace the type hint via
    /// [`merge_schema_fields`]. New keys not in the LHS are added either way:
    /// typed entries become full schema fields, literal entries become
    /// `Any`-typed fields whose `default_value` carries the literal.
    pub(crate) fn merge_schema_with_dict_pairs(
        &self,
        mut base_fields: HashMap<String, SchemaField>,
        pairs: &[(TokenKey, Node)],
        scope: &Arc<Scope>,
    ) -> Result<HashMap<String, SchemaField>, RuntimeError> {
        for (key, value_node) in pairs {
            let TokenKey::String(key_name, _, _) = key else {
                continue;
            };

            // Decide which shape this pair is. Type hint or predicate-shaped
            // body → schema definition. Anything else (string, int, dict
            // literal, reference, ...) → default-value patch.
            let is_field_shape = value_node.type_hint.is_some()
                || matches!(value_node.expr.as_ref(), Expr::Closure { .. });

            if is_field_shape {
                let (type_node, predicate) =
                    self.extract_field_type_and_predicate(value_node, scope)?;
                let mut field = SchemaField {
                    type_hint: type_node,
                    predicates: vec![predicate],
                    custom_error: None,
                    default_value: None,
                };
                for v_dec in &value_node.decorators {
                    let d_name = decorator_name(v_dec);
                    if let Some(plugin) = self.context.decorators.get(&d_name).cloned() {
                        let evaluated_args = self.evaluate_call_args(&v_dec.args, scope)?;
                        plugin.schema_field_meta(
                            self,
                            &mut field,
                            scope,
                            &evaluated_args,
                            v_dec.range,
                        )?;
                    }
                }
                let mut single = HashMap::new();
                single.insert(key_name.clone(), field);
                merge_schema_fields(&mut base_fields, single);
            } else {
                let val = self.eval_internal(value_node, scope, false)?;
                match base_fields.get_mut(key_name) {
                    Some(existing) => {
                        existing.default_value = Some(val);
                    }
                    None => {
                        base_fields.insert(
                            key_name.clone(),
                            SchemaField {
                                type_hint: TypeNode {
                                    path: vec!["Any".to_string()],
                                    generics: Vec::new(),
                                    is_optional: false,
                                    range: value_node.range,
                                    variant_fields: None,
                                    doc_comment: None,
                                },
                                predicates: vec![Value::Wildcard],
                                custom_error: None,
                                default_value: Some(val),
                            },
                        );
                    }
                }
            }
        }
        Ok(base_fields)
    }

    /// Convert an analyzer-produced [`SchemaDef`] into the runtime
    /// `HashMap<String, SchemaField>` form. This is the fast-path used
    /// when `Context::analyzed.schema(node.id)` hits — the analyzer has
    /// already split the body into typed fields, so we only do what
    /// genuinely requires the live scope: instantiate predicate
    /// closures, resolve base schemas via reference, and run
    /// per-decorator `schema_field_meta` hooks.
    pub fn build_schema_from_def(
        &self,
        def: &SchemaDef,
        scope: &Arc<Scope>,
    ) -> Result<HashMap<String, SchemaField>, RuntimeError> {
        let mut fields: HashMap<String, SchemaField> = HashMap::new();
        for base in &def.bases {
            let base_value = self.eval_internal(&base.node, scope, false)?;
            let Value::Schema { fields: base_fields, .. } = base_value else {
                return Err(RuntimeError::TypeMismatch {
                    expected: "Schema".to_string(),
                    found: base_value.type_name().to_string(),
                    range: base.node.range,
                });
            };
            merge_schema_fields(&mut fields, base_fields);
        }
        for field_def in &def.fields {
            self.apply_field_def(field_def, &mut fields, scope)?;
        }
        Ok(fields)
    }

    /// Apply a single `SchemaFieldDef` to the in-progress field map.
    ///
    /// Two shapes are supported:
    ///
    /// * **Field definition** — has a static type hint or a closure
    ///   predicate. AND-merges into any existing field of the same
    ///   name and overrides type_hint / decorator-supplied metadata.
    /// * **Default-only patch** — a plain literal value with no type
    ///   prefix and no closure predicate. Used in `Base + { x: "v" }`
    ///   to override the default of `x` (or, if `x` doesn't exist,
    ///   add a permissive `Any`-typed default-only field).
    fn apply_field_def(
        &self,
        def: &SchemaFieldDef,
        fields: &mut HashMap<String, SchemaField>,
        scope: &Arc<Scope>,
    ) -> Result<(), RuntimeError> {
        let value_node = def.value_node.as_ref();
        let is_field_shape = def.type_hint.is_some()
            || matches!(value_node.expr.as_ref(), relon_parser::Expr::Closure { .. })
            || matches!(value_node.expr.as_ref(), relon_parser::Expr::Type(_))
            || matches!(value_node.expr.as_ref(), relon_parser::Expr::Variable(_));

        if is_field_shape {
            // Fast path: a `SchemaFieldDef::type_hint` synthesized by the
            // analyzer (e.g. lifted from `@brand(X)`) won't be reflected on
            // the underlying `value_node.type_hint`, so a `Wildcard` value
            // would fail the `Type or Type Prefix` check inside
            // `extract_field_type_and_predicate`. Short-circuit here: if
            // we already have an authoritative type hint and the value is
            // just `*`, the predicate is trivially a wildcard.
            let (type_node, predicate) = if def.type_hint.is_some()
                && matches!(value_node.expr.as_ref(), relon_parser::Expr::Wildcard)
            {
                (def.type_hint.clone().unwrap(), Value::Wildcard)
            } else {
                self.extract_field_type_and_predicate(value_node, scope)?
            };
            let mut field = SchemaField {
                type_hint: def.type_hint.clone().unwrap_or(type_node),
                predicates: vec![predicate],
                custom_error: None,
                default_value: None,
            };
            for meta in &def.meta_decorators {
                if let Some(plugin) = self.context.decorators.get(&meta.name).cloned() {
                    let evaluated_args = self.evaluate_call_args(&meta.decorator.args, scope)?;
                    plugin.schema_field_meta(
                        self,
                        &mut field,
                        scope,
                        &evaluated_args,
                        meta.range,
                    )?;
                }
            }
            let mut single = HashMap::new();
            single.insert(def.name.clone(), field);
            merge_schema_fields(fields, single);
        } else {
            // Default-only patch: literal value, no type info. Evaluate
            // and either fold into an existing field or create a new
            // `Any`-typed default-only field.
            let val = self.eval_internal(value_node, scope, false)?;
            match fields.get_mut(&def.name) {
                Some(existing) => {
                    existing.default_value = Some(val);
                }
                None => {
                    fields.insert(
                        def.name.clone(),
                        SchemaField {
                            type_hint: TypeNode {
                                path: vec!["Any".to_string()],
                                generics: Vec::new(),
                                is_optional: false,
                                range: value_node.range,
                                variant_fields: None,
                                doc_comment: None,
                            },
                            predicates: vec![Value::Wildcard],
                            custom_error: None,
                            default_value: Some(val),
                        },
                    );
                }
            }
        }
        Ok(())
    }

    pub(crate) fn extract_field_type_and_predicate(
        &self,
        value_node: &Node,
        scope: &Arc<Scope>,
    ) -> Result<(TypeNode, Value), RuntimeError> {
        if let Some(t) = &value_node.type_hint {
            let pred = self.eval_internal(value_node, scope, true)?;
            return Ok((t.clone(), pred));
        }
        match value_node.expr.as_ref() {
            Expr::Variable(vpath) => {
                let path: Vec<String> = vpath.iter().map(|k| k.name()).collect();
                Ok((
                    TypeNode {
                        path,
                        generics: Vec::new(),
                        is_optional: false,
                        range: value_node.range,
                        variant_fields: None,
                        doc_comment: None,
                    },
                    Value::Wildcard,
                ))
            }
            _ => match self.eval_internal(value_node, scope, true)? {
                Value::Type(t) => Ok((t, Value::Wildcard)),
                other => Err(RuntimeError::TypeMismatch {
                    expected: "Type or Type Prefix".to_string(),
                    found: other.type_name().to_string(),
                    range: value_node.range,
                }),
            },
        }
    }
}

/// AND-merge `patch` into `base`: predicates accumulate (Wildcards skipped),
/// type hints get replaced, and `custom_error` / `default_value` are
/// overridden only when the patch supplies them.
pub(crate) fn merge_schema_fields(
    base: &mut HashMap<String, SchemaField>,
    patch: HashMap<String, SchemaField>,
) {
    for (k, patch) in patch {
        match base.get_mut(&k) {
            Some(existing) => {
                existing.type_hint = patch.type_hint;
                for pred in patch.predicates {
                    if !matches!(pred, Value::Wildcard) {
                        existing.predicates.push(pred);
                    }
                }
                if patch.custom_error.is_some() {
                    existing.custom_error = patch.custom_error;
                }
                if patch.default_value.is_some() {
                    existing.default_value = patch.default_value;
                }
            }
            None => {
                base.insert(k, patch);
            }
        }
    }
}
