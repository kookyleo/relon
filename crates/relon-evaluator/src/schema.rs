use crate::error::RuntimeError;
use crate::eval::{decorator_name, Evaluator};
use crate::native_fn::EvaluatedArg;
use crate::scope::Scope;
use crate::value::{SchemaField, Value};
use relon_analyzer::{SchemaDef, SchemaFieldDef};
use relon_parser::{is_builtin_type_name, Expr, Node, TokenKey, TokenRange, TypeNode};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

impl Evaluator {
    pub(crate) fn check_type(
        &self,
        value: &mut Value,
        type_hint: &TypeNode,
        scope: &Arc<Scope>,
        range: TokenRange,
    ) -> Result<(), RuntimeError> {
        self.check_type_internal(value, type_hint, scope, range, &mut HashSet::new(), 0)
    }

    fn check_type_internal(
        &self,
        value: &mut Value,
        type_hint: &TypeNode,
        scope: &Arc<Scope>,
        range: TokenRange,
        visited: &mut HashSet<(String, *const Value)>,
        depth: usize,
    ) -> Result<(), RuntimeError> {
        // Bail out before blowing the stack on a self-referential schema.
        // This is a *recursion-depth* bound, distinct from the
        // step-counter budget that gates overall evaluator work — see
        // `RuntimeError::RecursionLimitExceeded` vs `StepLimitExceeded`.
        const MAX_TYPE_CHECK_DEPTH: usize = 100;
        if depth > MAX_TYPE_CHECK_DEPTH {
            return Err(RuntimeError::RecursionLimitExceeded {
                limit: MAX_TYPE_CHECK_DEPTH,
                range,
            });
        }

        if type_hint.is_optional && matches!(value, Value::Null) {
            return Ok(());
        }

        let type_name = type_hint.path.join(".");
        if is_builtin_type_name(&type_name) {
            match (type_name.as_str(), value) {
                ("Any", _) => Ok(()),
                ("Int", Value::Int(_)) => Ok(()),
                ("Float", Value::Float(_)) => Ok(()),
                ("Number", Value::Int(_)) | ("Number", Value::Float(_)) => Ok(()),
                ("String", Value::String(_)) => Ok(()),
                ("Bool", Value::Bool(_)) => Ok(()),
                ("List", Value::List(l)) => {
                    if let Some(item_type) = type_hint.generics.first() {
                        let l_mut = Arc::make_mut(l);
                        for item in l_mut.iter_mut() {
                            self.check_type_internal(
                                item,
                                item_type,
                                scope,
                                range,
                                visited,
                                depth + 1,
                            )?;
                        }
                    }
                    Ok(())
                }
                ("Dict", Value::Dict(d)) => {
                    if type_hint.generics.len() == 2 {
                        let val_type = &type_hint.generics[1];
                        let d_mut = Arc::make_mut(d);
                        for val in d_mut.map.values_mut() {
                            self.check_type_internal(
                                val,
                                val_type,
                                scope,
                                range,
                                visited,
                                depth + 1,
                            )?;
                        }
                    }
                    Ok(())
                }
                ("Tuple", Value::List(l)) => {
                    // v1.7: a tuple is positionally-typed. The runtime
                    // representation reuses `Value::List`, so we
                    // length-check the list, then recurse per position
                    // into each declared element type.
                    let expected_arity = type_hint.generics.len();
                    if l.len() != expected_arity {
                        return Err(RuntimeError::TypeMismatch {
                            expected: format_type_node(type_hint),
                            found: format!("List of length {}", l.len()),
                            range,
                        });
                    }
                    let l_mut = Arc::make_mut(l);
                    for (item, slot_ty) in l_mut.iter_mut().zip(type_hint.generics.iter()) {
                        self.check_type_internal(item, slot_ty, scope, range, visited, depth + 1)?;
                    }
                    Ok(())
                }
                ("Enum", val) => {
                    // Two-pass match: cheap literal/primitive matchers
                    // first (no clone), then structural alternatives
                    // (clone-and-recurse).
                    let mut matched = false;
                    for alt in &type_hint.generics {
                        if Self::enum_alt_matches_cheaply(alt, val) {
                            matched = true;
                            break;
                        }
                    }
                    if !matched {
                        for alt in &type_hint.generics {
                            // Skip alts already ruled out by the cheap pass.
                            if Self::is_cheap_enum_alt(alt) {
                                continue;
                            }
                            let mut temp_val = val.clone();
                            if self
                                .check_type_internal(
                                    &mut temp_val,
                                    alt,
                                    scope,
                                    range,
                                    visited,
                                    depth + 1,
                                )
                                .is_ok()
                            {
                                matched = true;
                                break;
                            }
                        }
                    }
                    if matched {
                        Ok(())
                    } else {
                        let alts: Vec<String> =
                            type_hint.generics.iter().map(format_type_node).collect();
                        Err(RuntimeError::TypeMismatch {
                            expected: format!("one of [{}]", alts.join(", ")),
                            found: val.to_string(),
                            range,
                        })
                    }
                }
                (expected, found_val) => Err(RuntimeError::TypeMismatch {
                    expected: expected.to_string(),
                    found: found_val.type_name().to_string(),
                    range,
                }),
            }?;
            return Ok(());
        }

        // Custom Schema lookup. The first path segment is resolved against
        // local scope or BrandRegistry; subsequent segments dive into nested
        // dicts (e.g. `geo.Location` → `scope.geo.Location`).
        let path = &type_hint.path;
        let mut schema_val = scope
            .get_local(&path[0])
            .or_else(|| self.context.schemas.get(&path[0]).cloned())
            .ok_or_else(|| RuntimeError::VariableNotFound(path[0].clone(), range))?;
        for part in &path[1..] {
            schema_val = match schema_val {
                Value::Dict(d) => d
                    .map
                    .get(part)
                    .cloned()
                    .ok_or_else(|| RuntimeError::VariableNotFound(type_name.clone(), range))?,
                _ => {
                    return Err(RuntimeError::VariableNotFound(type_name.clone(), range));
                }
            };
        }

        match schema_val {
            Value::Schema { generics, fields } => {
                let mut subst_map = HashMap::new();
                for (i, gname) in generics.iter().enumerate() {
                    if let Some(gtype) = type_hint.generics.get(i) {
                        subst_map.insert(gname.clone(), gtype.clone());
                    }
                }
                let resolved_fields = if subst_map.is_empty() {
                    fields
                } else {
                    Self::substitute_generics_in_schema(fields, &subst_map)
                };

                let ptr = value as *const Value;
                if !visited.insert((type_name, ptr)) {
                    return Ok(());
                }

                self.apply_schema(resolved_fields, value, scope, range, visited, depth + 1)?;
                Ok(())
            }
            Value::EnumSchema {
                generics, variants, ..
            } => {
                let variant_name = match value {
                    Value::Dict(d) => d.brand.clone(),
                    _ => {
                        return Err(RuntimeError::TypeMismatch {
                            expected: format!("a variant of enum {type_name}"),
                            found: value.type_name().to_string(),
                            range,
                        })
                    }
                };
                let variant_name = variant_name.ok_or_else(|| RuntimeError::TypeMismatch {
                    expected: format!("branded variant of {type_name}"),
                    found: "plain dict".to_string(),
                    range,
                })?;
                let fields =
                    variants
                        .get(&variant_name)
                        .ok_or_else(|| RuntimeError::TypeMismatch {
                            expected: format!("valid variant of {type_name}"),
                            found: variant_name.clone(),
                            range,
                        })?;

                // Pair up declared generic param names with the
                // concrete types passed at the use-site (`Result<Int,
                // String>` → `{T -> Int, E -> String}`). Missing trailing
                // generics are simply not substituted; downstream type
                // checks will catch obvious mismatches.
                let mut subst_map = HashMap::new();
                for (i, gname) in generics.iter().enumerate() {
                    if let Some(gtype) = type_hint.generics.get(i) {
                        subst_map.insert(gname.clone(), gtype.clone());
                    }
                }

                let mut fields_map = HashMap::new();
                for (name, field_def) in fields {
                    fields_map.insert(name.clone(), field_def.clone());
                }
                let resolved_fields = if subst_map.is_empty() {
                    fields_map
                } else {
                    Self::substitute_generics_in_schema(fields_map, &subst_map)
                };

                let ptr = value as *const Value;
                if !visited.insert((type_name, ptr)) {
                    return Ok(());
                }

                self.apply_schema(resolved_fields, value, scope, range, visited, depth + 1)?;
                Ok(())
            }
            _ => Err(RuntimeError::TypeMismatch {
                expected: "Schema".to_string(),
                found: schema_val.type_name().to_string(),
                range,
            }),
        }
    }

    /// True if `alt` is matchable without cloning: a string-literal
    /// alternative (quoted or bareword) or a single-segment built-in
    /// type name with no generics. Bareword non-builtins are treated as
    /// candidate string literals — they could still resolve to a custom
    /// schema in a later pass, but the cheap pre-check is allowed to
    /// pre-empt that with a `Value::String` match.
    fn is_cheap_enum_alt(alt: &TypeNode) -> bool {
        alt.path.len() == 1 && alt.generics.is_empty() && !alt.is_optional
    }

    /// Cheap, no-clone match for an enum alternative against `val`.
    /// Returns `true` only if the alt definitively matches; complex
    /// alts (custom schemas, generics, optionals) fall through to the
    /// structural pass.
    fn enum_alt_matches_cheaply(alt: &TypeNode, val: &Value) -> bool {
        if !Self::is_cheap_enum_alt(alt) {
            return false;
        }
        let p = &alt.path[0];
        // Built-in primitive / collection check.
        let prim_match = match (p.as_str(), val) {
            ("Any", _) => Some(true),
            ("Null", Value::Null) => Some(true),
            ("Int", Value::Int(_)) => Some(true),
            ("Float", Value::Float(_)) => Some(true),
            ("Number", Value::Int(_) | Value::Float(_)) => Some(true),
            ("String", Value::String(_)) => Some(true),
            ("Bool", Value::Bool(_)) => Some(true),
            ("List", Value::List(_)) => Some(true),
            ("Dict", Value::Dict(_)) => Some(true),
            ("Closure" | "Fn", Value::Closure { .. }) => Some(true),
            _ if is_builtin_type_name(p) => Some(false),
            _ => None,
        };
        if let Some(m) = prim_match {
            return m;
        }
        // Bareword or quoted string literal alternative — match the
        // cleaned form against the string value.
        let clean = if Self::is_quoted_string_literal(p) {
            &p[1..p.len() - 1]
        } else {
            p.as_str()
        };
        matches!(val, Value::String(s) if s == clean)
    }

    fn is_quoted_string_literal(p: &str) -> bool {
        (p.starts_with('"') && p.ends_with('"') && p.len() >= 2)
            || (p.starts_with('\'') && p.ends_with('\'') && p.len() >= 2)
    }

    fn substitute_generics_in_schema(
        mut fields: HashMap<String, SchemaField>,
        subst_map: &HashMap<String, TypeNode>,
    ) -> HashMap<String, SchemaField> {
        for field in fields.values_mut() {
            Self::substitute_generics_in_type(&mut field.type_hint, subst_map);
        }
        fields
    }

    fn substitute_generics_in_type(t: &mut TypeNode, subst_map: &HashMap<String, TypeNode>) {
        if t.path.len() == 1 && t.generics.is_empty() {
            if let Some(replacement) = subst_map.get(&t.path[0]) {
                let is_optional = t.is_optional || replacement.is_optional;
                *t = replacement.clone();
                t.is_optional = is_optional;
                return;
            }
        }
        for generic in &mut t.generics {
            Self::substitute_generics_in_type(generic, subst_map);
        }
    }

    pub(crate) fn apply_schema(
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

    pub fn build_schema_from_def(
        &self,
        def: &SchemaDef,
        scope: &Arc<Scope>,
    ) -> Result<HashMap<String, SchemaField>, RuntimeError> {
        let mut fields: HashMap<String, SchemaField> = HashMap::new();
        for base in &def.bases {
            let base_value = self.eval_internal(&base.node, scope, false)?;
            let Value::Schema {
                fields: base_fields,
                ..
            } = base_value
            else {
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
                    let evaluated_args =
                        self.evaluate_directive_meta_args(&meta.directive, scope)?;
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

pub fn format_type_node(t: &TypeNode) -> String {
    let mut s = t.path.join(".");
    if !t.generics.is_empty() {
        s.push('<');
        s.push_str(
            &t.generics
                .iter()
                .map(format_type_node)
                .collect::<Vec<_>>()
                .join(", "),
        );
        s.push('>');
    }
    if t.is_optional {
        s.push('?');
    }
    s
}
