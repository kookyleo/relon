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
//!
//! ## Sub-module split
//!
//! The dispatch loop + struct definitions live in this file; each
//! sibling sub-module hangs more methods off the same `Walker<'a>` via
//! an `impl<'a> super::Walker<'a>` extension block so all sub-modules
//! share the same private fields without a trait abstraction.
//!
//! - **`scope`** — in-document scope-walk resolution: `resolve`
//!   (`&sibling` / `&root` / `&uncle` base lookup) and
//!   `resolve_variable` (closure-param + dict-sibling chain walk for
//!   bare `Variable` / `FnCall` heads).
//! - **`cross_module`** — `queue_cross_module` records pending
//!   `#import`-rooted references that the workspace post-pass later
//!   binds to their target module's NodeId.

mod cross_module;
mod scope;

#[cfg(test)]
mod tests;

use crate::tree::AnalyzedTree;
use relon_parser::{child_nodes, Expr, Node, NodeId, RefBase, TokenKey, TokenRange};
use std::collections::HashMap;

use crate::diagnostic::Diagnostic;

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
            //
            // `node.clone()` is now O(1) amortised because `Node::expr` is
            // an `Arc<Expr>` — the clone bumps the refcount instead of
            // recursively deep-copying the subtree. Total work across the
            // walk drops from O(N^2) to O(N).
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

    /// True for the synthetic `#main(...)` parameter frame: it carries
    /// no real dict `fields`, only `closure_params` (the params, whose
    /// types live on `closure_param_types`). `&root` resolution skips
    /// such a frame so it lands on the document-root dict rather than
    /// the param frame that sits below it on the stack.
    pub(crate) fn is_main_param_frame(&self) -> bool {
        self.fields.is_empty() && !self.closure_params.is_empty()
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
                id,
                iterable,
                condition,
            } => {
                // Comprehensions iterate over `iterable`; the element /
                // condition expressions both see iteration context.
                //
                // R1: the comprehension binding (`[ … for x in xs ]`)
                // introduces `x` into the element / condition scope. We
                // open a frame whose `closure_params` carries `x` so the
                // body's `Variable(x)` head resolves (lands in
                // `references`) instead of falling through to
                // `queue_cross_module`. The iterable itself is evaluated
                // in the *outer* scope (the binding isn't visible to its
                // own source), so we visit it before pushing the frame.
                self.iteration_depth += 1;
                self.visit(iterable);
                let mut frame = ScopeFrame::default();
                // The binding has no value-node of its own; use the
                // element's id as the stable sentinel, matching the
                // closure-param convention.
                frame.closure_params.insert(id.clone(), element.id);
                self.scope_stack.push(frame);
                self.visit(element);
                if let Some(c) = condition {
                    self.visit(c);
                }
                self.scope_stack.pop();
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
            Expr::Where { expr, bindings } => {
                // Phase 9.b-3: `expr where { a: va, b: vb }` extends the
                // body's scope with the binding names. Without the push
                // below, strict-mode resolution doesn't see `a` / `b`
                // inside `expr` and `check_unresolved_var` escalates to a
                // false-positive `UnknownReferenceType`. The bindings
                // node is itself a Dict — visiting it via the normal arm
                // pushes its own frame, which lets one binding's value
                // reference an earlier sibling binding (matching the
                // evaluator's dict-frame semantics for the bindings
                // dict).
                self.visit(bindings);
                if let Expr::Dict(pairs) = &*bindings.expr {
                    let frame = build_frame(pairs);
                    self.scope_stack.push(frame);
                    self.visit(expr);
                    self.scope_stack.pop();
                } else {
                    self.visit(expr);
                }
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
