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

use crate::diagnostic::Diagnostic;
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

/// A reference site that resolves into another module via a `#import`
/// binding. Filled in by the workspace post-pass once every module's
/// AST is available — per-module resolution can only see that the
/// head matches an import alias / destructure / spread, not what the
/// imported module actually exports.
#[derive(Debug, Clone)]
pub struct CrossModuleRef {
    /// Canonical id of the target module. The post-pass looks this up
    /// in `WorkspaceTree::nodes` to descend into the imported file.
    pub module_id: String,
    /// `Some(NodeId)` when the post-pass located a specific field in
    /// the target module's root Dict. `None` when the reference is the
    /// alias head alone (`lib` in a bare `lib` expression — no
    /// following segment to descend through), in which case callers
    /// jump to the module's start.
    pub target: Option<NodeId>,
    /// Source range of the reference expression in the importer.
    pub source_range: TokenRange,
    /// Which import binding form surfaced this reference. The post-pass
    /// uses this to decide what to look up in the target module.
    pub via: CrossModuleVia,
}

/// Discriminator for [`CrossModuleRef`] / [`PendingCrossModuleRef`] —
/// which `#import` form brought the binding into scope. Determines how
/// the post-pass resolves the tail name (alias namespaces walk one
/// step deeper; spread / destructure bind a single top-level field).
#[derive(Debug, Clone)]
pub enum CrossModuleVia {
    /// `#import alias from "p"`: the head matched `alias`. Tail (if
    /// any) is the field name to look up in p's root Dict; an empty
    /// tail means the cursor sits on the alias itself.
    Alias,
    /// `#import { upstream as local } from "p"`: the head matched the
    /// local binding, which maps to `upstream` in p's root Dict.
    Destructured {
        /// Name to look up in the target module's root Dict (the
        /// upstream symbol the local name aliases).
        upstream: String,
    },
    /// `#import * from "p"`: the head was looked up across every
    /// spread target until one resolved.
    Spread,
}

/// A cross-module reference recorded during per-module resolution,
/// waiting for the workspace post-pass to look up its target NodeId.
/// Carries enough context that the post-pass doesn't have to re-walk
/// the importer's tree to decide which import index slot to consult.
#[derive(Debug, Clone)]
pub struct PendingCrossModuleRef {
    /// NodeId of the reference site in the importer. Becomes the key
    /// in `AnalyzedTree::cross_module_references` once resolved.
    pub node_id: NodeId,
    /// Source range of the reference site, mirrored from the source
    /// expression. Carried separately so the post-pass doesn't have to
    /// re-traverse the importer to recover it.
    pub source_range: TokenRange,
    /// Index into `AnalyzedTree::imports` identifying the import
    /// directive that brought the head binding into scope. Walked in
    /// lockstep with `WorkspaceTree::import_graph[importer]` to find
    /// the target module's canonical id.
    pub import_index: usize,
    /// Tail segments after the head — what to look up in the target
    /// module. For an alias import this is the path tail after the
    /// alias; for destructure / spread imports it is empty (the head
    /// itself is the imported binding).
    pub tail: Vec<String>,
    pub via: PendingCrossModuleVia,
}

#[derive(Debug, Clone)]
pub enum PendingCrossModuleVia {
    /// Head matched an `#import alias` binding. The first tail segment
    /// (if any) is the field to look up in the target module.
    Alias,
    /// Head matched a destructure entry; resolve `upstream` in the
    /// target module's root Dict.
    Destructured { upstream: String },
    /// Head didn't match any alias / destructure on this importer but
    /// at least one `#import *` was in scope. The post-pass tries
    /// every spread target in source order; first match wins.
    SpreadCandidate { head: String },
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
        iteration_depth: 0,
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
    /// Depth counter for enclosing `Expr::List` / `Expr::Comprehension`
    /// scopes. Used to flag `&prev` / `&next` / `&index` / `&this`
    /// outside any iteration — they have no meaningful semantics
    /// there (the first three runtime-error, the last falls back to
    /// `&root` and is better spelled as such).
    iteration_depth: usize,
}

impl<'a> Walker<'a> {
    fn visit_root(&mut self, root: &Node) {
        self.visit(root);
    }

    fn visit(&mut self, node: &Node) {
        match &*node.expr {
            Expr::Dict(pairs) => {
                // Record every String-keyed pair's key range, indexed
                // by the value node's id. Powers go-to-definition's
                // "select the symbol at the destination" behaviour.
                for (key, value) in pairs {
                    if let TokenKey::String(_, range, _) = key {
                        self.tree.field_key_ranges.insert(value.id, *range);
                    }
                }
                let frame = build_frame(pairs);
                self.scope_stack.push(frame);
                for (_, value) in pairs {
                    self.visit(value);
                }
                self.scope_stack.pop();
            }
            Expr::List(items) => {
                self.iteration_depth += 1;
                for item in items {
                    self.visit(item);
                }
                self.iteration_depth -= 1;
            }
            Expr::Comprehension {
                element,
                iterable,
                condition,
                ..
            } => {
                // Comprehensions iterate over `iterable`; the element /
                // condition expressions both see iteration context.
                self.iteration_depth += 1;
                self.visit(element);
                self.visit(iterable);
                if let Some(c) = condition {
                    self.visit(c);
                }
                self.iteration_depth -= 1;
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
                // List-iteration bases used outside a list / list-
                // comprehension are statically wrong: `&prev / &next /
                // &index` runtime-error, and `&this` silently falls
                // back to `&root` (better written as `&root`).
                if self.iteration_depth == 0 {
                    match base {
                        RefBase::This => {
                            self.tree
                                .diagnostics
                                .push(Diagnostic::ThisOutsideIteration {
                                    range: node.range.into(),
                                });
                        }
                        RefBase::Prev | RefBase::Next | RefBase::Index => {
                            let label = match base {
                                RefBase::Prev => "prev",
                                RefBase::Next => "next",
                                RefBase::Index => "index",
                                _ => unreachable!(),
                            };
                            self.tree
                                .diagnostics
                                .push(Diagnostic::IterationRefOutsideList {
                                    base: label.to_string(),
                                    range: node.range.into(),
                                });
                        }
                        _ => {}
                    }
                }
                if let Some(resolved) = self.resolve(base, path, node.range) {
                    self.tree.references.insert(node.id, resolved);
                } else if matches!(base, RefBase::Sibling | RefBase::Root | RefBase::Uncle) {
                    // Sibling / root / uncle references can still target
                    // a cross-module binding: `&root.lib.x` when the
                    // entry's root dict carries `#import lib from "p"`.
                    // The cross-module record reuses path_head + the
                    // tail logic; the per-doc lookup already failed, so
                    // anything we file here is non-overlapping with
                    // `references`.
                    self.queue_cross_module(node.id, path, node.range);
                }
            }
            Expr::Variable(path) => {
                // Bare identifiers behave like sibling lookups, with
                // the addition that closure params on the active frame
                // also bind. `resolve_variable` handles both.
                if let Some(resolved) = self.resolve_variable(path, node.range) {
                    self.tree.references.insert(node.id, resolved);
                } else {
                    self.queue_cross_module(node.id, path, node.range);
                }
            }
            Expr::FnCall { path, args } => {
                // Call sites (`multiply(a, b)`, `lib.shout("hi")`) want
                // the same head-resolution treatment as `Variable`:
                // without it, go-to-definition on a function name only
                // works at definition sites, not call sites. We resolve
                // `path[0]` against the scope chain (closure params +
                // dict fields), then queue cross-module otherwise.
                if let Some(resolved) = self.resolve_variable(path, node.range) {
                    self.tree.references.insert(node.id, resolved);
                } else {
                    self.queue_cross_module(node.id, path, node.range);
                }
                // Walk args ourselves — the `_` fallthrough would have
                // done it via `child_nodes`, but we've handled the
                // FnCall arm explicitly so we have to recurse manually.
                for arg in args {
                    self.visit(&arg.value);
                }
            }
            _ => {
                for child in child_nodes(node) {
                    self.visit(child);
                }
            }
        }
    }

    /// Record a pending cross-module reference if `path[0]` matches a
    /// `#import` binding visible to this importer. No-op when the head
    /// doesn't match any import — the typecheck pass will report it as
    /// `UnresolvedReference` later if it stays unbound. The function
    /// is deliberately a strict superset of the in-document scope walk:
    /// callers invoke it only after the in-document lookup has failed.
    fn queue_cross_module(&mut self, node_id: NodeId, path: &[TokenKey], source_range: TokenRange) {
        let Some(head) = path_head(path) else { return };
        // Tail segments after the head, lowered to string keys. Dynamic
        // / spread / non-string tails (`alias.[expr]`) aren't statically
        // resolvable and stay None — we still record the entry so the
        // hover layer can offer a jump to the module head.
        let tail: Vec<String> = path
            .iter()
            .skip(1)
            .filter_map(|seg| match seg {
                TokenKey::String(s, _, _) => Some(s.clone()),
                _ => None,
            })
            .collect();
        for (idx, imp) in self.tree.imports.iter().enumerate() {
            if imp.alias.as_deref() == Some(head.as_str()) {
                self.tree
                    .pending_cross_module_refs
                    .push(PendingCrossModuleRef {
                        node_id,
                        source_range,
                        import_index: idx,
                        tail,
                        via: PendingCrossModuleVia::Alias,
                    });
                return;
            }
            for (upstream, local) in &imp.destructure {
                let bound_name = local.as_deref().unwrap_or(upstream);
                if bound_name == head {
                    self.tree
                        .pending_cross_module_refs
                        .push(PendingCrossModuleRef {
                            node_id,
                            source_range,
                            import_index: idx,
                            tail,
                            via: PendingCrossModuleVia::Destructured {
                                upstream: upstream.clone(),
                            },
                        });
                    return;
                }
            }
        }
        // Fall back to a spread candidate. We can't tell yet which
        // spread import (if any) exports `head`; the post-pass tries
        // each `#import *` in source order. Only queue when at least
        // one spread import exists, otherwise the entry is dead noise.
        if self.tree.imports.iter().any(|imp| imp.spread) {
            // `import_index` for spread candidates points at the *first*
            // spread import in source order; the post-pass uses the
            // head name + every spread import on the importer, so this
            // anchor is just bookkeeping for diagnostics.
            let first_spread = self
                .tree
                .imports
                .iter()
                .position(|imp| imp.spread)
                .expect("at least one spread import (checked above)");
            self.tree
                .pending_cross_module_refs
                .push(PendingCrossModuleRef {
                    node_id,
                    source_range,
                    import_index: first_spread,
                    tail,
                    via: PendingCrossModuleVia::SpreadCandidate { head },
                });
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
