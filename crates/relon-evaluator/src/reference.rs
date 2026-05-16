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
use std::sync::{Arc, Mutex};

/// Result of looking up a single dict key during reference path resolution.
/// Either we have a registered thunk to force lazily, or we've already
/// produced a value (typically from a spread expression that was evaluated
/// eagerly to inspect its contents).
pub(crate) enum ReferenceStep {
    Thunk(Arc<Thunk>),
    Value(Box<Value>),
}

/// Outcome of `resolve_dict_reference_step`. We need to distinguish
/// "field genuinely doesn't exist" from "field exists but `#private`
/// hides it across a dict boundary": in the latter case the caller
/// must NOT fall back to `scope.locals`, since `#private` values are
/// kept in the owning dict's locals (so same-dict siblings can see
/// them) and would otherwise leak through cross-dict references.
pub(crate) enum DictStepResult {
    Found(ReferenceStep),
    /// The named field exists on the dict but is `#private` and the
    /// access crossed a dict boundary; treat as invisible without
    /// consulting `locals` fallback.
    PrivateBlocked,
    NotFound,
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
        // Fast path: single-segment variable references (`x`, the
        // dominant shape in comprehension bodies) don't need the
        // diagnostic `parts` vector at all — `current_val` is already
        // the answer. dhat showed the unconditional `vec![first_name]`
        // landing as ~7 MB / 300 K blocks across the resolve_variable
        // call sites in the comprehension hot loop.
        if path.len() == 1 {
            return Ok(current_val);
        }
        // Multi-segment path: build the diagnostic vector. `first_name`
        // is moved (not cloned) into the head so the success branch
        // doesn't pay an extra allocation.
        let mut parts = vec![first_name];
        for part in &path[1..] {
            let is_optional = part.is_optional();
            // Decision 22 (Indexable lowering): when this segment is a
            // bracket access (`a[i]`) and the current value's schema
            // declares an `index()` witness, dispatch the method
            // *before* falling through to the structural Dict/List
            // lookup. The display path used for the not-found
            // diagnostic mirrors the structural-miss text shape
            // (`parts.join(".")` with `<dynamic>` as the segment
            // placeholder).
            if let TokenKey::Dynamic(expr_node, _) = part {
                let key_value = self.eval(expr_node, scope)?;
                let mut tentative_display = parts.clone();
                tentative_display.push("<dynamic>".to_string());
                let display_name = tentative_display.join(".");
                if let Some(result) = self.try_index_method(
                    &current_val,
                    key_value.clone(),
                    is_optional,
                    &display_name,
                    scope,
                    range,
                )? {
                    parts.push("<dynamic>".to_string());
                    current_val = result;
                    continue;
                }
                // No witness — coerce the evaluated key into the
                // String / Int form the structural fallback expects.
                let key = match key_value {
                    Value::String(s) => s,
                    Value::Int(i) => i.to_string(),
                    other => {
                        return Err(RuntimeError::TypeMismatch {
                            expected: "String or Int for dynamic key".to_string(),
                            found: other.type_name().to_string(),
                            range: expr_node.range,
                        })
                    }
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
                continue;
            }
            let key = part.to_string_key();
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
                return Ok(Value::Int(context.current_index() as i64));
            }
            RefBase::Prev => {
                let context = scope.list_context.as_ref().ok_or_else(|| {
                    RuntimeError::VariableNotFound(
                        "&prev can only be used inside a list".to_string(),
                        range,
                    )
                })?;
                let cur_index = context.current_index();
                if cur_index == 0 {
                    return Ok(Value::Null);
                }
                let target_index = cur_index - 1;
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
                let cur_index = context.current_index();
                if cur_index + 1 >= context.elements.len() {
                    return Ok(Value::Null);
                }
                let target_index = cur_index + 1;
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
                // Build a display string from static segment names only.
                // Bug 4: never evaluate dynamic segments solely to mint
                // a display — the real lookup in `eval_reference_path_from`
                // already evaluates them, and host fns with side effects
                // would otherwise fire twice.
                let display = if path.is_empty() {
                    "&this".to_string()
                } else {
                    path.iter().map(|k| k.name()).collect::<Vec<_>>().join(".")
                };
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

        // Bug 4: paths containing a `TokenKey::Dynamic` segment skip the
        // path_cache entirely. A previous attempt evaluated the dynamic
        // expression up-front to mint a cache key, but the actual lookup
        // in `eval_reference_path_from` re-evaluates it — doubling any
        // host-side side effects (and leaking a discrepancy if the
        // expression isn't pure). Static-only paths keep the cache.
        let has_dynamic = target_path
            .iter()
            .any(|k| matches!(k, TokenKey::Dynamic(_, _)));

        let static_keys: Option<Vec<String>> = if has_dynamic {
            None
        } else {
            Some(target_path.iter().map(|k| k.name()).collect())
        };
        let path_str = match &static_keys {
            Some(v) => v.join("."),
            None => target_path
                .iter()
                .map(|k| k.name())
                .collect::<Vec<_>>()
                .join("."),
        };

        // Bug 2: include `owning_depth` in the cache key. Otherwise
        // `&sibling.<priv>` (which is allowed within the owning dict)
        // and `&root.<priv>` (which must be blocked) share the same
        // path tuple at the top level and thus the same cache slot —
        // the second lookup would hand back the first's value and
        // bypass the privacy check entirely. Encoding `owning_depth`
        // (None = always cross-dict, Some(n) = at most n owning steps)
        // partitions the cache so the two reference styles can't
        // collide.
        let owning_tag = match owning_depth {
            None => "x".to_string(),
            Some(n) => format!("o{n}"),
        };
        if !target_path.is_empty() {
            if let Some(keys) = static_keys.as_ref() {
                let cache_key = format!("{owning_tag}|{}", scope.path_cache_key(keys));
                if let Some(cached) = self.context.path_cache.lock().unwrap().get(&cache_key) {
                    return Ok(cached.clone());
                }
            }
        }
        let result =
            self.eval_reference_path(root, &target_path, scope, &path_str, range, owning_depth);
        if let Ok(value) = &result {
            if !target_path.is_empty() {
                if let Some(keys) = static_keys.as_ref() {
                    let cache_key = format!("{owning_tag}|{}", scope.path_cache_key(keys));
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
            list_context: Some(Arc::new(ListContext::fixed(index, context.elements.clone()))),
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
                current_dir: original_scope.current_dir.clone(),
                cache_namespace: original_scope.cache_namespace.clone(),
                root_ref: original_scope.root_ref.clone(),
                ..Default::default()
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
                    DictStepResult::Found(ReferenceStep::Thunk(thunk)) => {
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
                    DictStepResult::Found(ReferenceStep::Value(value)) => {
                        self.lookup_value_path(*value, remaining_path, display_path, scope, range)
                    }
                    DictStepResult::PrivateBlocked => {
                        // Field exists but is `#private` and we crossed
                        // a dict boundary. Treat as invisible — and
                        // critically, do NOT fall through to the
                        // `locals` lookup below, since `#private` keeps
                        // the value in the owning dict's locals and
                        // that fallback would silently leak it.
                        if is_optional {
                            Ok(Value::Null)
                        } else {
                            Err(RuntimeError::VariableNotFound(
                                display_path.to_string(),
                                range,
                            ))
                        }
                    }
                    DictStepResult::NotFound => {
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
                // Bug 3: an AST path stepping *into* a list element
                // (e.g. `&sibling.list[0].y`) must arrive there with a
                // proper `list_context` so the element body can use
                // `&index` / `&prev` / `&next`. Mirror the way `Expr::List`
                // evaluation in `eval.rs` builds per-element thunks +
                // `with_list_context`, so all list-context references
                // resolve identically whether the element is being
                // forced for materialization or via a reference path.
                let element_thunks: Vec<Arc<Thunk>> = elements
                    .iter()
                    .enumerate()
                    .map(|(i, el)| {
                        let thunk_scope = scope.with_path(i.to_string());
                        Arc::new(Thunk::new(
                            el.clone(),
                            thunk_scope,
                            Vec::new(),
                            String::new(),
                        ))
                    })
                    .collect();
                let item_scope = scope.with_list_context(index, element_thunks);
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
    ) -> Result<DictStepResult, RuntimeError> {
        for (key, value_node) in pairs.iter().rev() {
            match key {
                TokenKey::Spread(_) => {
                    // Spread results come from a `Value::Dict`, which has
                    // already had its `#private` fields stripped at dict
                    // build time — so this branch is naturally safe.
                    let spread_value = self.eval(value_node, scope)?;
                    if let Value::Dict(d) = spread_value {
                        if let Some(value) = d.map.get(part) {
                            return Ok(DictStepResult::Found(ReferenceStep::Value(Box::new(
                                value.clone(),
                            ))));
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
                        //
                        // Bug 2: must report this as `PrivateBlocked`
                        // rather than `NotFound`, so the caller doesn't
                        // fall back to `scope.locals` — `#private`
                        // values are deliberately seeded into the
                        // owning dict's locals (eval.rs:767) so
                        // same-dict siblings can see them, and a naive
                        // locals fallback would leak that across the
                        // boundary.
                        if block_private && is_private_field(value_node) {
                            return Ok(DictStepResult::PrivateBlocked);
                        }
                        if let Some(thunk) = scope.get_own_thunk(part) {
                            return Ok(DictStepResult::Found(ReferenceStep::Thunk(thunk)));
                        }
                    }
                }
            }
        }
        Ok(DictStepResult::NotFound)
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
            // Decision 22 (Indexable lowering): bracket-access segments
            // dispatch through the receiver's `index()` witness when
            // its schema declares one, before the structural Dict /
            // List fallback below kicks in.
            if let TokenKey::Dynamic(expr_node, _) = part {
                let key_value = self.eval(expr_node, scope)?;
                if let Some(result) = self.try_index_method(
                    &current_val,
                    key_value.clone(),
                    is_optional,
                    display_path,
                    scope,
                    range,
                )? {
                    current_val = result;
                    if current_val == Value::Null && is_optional {
                        return Ok(Value::Null);
                    }
                    continue;
                }
                // No witness — fall through to the structural lookup
                // by coercing the key into a String / Int.
                let key = match key_value {
                    Value::String(s) => s,
                    Value::Int(i) => i.to_string(),
                    other => {
                        return Err(RuntimeError::TypeMismatch {
                            expected: "String or Int for dynamic key".to_string(),
                            found: other.type_name().to_string(),
                            range: expr_node.range,
                        })
                    }
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
                continue;
            }
            // Fix 2: dynamic segments must be evaluated against the live
            // scope; falling back to `part.name()` would silently look up
            // the literal string `"<dynamic>"`.
            let key = part.name();

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
