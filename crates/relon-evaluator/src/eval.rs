use crate::error::RuntimeError;
use crate::value::Value;
use ordered_float::OrderedFloat;
use relon_parser::{Expr, FStringPart, Node, Operator, RefBase, TokenKey, TokenRange};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::{Arc, Mutex};

pub trait RelonFunction: Send + Sync {
    fn call(&self, args: Vec<Value>, range: TokenRange) -> Result<Value, RuntimeError>;
}

pub struct Context {
    pub globals: HashMap<String, Value>,
    pub functions: HashMap<String, Arc<dyn RelonFunction>>,
    pub root_node: Option<Node>,
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

    pub fn with_root(mut self, root: Node) -> Self {
        self.root_node = Some(root);
        self
    }

    pub fn register_fn<S: Into<String>>(&mut self, name: S, f: Arc<dyn RelonFunction>) {
        self.functions.insert(name.into(), f);
    }
}

#[derive(Default)]
pub struct Scope {
    pub parent: Option<std::sync::Arc<Scope>>,
    pub path_node: Option<String>,
    pub locals: std::sync::Mutex<std::collections::HashMap<String, crate::value::Value>>,
    pub current_dir: String,
    pub cache_namespace: String,
    pub(crate) thunks: Mutex<HashMap<String, Arc<Thunk>>>,
}

impl std::fmt::Debug for Scope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Scope")
            .field("path_node", &self.path_node)
            .field("current_dir", &self.current_dir)
            .field("cache_namespace", &self.cache_namespace)
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

    fn path_cache_key(&self, path: &str) -> String {
        let namespace = if self.cache_namespace.is_empty() {
            &self.current_dir
        } else {
            &self.cache_namespace
        };
        format!("{namespace}::{path}")
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
            thunks: Mutex::new(HashMap::new()),
        })
    }
}

pub struct Evaluator<'a> {
    pub context: &'a Context,
}

struct EvaluatedArg {
    pub name: Option<String>,
    pub value: Value,
}

pub(crate) struct Thunk {
    node: Node,
    scope: Arc<Scope>,
    path: Vec<String>,
    cache_key: String,
    value: Mutex<Option<Value>>,
}

enum ReferenceStep {
    Thunk(Arc<Thunk>),
    Value(Value),
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
        node.decorators.iter().any(|d| {
            let name = d
                .path
                .iter()
                .map(|k| k.to_string_key())
                .collect::<Vec<_>>()
                .join(".");
            name == "fn" || name == "def" || name == "args"
        })
    }

    pub fn eval(&self, node: &Node, scope: &Arc<Scope>) -> Result<Value, RuntimeError> {
        let mut current_scope = Arc::clone(scope);

        for dec in &node.decorators {
            let name = dec
                .path
                .iter()
                .map(|k| k.to_string_key())
                .collect::<Vec<_>>()
                .join(".");
            if name == "fn" || name == "def" || name == "args" {
                return self.create_closure(dec, node, &current_scope);
            }
            if name == "import" {
                current_scope = self.apply_import_decorator(dec, &current_scope)?;
            }
        }

        match node.expr.as_ref() {
            Expr::Null => Ok(Value::Null),
            Expr::Bool(b) => Ok(Value::Bool(*b)),
            Expr::Int(i) => Ok(Value::Int(*i)),
            Expr::Float(f) => Ok(Value::Float(*f)),
            Expr::String(s) => Ok(Value::String(s.clone())),

            Expr::List(elements) => {
                let mut values = Vec::new();
                for (i, el) in elements.iter().enumerate() {
                    let item_scope = current_scope.with_path(i.to_string());
                    let val = self.eval(el, &item_scope)?;
                    if let Expr::Spread(_) = el.expr.as_ref() {
                        if let Value::List(l) = val {
                            values.extend(l);
                        } else {
                            return Err(RuntimeError::TypeMismatch {
                                expected: "List".to_string(),
                                found: val.type_name().to_string(),
                                range: el.range,
                            });
                        }
                    } else {
                        values.push(val);
                    }
                }
                Ok(Value::List(values))
            }

            Expr::Dict(pairs) => {
                let dict_scope = Arc::new(Scope {
                    parent: Some(Arc::clone(&current_scope)),
                    path_node: None,
                    locals: Mutex::new(HashMap::new()),
                    current_dir: current_scope.current_dir.clone(),
                    cache_namespace: current_scope.cache_namespace.clone(),
                    thunks: Mutex::new(HashMap::new()),
                });

                self.prepare_dict_scope(node, &dict_scope)?;

                // Phase 1: Handle spreads so imported or merged names are visible to peers.
                for (key, val_node) in pairs {
                    if let TokenKey::Spread(_) = key {
                        if let Ok(Value::Dict(map)) = self.eval(val_node, &dict_scope) {
                            for (key, value) in &map {
                                self.cache_dict_child_value(&dict_scope, key, value);
                            }
                            dict_scope.locals.lock().unwrap().extend(map);
                        }
                    }
                }

                // Phase 2: Force fields in source order for the final materialized value.
                let mut map = HashMap::new();
                for (key, val_node) in pairs {
                    if let TokenKey::Spread(_) = key {
                        let val = self.eval(val_node, &dict_scope)?;
                        if let Value::Dict(d) = val {
                            for (key, value) in &d {
                                self.cache_dict_child_value(&dict_scope, key, value);
                            }
                            dict_scope.locals.lock().unwrap().extend(d.clone());
                            map.extend(d);
                        } else {
                            return Err(RuntimeError::TypeMismatch {
                                expected: "Dict".to_string(),
                                found: val.type_name().to_string(),
                                range: val_node.range,
                            });
                        }
                    } else {
                        let k_str = key.to_string_key();
                        let val = if let Some(thunk) = dict_scope.get_own_thunk(&k_str) {
                            self.force_thunk(&thunk)?
                        } else {
                            let item_scope = dict_scope.with_path(k_str.clone());
                            self.eval(val_node, &item_scope)?
                        };
                        map.insert(k_str.clone(), val.clone());
                        self.cache_dict_child_value(&dict_scope, &k_str, &val);
                        dict_scope.locals.lock().unwrap().insert(k_str, val);
                    }
                }
                Ok(Value::Dict(map))
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
        }
        .and_then(|mut val| {
            let is_def = node.decorators.iter().any(|d| {
                let name = d
                    .path
                    .iter()
                    .map(|k| k.to_string_key())
                    .collect::<Vec<_>>()
                    .join(".");
                name == "fn" || name == "def" || name == "args"
            });
            if !is_def {
                for dec in &node.decorators {
                    let name = dec
                        .path
                        .iter()
                        .map(|k| k.to_string_key())
                        .collect::<Vec<_>>()
                        .join(".");
                    if name == "import" {
                        continue;
                    }
                    let mut dec_args = Vec::new();
                    for arg in &dec.args {
                        dec_args.push(EvaluatedArg {
                            name: arg.name.clone(),
                            value: self.eval(&arg.value, &current_scope)?,
                        });
                    }
                    val = self.apply_decorator(
                        &dec.path,
                        val.clone(),
                        dec_args,
                        &current_scope,
                        dec.range,
                    )?;
                }
            }
            Ok(val)
        })
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
            if let Value::Dict(map) = evaluated_module {
                new_locals.extend(map);
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
        let canonical_path = target_path.to_string_lossy().to_string();
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
        self.context
            .loading_modules
            .lock()
            .unwrap()
            .push(canonical_path.clone());
        let content = std::fs::read_to_string(&target_path)
            .map_err(|e| RuntimeError::IoError(e.to_string()))?;
        let mut input = relon_parser::Span::new(&content);
        let node = relon_parser::parse_base(&mut input)
            .map_err(|_| RuntimeError::ModuleNotFound(canonical_path.clone(), range.into()))?;
        let module_scope = Arc::new(Scope {
            current_dir: target_path
                .parent()
                .unwrap_or(Path::new("."))
                .to_string_lossy()
                .to_string(),
            cache_namespace: canonical_path.clone(),
            ..Default::default()
        });
        let evaluated = self.eval(&node, &module_scope);
        self.context.loading_modules.lock().unwrap().pop();
        let evaluated = evaluated?;
        self.context
            .module_cache
            .lock()
            .unwrap()
            .insert(canonical_path, evaluated.clone());
        Ok(evaluated)
    }

    fn load_virtual_module(&self, path: &str, range: TokenRange) -> Result<Value, RuntimeError> {
        let content = match path {
            "std/math" => include_str!("std_relon/math.relon"),
            "std/list" => include_str!("std_relon/list.relon"),
            _ => return Err(RuntimeError::ModuleNotFound(path.to_string(), range.into())),
        };
        let mut input = relon_parser::Span::new(content);
        let node = relon_parser::parse_base(&mut input)
            .map_err(|_| RuntimeError::ModuleNotFound(path.to_string(), range.into()))?;
        self.eval(
            &node,
            &Arc::new(Scope {
                cache_namespace: path.to_string(),
                ..Default::default()
            }),
        )
    }

    fn create_closure(
        &self,
        dec: &relon_parser::Decorator,
        node: &Node,
        scope: &Arc<Scope>,
    ) -> Result<Value, RuntimeError> {
        let mut params = Vec::new();
        for arg in &dec.args {
            if arg.name.is_some() {
                return Err(RuntimeError::UnsupportedOperator(
                    "Labels not allowed".to_string(),
                    arg.value.range,
                ));
            }
            if let Expr::Variable(path) = arg.value.expr.as_ref() {
                if let Some(TokenKey::String(name, _)) = path.first() {
                    params.push(name.clone());
                    continue;
                }
            }
            return Err(RuntimeError::TypeMismatch {
                expected: "name".to_string(),
                found: format!("{:?}", arg.value.expr),
                range: arg.value.range,
            });
        }
        let mut body = node.clone();
        if let Some(pos) = body.decorators.iter().position(|d| d.range == dec.range) {
            body.decorators.remove(pos);
        }
        Ok(Value::Closure {
            params,
            body,
            captured_env: Arc::clone(scope),
        })
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
        if path.len() == 1 {
            let name = path[0].to_string_key();
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
        if path.len() == 1 {
            let name = path[0].to_string_key();
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
        let closure_scope = Arc::new(Scope {
            parent: Some(Arc::clone(captured_env)),
            path_node: None,
            locals: Mutex::new(bindings),
            current_dir: captured_env.current_dir.clone(),
            cache_namespace: captured_env.cache_namespace.clone(),
            thunks: Mutex::new(HashMap::new()),
        });
        self.eval(body, &closure_scope)
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
        let name = first.to_string_key();
        let mut current_val = if let Some(val) = scope.get_local(&name) {
            val
        } else if let Some(thunk) = scope.get_thunk(&name) {
            self.force_thunk(&thunk)?
        } else if let Some(val) = self.context.globals.get(&name) {
            val.clone()
        } else {
            return Err(RuntimeError::VariableNotFound(name, range));
        };
        for part in &path[1..] {
            let key = part.to_string_key();
            match current_val {
                Value::Dict(ref map) => {
                    if let Some(val) = map.get(&key) {
                        current_val = val.clone();
                    } else {
                        return Err(RuntimeError::VariableNotFound(
                            format!("{}.{}", name, key),
                            range,
                        ));
                    }
                }
                Value::List(ref list) => {
                    if let TokenKey::Index(idx) = part {
                        if let Some(val) = list.get(*idx) {
                            current_val = val.clone();
                        } else {
                            return Err(RuntimeError::VariableNotFound(
                                format!("{}[{}]", name, idx),
                                range,
                            ));
                        }
                    } else {
                        return Err(RuntimeError::TypeMismatch {
                            expected: "Index".to_string(),
                            found: "String".to_string(),
                            range,
                        });
                    }
                }
                _ => {
                    return Err(RuntimeError::TypeMismatch {
                        expected: "Dict/List".to_string(),
                        found: current_val.type_name().to_string(),
                        range,
                    })
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
            (Operator::Add, Value::String(a), b) => Ok(Value::String(format!("{}{}", a, b))),
            (Operator::Add, a, Value::String(b)) => Ok(Value::String(format!("{}{}", a, b))),
            (Operator::Add, Value::Int(a), Value::Int(b)) => Ok(Value::Int(a + b)),
            (Operator::Add, Value::Float(a), Value::Float(b)) => Ok(Value::Float(*a + *b)),
            (Operator::Add, Value::Int(a), Value::Float(b)) => {
                Ok(Value::Float(OrderedFloat(*a as f64) + *b))
            }
            (Operator::Add, Value::Float(a), Value::Int(b)) => {
                Ok(Value::Float(*a + OrderedFloat(*b as f64)))
            }
            (Operator::Sub, Value::Int(a), Value::Int(b)) => Ok(Value::Int(a - b)),
            (Operator::Sub, Value::Float(a), Value::Float(b)) => Ok(Value::Float(*a - *b)),
            (Operator::Sub, Value::Int(a), Value::Float(b)) => {
                Ok(Value::Float(OrderedFloat(*a as f64) - *b))
            }
            (Operator::Sub, Value::Float(a), Value::Int(b)) => {
                Ok(Value::Float(*a - OrderedFloat(*b as f64)))
            }
            (Operator::Mul, Value::Int(a), Value::Int(b)) => Ok(Value::Int(a * b)),
            (Operator::Mul, Value::Float(a), Value::Float(b)) => Ok(Value::Float(*a * *b)),
            (Operator::Mul, Value::Int(a), Value::Float(b)) => {
                Ok(Value::Float(OrderedFloat(*a as f64) * *b))
            }
            (Operator::Mul, Value::Float(a), Value::Int(b)) => {
                Ok(Value::Float(*a * OrderedFloat(*b as f64)))
            }
            (Operator::Div, Value::Int(a), Value::Int(b)) => {
                if *b == 0 {
                    Err(RuntimeError::DivisionByZero(right.range))
                } else {
                    Ok(Value::Int(a / b))
                }
            }
            (Operator::Div, a, b) => {
                let fa = match a {
                    Value::Int(i) => *i as f64,
                    Value::Float(f) => f.into_inner(),
                    _ => 0.0,
                };
                let fb = match b {
                    Value::Int(i) => *i as f64,
                    Value::Float(f) => f.into_inner(),
                    _ => 0.0,
                };
                if fb == 0.0 {
                    Err(RuntimeError::DivisionByZero(right.range))
                } else {
                    Ok(Value::Float(OrderedFloat(fa / fb)))
                }
            }
            (Operator::Mod, Value::Int(a), Value::Int(b)) => {
                if *b == 0 {
                    Err(RuntimeError::DivisionByZero(right.range))
                } else {
                    Ok(Value::Int(a % b))
                }
            }
            (Operator::Mod, a, b) => {
                let fa = match a {
                    Value::Int(i) => *i as f64,
                    Value::Float(f) => f.into_inner(),
                    _ => 0.0,
                };
                let fb = match b {
                    Value::Int(i) => *i as f64,
                    Value::Float(f) => f.into_inner(),
                    _ => 0.0,
                };
                if fb == 0.0 {
                    Err(RuntimeError::DivisionByZero(right.range))
                } else {
                    Ok(Value::Float(OrderedFloat(fa % fb)))
                }
            }
            (Operator::Eq, a, b) => Ok(Value::Bool(a == b)),
            (Operator::Ne, a, b) => Ok(Value::Bool(a != b)),
            (Operator::Lt, Value::Int(a), Value::Int(b)) => Ok(Value::Bool(a < b)),
            (Operator::Gt, Value::Int(a), Value::Int(b)) => Ok(Value::Bool(a > b)),
            (Operator::Le, Value::Int(a), Value::Int(b)) => Ok(Value::Bool(a <= b)),
            (Operator::Ge, Value::Int(a), Value::Int(b)) => Ok(Value::Bool(a >= b)),
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
            _ => Err(RuntimeError::UnsupportedOperator(
                format!("{:?}", op),
                node.range,
            )),
        }
    }

    fn resolve_reference(
        &self,
        base: &RefBase,
        path: &[TokenKey],
        scope: &Arc<Scope>,
        range: TokenRange,
    ) -> Result<Value, RuntimeError> {
        let root = self
            .context
            .root_node
            .as_ref()
            .ok_or(RuntimeError::VariableNotFound("No root".to_string(), range))?;
        let mut target_path = match base {
            RefBase::Root => Vec::new(),
            RefBase::Sibling => {
                let mut p = scope.full_path();
                p.pop();
                p
            }
            RefBase::Uncle => {
                let mut p = scope.full_path();
                p.pop();
                p.pop();
                p
            }
        };
        for part in path {
            target_path.push(part.to_string_key());
        }
        let path_str = target_path.join(".");
        if !path_str.is_empty() {
            let cache_key = scope.path_cache_key(&path_str);
            if let Some(cached) = self.context.path_cache.lock().unwrap().get(&cache_key) {
                return Ok(cached.clone());
            }
        }
        let result = self.eval_reference_path(root, &target_path, scope, &path_str, range);
        if let Ok(value) = &result {
            if !path_str.is_empty() {
                let cache_key = scope.path_cache_key(&path_str);
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
        path: &[String],
        original_scope: &Arc<Scope>,
        display_path: &str,
        range: TokenRange,
    ) -> Result<Value, RuntimeError> {
        let root_scope = Arc::new(Scope {
            parent: None,
            path_node: None,
            locals: Mutex::new(HashMap::new()),
            current_dir: original_scope.current_dir.clone(),
            cache_namespace: original_scope.cache_namespace.clone(),
            thunks: Mutex::new(HashMap::new()),
        });
        self.eval_reference_path_from(root, &root_scope, path, display_path, range)
    }

    fn eval_reference_path_from(
        &self,
        node: &Node,
        scope: &Arc<Scope>,
        path: &[String],
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
                let remaining_path = &path[1..];
                match self.resolve_dict_reference_step(pairs, part, scope)? {
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
                        self.lookup_value_path(value, remaining_path, display_path, range)
                    }
                    None => Err(RuntimeError::VariableNotFound(
                        display_path.to_string(),
                        range,
                    )),
                }
            }
            Expr::List(elements) => {
                let part = &path[0];
                let index = part
                    .parse::<usize>()
                    .map_err(|_| RuntimeError::VariableNotFound(display_path.to_string(), range))?;
                let item_scope = scope.with_path(part.clone());
                let item = elements.get(index).ok_or_else(|| {
                    RuntimeError::VariableNotFound(display_path.to_string(), range)
                })?;
                self.eval_reference_path_from(item, &item_scope, &path[1..], display_path, range)
            }
            _ => {
                let value = self.eval_node_with_path_cache(node, scope, display_path)?;
                self.lookup_value_path(value, path, display_path, range)
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
                    match spread_value {
                        Value::Dict(map) => {
                            if let Some(value) = map.get(part) {
                                return Ok(Some(ReferenceStep::Value(value.clone())));
                            }
                        }
                        other => {
                            return Err(RuntimeError::TypeMismatch {
                                expected: "Dict".to_string(),
                                found: other.type_name().to_string(),
                                range: value_node.range,
                            });
                        }
                    }
                }
                _ if key.to_string_key() == part => {
                    if let Some(thunk) = scope.get_own_thunk(part) {
                        return Ok(Some(ReferenceStep::Thunk(thunk)));
                    }
                }
                _ => {}
            }
        }

        Ok(None)
    }

    fn eval_node_with_path_cache(
        &self,
        node: &Node,
        scope: &Arc<Scope>,
        display_path: &str,
    ) -> Result<Value, RuntimeError> {
        if display_path.is_empty() {
            return self.eval(node, scope);
        }

        let cache_key = scope.path_cache_key(display_path);
        if self
            .context
            .evaluating_paths
            .lock()
            .unwrap()
            .contains(&cache_key)
        {
            return Err(RuntimeError::CircularReference(
                display_path.split('.').map(str::to_string).collect(),
            ));
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
            self.context
                .path_cache
                .lock()
                .unwrap()
                .insert(thunk.cache_key.clone(), value.clone());
        }
        result
    }

    fn cache_dict_child_value(&self, scope: &Arc<Scope>, key: &str, value: &Value) {
        let mut path = scope.full_path();
        path.push(key.to_string());
        let path_str = path.join(".");
        if path_str.is_empty() {
            return;
        }
        self.context
            .path_cache
            .lock()
            .unwrap()
            .insert(scope.path_cache_key(&path_str), value.clone());
    }

    fn lookup_value_path(
        &self,
        mut current_val: Value,
        path: &[String],
        display_path: &str,
        range: TokenRange,
    ) -> Result<Value, RuntimeError> {
        for part in path {
            current_val = match current_val {
                Value::Dict(map) => map.get(part).cloned().ok_or_else(|| {
                    RuntimeError::VariableNotFound(display_path.to_string(), range)
                })?,
                Value::List(list) => {
                    let index = part.parse::<usize>().map_err(|_| {
                        RuntimeError::VariableNotFound(display_path.to_string(), range)
                    })?;
                    list.get(index).cloned().ok_or_else(|| {
                        RuntimeError::VariableNotFound(display_path.to_string(), range)
                    })?
                }
                other => {
                    return Err(RuntimeError::TypeMismatch {
                        expected: "Dict/List".to_string(),
                        found: other.type_name().to_string(),
                        range,
                    });
                }
            };
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
                if Self::is_logic_definition(value_node) {
                    let key_str = key.to_string_key();
                    if !Self::is_valid_identifier(&key_str) {
                        return Err(RuntimeError::InvalidIdentifier(key_str, value_node.range));
                    }
                    let closure = self.eval(value_node, scope)?;
                    scope.locals.lock().unwrap().insert(key_str, closure);
                }
            }
        }
        Ok(())
    }

    fn register_dict_thunks(&self, pairs: &[(TokenKey, Node)], scope: &Arc<Scope>) {
        let mut thunks = scope.thunks.lock().unwrap();
        for (key, value_node) in pairs {
            if matches!(key, TokenKey::Spread(_)) {
                continue;
            }
            let key_str = key.to_string_key();
            if thunks.contains_key(&key_str) {
                continue;
            }
            let item_scope = scope.with_path(key_str.clone());
            let path = item_scope.full_path();
            let path_str = path.join(".");
            thunks.insert(
                key_str,
                Arc::new(Thunk {
                    node: value_node.clone(),
                    scope: item_scope.clone(),
                    path,
                    cache_key: item_scope.path_cache_key(&path_str),
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
            Value::Dict(d) => write!(f, "{:?}", d),
            Value::Closure { .. } => write!(f, "<closure>"),
        }
    }
}
