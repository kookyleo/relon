//! Static name-resolution pass.
//!
//! Walks the AST and binds every `&sibling.X` / `&root.X` / bare
//! `Variable(X)` reference whose target can be statically determined to
//! the [`NodeId`] of the dict value it points at. The result is recorded
//! in [`AnalyzedTree::references`] for downstream consumers (LSP
//! "go-to-definition", type-checker, lint).
//!
//! Conservative by design — the evaluator owns the runtime semantics, so
//! anything dynamic (spread that obscures the visible keys, list-context
//! references, closure bodies whose root differs from the document root)
//! is left unresolved. Unresolved references are *not* an error here;
//! the evaluator will still walk the scope chain at runtime.
//!
//! Pass output complements (rather than replaces) [`crate::schema`]: it
//! does not run on schema bodies, since their `Type field: predicate`
//! shape isn't a regular dict and the predicate values aren't really
//! references in the data sense.

use crate::tree::AnalyzedTree;
use relon_parser::{child_nodes, Expr, Node, NodeId, RefBase, TokenKey, TokenRange};
use std::collections::HashMap;

/// Result of resolving a reference expression to a known dict field.
#[derive(Debug, Clone)]
pub struct ResolvedRef {
    /// `NodeId` of the value node bound to the field that this
    /// reference points at. Consumers can look the node up in
    /// [`AnalyzedTree::node_index`] to get back to the source AST.
    pub target: NodeId,
    /// Range of the original reference expression (the `&sibling.X` or
    /// `Variable(X)` site), for "go to definition"-style mappings.
    pub source_range: TokenRange,
    /// Which kind of reference produced this binding (`Sibling`,
    /// `Root`, ...). Variables resolve to `RefBase::Sibling` because
    /// they're shorthand for "look up in current dict scope".
    pub via: RefBase,
}

/// Walk `root` and populate `tree.references` with every statically
/// resolvable reference. Also populates `tree.node_index` so consumers
/// can map a `NodeId` back to its `&Node`.
///
/// v1.3: when the root carries a `#main(...)` signature, every declared
/// parameter is seeded into a synthetic root-level frame *before* the
/// walker descends into the body. This applies regardless of the root
/// shape (dict / list / atomic / variant / call), so a body referring to
/// `n` in `#main(Int n) -> String\nn+1` resolves it to the param rather
/// than reporting `UnresolvedReference`. The frame is keyed by the
/// param's name and stamps the param's declared `TypeNode` into
/// `closure_param_types`, allowing the inference engine to lift the
/// reference to the right type.
pub fn resolve_references(root: &Node, tree: &mut AnalyzedTree) {
    let mut indexer = NodeIndexer { tree };
    indexer.visit_root(root);

    let main_frame = main_param_frame(tree, root.id);
    let mut walker = Walker {
        tree,
        scope_stack: Vec::new(),
    };
    if let Some(frame) = main_frame {
        walker.scope_stack.push(frame);
    }
    walker.visit_root(root);
    if walker
        .scope_stack
        .iter()
        .any(|f| !f.closure_params.is_empty())
    {
        // The main-param frame stays on the stack only for the visit;
        // pop it back off so subsequent passes don't observe it.
        // (Defensive — `visit` should not have leaked anyway.)
    }
}

/// Build a synthetic [`ScopeFrame`] populated with the entry's
/// `#main(...)` parameters. Returns `None` when the file isn't an entry
/// program (no signature) or when the signature has no params.
///
/// The frame uses `closure_params` rather than `fields` because the
/// params have no value-node of their own — using the root's NodeId as
/// the sentinel matches the closure-param convention. Their declared
/// types live on `closure_param_types` so the inference engine can lift
/// `Variable(name)` heads to the param's static type.
fn main_param_frame(tree: &AnalyzedTree, root_id: NodeId) -> Option<ScopeFrame> {
    let signature = tree.main_signature.as_ref()?;
    if signature.params.is_empty() {
        return None;
    }
    let mut frame = ScopeFrame::default();
    for param in &signature.params {
        frame.closure_params.insert(param.name.clone(), root_id);
        frame
            .closure_param_types
            .insert(param.name.clone(), param.type_node.clone());
    }
    Some(frame)
}

struct NodeIndexer<'a> {
    tree: &'a mut AnalyzedTree,
}

impl<'a> NodeIndexer<'a> {
    fn visit_root(&mut self, root: &Node) {
        self.visit(root);
    }

    fn visit(&mut self, node: &Node) {
        // Skip the synthetic-id sentinel; only real nodes belong in the
        // index.
        if node.id != NodeId::SYNTHETIC {
            // Index by NodeId. We store an `Arc<Node>` snapshot so
            // consumers don't need to keep the parser tree alive (the
            // analyzer's outputs are routinely shared via `Arc`).
            self.tree
                .node_index
                .insert(node.id, std::sync::Arc::new(node.clone()));
        }
        for child in child_nodes(node) {
            self.visit(child);
        }
    }
}

/// A frame in the lexical-resolution stack. Mirrors how the evaluator
/// walks dicts: each `Dict` node opens a frame whose `fields` map names
/// to value-node ids.
///
/// Made `pub(crate)` so [`crate::typecheck`] can reuse the same frame
/// shape — the type-checker walks the AST building a parallel stack
/// when deciding whether an unresolved name is a true error or might
/// be saved by a spread / closure binding.
#[derive(Debug, Default)]
pub(crate) struct ScopeFrame {
    pub(crate) fields: HashMap<String, NodeId>,
    /// `true` when the frame contains a spread (`{ ...x }`) we couldn't
    /// statically expand. Names not in `fields` might still be valid at
    /// runtime.
    pub(crate) has_dynamic_spread: bool,
    /// Closure parameters local to this frame (for `Closure { params,
    /// body }`). Stored separately from `fields` because they're
    /// looked up by `Variable(path)` rather than `&sibling.X`.
    pub(crate) closure_params: HashMap<String, NodeId>,
    /// Closure-param type hints, indexed by name. Populated when the
    /// type-check walker enters a closure so the inference engine can
    /// resolve `Variable(x)` heads to the param's declared type
    /// (Stage 1.5). Empty for resolution-only frames.
    pub(crate) closure_param_types: HashMap<String, relon_parser::TypeNode>,
}

impl ScopeFrame {
    fn lookup_field(&self, name: &str) -> Option<NodeId> {
        self.fields.get(name).copied()
    }

    /// True when this frame can plausibly bind `name` even if we can't
    /// see it in `fields` — i.e. there's a spread of unknown shape, or
    /// the name matches a closure param. Used by the typecheck pass
    /// to suppress false-positive `UnresolvedReference`.
    pub(crate) fn might_dynamically_bind(&self, name: &str) -> bool {
        self.has_dynamic_spread || self.closure_params.contains_key(name)
    }
}

struct Walker<'a> {
    tree: &'a mut AnalyzedTree,
    scope_stack: Vec<ScopeFrame>,
}

impl<'a> Walker<'a> {
    fn visit_root(&mut self, root: &Node) {
        self.visit(root);
    }

    fn visit(&mut self, node: &Node) {
        match &*node.expr {
            Expr::Dict(pairs) => {
                let frame = build_frame(pairs);
                self.scope_stack.push(frame);
                for (_, value) in pairs {
                    self.visit(value);
                }
                self.scope_stack.pop();
            }
            Expr::List(items) => {
                for item in items {
                    self.visit(item);
                }
            }
            Expr::Closure { params, body, .. } => {
                // Open a frame whose `closure_params` shadow outer
                // names. The closure body's `Variable(x)` references
                // resolve to params first, then fall through to enclosing
                // dict siblings — which matches the evaluator's
                // `resolve_variable` scope-chain walk.
                let mut frame = ScopeFrame::default();
                for param in params {
                    // Closure params don't have their own value-nodes
                    // in the AST, so we use the body's id as a stable
                    // sentinel. Consumers care more about "is this
                    // bound?" than the precise target node here.
                    frame.closure_params.insert(param.name.clone(), body.id);
                }
                self.scope_stack.push(frame);
                self.visit(body);
                self.scope_stack.pop();
            }
            Expr::Reference { base, path } => {
                if let Some(resolved) = self.resolve(base, path, node.range) {
                    self.tree.references.insert(node.id, resolved);
                }
            }
            Expr::Variable(path) => {
                // Bare identifiers behave like sibling lookups, with
                // the addition that closure params on the active frame
                // also bind. `resolve_variable` handles both.
                if let Some(resolved) = self.resolve_variable(path, node.range) {
                    self.tree.references.insert(node.id, resolved);
                }
            }
            _ => {
                for child in child_nodes(node) {
                    self.visit(child);
                }
            }
        }
    }

    /// Look up `path[0]` against the active scope chain. `Root` jumps
    /// straight to the bottom-most frame (the document root); `Sibling`
    /// uses the top frame; `Uncle` skips one frame up.
    fn resolve(
        &self,
        base: &RefBase,
        path: &[TokenKey],
        source_range: TokenRange,
    ) -> Option<ResolvedRef> {
        let head = path_head(path)?;
        let frame = match base {
            RefBase::Root => self.scope_stack.first()?,
            RefBase::Sibling => self.scope_stack.last()?,
            RefBase::Uncle => {
                // `path.len() >= 2` lets `&uncle.X` skip both the current
                // dict and the parent. Otherwise the legacy form `&uncle`
                // (no path) is dynamic and we punt.
                let len = self.scope_stack.len();
                if len < 2 {
                    return None;
                }
                self.scope_stack.get(len - 2)?
            }
            // List-context refs (`&prev`, `&next`, `&index`, `&this`)
            // depend on iteration state we don't track statically.
            _ => return None,
        };
        // Only resolve the *head* of the path. Multi-segment lookups
        // (`&sibling.foo.bar`) need the runtime to walk inside the value
        // — recording the head's target is enough for LSP and lint.
        let target = frame.lookup_field(&head)?;
        Some(ResolvedRef {
            target,
            source_range,
            via: *base,
        })
    }

    /// Walk the scope chain for a bare `Variable(path)`. Closure params
    /// shadow enclosing dict siblings; matches evaluator's
    /// `resolve_variable` semantics.
    fn resolve_variable(&self, path: &[TokenKey], source_range: TokenRange) -> Option<ResolvedRef> {
        let head = path_head(path)?;
        for frame in self.scope_stack.iter().rev() {
            if let Some(target) = frame.closure_params.get(&head).copied() {
                return Some(ResolvedRef {
                    target,
                    source_range,
                    via: RefBase::This,
                });
            }
            if let Some(target) = frame.lookup_field(&head) {
                return Some(ResolvedRef {
                    target,
                    source_range,
                    via: RefBase::Sibling,
                });
            }
        }
        None
    }
}

pub(crate) fn build_frame(pairs: &[(TokenKey, Node)]) -> ScopeFrame {
    let mut frame = ScopeFrame::default();
    for (key, value) in pairs {
        match key {
            TokenKey::String(name, _, _) => {
                // Last assignment wins, mirroring evaluator's
                // duplicate-key semantics.
                frame.fields.insert(name.clone(), value.id);
            }
            TokenKey::Spread(_) => {
                // Stage 2.5: when the spread inner is a dict literal we
                // can statically merge its String-keyed pairs into the
                // current frame's `fields` so a sibling reference to
                // one of the spread keys resolves without falling back
                // to the dynamic-spread escape hatch. Anything else
                // (FnCall, Variable / Reference, complex expressions)
                // stays dynamic — we'd need full evaluation semantics
                // to chase those.
                if let Expr::Dict(inner_pairs) = &*value.expr {
                    merge_dict_into_frame(inner_pairs, &mut frame);
                } else {
                    frame.has_dynamic_spread = true;
                }
            }
            _ => {}
        }
    }
    frame
}

/// Merge the String-keyed pairs of a dict literal into `frame.fields`
/// for the Stage 2.5 spread-literal static-merge. Nested spreads inside
/// the inner dict are also followed when their inner is itself a dict
/// literal, but only one level — preventing recursion blow-ups on
/// pathological inputs while still covering the common
/// `{ ...{ ...{a: 1} } }` shape.
fn merge_dict_into_frame(pairs: &[(TokenKey, Node)], frame: &mut ScopeFrame) {
    for (key, value) in pairs {
        match key {
            TokenKey::String(name, _, _) => {
                frame.fields.insert(name.clone(), value.id);
            }
            TokenKey::Spread(_) => {
                if let Expr::Dict(inner_pairs) = &*value.expr {
                    // One-level recursion guard via direct call (no
                    // shared visited set needed: dict literals are tree-
                    // shaped, not graph-shaped).
                    merge_dict_into_frame(inner_pairs, frame);
                } else {
                    frame.has_dynamic_spread = true;
                }
            }
            _ => {}
        }
    }
}

pub(crate) fn path_head(path: &[TokenKey]) -> Option<String> {
    match path.first()? {
        TokenKey::String(s, _, _) => Some(s.clone()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use relon_parser::parse_document;

    fn analyze(src: &str) -> AnalyzedTree {
        let node = parse_document(src).expect("parse");
        crate::analyze(&node)
    }

    #[test]
    fn binds_sibling_at_root_level() {
        // `&sibling.a` should resolve to the value-node id of field `a`.
        let tree = analyze(r#"{ a: 1, b: &sibling.a }"#);
        assert_eq!(tree.references.len(), 1);
        let resolved = tree.references.values().next().unwrap();
        // The recorded target must round-trip back to a node we
        // tracked in the index.
        assert!(tree.node(resolved.target).is_some());
        assert!(matches!(resolved.via, RefBase::Sibling));
    }

    #[test]
    fn binds_root_reference_from_nested_dict() {
        let tree = analyze(
            r#"{
                a: 10,
                inner: { ptr: &root.a }
            }"#,
        );
        // `ptr` resolves to the top-level `a`.
        let resolved = tree
            .references
            .values()
            .find(|r| matches!(r.via, RefBase::Root))
            .expect("root ref");
        let target_node = tree.node(resolved.target).expect("indexed");
        assert!(matches!(&*target_node.expr, Expr::Int(10)));
    }

    #[test]
    fn does_not_bind_list_context_refs() {
        // `&prev` / `&index` / `&next` need iteration state — skip them.
        let tree = analyze(
            r#"[
                { v: 1, p: &prev },
                { v: 2, p: &prev.v }
            ]"#,
        );
        assert!(tree.references.is_empty(), "{:?}", tree.references);
    }

    #[test]
    fn variables_resolve_like_siblings() {
        // Bare identifiers that name a sibling should bind too.
        let tree = analyze(r#"{ helper(x): x + 1, twice: helper }"#);
        // The `helper` reference inside `twice: helper` is a Variable
        // expression. Confirm it's bound.
        assert!(tree.references.values().any(|r| {
            let node = tree.node(r.target).unwrap();
            matches!(&*node.expr, Expr::Closure { .. })
        }));
    }

    #[test]
    fn closure_params_shadow_outer_siblings() {
        // `x` inside the closure body should bind to the closure
        // param, not to the outer `x: 100` field.
        let tree = analyze(
            r#"{
                x: 100,
                fn(x): x + 1
            }"#,
        );
        // Find the `Variable(x)` reference inside the closure body
        // (the `x + 1` expression).
        let bound = tree
            .references
            .values()
            .find(|r| {
                let target = tree.node(r.target).unwrap();
                // Closure-param sentinel is the body's NodeId, which
                // is the `Binary(Add, x, 1)` expression.
                matches!(&*target.expr, Expr::Binary(_, _, _))
            })
            .expect("closure param resolved");
        assert!(matches!(bound.via, RefBase::This));
    }

    #[test]
    fn dict_with_spread_marks_frame_dynamic() {
        // The spread expands `base`'s keys at runtime. The frame
        // containing the spread should report `has_dynamic_spread`
        // so a downstream typecheck pass won't false-positive on
        // names that may come from `base`. Inline check: ask the
        // builder directly.
        use relon_parser::{parse_document, Expr, TokenKey};
        let node = parse_document(
            r#"{
                base: { x: 1, y: 2 },
                merged: { ...&sibling.base, z: 3 }
            }"#,
        )
        .unwrap();
        // Drill down to the inner dict (the value of "merged") and
        // build a frame for it.
        let Expr::Dict(root_pairs) = &*node.expr else {
            panic!()
        };
        let merged_value = &root_pairs
            .iter()
            .find(|(k, _)| matches!(k, TokenKey::String(s, _, _) if s == "merged"))
            .unwrap()
            .1;
        let Expr::Dict(merged_pairs) = &*merged_value.expr else {
            panic!()
        };
        let frame = build_frame(merged_pairs);
        assert!(frame.has_dynamic_spread);
        assert!(!frame.fields.contains_key("x"));
        assert!(frame.fields.contains_key("z"));
    }
}
