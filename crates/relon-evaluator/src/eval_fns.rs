    fn resolve_reference(
        &self,
        base: &RefBase,
        path: &[TokenKey],
        scope: &Arc<Scope>,
        range: TokenRange,
    ) -> Result<Value, RuntimeError> {
        let root = self.context.root_node.as_ref().ok_or_else(|| {
            RuntimeError::VariableNotFound("No root".to_string(), range)
        })?;
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
        if self.context.evaluating_paths.borrow().contains(&path_str) {
            return Err(RuntimeError::CircularReference(target_path));
        }
        self.context
            .evaluating_paths
            .borrow_mut()
            .insert(path_str.clone());
            
        let result = match self.find_node_by_path_with_scope(root, &target_path, scope) {
            Some((target_node, target_scope)) => self.eval(target_node, &target_scope),
            None => Err(RuntimeError::VariableNotFound(path_str.clone(), range)),
        };
        self.context.evaluating_paths.borrow_mut().remove(&path_str);
        result
    }

    fn find_node_by_path_with_scope<'b>(
        &self,
        root: &'b Node,
        path: &[String],
        original_scope: &Arc<Scope>,
    ) -> Option<(&'b Node, Arc<Scope>)> {
        let mut current = root;
        let mut current_scope = Arc::new(Scope {
            parent: None,
            path_node: None,
            locals: std::cell::RefCell::new(HashMap::new()),
            current_dir: original_scope.current_dir.clone(),
        });

        // Add definitions from root if root is a dict
        if let Expr::Dict(pairs) = current.expr.as_ref() {
            for (k, v) in pairs {
                if let TokenKey::Spread(_) = k { continue; }
                let k_str = k.to_string_key();
                if v.decorators.iter().any(|d| d.id.name() == "fn" || d.id.name() == "def" || d.id.name() == "args") {
                    if let Ok(closure) = self.eval(v, &current_scope) {
                        current_scope.locals.borrow_mut().insert(k_str, closure);
                    }
                }
            }
        }

        for part in path {
            current_scope = current_scope.with_path(part.clone());
            match current.expr.as_ref() {
                Expr::Dict(pairs) => {
                    current = pairs.iter().find(|(k, _)| k.to_string_key() == *part).map(|(_, v)| v)?;
                    if let Expr::Dict(inner_pairs) = current.expr.as_ref() {
                        for (k, v) in inner_pairs {
                            if let TokenKey::Spread(_) = k { continue; }
                            let k_str = k.to_string_key();
                            if v.decorators.iter().any(|d| d.id.name() == "fn" || d.id.name() == "def" || d.id.name() == "args") {
                                if let Ok(closure) = self.eval(v, &current_scope) {
                                    current_scope.locals.borrow_mut().insert(k_str, closure);
                                }
                            }
                        }
                    }
                }
                Expr::List(elements) => {
                    let index = part.parse::<usize>().ok()?;
                    current = elements.get(index)?;
                }
                _ => return None,
            }
        }
        Some((current, current_scope))
    }
}
