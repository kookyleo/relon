use crate::error::RuntimeError;
use crate::value::{Value, ValueDict};
use ordered_float::OrderedFloat;
use relon_parser::{
    parse_document, Expr, FStringPart, Node, Operator, RefBase, TokenKey, TokenRange,
};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;
use std::sync::{Arc, Mutex};

pub trait RelonFunction: Send + Sync {
    fn call(&self, args: Vec<Value>, range: TokenRange) -> Result<Value, RuntimeError>;
}

pub struct Context {
    pub globals: HashMap<String, Value>,
    pub functions: HashMap<String, Arc<dyn RelonFunction>>,
    pub root_node: Option<Arc<Node>>,
    pub module_cache: Mutex<HashMap<String, Value>>,
    pub(crate) path_cache: Mutex<HashMap<String, Value>>,
    pub(crate) evaluating_paths: Mutex<HashSet<String>>,
    pub(crate) loading_modules: Mutex<Vec<String>>,
}

impl Default for Context {
    fn default() -> Self {
        Self::new()
    }
}

impl Context {
    pub fn new() -> Self {
        let mut ctx = Self {
            globals: HashMap::new(),
            functions: HashMap::new(),
            root_node: None,
            module_cache: Mutex::new(HashMap::new()),
            path_cache: Mutex::new(HashMap::new()),
            evaluating_paths: Mutex::new(HashSet::new()),
            loading_modules: Mutex::new(Vec::new()),
        };
        crate::stdlib::register_to(&mut ctx);
        ctx
    }

    pub fn enter_loading_module<S: Into<String>>(&self, path: S) -> LoadingModuleGuard<'_> {
        let path = path.into();
        self.loading_modules.lock().unwrap().push(path.clone());
        LoadingModuleGuard {
            loading_modules: &self.loading_modules,
            path,
        }
    }

    pub fn with_root(mut self, root: Node) -> Self {
        self.root_node = Some(Arc::new(root));
        self
    }

    pub fn register_fn<S: Into<String>>(&mut self, name: S, f: Arc<dyn RelonFunction>) {
        self.functions.insert(name.into(), f);
    }
}

pub struct ListContext {
    pub index: usize,
    pub elements: Vec<Arc<Thunk>>,
}

#[derive(Default)]
pub struct Scope {
    pub parent: Option<std::sync::Arc<Scope>>,
    pub path_node: Option<String>,
    pub locals: std::sync::Mutex<std::collections::HashMap<String, crate::value::Value>>,
    pub current_dir: String,
    pub cache_namespace: String,
    pub reference_root: Option<Arc<Node>>,
    pub reference_root_parent: Option<Arc<Scope>>,
    pub reference_root_scope: Option<Arc<Scope>>,
    pub list_context: Option<Arc<ListContext>>,
    pub(crate) thunks: Mutex<HashMap<String, Arc<Thunk>>>,
}

impl std::fmt::Debug for Scope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Scope")
            .field("path_node", &self.path_node)
            .field("current_dir", &self.current_dir)
            .field("cache_namespace", &self.cache_namespace)
            .field("has_reference_root", &self.reference_root.is_some())
            .field("index", &self.list_context.as_ref().map(|c| c.index))
            .finish()
    }
}

impl Clone for Scope {
    fn clone(&self) -> Self {
        Self {
            parent: self.parent.clone(),
            path_node: self.path_node.clone(),
            locals: Mutex::new(self.locals.lock().unwrap().clone()),
            current_dir: self.current_dir.clone(),
            cache_namespace: self.cache_namespace.clone(),
            reference_root: self.reference_root.clone(),
            reference_root_parent: self.reference_root_parent.clone(),
            reference_root_scope: self.reference_root_scope.clone(),
            list_context: self.list_context.clone(),
            thunks: Mutex::new(self.thunks.lock().unwrap().clone()),
        }
    }
}

impl Scope {
    pub fn get_local(&self, name: &str) -> Option<Value> {
        if let Some(v) = self.locals.lock().unwrap().get(name) {
            Some(v.clone())
        } else if let Some(parent) = &self.parent {
            parent.get_local(name)
        } else {
            None
        }
    }

    fn get_thunk(&self, name: &str) -> Option<Arc<Thunk>> {
        if let Some(thunk) = self.thunks.lock().unwrap().get(name) {
            Some(Arc::clone(thunk))
        } else if let Some(parent) = &self.parent {
            parent.get_thunk(name)
        } else {
            None
        }
    }

    fn get_own_thunk(&self, name: &str) -> Option<Arc<Thunk>> {
        self.thunks.lock().unwrap().get(name).map(Arc::clone)
    }

    pub fn full_path(&self) -> Vec<String> {
        let mut path = Vec::new();
        let mut current = Some(self);
        while let Some(scope) = current {
            if let Some(node) = &scope.path_node {
                path.push(node.clone());
            }
            if let Some(parent) = &scope.parent {
                current = Some(parent.as_ref());
            } else {
                current = None;
            }
        }
        path.reverse();
        path
    }

    pub(crate) fn path_cache_key(&self, path: &[String]) -> String {
        let namespace = if self.cache_namespace.is_empty() {
            &self.current_dir
        } else {
            &self.cache_namespace
        };
        let encoded_path = path
            .iter()
            .map(|s| format!("{}:{}", s.len(), s))
            .collect::<Vec<_>>()
            .join("/");
        format!("{namespace}::{encoded_path}")
    }

    pub fn with_local(self: &Arc<Self>, name: String, val: Value) -> Arc<Self> {
        let mut locals = HashMap::new();
        locals.insert(name, val);
        Arc::new(Self {
            parent: Some(Arc::clone(self)),
            path_node: None,
            locals: Mutex::new(locals),
            current_dir: self.current_dir.clone(),
            cache_namespace: self.cache_namespace.clone(),
            reference_root: self.reference_root.clone(),
            reference_root_parent: self.reference_root_parent.clone(),
            reference_root_scope: self.reference_root_scope.clone(),
            list_context: self.list_context.clone(),
            thunks: Mutex::new(HashMap::new()),
        })
    }

    pub fn with_locals(self: &Arc<Self>, new_locals: HashMap<String, Value>) -> Arc<Self> {
        Arc::new(Self {
            parent: Some(Arc::clone(self)),
            path_node: None,
            locals: Mutex::new(new_locals),
            current_dir: self.current_dir.clone(),
            cache_namespace: self.cache_namespace.clone(),
            reference_root: self.reference_root.clone(),
            reference_root_parent: self.reference_root_parent.clone(),
            reference_root_scope: self.reference_root_scope.clone(),
            list_context: self.list_context.clone(),
            thunks: Mutex::new(HashMap::new()),
        })
    }

    pub fn with_path(self: &Arc<Self>, node: String) -> Arc<Self> {
        Arc::new(Self {
            parent: Some(Arc::clone(self)),
            path_node: Some(node),
            locals: Mutex::new(HashMap::new()),
            current_dir: self.current_dir.clone(),
            cache_namespace: self.cache_namespace.clone(),
            reference_root: self.reference_root.clone(),
            reference_root_parent: self.reference_root_parent.clone(),
            reference_root_scope: self.reference_root_scope.clone(),
            list_context: self.list_context.clone(),
            thunks: Mutex::new(HashMap::new()),
        })
    }

    pub fn with_list_context(
        self: &Arc<Self>,
        index: usize,
        elements: Vec<Arc<Thunk>>,
    ) -> Arc<Self> {
        Arc::new(Self {
            parent: Some(Arc::clone(self)),
            path_node: Some(index.to_string()),
            locals: Mutex::new(HashMap::new()),
            current_dir: self.current_dir.clone(),
            cache_namespace: self.cache_namespace.clone(),
            reference_root: self.reference_root.clone(),
            reference_root_parent: self.reference_root_parent.clone(),
            reference_root_scope: self.reference_root_scope.clone(),
            list_context: Some(Arc::new(ListContext { index, elements })),
            thunks: Mutex::new(HashMap::new()),
        })
    }
}

pub struct Evaluator<'a> {
    pub context: &'a Context,
}

pub struct EvaluatedArg {
    pub name: Option<String>,
    pub value: Value,
}

pub struct Thunk {
    node: Node,
    scope: Arc<Scope>,
    path: Vec<String>,
    cache_key: String,
    value: Mutex<Option<Value>>,
}

enum ReferenceStep {
    Thunk(Arc<Thunk>),
    Value(Box<Value>),
}

#[derive(Clone, Copy)]
enum NumericValue {
    Int(i64),
    Float(OrderedFloat<f64>),
}

impl NumericValue {
    fn as_f64(self) -> f64 {
        match self {
            Self::Int(value) => value as f64,
            Self::Float(value) => value.into_inner(),
        }
    }
}

pub struct LoadingModuleGuard<'a> {
    loading_modules: &'a Mutex<Vec<String>>,
    path: String,
}

impl Drop for LoadingModuleGuard<'_> {
    fn drop(&mut self) {
        let mut loading_modules = self.loading_modules.lock().unwrap();
        if let Some(index) = loading_modules
            .iter()
            .rposition(|module| module == &self.path)
        {
            loading_modules.remove(index);
        }
    }
}

impl<'a> Evaluator<'a> {
    pub fn new(context: &'a Context) -> Self {
        Self { context }
    }

    fn is_valid_identifier(s: &str) -> bool {
        if s.is_empty() {
            return false;
        }
        let mut chars = s.chars();
        let first = chars.next().unwrap();
        if !first.is_ascii_alphabetic() && first != '_' {
            return false;
        }
        chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
    }

    fn is_logic_definition(node: &Node) -> bool {
        matches!(node.expr.as_ref(), Expr::Closure { .. })
    }

    pub fn eval(&self, node: &Node, scope: &Arc<Scope>) -> Result<Value, RuntimeError> {
        self.eval_internal(node, scope, false)
    }

    fn eval_internal(
        &self,
        node: &Node,
        scope: &Arc<Scope>,
        is_schema_pred: bool,
    ) -> Result<Value, RuntimeError> {
        let mut current_scope = Arc::clone(scope);

        for dec in &node.decorators {
            let name = dec
                .path
                .iter()
                .map(|k| k.to_string_key())
                .collect::<Vec<_>>()
                .join(".");
            if name == "import" {
                current_scope = self.apply_import_decorator(dec, &current_scope)?;
            } else if name == "schema" {
                match node.expr.as_ref() {
                    Expr::Dict(pairs) => {
                        let fields = self.extract_schema_fields_from_dict(pairs, &current_scope)?;
                        return Ok(Value::Schema(fields));
                    }
                    Expr::Binary(Operator::Add, _, _) => {
                        let fields = self.extract_schema_for_node(node, &current_scope)?;
                        return Ok(Value::Schema(fields));
                    }
                    _ => {}
                }
            }
        }

        let mut val = match node.expr.as_ref() {
            Expr::Null => Ok(Value::Null),
            Expr::Bool(b) => Ok(Value::Bool(*b)),
            Expr::Int(i) => Ok(Value::Int(*i)),
            Expr::Float(f) => Ok(Value::Float(*f)),
            Expr::String(s) => Ok(Value::String(s.clone())),

            Expr::List(elements) => {
                let mut thunks = Vec::new();
                for (i, el) in elements.iter().enumerate() {
                    let item_scope = current_scope.with_path(i.to_string());
                    thunks.push(Arc::new(Thunk {
                        node: el.clone(),
                        scope: item_scope,
                        path: Vec::new(),
                        cache_key: String::new(),
                        value: Mutex::new(None),
                    }));
                }

                let mut values = Vec::new();
                for (i, thunk) in thunks.iter().enumerate() {
                    let item_scope = current_scope.with_list_context(i, thunks.clone());
                    let element_val = self.force_thunk_with_scope(thunk, &item_scope)?;

                    if let Expr::Spread(_) = thunk.node.expr.as_ref() {
                        if let Value::List(l) = element_val {
                            values.extend(l);
                        } else {
                            return Err(RuntimeError::TypeMismatch {
                                expected: "List".to_string(),
                                found: element_val.type_name().to_string(),
                                range: thunk.node.range,
                            });
                        }
                    } else {
                        values.push(element_val);
                    }
                }
                Ok(Value::List(values))
            }

            Expr::Dict(pairs) => {
                let is_root = current_scope
                    .reference_root
                    .as_ref()
                    .is_some_and(|r| std::ptr::eq(r.as_ref() as *const _, node as *const _));

                let mut dict_scope = Arc::new(Scope {
                    parent: Some(Arc::clone(&current_scope)),
                    path_node: None,
                    locals: Mutex::new(HashMap::new()),
                    current_dir: current_scope.current_dir.clone(),
                    cache_namespace: current_scope.cache_namespace.clone(),
                    reference_root: current_scope.reference_root.clone(),
                    reference_root_parent: current_scope.reference_root_parent.clone(),
                    reference_root_scope: current_scope.reference_root_scope.clone(),
                    list_context: current_scope.list_context.clone(),
                    thunks: Mutex::new(HashMap::new()),
                });

                if is_root {
                    let mut modified = (*dict_scope).clone();
                    modified.reference_root_scope = Some(dict_scope.clone());
                    dict_scope = Arc::new(modified);
                }

                self.prepare_dict_scope(node, &dict_scope)?;

                let mut map = BTreeMap::new();
                for (key, value_node) in pairs {
                    match key {
                        TokenKey::Spread(_) => {
                            let val = self.eval(value_node, &dict_scope)?;
                            if let Value::Dict(d) = val {
                                // Important: overrides existing keys in map
                                for (k, v) in d.map {
                                    map.insert(k.clone(), v.clone());
                                    dict_scope.locals.lock().unwrap().insert(k, v);
                                }
                            } else {
                                return Err(RuntimeError::TypeMismatch {
                                    expected: "Dict".to_string(),
                                    found: val.type_name().to_string(),
                                    range: value_node.range,
                                });
                            }
                        }
                        _ => {
                            let key_str = match key {
                                TokenKey::String(s, _, _) => s.clone(),
                                TokenKey::Dynamic(expr_node, _) => {
                                    match self.eval(expr_node, &dict_scope)? {
                                        Value::String(s) => s,
                                        Value::Int(i) => i.to_string(),
                                        other => {
                                            return Err(RuntimeError::TypeMismatch {
                                                expected: "String or Int".to_string(),
                                                found: other.type_name().to_string(),
                                                range: expr_node.range,
                                            })
                                        }
                                    }
                                }
                                _ => key.to_string_key(),
                            };

                            let val = if let Some(thunk) = dict_scope.get_own_thunk(&key_str) {
                                self.force_thunk(&thunk)?
                            } else {
                                let item_scope = dict_scope.with_path(key_str.clone());
                                self.eval(value_node, &item_scope)?
                            };

                            if !key_str.starts_with('_') || !matches!(val, Value::Closure { .. }) {
                                map.insert(key_str.clone(), val.clone());
                            }
                            dict_scope.locals.lock().unwrap().insert(key_str, val);
                        }
                    }
                }
                Ok(Value::Dict(ValueDict { map, brand: None }))
            }

            Expr::Spread(inner) => self.eval(inner, &current_scope),
            Expr::Comprehension {
                element,
                id,
                iterable,
                condition,
            } => {
                let iter_val = self.eval(iterable, &current_scope)?;
                let items = match iter_val {
                    Value::List(l) => l,
                    _ => {
                        return Err(RuntimeError::TypeMismatch {
                            expected: "List".to_string(),
                            found: iter_val.type_name().to_string(),
                            range: iterable.range,
                        })
                    }
                };
                let mut result = Vec::new();
                for item in items {
                    let mut iter_scope_map = HashMap::new();
                    iter_scope_map.insert(id.clone(), item);
                    let iter_scope = current_scope.with_locals(iter_scope_map);

                    let should_include = if let Some(cond) = condition {
                        self.eval(cond, &iter_scope)?.is_truthy()
                    } else {
                        true
                    };
                    if should_include {
                        result.push(self.eval(element, &iter_scope)?);
                    }
                }
                Ok(Value::List(result))
            }
            Expr::Reference { base, path } => {
                self.resolve_reference(base, path, &current_scope, node.range)
            }
            Expr::Variable(path) => self.resolve_variable(path, &current_scope, node.range),
            Expr::Closure {
                params,
                return_type: _,
                body,
            } => {
                let param_names = params.iter().map(|p| p.name.clone()).collect();
                let captured_env = if scope.path_node.is_some() {
                    scope.parent.clone().unwrap_or_else(|| Arc::clone(scope))
                } else {
                    Arc::clone(scope)
                };

                Ok(Value::Closure {
                    params: param_names,
                    body: body.clone(),
                    captured_env,
                })
            }
            Expr::FnCall { path, args } => {
                let mut evaluated_args = Vec::new();
                for arg in args {
                    evaluated_args.push(EvaluatedArg {
                        name: arg.name.clone(),
                        value: self.eval(&arg.value, &current_scope)?,
                    });
                }
                self.call_function(path, evaluated_args, &current_scope, node.range)
            }
            Expr::Binary(Operator::Pipe, left, right) => {
                let left_val = self.eval(left, &current_scope)?;
                match right.expr.as_ref() {
                    Expr::FnCall { path, args } => {
                        let mut evaluated_args = vec![EvaluatedArg {
                            name: None,
                            value: left_val,
                        }];
                        for arg in args {
                            evaluated_args.push(EvaluatedArg {
                                name: arg.name.clone(),
                                value: self.eval(&arg.value, &current_scope)?,
                            });
                        }
                        self.call_function(path, evaluated_args, &current_scope, right.range)
                    }
                    _ => {
                        let right_val = self.eval(right, &current_scope)?;
                        if let Value::Closure {
                            params,
                            body,
                            captured_env,
                        } = right_val
                        {
                            self.eval_closure(
                                &params,
                                &body,
                                vec![EvaluatedArg {
                                    name: None,
                                    value: left_val,
                                }],
                                &captured_env,
                                right.range,
                            )
                        } else {
                            Err(RuntimeError::UnsupportedOperator(
                                "Pipe requires a function or closure on the right".to_string(),
                                right.range,
                            ))
                        }
                    }
                }
            }
            Expr::Binary(Operator::And, left, right) => {
                let l = self.eval(left, &current_scope)?;
                if !l.is_truthy() {
                    Ok(l)
                } else {
                    self.eval(right, &current_scope)
                }
            }
            Expr::Binary(Operator::Or, left, right) => {
                let l = self.eval(left, &current_scope)?;
                if l.is_truthy() {
                    Ok(l)
                } else {
                    self.eval(right, &current_scope)
                }
            }
            Expr::Binary(op, left, right) => self.eval_binary(*op, left, right, &current_scope),
            Expr::Unary(op, node) => self.eval_unary(*op, node, &current_scope),
            Expr::Ternary { cond, then, els } => {
                if self.eval(cond, &current_scope)?.is_truthy() {
                    self.eval(then, &current_scope)
                } else {
                    self.eval(els, &current_scope)
                }
            }
            Expr::Where { expr, bindings } => {
                let bindings_val = self.eval(bindings, &current_scope)?;
                if let Value::Dict(d) = bindings_val {
                    let map_as_hashmap: std::collections::HashMap<String, Value> =
                        d.map.into_iter().collect();
                    let local_scope = current_scope.with_locals(map_as_hashmap);
                    self.eval(expr, &local_scope)
                } else {
                    Err(RuntimeError::TypeMismatch {
                        expected: "Dict".to_string(),
                        found: bindings_val.type_name().to_string(),
                        range: bindings.range,
                    })
                }
            }
            Expr::Match { expr, arms } => {
                let val = self.eval(expr, &current_scope)?;
                for (pattern_node, result_node) in arms {
                    match pattern_node.expr.as_ref() {
                        Expr::Wildcard => {
                            return self.eval(result_node, &current_scope);
                        }
                        Expr::Type(type_node) => {
                            if let Value::Dict(ref d) = val {
                                if let Some(ref brand) = d.brand {
                                    if type_node.path.len() == 1 && &type_node.path[0] == brand {
                                        return self.eval(result_node, &current_scope);
                                    }
                                    let tname = &type_node.path[0];
                                    if !matches!(
                                        tname.as_str(),
                                        "Int"
                                            | "String"
                                            | "Bool"
                                            | "Any"
                                            | "Null"
                                            | "List"
                                            | "Dict"
                                            | "Enum"
                                    ) {
                                        continue;
                                    }
                                }
                            }

                            let mut temp_val = val.clone();
                            if self
                                .check_type(
                                    &mut temp_val,
                                    type_node,
                                    &current_scope,
                                    pattern_node.range,
                                )
                                .is_ok()
                            {
                                return self.eval(result_node, &current_scope);
                            }
                        }
                        _ => {}
                    }
                }
                Err(RuntimeError::TypeMismatch {
                    expected: "a matching arm".to_string(),
                    found: format!("value {}", val),
                    range: node.range,
                })
            }
            Expr::FString(parts) => {
                let mut result = String::new();
                for part in parts {
                    match part {
                        FStringPart::Literal(s) => result.push_str(s),
                        FStringPart::Interpolation(node) => {
                            let val = self.eval(node, &current_scope)?;
                            result.push_str(&format!("{}", val));
                        }
                    }
                }
                Ok(Value::String(result))
            }
            Expr::Type(t) => Ok(Value::Type(t.clone())),
            Expr::Wildcard => Ok(Value::Wildcard),
        }?;

        if !is_schema_pred {
            for dec in &node.decorators {
                let name = dec
                    .path
                    .iter()
                    .map(|k| k.to_string_key())
                    .collect::<Vec<_>>()
                    .join(".");
                if name == "import" || name == "schema" {
                    continue;
                }
                let mut dec_args = Vec::new();
                for arg in &dec.args {
                    dec_args.push(EvaluatedArg {
                        name: arg.name.clone(),
                        value: self.eval_internal(&arg.value, &current_scope, is_schema_pred)?,
                    });
                }
                val = self.apply_decorator(&dec.path, val, dec_args, &current_scope, dec.range)?;
            }
        }

        if let Some(type_hint) = &node.type_hint {
            if !is_schema_pred && !matches!(val, Value::Wildcard) {
                self.check_type(&mut val, type_hint, &current_scope, node.range)?;

                if let Value::Dict(ref mut d) = val {
                    if type_hint.path.len() == 1 {
                        let tname = &type_hint.path[0];
                        if !matches!(
                            tname.as_str(),
                            "Int" | "String" | "Bool" | "Any" | "Null" | "List" | "Dict" | "Enum"
                        ) {
                            d.brand = Some(tname.clone());
                        }
                    } else {
                        d.brand = Some(type_hint.path.join("."));
                    }
                }
            }
        }

        Ok(val)
    }

    fn check_type(
        &self,
        value: &mut Value,
        expected: &relon_parser::TypeNode,
        scope: &Arc<Scope>,
        range: TokenRange,
    ) -> Result<(), RuntimeError> {
        self.check_type_internal(value, expected, scope, range, &mut HashSet::new(), 0)
    }

    fn check_type_internal(
        &self,
        value: &mut Value,
        expected: &relon_parser::TypeNode,
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

        let expected_str = Self::format_type_node(expected);

        // Recursion guard for custom schemas
        let tname = expected.path.join(".");
        if !matches!(
            tname.as_str(),
            "Int" | "String" | "Bool" | "Any" | "Null" | "List" | "Dict" | "Enum"
        ) {
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
                "List" => {
                    if let Value::List(l) = value {
                        if let Some(generic) = expected.generics.first() {
                            for item in l.iter_mut() {
                                self.check_type_internal(
                                    item,
                                    generic,
                                    scope,
                                    range,
                                    visited,
                                    depth + 1,
                                )?;
                            }
                        }
                        true
                    } else {
                        false
                    }
                }
                "Dict" => {
                    if let Value::Dict(d) = value {
                        if expected.generics.len() == 2 {
                            let val_type = &expected.generics[1];
                            for val in d.map.values_mut() {
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
                        true
                    } else {
                        false
                    }
                }
                "Closure" | "Fn" => matches!(value, Value::Closure { .. }),
                "Enum" => {
                    let mut matched = false;
                    for choice in &expected.generics {
                        let mut temp = value.clone();
                        if self
                            .check_type_internal(
                                &mut temp,
                                choice,
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
                        if let Value::String(s) = value {
                            if choice.path.len() == 1 && choice.path[0] == *s {
                                matched = true;
                                break;
                            }
                        }
                    }
                    matched
                }
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

    fn check_custom_schema(
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
                        RuntimeError::VariableNotFound(format!("{}.{}", path[0], part), range)
                    })?;
                }
                _ => return Ok(false),
            }
        }

        match current_val {
            Value::Schema(fields) => {
                if let Value::Dict(ref mut d) = value {
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
                            match &field.predicate {
                                Value::Wildcard => {}
                                Value::Closure { .. } => {
                                    let result = self.call_function_by_value(
                                        field.predicate.clone(),
                                        vec![EvaluatedArg {
                                            name: None,
                                            value: field_val.clone(),
                                        }],
                                        scope,
                                        range,
                                    )?;
                                    if !result.is_truthy() {
                                        let err_msg =
                                            field.custom_error.clone().unwrap_or_else(|| {
                                                format!("predicate constraint for '{}'", field_name)
                                            });
                                        return Err(RuntimeError::TypeMismatch {
                                            expected: err_msg,
                                            found: field_val.to_string(),
                                            range,
                                        });
                                    }
                                }
                                _ => {}
                            }
                        } else if let Some(ref def) = field.default_value {
                            d.map.insert(field_name.clone(), def.clone());
                        } else if field.type_hint.is_optional {
                            continue;
                        } else {
                            return Err(RuntimeError::TypeMismatch {
                                expected: format!("field '{}'", field_name),
                                found: "missing".to_string(),
                                range,
                            });
                        }
                    }
                    Ok(true)
                } else {
                    Ok(false)
                }
            }
            Value::Type(t) => {
                // Prevent self-recursion for Type alias
                if t.path == path {
                    return Ok(false);
                }
                self.check_type_internal(value, &t, scope, range, visited, depth)
                    .map(|_| true)
            }
            _ => Ok(false),
        }
    }

    fn extract_schema_fields_from_dict(
        &self,
        pairs: &[(TokenKey, Node)],
        scope: &Arc<Scope>,
    ) -> Result<HashMap<String, crate::value::SchemaField>, RuntimeError> {
        let mut schema_fields = HashMap::new();
        for (key, value_node) in pairs {
            if let TokenKey::String(key_name, _, _) = key {
                let (type_node, predicate) = if let Some(t) = &value_node.type_hint {
                    let pred = self.eval_internal(value_node, scope, true)?;
                    (t.clone(), pred)
                } else {
                    match value_node.expr.as_ref() {
                        Expr::Variable(vpath) => {
                            let path: Vec<String> = vpath.iter().map(|k| k.name()).collect();
                            (
                                relon_parser::TypeNode {
                                    path,
                                    generics: Vec::new(),
                                    is_optional: false,
                                    range: value_node.range,
                                },
                                Value::Wildcard,
                            )
                        }
                        _ => {
                            let val = self.eval_internal(value_node, scope, true)?;
                            match val {
                                Value::Type(t) => (t, Value::Wildcard),
                                other => {
                                    return Err(RuntimeError::TypeMismatch {
                                        expected: "Type or Type Prefix".to_string(),
                                        found: other.type_name().to_string(),
                                        range: value_node.range,
                                    });
                                }
                            }
                        }
                    }
                };

                let mut custom_error = None;
                let mut default_value = None;
                for v_dec in &value_node.decorators {
                    let d_name = v_dec
                        .path
                        .iter()
                        .map(|k| k.to_string_key())
                        .collect::<Vec<_>>()
                        .join(".");
                    if d_name == "expect" || d_name == "error" || d_name == "msg" {
                        if let Some(arg) = v_dec.args.first() {
                            let msg_val = self.eval_internal(&arg.value, scope, false)?;
                            custom_error = Some(msg_val.to_string());
                        }
                    } else if d_name == "default" {
                        if let Some(arg) = v_dec.args.first() {
                            let def_val = self.eval_internal(&arg.value, scope, false)?;
                            default_value = Some(def_val);
                        }
                    }
                }

                schema_fields.insert(
                    key_name.clone(),
                    crate::value::SchemaField {
                        type_hint: type_node,
                        predicate,
                        custom_error,
                        default_value,
                    },
                );
            }
        }
        Ok(schema_fields)
    }

    /// Walk a schema-position expression and produce its field map.
    ///
    /// Used by both the top-level `@schema` handler (Binary case) and recursive
    /// composition. We traverse the expression tree directly so siblings on
    /// either side of `+` are interpreted as schema definitions, not as data
    /// dicts that would lose their `Type field: pred` annotations through
    /// regular dict evaluation.
    fn extract_schema_for_node(
        &self,
        node: &Node,
        scope: &Arc<Scope>,
    ) -> Result<HashMap<String, crate::value::SchemaField>, RuntimeError> {
        match node.expr.as_ref() {
            Expr::Dict(pairs) => self.extract_schema_fields_from_dict(pairs, scope),
            Expr::Binary(Operator::Add, left, right) => {
                let mut left_fields = self.extract_schema_for_node(left, scope)?;
                let right_fields = self.extract_schema_for_node(right, scope)?;
                for (k, patch) in right_fields {
                    if let Some(base) = left_fields.get_mut(&k) {
                        base.type_hint = patch.type_hint;
                        if !matches!(patch.predicate, Value::Wildcard) {
                            base.predicate = patch.predicate;
                        }
                        if patch.custom_error.is_some() {
                            base.custom_error = patch.custom_error;
                        }
                        if patch.default_value.is_some() {
                            base.default_value = patch.default_value;
                        }
                    } else {
                        left_fields.insert(k, patch);
                    }
                }
                Ok(left_fields)
            }
            _ => {
                // Reference / Variable / Type / etc. — must already evaluate to a Schema.
                let val = self.eval_internal(node, scope, false)?;
                match val {
                    Value::Schema(fields) => Ok(fields),
                    other => Err(RuntimeError::TypeMismatch {
                        expected: "Schema".to_string(),
                        found: other.type_name().to_string(),
                        range: node.range,
                    }),
                }
            }
        }
    }

    fn format_type_node(node: &relon_parser::TypeNode) -> String {
        let suffix = if node.is_optional { "?" } else { "" };
        let path_str = node.path.join(".");
        if node.generics.is_empty() {
            format!("{}{}", path_str, suffix)
        } else {
            let generics: Vec<String> = node.generics.iter().map(Self::format_type_node).collect();
            format!("{}<{}>{}", path_str, generics.join(", "), suffix)
        }
    }

    fn apply_import_decorator(
        &self,
        dec: &relon_parser::Decorator,
        scope: &Arc<Scope>,
    ) -> Result<Arc<Scope>, RuntimeError> {
        let mut path_str = String::new();
        let mut alias: Option<String> = None;
        let mut should_spread = false;
        for arg in &dec.args {
            let val = self.eval(&arg.value, scope)?;
            match arg.name.as_deref() {
                Some("path") | None if path_str.is_empty() => {
                    if let Value::String(s) = val {
                        path_str = s;
                    }
                }
                Some("as") => {
                    if let Value::String(s) = val {
                        alias = Some(s);
                    }
                }
                Some("spread") => {
                    if let Value::Bool(b) = val {
                        should_spread = b;
                    }
                }
                _ => {}
            }
        }
        let evaluated_module = self.load_module(&path_str, scope, dec.range)?;
        let final_alias = if let Some(a) = alias {
            Some(a)
        } else if !should_spread {
            Path::new(&path_str)
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
        } else {
            None
        };
        let mut new_locals = HashMap::new();
        if let Some(a) = final_alias {
            new_locals.insert(a, evaluated_module.clone());
        }
        if should_spread {
            if let Value::Dict(d) = evaluated_module {
                for (k, v) in d.map {
                    if !k.starts_with('_') {
                        new_locals.insert(k, v);
                    }
                }
            } else {
                return Err(RuntimeError::TypeMismatch {
                    expected: "Dict".to_string(),
                    found: evaluated_module.type_name().to_string(),
                    range: dec.range,
                });
            }
        }
        Ok(scope.with_locals(new_locals))
    }

    fn load_module(
        &self,
        path_str: &str,
        scope: &Arc<Scope>,
        range: TokenRange,
    ) -> Result<Value, RuntimeError> {
        if path_str.starts_with("std/") {
            return self.load_virtual_module(path_str, range);
        }
        let target_path = Path::new(&scope.current_dir).join(path_str);
        let canonical_target = std::fs::canonicalize(&target_path).map_err(|e| {
            RuntimeError::IoError(format!("{}: {e}", target_path.to_string_lossy()))
        })?;
        let canonical_path = canonical_target.to_string_lossy().to_string();
        if let Some(cached) = self
            .context
            .module_cache
            .lock()
            .unwrap()
            .get(&canonical_path)
        {
            return Ok(cached.clone());
        }
        if self
            .context
            .loading_modules
            .lock()
            .unwrap()
            .contains(&canonical_path)
        {
            return Err(RuntimeError::CircularImport(
                self.context.loading_modules.lock().unwrap().clone(),
                range.into(),
            ));
        }
        let _loading_guard = self.context.enter_loading_module(canonical_path.clone());
        let content = std::fs::read_to_string(&canonical_target)
            .map_err(|e| RuntimeError::IoError(e.to_string()))?;
        let node = parse_document(&content).map_err(|error| RuntimeError::ModuleParseError {
            path: canonical_path.clone(),
            message: error.to_string(),
            range: range.into(),
        })?;
        let module_scope = Arc::new(Scope {
            current_dir: canonical_target
                .parent()
                .unwrap_or(Path::new("."))
                .to_string_lossy()
                .to_string(),
            cache_namespace: canonical_path.clone(),
            reference_root: Some(Arc::new(node.clone())),
            ..Default::default()
        });
        let evaluated = self.eval(&node, &module_scope)?;
        self.context
            .module_cache
            .lock()
            .unwrap()
            .insert(canonical_path, evaluated.clone());
        Ok(evaluated)
    }

    fn load_virtual_module(&self, path: &str, range: TokenRange) -> Result<Value, RuntimeError> {
        let content = match path {
            "std/dict" => include_str!("std_relon/dict.relon"),
            "std/is" => include_str!("std_relon/is.relon"),
            "std/math" => include_str!("std_relon/math.relon"),
            "std/list" => include_str!("std_relon/list.relon"),
            "std/string" => include_str!("std_relon/string.relon"),
            "std/value" => include_str!("std_relon/value.relon"),
            _ => return Err(RuntimeError::ModuleNotFound(path.to_string(), range.into())),
        };
        let node = parse_document(content).map_err(|error| RuntimeError::ModuleParseError {
            path: path.to_string(),
            message: error.to_string(),
            range: range.into(),
        })?;
        self.eval(
            &node,
            &Arc::new(Scope {
                cache_namespace: path.to_string(),
                reference_root: Some(Arc::new(node.clone())),
                ..Default::default()
            }),
        )
    }

    fn call_function_by_value(
        &self,
        func: Value,
        args: Vec<EvaluatedArg>,
        _scope: &Arc<Scope>,
        range: TokenRange,
    ) -> Result<Value, RuntimeError> {
        match func {
            Value::Closure {
                params,
                body,
                captured_env,
            } => {
                let mut local_vars = HashMap::new();
                for (i, param_name) in params.iter().enumerate() {
                    if let Some(arg) = args.get(i) {
                        local_vars.insert(param_name.clone(), arg.value.clone());
                    } else {
                        return Err(RuntimeError::TypeMismatch {
                            expected: format!("at least {} arguments", params.len()),
                            found: format!("{}", args.len()),
                            range,
                        });
                    }
                }
                let call_scope = captured_env.with_locals(local_vars);
                self.eval(&body, &call_scope)
            }
            _ => Err(RuntimeError::TypeMismatch {
                expected: "Closure".to_string(),
                found: func.type_name().to_string(),
                range,
            }),
        }
    }

    fn call_function(
        &self,
        path: &[TokenKey],
        args: Vec<EvaluatedArg>,
        scope: &Arc<Scope>,
        range: TokenRange,
    ) -> Result<Value, RuntimeError> {
        if let Ok(Value::Closure {
            params,
            body,
            captured_env,
        }) = self.resolve_variable(path, scope, range)
        {
            return self.eval_closure(&params, &body, args, &captured_env, range);
        }
        if let Some(name) = Self::native_function_name(path) {
            if let Some(func) = self.context.functions.get(&name) {
                let positional: Vec<Value> = args.into_iter().map(|a| a.value).collect();
                return func.call(positional, range);
            }
        }
        Err(RuntimeError::FunctionNotFound(
            path.iter()
                .map(|k| k.to_string_key())
                .collect::<Vec<_>>()
                .join("."),
            range,
        ))
    }

    fn apply_decorator(
        &self,
        path: &[TokenKey],
        value: Value,
        args: Vec<EvaluatedArg>,
        scope: &Arc<Scope>,
        range: TokenRange,
    ) -> Result<Value, RuntimeError> {
        if let Ok(Value::Closure {
            params,
            body,
            captured_env,
        }) = self.resolve_variable(path, scope, range)
        {
            let mut combined = vec![EvaluatedArg { name: None, value }];
            combined.extend(args);
            return self.eval_closure(&params, &body, combined, &captured_env, range);
        }
        if let Some(name) = Self::native_function_name(path) {
            if let Some(func) = self.context.functions.get(&name) {
                let mut positional = vec![value.clone()];
                positional.extend(args.into_iter().map(|a| a.value));
                return func.call(positional, range);
            }
            if name == "value" {
                if let Some(a) = args.first() {
                    return Ok(a.value.clone());
                } else {
                    return Ok(value);
                }
            }
            if name == "expect" || name == "msg" || name == "error" {
                return Ok(value);
            }
        }
        Err(RuntimeError::UnsupportedOperator(
            format!(
                "Decorator @{} not found",
                path.iter()
                    .map(|k| k.to_string_key())
                    .collect::<Vec<_>>()
                    .join(".")
            ),
            range,
        ))
    }

    fn native_function_name(path: &[TokenKey]) -> Option<String> {
        let mut parts = Vec::with_capacity(path.len());
        for part in path {
            match part {
                TokenKey::String(name, _, _) => parts.push(name.as_str()),
                _ => return None,
            }
        }
        Some(parts.join("."))
    }

    fn eval_closure(
        &self,
        params: &[String],
        body: &Node,
        args: Vec<EvaluatedArg>,
        captured_env: &Arc<Scope>,
        range: TokenRange,
    ) -> Result<Value, RuntimeError> {
        let mut bindings = HashMap::new();
        let mut pos_idx = 0;
        for arg in &args {
            if arg.name.is_none() {
                if pos_idx < params.len() {
                    bindings.insert(params[pos_idx].clone(), arg.value.clone());
                    pos_idx += 1;
                } else {
                    return Err(RuntimeError::TypeMismatch {
                        expected: format!("at most {}", params.len()),
                        found: "more".to_string(),
                        range,
                    });
                }
            }
        }
        for arg in &args {
            if let Some(name) = &arg.name {
                if !params.contains(name) {
                    return Err(RuntimeError::VariableNotFound(name.clone(), range));
                }
                if bindings.contains_key(name) {
                    return Err(RuntimeError::UnsupportedOperator(
                        format!("Duplicate {}", name),
                        range,
                    ));
                }
                bindings.insert(name.clone(), arg.value.clone());
            }
        }
        if bindings.len() < params.len() {
            return Err(RuntimeError::TypeMismatch {
                expected: format!("{}", params.len()),
                found: format!("{}", bindings.len()),
                range,
            });
        }
        let bindings_scope = Arc::new(Scope {
            parent: Some(Arc::clone(captured_env)),
            path_node: None,
            locals: Mutex::new(bindings),
            current_dir: captured_env.current_dir.clone(),
            cache_namespace: captured_env.cache_namespace.clone(),
            reference_root: captured_env.reference_root.clone(),
            reference_root_parent: captured_env.reference_root_parent.clone(),
            reference_root_scope: captured_env.reference_root_scope.clone(),
            list_context: None,
            thunks: Mutex::new(HashMap::new()),
        });
        let body_scope = Arc::new(Scope {
            parent: Some(Arc::clone(&bindings_scope)),
            path_node: None,
            locals: Mutex::new(HashMap::new()),
            current_dir: bindings_scope.current_dir.clone(),
            cache_namespace: bindings_scope.cache_namespace.clone(),
            reference_root: Some(Arc::new(body.clone())),
            reference_root_parent: Some(bindings_scope.clone()),
            reference_root_scope: bindings_scope.reference_root_scope.clone(),
            list_context: None,
            thunks: Mutex::new(HashMap::new()),
        });
        self.eval(body, &body_scope)
    }

    fn resolve_variable(
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
        } else if let Some(val) = self.context.globals.get(&first_name) {
            val.clone()
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

    fn eval_binary(
        &self,
        op: Operator,
        left: &Node,
        right: &Node,
        scope: &Arc<Scope>,
    ) -> Result<Value, RuntimeError> {
        let l = self.eval(left, scope)?;
        let r = self.eval(right, scope)?;
        match (op, &l, &r) {
            (Operator::Add, Value::Dict(_), Value::Dict(_)) => {
                let mut merged = l.clone();
                merged.deep_merge(&r);

                if let Value::Dict(ref d) = merged {
                    if let Some(ref brand_name) = d.brand {
                        if let Some(Value::Schema(_)) = scope.get_local(brand_name) {
                            let mut to_check = merged.clone();
                            let type_node = relon_parser::TypeNode {
                                path: vec![brand_name.clone()],
                                generics: Vec::new(),
                                is_optional: false,
                                range: left.range,
                            };
                            self.check_type(&mut to_check, &type_node, scope, left.range)?;
                            return Ok(to_check);
                        }
                    }
                }
                Ok(merged)
            }
            (Operator::Add, Value::Schema(_), Value::Schema(_))
            | (Operator::Add, Value::Schema(_), Value::Dict(_)) => {
                let mut merged = l.clone();
                merged.deep_merge(&r);
                Ok(merged)
            }
            (Operator::Add, Value::String(a), b) => Ok(Value::String(format!("{}{}", a, b))),
            (Operator::Add, a, Value::String(b)) => Ok(Value::String(format!("{}{}", a, b))),
            (Operator::Add | Operator::Sub | Operator::Mul, _, _) => {
                Self::eval_numeric_arithmetic(op, &l, left.range, &r, right.range)
            }
            (Operator::Div | Operator::Mod, _, _) => {
                Self::eval_numeric_division(op, &l, left.range, &r, right.range)
            }
            (Operator::Eq, a, b) => Ok(Value::Bool(a == b)),
            (Operator::Ne, a, b) => Ok(Value::Bool(a != b)),
            (Operator::Lt | Operator::Gt | Operator::Le | Operator::Ge, _, _) => {
                Self::eval_numeric_comparison(op, &l, left.range, &r, right.range)
            }
            _ => Err(RuntimeError::UnsupportedOperator(
                format!("{:?}", op),
                left.range,
            )),
        }
    }

    fn eval_unary(
        &self,
        op: Operator,
        node: &Node,
        scope: &Arc<Scope>,
    ) -> Result<Value, RuntimeError> {
        let val = self.eval(node, scope)?;
        match (op, val) {
            (Operator::Not, v) => Ok(Value::Bool(!v.is_truthy())),
            (Operator::Sub, Value::Int(i)) => Ok(Value::Int(-i)),
            (Operator::Sub, Value::Float(f)) => Ok(Value::Float(-f)),
            (Operator::Sub, v) => Err(RuntimeError::TypeMismatch {
                expected: "Number".to_string(),
                found: v.type_name().to_string(),
                range: node.range,
            }),
            _ => Err(RuntimeError::UnsupportedOperator(
                format!("{:?}", op),
                node.range,
            )),
        }
    }

    fn eval_numeric_arithmetic(
        op: Operator,
        left: &Value,
        left_range: TokenRange,
        right: &Value,
        right_range: TokenRange,
    ) -> Result<Value, RuntimeError> {
        let left = Self::expect_number(left, left_range)?;
        let right = Self::expect_number(right, right_range)?;
        match (op, left, right) {
            (Operator::Add, NumericValue::Int(a), NumericValue::Int(b)) => Ok(Value::Int(a + b)),
            (Operator::Sub, NumericValue::Int(a), NumericValue::Int(b)) => Ok(Value::Int(a - b)),
            (Operator::Mul, NumericValue::Int(a), NumericValue::Int(b)) => Ok(Value::Int(a * b)),
            (Operator::Add, a, b) => Ok(Value::Float(OrderedFloat(a.as_f64() + b.as_f64()))),
            (Operator::Sub, a, b) => Ok(Value::Float(OrderedFloat(a.as_f64() - b.as_f64()))),
            (Operator::Mul, a, b) => Ok(Value::Float(OrderedFloat(a.as_f64() * b.as_f64()))),
            _ => unreachable!("non-arithmetic operator passed to eval_numeric_arithmetic"),
        }
    }

    fn eval_numeric_division(
        op: Operator,
        left: &Value,
        left_range: TokenRange,
        right: &Value,
        right_range: TokenRange,
    ) -> Result<Value, RuntimeError> {
        let left = Self::expect_number(left, left_range)?;
        let right = Self::expect_number(right, right_range)?;
        if right.as_f64() == 0.0 {
            return Err(RuntimeError::DivisionByZero(right_range));
        }
        match (op, left, right) {
            (Operator::Div, NumericValue::Int(a), NumericValue::Int(b)) => Ok(Value::Int(a / b)),
            (Operator::Mod, NumericValue::Int(a), NumericValue::Int(b)) => Ok(Value::Int(a % b)),
            (Operator::Div, a, b) => Ok(Value::Float(OrderedFloat(a.as_f64() / b.as_f64()))),
            (Operator::Mod, a, b) => Ok(Value::Float(OrderedFloat(a.as_f64() % b.as_f64()))),
            _ => unreachable!("non-division operator passed to eval_numeric_division"),
        }
    }

    fn eval_numeric_comparison(
        op: Operator,
        left: &Value,
        left_range: TokenRange,
        right: &Value,
        right_range: TokenRange,
    ) -> Result<Value, RuntimeError> {
        let left = Self::expect_number(left, left_range)?.as_f64();
        let right = Self::expect_number(right, right_range)?.as_f64();
        let result = match op {
            Operator::Lt => left < right,
            Operator::Gt => left > right,
            Operator::Le => left <= right,
            Operator::Ge => left >= right,
            _ => unreachable!("non-comparison operator passed to eval_numeric_comparison"),
        };
        Ok(Value::Bool(result))
    }

    fn expect_number(value: &Value, range: TokenRange) -> Result<NumericValue, RuntimeError> {
        match value {
            Value::Int(value) => Ok(NumericValue::Int(*value)),
            Value::Float(value) => Ok(NumericValue::Float(*value)),
            _ => Err(RuntimeError::TypeMismatch {
                expected: "Number".to_string(),
                found: value.type_name().to_string(),
                range,
            }),
        }
    }

    fn resolve_reference(
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
                let thunk = context.elements.get(context.index - 1).unwrap();
                let val = self.force_thunk(thunk)?;
                return self.lookup_value_path(val, path, "&prev", range);
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
                let thunk = context.elements.get(context.index + 1).unwrap();
                let val = self.force_thunk(thunk)?;
                return self.lookup_value_path(val, path, "&next", range);
            }
            RefBase::This => {
                let root = scope
                    .reference_root
                    .as_deref()
                    .or(self.context.root_node.as_deref())
                    .ok_or_else(|| {
                        RuntimeError::VariableNotFound("No root for &this".to_string(), range)
                    })?;
                return self.eval_reference_path(root, path, scope, "&this", range);
            }
            _ => {}
        }

        let root = scope
            .reference_root
            .as_deref()
            .or(self.context.root_node.as_deref())
            .ok_or(RuntimeError::VariableNotFound("No root".to_string(), range))?;
        let mut target_path: Vec<TokenKey> = match base {
            RefBase::Root => Vec::new(),
            RefBase::Sibling => {
                let mut p = scope.full_path();
                p.pop();
                p.into_iter()
                    .map(|s| TokenKey::String(s, range, false))
                    .collect()
            }
            RefBase::Uncle => {
                let mut p = scope.full_path();
                p.pop();
                p.pop();
                p.into_iter()
                    .map(|s| TokenKey::String(s, range, false))
                    .collect()
            }
            _ => unreachable!(),
        };
        target_path.extend_from_slice(path);

        let path_str_vec: Vec<String> = target_path.iter().map(|k| k.name()).collect();
        let path_str = path_str_vec.join(".");

        if !target_path.is_empty() {
            let cache_key = scope.path_cache_key(&path_str_vec);
            if let Some(cached) = self.context.path_cache.lock().unwrap().get(&cache_key) {
                return Ok(cached.clone());
            }
        }
        let result = self.eval_reference_path(root, &target_path, scope, &path_str, range);
        if let Ok(value) = &result {
            if !target_path.is_empty() {
                let cache_key = scope.path_cache_key(&path_str_vec);
                self.context
                    .path_cache
                    .lock()
                    .unwrap()
                    .insert(cache_key, value.clone());
            }
        }
        result
    }

    fn eval_reference_path(
        &self,
        root: &Node,
        path: &[TokenKey],
        original_scope: &Arc<Scope>,
        display_path: &str,
        range: TokenRange,
    ) -> Result<Value, RuntimeError> {
        let mut target_scope = None;
        let mut current = Some(original_scope.clone());
        while let Some(scope) = current {
            if let Some(ref_root) = &scope.reference_root {
                if std::ptr::eq(ref_root.as_ref() as *const _, root as *const _) {
                    if let Some(root_scope) = &scope.reference_root_scope {
                        target_scope = Some(root_scope.clone());
                        break;
                    }
                }
            }
            current = scope.parent.clone();
        }

        let root_scope = target_scope.unwrap_or_else(|| {
            Arc::new(Scope {
                parent: original_scope.reference_root_parent.clone(),
                path_node: None,
                locals: Mutex::new(HashMap::new()),
                current_dir: original_scope.current_dir.clone(),
                cache_namespace: original_scope.cache_namespace.clone(),
                reference_root: original_scope.reference_root.clone(),
                reference_root_parent: original_scope.reference_root_parent.clone(),
                reference_root_scope: original_scope.reference_root_scope.clone(),
                list_context: None,
                thunks: Mutex::new(HashMap::new()),
            })
        });

        self.eval_reference_path_from(root, &root_scope, path, display_path, range)
    }

    fn eval_reference_path_from(
        &self,
        node: &Node,
        scope: &Arc<Scope>,
        path: &[TokenKey],
        display_path: &str,
        range: TokenRange,
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
                match self.resolve_dict_reference_step(pairs, &key, scope)? {
                    Some(ReferenceStep::Thunk(thunk)) => {
                        if remaining_path.is_empty() {
                            self.force_thunk(&thunk)
                        } else if matches!(thunk.node.expr.as_ref(), Expr::Dict(_) | Expr::List(_))
                        {
                            self.eval_reference_path_from(
                                &thunk.node,
                                &thunk.scope,
                                remaining_path,
                                display_path,
                                range,
                            )
                        } else {
                            let value = self.force_thunk(&thunk)?;
                            self.lookup_value_path(value, remaining_path, display_path, range)
                        }
                    }
                    Some(ReferenceStep::Value(value)) => {
                        self.lookup_value_path(*value, remaining_path, display_path, range)
                    }
                    None => {
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
                    self.eval_reference_path_from(it, &item_scope, &path[1..], display_path, range)
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
                    self.lookup_value_path(value, path, display_path, range)
                }
            }
        }
    }

    fn resolve_dict_reference_step(
        &self,
        pairs: &[(TokenKey, Node)],
        part: &str,
        scope: &Arc<Scope>,
    ) -> Result<Option<ReferenceStep>, RuntimeError> {
        for (key, value_node) in pairs.iter().rev() {
            match key {
                TokenKey::Spread(_) => {
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
            return Err(RuntimeError::CircularReference(full_path));
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

    fn force_thunk(&self, thunk: &Arc<Thunk>) -> Result<Value, RuntimeError> {
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
            return Err(RuntimeError::CircularReference(thunk.path.clone()));
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

    fn force_thunk_with_scope(
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
        range: TokenRange,
    ) -> Result<Value, RuntimeError> {
        for part in path {
            let key = part.name();
            let is_optional = part.is_optional();

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

    fn prepare_dict_scope(&self, node: &Node, scope: &Arc<Scope>) -> Result<(), RuntimeError> {
        if let Expr::Dict(pairs) = node.expr.as_ref() {
            self.register_dict_thunks(pairs, scope);
            for (key, value_node) in pairs {
                if matches!(key, TokenKey::Spread(_)) {
                    continue;
                }
                let is_schema = value_node.decorators.iter().any(|d| {
                    let name = d
                        .path
                        .iter()
                        .map(|k| k.to_string_key())
                        .collect::<Vec<_>>()
                        .join(".");
                    name == "schema"
                });

                // Only eager-eval `@schema` whose body is a literal Dict — that
                // form has a fixed, side-effect-free extraction path. Compositional
                // forms (e.g. `&sibling.Base + { ... }`) reference siblings while
                // being evaluated, which would re-enter `prepare_dict_scope` on the
                // same dict and recurse forever. Defer those to lazy thunk eval.
                let is_dict_schema = is_schema && matches!(value_node.expr.as_ref(), Expr::Dict(_));

                if Self::is_logic_definition(value_node) || is_dict_schema {
                    let key_str = match key {
                        TokenKey::String(s, _, _) => s.clone(),
                        TokenKey::Dynamic(expr_node, _) => match self.eval(expr_node, scope)? {
                            Value::String(s) => s,
                            Value::Int(i) => i.to_string(),
                            other => {
                                return Err(RuntimeError::TypeMismatch {
                                    expected: "String or Int for key".to_string(),
                                    found: other.type_name().to_string(),
                                    range: expr_node.range,
                                })
                            }
                        },
                        _ => key.to_string_key(),
                    };
                    if !Self::is_valid_identifier(&key_str) {
                        return Err(RuntimeError::InvalidIdentifier(key_str, value_node.range));
                    }

                    if is_dict_schema {
                        scope
                            .locals
                            .lock()
                            .unwrap()
                            .insert(key_str.clone(), Value::Schema(HashMap::new()));
                    }

                    let val = self.eval(value_node, scope)?;
                    scope.locals.lock().unwrap().insert(key_str, val);
                }
            }
        }
        Ok(())
    }

    fn register_dict_thunks(&self, pairs: &[(TokenKey, Node)], scope: &Arc<Scope>) {
        let mut thunks = scope.thunks.lock().unwrap();
        for (key, value_node) in pairs {
            let key_str = match key {
                TokenKey::String(s, _, _) => s.clone(),
                TokenKey::Dummy => "_".to_string(),
                TokenKey::Index(i, _) => i.to_string(),
                TokenKey::Spread(_) => continue,
                TokenKey::Dynamic(expr_node, _) => match self.eval(expr_node, scope) {
                    Ok(Value::String(s)) => s,
                    Ok(Value::Int(i)) => i.to_string(),
                    _ => continue,
                },
            };
            let item_scope = scope.with_path(key_str.clone());
            let path = item_scope.full_path();
            thunks.insert(
                key_str,
                Arc::new(Thunk {
                    node: value_node.clone(),
                    scope: item_scope.clone(),
                    path: path.clone(),
                    cache_key: item_scope.path_cache_key(&path),
                    value: Mutex::new(None),
                }),
            );
        }
    }
}

impl std::fmt::Display for Value {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Value::Null => write!(f, "null"),
            Value::Bool(b) => write!(f, "{}", b),
            Value::Int(i) => write!(f, "{}", i),
            Value::Float(fl) => write!(f, "{}", fl),
            Value::String(s) => write!(f, "{}", s),
            Value::List(l) => write!(f, "{:?}", l),
            Value::Dict(d) => write!(f, "{:?}", d.map),
            Value::Closure { .. } => write!(f, "<closure>"),
            Value::Schema(_) => write!(f, "<schema>"),
            Value::Type(t) => write!(f, "Type<{}>", Evaluator::format_type_node(t)),
            Value::Wildcard => write!(f, "*"),
        }
    }
}
