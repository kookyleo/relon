//! Reference resolution: `&root`, `&sibling`, `&uncle`, `&this`, `&prev`,
//! `&next`, `&index`, plain variable lookups, and the lazy-thunk machinery
//! that keeps the dict-evaluation order independent of declaration order.
//!
//! Split out from [`crate::eval`] because reference resolution forms a
//! self-contained sub-system: it has its own `ReferenceStep` enum, its own
//! caching protocol against [`crate::eval::Context::path_cache`] and
//! `evaluating_paths`, and its own circular-detection logic. Keeping it
//! adjacent to (but separate from) the main `eval_internal` dispatcher makes
//! both halves easier to follow.

use crate::error::RuntimeError;
use crate::eval::{is_private_field, Evaluator};
use crate::scope::{ListContext, Scope, Thunk};
use crate::value::Value;
use relon_parser::{Expr, Node, RefBase, TokenKey, TokenRange};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// Result of looking up a single dict key during reference path resolution.
/// Either we have a registered thunk to force lazily, or we've already
/// produced a value (typically from a spread expression that was evaluated
/// eagerly to inspect its contents).
pub(crate) enum ReferenceStep {
    Thunk(Arc<Thunk>),
    Value(Box<Value>),
}

/// Decrement `owning_depth` for the next step, or convert it to `None`
/// once we've descended past the owning dict. See the docstring on
/// `Evaluator::resolve_reference` for the meaning of the counter.
fn child_owning_depth(d: Option<usize>) -> Option<usize> {
    match d {
        Some(n) if n > 0 => Some(n - 1),
        _ => None,
    }
}

impl Evaluator {
    pub(crate) fn resolve_variable(
        &self,
        path: &[TokenKey],
        scope: &Arc<Scope>,
        range: TokenRange,
    ) -> Result<Value, RuntimeError> {
        if path.is_empty() {
            return Err(RuntimeError::VariableNotFound(
                "Empty path".to_string(),
                range,
            ));
        }
        let first = &path[0];
        let first_name = first.to_string_key();
        let mut current_val = if let Some(val) = scope.get_local(&first_name) {
            val
        } else if let Some(thunk) = scope.get_thunk(&first_name) {
            self.force_thunk(&thunk)?
        } else {
            return Err(RuntimeError::VariableNotFound(first_name, range));
        };
        let mut parts = vec![first_name.clone()];
        for part in &path[1..] {
            let is_optional = part.is_optional();
            let key = match part {
                TokenKey::Dynamic(expr_node, _) => {
                    let val = self.eval(expr_node, scope)?;
                    match val {
                        Value::String(s) => s,
                        Value::Int(i) => i.to_string(),
                        other => {
                            return Err(RuntimeError::TypeMismatch {
                                expected: "String or Int for dynamic key".to_string(),
                                found: other.type_name().to_string(),
                                range: expr_node.range,
                            })
                        }
                    }
                }
                _ => part.to_string_key(),
            };
            parts.push(key.clone());
            let display_name = parts.join(".");

            match current_val {
                Value::Dict(ref d) => {
                    if let Some(val) = d.map.get(&key) {
                        current_val = val.clone();
                    } else if is_optional {
                        return Ok(Value::Null);
                    } else {
                        return Err(RuntimeError::VariableNotFound(display_name, range));
                    }
                }
                Value::List(ref list) => {
                    let idx = key
                        .parse::<usize>()
                        .map_err(|_| RuntimeError::TypeMismatch {
                            expected: "Index".to_string(),
                            found: "String".to_string(),
                            range,
                        })?;
                    if let Some(val) = list.get(idx) {
                        current_val = val.clone();
                    } else if is_optional {
                        return Ok(Value::Null);
                    } else {
                        return Err(RuntimeError::VariableNotFound(display_name, range));
                    }
                }
                Value::Null if is_optional => return Ok(Value::Null),
                _ => {
                    if is_optional {
                        return Ok(Value::Null);
                    }
                    return Err(RuntimeError::TypeMismatch {
                        expected: "Dict/List".to_string(),
                        found: current_val.type_name().to_string(),
                        range,
                    });
                }
            }
        }
        Ok(current_val)
    }

    pub(crate) fn resolve_reference(
        &self,
        base: &RefBase,
        path: &[TokenKey],
        scope: &Arc<Scope>,
        range: TokenRange,
    ) -> Result<Value, RuntimeError> {
        match base {
            RefBase::Index => {
                let context = scope.list_context.as_ref().ok_or_else(|| {
                    RuntimeError::VariableNotFound(
                        "&index can only be used inside a list".to_string(),
                        range,
                    )
                })?;
                return Ok(Value::Int(context.index as i64));
            }
            RefBase::Prev => {
                let context = scope.list_context.as_ref().ok_or_else(|| {
                    RuntimeError::VariableNotFound(
                        "&prev can only be used inside a list".to_string(),
                        range,
                    )
                })?;
                if context.index == 0 {
                    return Ok(Value::Null);
                }
                let target_index = context.index - 1;
                let thunk = context.elements.get(target_index).unwrap();
                let target_scope = self.list_element_scope(&thunk.scope, context, target_index);
                let val = self.force_thunk_with_scope(thunk, &target_scope)?;
                return self.lookup_value_path(val, path, "&prev", scope, range);
            }
            RefBase::Next => {
                let context = scope.list_context.as_ref().ok_or_else(|| {
                    RuntimeError::VariableNotFound(
                        "&next can only be used inside a list".to_string(),
                        range,
                    )
                })?;
                if context.index + 1 >= context.elements.len() {
                    return Ok(Value::Null);
                }
                let target_index = context.index + 1;
                let thunk = context.elements.get(target_index).unwrap();
                let target_scope = self.list_element_scope(&thunk.scope, context, target_index);
                let val = self.force_thunk_with_scope(thunk, &target_scope)?;
                return self.lookup_value_path(val, path, "&next", scope, range);
            }
            RefBase::This => {
                let root = scope
                    .root_ref
                    .as_ref()
                    .map(|r| r.node.as_ref())
                    .or(self.context.root_node.as_deref())
                    .ok_or_else(|| {
                        RuntimeError::VariableNotFound("No root for &this".to_string(), range)
                    })?;
                let display = self
                    .cache_path_keys(path, scope)
                    .map(|keys| keys.join("."))
                    .unwrap_or_else(|| "&this".to_string());
                // `&this` has no owning-dict relationship — every step is
                // a cross-dict access from the perspective of `#private`.
                return self.eval_reference_path(root, path, scope, &display, range, None);
            }
            _ => {}
        }

        let root = scope
            .root_ref
            .as_ref()
            .map(|r| r.node.as_ref())
            .or(self.context.root_node.as_deref())
            .ok_or(RuntimeError::VariableNotFound("No root".to_string(), range))?;
        // `owning_depth` counts the dict-steps between `root` and the
        // dict that *owns* the reference site (i.e. the `&sibling` /
        // `&uncle` anchor). When the path consumes that many dict steps
        // the next field access is *inside* the owning dict and may
        // see `#private` siblings; deeper accesses are cross-dict.
        // `None` means "no owning relationship" — `&root` always
        // crosses a dict boundary.
        let (mut target_path, owning_depth): (Vec<TokenKey>, Option<usize>) = match base {
            RefBase::Root => (Vec::new(), None),
            RefBase::Sibling => {
                let mut p = scope.full_path();
                p.pop();
                let depth = p.len();
                (
                    p.into_iter()
                        .map(|s| TokenKey::String(s, range, false))
                        .collect(),
                    Some(depth),
                )
            }
            RefBase::Uncle => {
                let mut p = scope.full_path();
                p.pop();
                p.pop();
                let depth = p.len();
                (
                    p.into_iter()
                        .map(|s| TokenKey::String(s, range, false))
                        .collect(),
                    Some(depth),
                )
            }
            _ => unreachable!(),
        };
        target_path.extend_from_slice(path);

        // Fix 1: build cache keys by *evaluating* dynamic segments so that
        // `&sibling.obj[&sibling.k1]` and `&sibling.obj[&sibling.k2]` get
        // distinct cache entries. Static-only paths still use `name()`.
        let resolved_keys = self.cache_path_keys(&target_path, scope);
        let path_str = resolved_keys
            .as_ref()
            .map(|v| v.join("."))
            .unwrap_or_else(|| {
                target_path
                    .iter()
                    .map(|k| k.name())
                    .collect::<Vec<_>>()
                    .join(".")
            });

        if !target_path.is_empty() {
            if let Some(keys) = resolved_keys.as_ref() {
                let cache_key = scope.path_cache_key(keys);
                if let Some(cached) = self.context.path_cache.lock().unwrap().get(&cache_key) {
                    return Ok(cached.clone());
                }
            }
        }
        let result =
            self.eval_reference_path(root, &target_path, scope, &path_str, range, owning_depth);
        if let Ok(value) = &result {
            if !target_path.is_empty() {
                if let Some(keys) = resolved_keys.as_ref() {
                    let cache_key = scope.path_cache_key(keys);
                    self.context
                        .path_cache
                        .lock()
                        .unwrap()
                        .insert(cache_key, value.clone());
                }
            }
        }
        result
    }

    /// Resolve every `TokenKey::Dynamic` segment of `path` against `scope`
    /// so the resulting `Vec<String>` can be used as a cache key. Returns
    /// `None` if any dynamic segment fails to evaluate or yields a
    /// non-key-typed value — in that case the caller bypasses the cache
    /// rather than risk a key collision on `<dynamic>`.
    fn cache_path_keys(&self, path: &[TokenKey], scope: &Arc<Scope>) -> Option<Vec<String>> {
        let mut out = Vec::with_capacity(path.len());
        for p in path {
            match p {
                TokenKey::Dynamic(expr, _) => match self.eval(expr, scope).ok()? {
                    Value::String(s) => out.push(s),
                    Value::Int(i) => out.push(i.to_string()),
                    _ => return None,
                },
                _ => out.push(p.name()),
            }
        }
        Some(out)
    }

    /// Build the scope used to force a sibling list element on `&prev` /
    /// `&next` access. Reuses the element thunk's owning scope (which already
    /// has the right `path_node` and lexical parent) but installs a
    /// `list_context` whose `index` points at the requested neighbour, so
    /// the forced element evaluates with `&index` / `&prev` / `&next`
    /// resolving relative to *its* slot rather than the caller's.
    fn list_element_scope(
        &self,
        base: &Arc<Scope>,
        context: &Arc<ListContext>,
        index: usize,
    ) -> Arc<Scope> {
        Arc::new(Scope {
            parent: base.parent.clone(),
            path_node: base.path_node.clone(),
            locals: Mutex::new(base.locals.lock().unwrap().clone()),
            current_dir: base.current_dir.clone(),
            cache_namespace: base.cache_namespace.clone(),
            root_ref: base.root_ref.clone(),
            list_context: Some(Arc::new(ListContext {
                index,
                elements: context.elements.clone(),
            })),
            thunks: Mutex::new(base.thunks.lock().unwrap().clone()),
        })
    }

    fn eval_reference_path(
        &self,
        root: &Node,
        path: &[TokenKey],
        original_scope: &Arc<Scope>,
        display_path: &str,
        range: TokenRange,
        owning_depth: Option<usize>,
    ) -> Result<Value, RuntimeError> {
        let mut target_scope = None;
        let mut current = Some(original_scope.clone());
        while let Some(scope) = current {
            if let Some(rr) = &scope.root_ref {
                if std::ptr::eq(rr.node.as_ref() as *const _, root as *const _) {
                    if let Some(root_scope) = &rr.scope {
                        target_scope = Some(root_scope.clone());
                        break;
                    }
                }
            }
            current = scope.parent.clone();
        }

        let root_scope = target_scope.unwrap_or_else(|| {
            let parent = original_scope
                .root_ref
                .as_ref()
                .and_then(|r| r.parent_fallback.clone());
            Arc::new(Scope {
                parent,
                path_node: None,
                locals: Mutex::new(HashMap::new()),
                current_dir: original_scope.current_dir.clone(),
                cache_namespace: original_scope.cache_namespace.clone(),
                root_ref: original_scope.root_ref.clone(),
                list_context: None,
                thunks: Mutex::new(HashMap::new()),
            })
        });

        self.eval_reference_path_from(root, &root_scope, path, display_path, range, owning_depth)
    }

    fn eval_reference_path_from(
        &self,
        node: &Node,
        scope: &Arc<Scope>,
        path: &[TokenKey],
        display_path: &str,
        range: TokenRange,
        owning_depth: Option<usize>,
    ) -> Result<Value, RuntimeError> {
        if path.is_empty() {
            return self.eval_node_with_path_cache(node, scope, display_path);
        }

        match node.expr.as_ref() {
            Expr::Dict(pairs) => {
                self.prepare_dict_scope(node, scope)?;
                let part = &path[0];
                let is_optional = part.is_optional();
                let key = match part {
                    TokenKey::Dynamic(expr_node, _) => {
                        let val = self.eval(expr_node, scope)?;
                        match val {
                            Value::String(s) => s,
                            Value::Int(i) => i.to_string(),
                            other => {
                                return Err(RuntimeError::TypeMismatch {
                                    expected: "String or Int for dynamic key".to_string(),
                                    found: other.type_name().to_string(),
                                    range: expr_node.range,
                                })
                            }
                        }
                    }
                    _ => part.name(),
                };
                let remaining_path = &path[1..];
                // Fix 4: when this dict step lands *outside* the
                // reference's owning dict, hide `#private` fields. The
                // caller threads `owning_depth` so `&sibling`/`&uncle`
                // reach their final step with `Some(0)` (allow private),
                // while `&root` and any deeper step starts with `None`
                // or an exhausted counter (block private).
                let block_private = owning_depth != Some(0);
                match self.resolve_dict_reference_step(pairs, &key, scope, block_private)? {
                    Some(ReferenceStep::Thunk(thunk)) => {
                        if remaining_path.is_empty() {
                            self.force_thunk(&thunk)
                        } else if matches!(thunk.node.expr.as_ref(), Expr::Dict(_) | Expr::List(_))
                        {
                            // Stepping into a sub-node — anything beyond
                            // here is cross-dict from the reference's
                            // perspective, so encode that as `None`.
                            self.eval_reference_path_from(
                                &thunk.node,
                                &thunk.scope,
                                remaining_path,
                                display_path,
                                range,
                                child_owning_depth(owning_depth),
                            )
                        } else {
                            let value = self.force_thunk(&thunk)?;
                            self.lookup_value_path(
                                value,
                                remaining_path,
                                display_path,
                                &thunk.scope,
                                range,
                            )
                        }
                    }
                    Some(ReferenceStep::Value(value)) => {
                        self.lookup_value_path(*value, remaining_path, display_path, scope, range)
                    }
                    None => {
                        // Not a dict-field — fall back to scope locals
                        // so standalone `#schema X Body` directives
                        // (whose names live in `scope.locals` rather
                        // than as dict pairs) are still reachable
                        // through `&root`/`&sibling.X`.
                        if let Some(local_val) = scope.get_local(&key) {
                            return self.lookup_value_path(
                                local_val,
                                remaining_path,
                                display_path,
                                scope,
                                range,
                            );
                        }
                        if is_optional {
                            Ok(Value::Null)
                        } else {
                            Err(RuntimeError::VariableNotFound(
                                display_path.to_string(),
                                range,
                            ))
                        }
                    }
                }
            }
            Expr::List(elements) => {
                let part = &path[0];
                let is_optional = part.is_optional();
                let key = match part {
                    TokenKey::Dynamic(expr_node, _) => {
                        let val = self.eval(expr_node, scope)?;
                        match val {
                            Value::String(s) => s,
                            Value::Int(i) => i.to_string(),
                            other => {
                                return Err(RuntimeError::TypeMismatch {
                                    expected: "String or Int for dynamic key".to_string(),
                                    found: other.type_name().to_string(),
                                    range: expr_node.range,
                                })
                            }
                        }
                    }
                    _ => part.name(),
                };
                let index = key
                    .parse::<usize>()
                    .map_err(|_| RuntimeError::VariableNotFound(display_path.to_string(), range))?;
                let item_scope = scope.with_path(key.clone());
                let item = elements.get(index);
                if let Some(it) = item {
                    self.eval_reference_path_from(
                        it,
                        &item_scope,
                        &path[1..],
                        display_path,
                        range,
                        child_owning_depth(owning_depth),
                    )
                } else if is_optional {
                    Ok(Value::Null)
                } else {
                    Err(RuntimeError::VariableNotFound(
                        display_path.to_string(),
                        range,
                    ))
                }
            }
            _ => {
                let part = &path[0];
                if part.is_optional() {
                    Ok(Value::Null)
                } else {
                    let value = self.eval_node_with_path_cache(node, scope, display_path)?;
                    self.lookup_value_path(value, path, display_path, scope, range)
                }
            }
        }
    }

    fn resolve_dict_reference_step(
        &self,
        pairs: &[(TokenKey, Node)],
        part: &str,
        scope: &Arc<Scope>,
        block_private: bool,
    ) -> Result<Option<ReferenceStep>, RuntimeError> {
        for (key, value_node) in pairs.iter().rev() {
            match key {
                TokenKey::Spread(_) => {
                    // Spread results come from a `Value::Dict`, which has
                    // already had its `#private` fields stripped at dict
                    // build time — so this branch is naturally safe.
                    let spread_value = self.eval(value_node, scope)?;
                    if let Value::Dict(d) = spread_value {
                        if let Some(value) = d.map.get(part) {
                            return Ok(Some(ReferenceStep::Value(Box::new(value.clone()))));
                        }
                    }
                }
                _ => {
                    let key_str = match key {
                        TokenKey::String(s, _, _) => s.clone(),
                        TokenKey::Dynamic(expr_node, _) => match self.eval(expr_node, scope)? {
                            Value::String(s) => s,
                            Value::Int(i) => i.to_string(),
                            _ => continue,
                        },
                        _ => key.to_string_key(),
                    };
                    if key_str == part {
                        // Fix 4: pretend the field doesn't exist when the
                        // caller crossed a dict boundary to reach it.
                        // Same-dict sibling access goes through
                        // `resolve_variable`, which reads the thunk table
                        // directly without coming through here.
                        if block_private && is_private_field(value_node) {
                            return Ok(None);
                        }
                        if let Some(thunk) = scope.get_own_thunk(part) {
                            return Ok(Some(ReferenceStep::Thunk(thunk)));
                        }
                    }
                }
            }
        }
        Ok(None)
    }

    fn eval_node_with_path_cache(
        &self,
        node: &Node,
        scope: &Arc<Scope>,
        _display_path: &str,
    ) -> Result<Value, RuntimeError> {
        let full_path = scope.full_path();

        let cache_key = scope.path_cache_key(&full_path);
        if self
            .context
            .evaluating_paths
            .lock()
            .unwrap()
            .contains(&cache_key)
        {
            return Err(RuntimeError::CircularReference {
                cycle: full_path,
                range: node.range,
            });
        }
        if let Some(cached) = self.context.path_cache.lock().unwrap().get(&cache_key) {
            return Ok(cached.clone());
        }

        self.context
            .evaluating_paths
            .lock()
            .unwrap()
            .insert(cache_key.clone());
        let result = self.eval(node, scope);
        self.context
            .evaluating_paths
            .lock()
            .unwrap()
            .remove(&cache_key);
        if let Ok(value) = &result {
            self.context
                .path_cache
                .lock()
                .unwrap()
                .insert(cache_key, value.clone());
        }
        result
    }

    pub(crate) fn force_thunk(&self, thunk: &Arc<Thunk>) -> Result<Value, RuntimeError> {
        if let Some(value) = thunk.value.lock().unwrap().clone() {
            return Ok(value);
        }

        if self
            .context
            .evaluating_paths
            .lock()
            .unwrap()
            .contains(&thunk.cache_key)
        {
            return Err(RuntimeError::CircularReference {
                cycle: thunk.path.clone(),
                range: thunk.node.range,
            });
        }

        self.context
            .evaluating_paths
            .lock()
            .unwrap()
            .insert(thunk.cache_key.clone());
        let result = self.eval(&thunk.node, &thunk.scope);
        self.context
            .evaluating_paths
            .lock()
            .unwrap()
            .remove(&thunk.cache_key);
        if let Ok(value) = &result {
            thunk.value.lock().unwrap().replace(value.clone());
        }
        result
    }

    pub(crate) fn force_thunk_with_scope(
        &self,
        thunk: &Arc<Thunk>,
        scope: &Arc<Scope>,
    ) -> Result<Value, RuntimeError> {
        if let Some(value) = thunk.value.lock().unwrap().clone() {
            return Ok(value);
        }

        let result = self.eval(&thunk.node, scope);
        if let Ok(value) = &result {
            thunk.value.lock().unwrap().replace(value.clone());
        }
        result
    }

    fn lookup_value_path(
        &self,
        mut current_val: Value,
        path: &[TokenKey],
        display_path: &str,
        scope: &Arc<Scope>,
        range: TokenRange,
    ) -> Result<Value, RuntimeError> {
        for part in path {
            let is_optional = part.is_optional();
            // Fix 2: dynamic segments must be evaluated against the live
            // scope; falling back to `part.name()` would silently look up
            // the literal string `"<dynamic>"`.
            let key = match part {
                TokenKey::Dynamic(expr_node, _) => {
                    let val = self.eval(expr_node, scope)?;
                    match val {
                        Value::String(s) => s,
                        Value::Int(i) => i.to_string(),
                        other => {
                            return Err(RuntimeError::TypeMismatch {
                                expected: "String or Int for dynamic key".to_string(),
                                found: other.type_name().to_string(),
                                range: expr_node.range,
                            })
                        }
                    }
                }
                _ => part.name(),
            };

            current_val = match current_val {
                Value::Dict(ref d) => {
                    if let Some(v) = d.map.get(&key) {
                        v.clone()
                    } else if is_optional {
                        Value::Null
                    } else {
                        return Err(RuntimeError::VariableNotFound(
                            display_path.to_string(),
                            range,
                        ));
                    }
                }
                Value::List(list) => {
                    let index = key.parse::<usize>().map_err(|_| {
                        RuntimeError::VariableNotFound(display_path.to_string(), range)
                    })?;
                    if let Some(v) = list.get(index) {
                        v.clone()
                    } else if is_optional {
                        Value::Null
                    } else {
                        return Err(RuntimeError::VariableNotFound(
                            display_path.to_string(),
                            range,
                        ));
                    }
                }
                Value::Null if is_optional => Value::Null,
                other => {
                    if is_optional {
                        Value::Null
                    } else {
                        return Err(RuntimeError::TypeMismatch {
                            expected: "Dict/List".to_string(),
                            found: other.type_name().to_string(),
                            range,
                        });
                    }
                }
            };
            if current_val == Value::Null && is_optional {
                return Ok(Value::Null);
            }
        }

        Ok(current_val)
    }
}
