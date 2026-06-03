//! Static capability reachability analysis.
//!
//! Walks every `FnCall` reachable from the workspace's modules — across
//! `#import` boundaries and through closure bodies — and surfaces a
//! [`Diagnostic::CapabilityRequired`] when the called fn is gated and
//! the host's `caps` grant doesn't satisfy the gate's requirement.
//!
//! This is the static mirror of `RuntimeError::CapabilityDenied`: any
//! call that would be denied at evaluation time on a literal,
//! source-reachable path is rejected here so hosts get one diagnostic
//! pass instead of trip-and-recover at runtime.
//!
//! ## Reachability model — v1
//!
//! v1 uses *textual reachability*: every `Expr::FnCall` in any module's
//! `node_index` is treated as reachable. We don't prune dead branches
//! (`if false { read_file() }` still flags) and we don't resolve
//! `obj.method` virtual dispatch (a multi-segment path with non-string
//! / spread / index segments is silently skipped). The walker is a
//! single linear pass over `node_index`; closure bodies and match arms
//! are already in the index because the resolve pass and typecheck
//! walker visit them.
//!
//! ## Why this lives at workspace scope
//!
//! `caps` and `host_fn_gates` are workspace-level facts (the host
//! installs them once when constructing the evaluator). Carrying the
//! check at workspace level — rather than as a per-module pass — means
//! a fn defined in module `A` and called from a `for`-loop body in
//! module `B` still flags, even though `B`'s analyze pass has no idea
//! `A` registered a gate. The entry tree owns the resulting diagnostics
//! so callers see them through the same `WorkspaceTree::all_error_diagnostics`
//! channel as the rest of Stage 4.

use crate::cap::{Capabilities, NativeFnGate};
use crate::const_fold::{self, ConstValue};
use crate::diagnostic::Diagnostic;
use crate::tree::AnalyzedTree;
use crate::workspace::WorkspaceTree;
use miette::SourceSpan;
use relon_parser::{child_nodes, Expr, Node, NodeId, Operator, TokenKey};
use rustc_hash::{FxHashMap, FxHashSet};
use std::collections::HashMap;
use std::sync::Arc;

/// Drive the reachability check over `workspace`. Reads every module's
/// `node_index`, compares each `FnCall`'s callee against the entry
/// tree's `host_fn_gates` and `caps`, and pushes any diagnostics onto
/// the entry tree's `diagnostics` list.
pub(crate) fn run(workspace: &mut WorkspaceTree) {
    // The entry tree owns the workspace-wide caps + gates. If it isn't
    // present (entry parse failed) there's nothing for us to compare
    // against — the workspace's own `ModuleParseError` covers it.
    let entry_id = workspace.entry_id.clone();
    let Some(entry_tree) = workspace.modules.get(&entry_id) else {
        return;
    };
    let caps = entry_tree.caps.clone();
    let gates = entry_tree.host_fn_gates.clone();

    // No gates → nothing to enforce. Skip the walk entirely so hosts
    // that only register pure fns (empty gate map) pay zero overhead.
    if gates.is_empty() {
        return;
    }

    // v1.1 control-flow pruning + single-pass walk: collect every node
    // id that lives under a statically-dead branch (`false ? ... : 0`
    // then-side, `false && ...` rhs, `true || ...` rhs, etc.) and
    // queue FnCall candidates in the *same* pass over `node_index`.
    // Per-module: dead-branch ids never cross module boundaries, and
    // `child_nodes` doesn't follow imports — so we collect against
    // the same module we walk.
    //
    // Fold memoisation: `dead_branch_of` folds the condition of every
    // Ternary / `&&` / `||` it sees. Callers can hit the same cond
    // node twice when control-flow shapes nest (e.g. a Ternary whose
    // `cond` is itself an `&&`). The `fold_cache` keyed on `NodeId`
    // collapses those repeats to a single `try_fold`, trading a small
    // map for skipping recursive descent through shared cond subtrees.
    //
    // Scope: capability_check only. The type-checker's walker still
    // visits dead branches so const-fold diagnostics (DivByZero,
    // Overflow) and resolve-time diagnostics (UnresolvedReference)
    // continue to fire; pruning those is a v1.2+ decision (see #41).
    let mut diagnostics: Vec<Diagnostic> = Vec::new();
    for tree in workspace.modules.values() {
        walk_tree_for_gated_calls(tree, &caps, &gates, &mut diagnostics);
    }

    if diagnostics.is_empty() {
        return;
    }

    // Attach to the entry tree. The Arc was created by `build()`'s BFS
    // and hasn't been shared yet; `Arc::get_mut` succeeds. If a future
    // pass clones the Arc earlier, switch to `Arc::make_mut` — the
    // borrow checker will surface the missing `Clone` impl.
    if let Some(arc_tree) = workspace.modules.get_mut(&entry_id) {
        if let Some(tree) = Arc::get_mut(arc_tree) {
            tree.diagnostics.extend(diagnostics);
        }
    }
}

/// Single-tree reachability check. The compiled backends
/// (bytecode / cranelift / llvm) drive analysis through the per-file
/// [`crate::analyze_with_options`] entry, which has no
/// [`WorkspaceTree`] — so [`run`]'s workspace walk never reaches them
/// and a gated native call would otherwise compile with the static
/// guard silently skipped. This runs the same walk over one tree's
/// own `node_index`, using its `caps` + `host_fn_gates`, and appends
/// any [`Diagnostic::CapabilityRequired`] (Error severity) to the
/// tree's own diagnostics so the build fails before lowering.
pub(crate) fn run_single(tree: &mut AnalyzedTree) {
    // No gates → nothing to enforce; zero overhead for pure-fn hosts
    // and every host-fn-free source.
    if tree.host_fn_gates.is_empty() {
        return;
    }
    let caps = tree.caps.clone();
    let gates = tree.host_fn_gates.clone();
    let mut diagnostics: Vec<Diagnostic> = Vec::new();
    walk_tree_for_gated_calls(tree, &caps, &gates, &mut diagnostics);
    tree.diagnostics.extend(diagnostics);
}

/// Shared per-tree walk: queue every `FnCall` and every
/// statically-dead-branch id in one pass over `node_index`, then emit
/// a diagnostic for each reachable gated call whose cap isn't granted.
/// Used by both the workspace [`run`] and the single-tree
/// [`run_single`].
fn walk_tree_for_gated_calls(
    tree: &AnalyzedTree,
    caps: &Capabilities,
    gates: &HashMap<String, NativeFnGate>,
    diagnostics: &mut Vec<Diagnostic>,
) {
    let mut dead_ids: FxHashSet<NodeId> = FxHashSet::default();
    let mut fn_calls: Vec<&Node> = Vec::new();
    let mut fold_cache: FxHashMap<NodeId, Option<ConstValue>> = FxHashMap::default();
    for node in tree.node_index.values() {
        if matches!(node.expr.as_ref(), Expr::FnCall { .. }) {
            fn_calls.push(node);
        }
        if let Some(dead) = dead_branch_of_cached(node, &mut fold_cache) {
            collect_descendant_ids(dead, &mut dead_ids);
        }
    }
    for node in fn_calls {
        if dead_ids.contains(&node.id) {
            continue;
        }
        check_node(node, caps, gates, diagnostics);
    }
}

/// Inspect a single `Node`. Only `Expr::FnCall` is interesting — every
/// other shape is reached transitively through the workspace's
/// `node_index` (the resolve pass walks the entire tree, so closure
/// bodies / match arms / list elements all land in the index without
/// us having to recurse here).
fn check_node(
    node: &Node,
    caps: &Capabilities,
    gates: &HashMap<String, NativeFnGate>,
    out: &mut Vec<Diagnostic>,
) {
    let Expr::FnCall { path, .. } = node.expr.as_ref() else {
        return;
    };
    // Recover the dotted name. Any non-string segment (Dynamic / Spread
    // / Index) means the call target is computed at runtime — silently
    // fall back to the runtime check.
    let Some(name) = native_function_name(path) else {
        return;
    };
    // Unregistered name → not the analyzer's business. Could be a
    // closure call, a stdlib fn (no gate), an import alias, …
    let Some(gate) = gates.get(&name) else {
        return;
    };
    // Emit one diagnostic per missing bit. Analyzer is a batch reporter,
    // so a fn declaring `reads_fs + network` with neither granted shows
    // up as two diagnostics — runtime would stop at the first. When
    // every required bit is granted the loop body is skipped and no
    // diagnostic is emitted.
    for bit in gate.missing_bits(caps) {
        out.push(Diagnostic::CapabilityRequired {
            fn_name: name.clone(),
            capability: bit.to_string(),
            range: SourceSpan::from(node.range),
        });
    }
}

/// Recursively collect `node`'s id and every descendant id reachable
/// through `child_nodes`. Used to mark a statically-dead branch as
/// unreachable for the FnCall walk: any FnCall whose own id (or whose
/// containing-expression's id) lands in the resulting set is silenced.
///
/// Mirrors the parser's `child_nodes` set — decorators, directives,
/// and type hints are intentionally skipped because they're processed
/// by separate walkers and their reachability isn't gated by the
/// control-flow head sitting above this expression.
fn collect_descendant_ids(node: &Node, out: &mut FxHashSet<NodeId>) {
    out.insert(node.id);
    for child in child_nodes(node) {
        collect_descendant_ids(child, out);
    }
}

/// Identify the dead branch of a control-flow node whose decision is
/// statically known, memoising the underlying [`const_fold::try_fold`]
/// call by cond [`NodeId`]. Returns the unreachable child when one
/// exists, `None` when both branches stay live (cond non-constant or
/// `node` isn't a control-flow shape).
///
/// Recognises:
/// * `Expr::Ternary` — `true ? t : e` → dead is `e`; `false ? t : e`
///   → dead is `t`.
/// * `Expr::Binary(Operator::And, l, r)` — `false && r` → dead is `r`
///   (short-circuit). `true && r` keeps `r` live (its value decides
///   the whole expression).
/// * `Expr::Binary(Operator::Or, l, r)` — `true || r` → dead is `r`.
///   `false || r` keeps `r` live.
///
/// The cache keeps the `Ok(Some(_))` branch of [`const_fold::try_fold`]
/// so a cond reached from multiple control-flow heads pays one
/// recursive fold instead of one per visit; fold-time errors stay
/// uncached because the type-checker's walker is the channel that
/// surfaces them. Does *not* recurse — `run` walks the tree itself and
/// calls this helper at each node it considers for pruning.
fn dead_branch_of_cached<'a>(
    node: &'a Node,
    cache: &mut FxHashMap<NodeId, Option<ConstValue>>,
) -> Option<&'a Node> {
    match node.expr.as_ref() {
        Expr::Ternary { cond, then, els } => match const_bool_cached(cond, cache)? {
            true => Some(els),
            false => Some(then),
        },
        Expr::Binary(Operator::And, l, r) => {
            if const_bool_cached(l, cache) == Some(false) {
                Some(r)
            } else {
                None
            }
        }
        Expr::Binary(Operator::Or, l, r) => {
            if const_bool_cached(l, cache) == Some(true) {
                Some(r)
            } else {
                None
            }
        }
        _ => None,
    }
}

/// Resolve `cond` to a static bool, memoising the underlying
/// [`const_fold::try_fold`] result by [`NodeId`]. Non-bool fold results
/// and fold-time errors collapse to `None` so the dead-branch decision
/// stays in lock-step with [`dead_branch_of_cached`]: only a literal
/// `Bool` cond prunes; everything else keeps both branches live.
fn const_bool_cached(
    cond: &Node,
    cache: &mut FxHashMap<NodeId, Option<ConstValue>>,
) -> Option<bool> {
    let entry = cache
        .entry(cond.id)
        .or_insert_with(|| const_fold::try_fold(cond).ok().flatten());
    match entry {
        Some(ConstValue::Bool(b)) => Some(*b),
        _ => None,
    }
}

/// Extract the dotted callee name from a `FnCall` path. Returns `None`
/// when the path contains anything other than `TokenKey::String`
/// segments — i.e. the call target is computed (`fns[k]()`,
/// `fns.{spread}()`, …) and a static check would be unsound.
///
/// Mirror of the evaluator's private `Evaluator::native_function_name`
/// in `eval.rs:1652`. The two implementations must stay in lock-step;
/// duplicating them is preferable to giving the analyzer a dependency
/// on the evaluator.
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cap::Capabilities;
    use crate::workspace::{LoadError, LoadedModule, ModuleLoader};
    use crate::AnalyzeOptions;
    use std::collections::{HashMap, HashSet};
    use std::path::{Path, PathBuf};

    /// In-memory loader: same shape as the one in `workspace_build::tests`,
    /// duplicated here so this module can run independently.
    struct MapLoader {
        files: HashMap<String, (String, String)>,
    }

    impl MapLoader {
        fn new() -> Self {
            Self {
                files: HashMap::new(),
            }
        }
        fn add(&mut self, raw: &str, canonical: &str, source: &str) -> &mut Self {
            self.files
                .insert(raw.to_string(), (canonical.to_string(), source.to_string()));
            self
        }
    }

    impl ModuleLoader for MapLoader {
        fn load(&mut self, path: &str, _current_dir: &Path) -> Result<LoadedModule, LoadError> {
            match self.files.get(path) {
                Some((canon, source)) => Ok(LoadedModule {
                    canonical_id: canon.clone(),
                    source: source.clone(),
                    current_dir: PathBuf::from("."),
                }),
                None => Err(LoadError::NotFound),
            }
        }
    }

    fn options_with_read_file_gate(caps: Capabilities) -> AnalyzeOptions {
        options_with_gate(
            "read_file",
            NativeFnGate {
                reads_fs: true,
                ..NativeFnGate::default()
            },
            caps,
        )
    }

    fn options_with_gate(name: &str, gate: NativeFnGate, caps: Capabilities) -> AnalyzeOptions {
        let mut gates: HashMap<String, NativeFnGate> = HashMap::new();
        gates.insert(name.to_string(), gate);
        let mut names = HashSet::new();
        names.insert(name.to_string());
        AnalyzeOptions {
            host_fn_names: names,
            host_fn_signatures: HashMap::new(),
            host_fn_gates: gates,
            caps,
            strict_mode: false,
            ..AnalyzeOptions::default()
        }
    }

    fn build_with_options(
        entry_id: &str,
        entry_source: &str,
        loader: &mut MapLoader,
        options: &AnalyzeOptions,
    ) -> WorkspaceTree {
        crate::workspace_build::build(
            entry_id.to_string(),
            entry_source,
            PathBuf::from("/abs"),
            loader,
            options,
        )
    }

    fn cap_diags(ws: &WorkspaceTree) -> Vec<&Diagnostic> {
        ws.modules
            .values()
            .flat_map(|t| t.diagnostics.iter())
            .filter(|d| matches!(d, Diagnostic::CapabilityRequired { .. }))
            .collect()
    }

    // -------------------------------------------------------------------
    // 1. Direct call from entry, caps deny → flagged.
    #[test]
    fn direct_call_without_grant_is_flagged() {
        let opts = options_with_read_file_gate(Capabilities::default());
        let mut loader = MapLoader::new();
        let ws = build_with_options(
            "/abs/entry",
            r#"{ x: read_file("a.txt") }"#,
            &mut loader,
            &opts,
        );
        let diags = cap_diags(&ws);
        assert_eq!(diags.len(), 1, "{diags:#?}");
        assert!(matches!(
            diags[0],
            Diagnostic::CapabilityRequired { fn_name, capability, .. }
                if fn_name == "read_file" && capability == "reads_fs"
        ));
    }

    // -------------------------------------------------------------------
    // 2. Imported module calls the gated fn → still flagged (cross-module
    //    reachability).
    #[test]
    fn cross_module_call_is_flagged() {
        let opts = options_with_read_file_gate(Capabilities::default());
        let mut loader = MapLoader::new();
        loader.add("./lib", "/abs/lib", r#"{ data: read_file("a.txt") }"#);
        let ws = build_with_options(
            "/abs/entry",
            r#"#import lib from "./lib"
            { v: lib.data }"#,
            &mut loader,
            &opts,
        );
        let diags = cap_diags(&ws);
        assert!(
            !diags.is_empty(),
            "{:?}",
            ws.modules
                .values()
                .flat_map(|t| t.diagnostics.clone())
                .collect::<Vec<_>>()
        );
    }

    // -------------------------------------------------------------------
    // 3. Closure body call → flagged.
    #[test]
    fn closure_body_call_is_flagged() {
        let opts = options_with_read_file_gate(Capabilities::default());
        let mut loader = MapLoader::new();
        let ws = build_with_options(
            "/abs/entry",
            r#"{ load: (name) => read_file(name) }"#,
            &mut loader,
            &opts,
        );
        let diags = cap_diags(&ws);
        assert_eq!(diags.len(), 1, "{diags:#?}");
    }

    // -------------------------------------------------------------------
    // 4. Stdlib call (`len`) is not in `host_fn_gates` → silent.
    #[test]
    fn stdlib_call_is_silent() {
        let opts = options_with_read_file_gate(Capabilities::default());
        let mut loader = MapLoader::new();
        let ws = build_with_options("/abs/entry", r#"{ n: len([1, 2, 3]) }"#, &mut loader, &opts);
        assert!(cap_diags(&ws).is_empty());
    }

    // -------------------------------------------------------------------
    // 5. caps.reads_fs grants the cap → silent.
    #[test]
    fn reads_fs_grant_silences_diagnostic() {
        let caps = Capabilities {
            reads_fs: true,
            ..Capabilities::default()
        };
        let opts = options_with_read_file_gate(caps);
        let mut loader = MapLoader::new();
        let ws = build_with_options(
            "/abs/entry",
            r#"{ x: read_file("a.txt") }"#,
            &mut loader,
            &opts,
        );
        assert!(cap_diags(&ws).is_empty());
    }

    // -------------------------------------------------------------------
    // 6. Empty `host_fn_gates` → silent (host never registered any gated
    //    fn). Guards the early-return shortcut.
    #[test]
    fn empty_gates_skip_check() {
        let mut names = HashSet::new();
        names.insert("read_file".to_string());
        let opts = AnalyzeOptions {
            host_fn_names: names,
            host_fn_signatures: HashMap::new(),
            host_fn_gates: HashMap::new(),
            caps: Capabilities::default(),
            strict_mode: false,
            ..AnalyzeOptions::default()
        };
        let mut loader = MapLoader::new();
        let ws = build_with_options(
            "/abs/entry",
            r#"{ x: read_file("a.txt") }"#,
            &mut loader,
            &opts,
        );
        assert!(cap_diags(&ws).is_empty());
    }

    // -------------------------------------------------------------------
    // 7. Dynamic call (callee constructed via index/spread) → silent.
    //    Modeled as a stdlib pipeline that maps over a list of fn names.
    //    A multi-segment path containing anything other than
    //    `TokenKey::String` triggers the silent fallback.
    //
    //    The Relon parser doesn't currently produce `TokenKey::Dynamic`
    //    in `FnCall::path` for any surface syntax — every hand-written
    //    call resolves to bare strings — but the gate accepts it
    //    defensively so a future parser change can't silently weaken
    //    the check.
    #[test]
    fn dynamic_call_is_silent() {
        // Mirror the production `native_function_name` decision: a
        // path with a non-string segment must yield None (silent).
        let path = vec![
            TokenKey::String(
                "read_file".to_string(),
                relon_parser::TokenRange::default(),
                false,
            ),
            TokenKey::Dummy,
        ];
        assert!(super::native_function_name(&path).is_none());
    }

    // -------------------------------------------------------------------
    // 8. analyzer reports → has_errors() flips, evaluator-side check
    //    becomes redundant.
    #[test]
    fn capability_required_blocks_evaluation() {
        let opts = options_with_read_file_gate(Capabilities::default());
        let mut loader = MapLoader::new();
        let ws = build_with_options(
            "/abs/entry",
            r#"{ x: read_file("a.txt") }"#,
            &mut loader,
            &opts,
        );
        assert!(
            ws.has_errors(),
            "expected workspace to flag the capability denial"
        );
    }

    // -------------------------------------------------------------------
    // 9. A → B → read_file across two import hops is still flagged.
    #[test]
    fn transitive_chain_is_flagged() {
        let opts = options_with_read_file_gate(Capabilities::default());
        let mut loader = MapLoader::new();
        loader
            .add(
                "./b",
                "/abs/b",
                r#"#import c from "./c"
                { mid: c.leaf }"#,
            )
            .add("./c", "/abs/c", r#"{ leaf: read_file("a.txt") }"#);
        let ws = build_with_options(
            "/abs/entry",
            r#"#import b from "./b"
            { top: b.mid }"#,
            &mut loader,
            &opts,
        );
        let diags = cap_diags(&ws);
        assert_eq!(diags.len(), 1, "{diags:#?}");
    }

    // -------------------------------------------------------------------
    // v1.1 — control-flow pruning: dead branches under a statically-known
    // ternary cond no longer flag. `false ? read_file() : 0` keeps the
    // FnCall in the AST (and in `node_index`), but `dead_branch_of`
    // hides it from the capability walk.
    #[test]
    fn dead_ternary_branch_is_not_flagged() {
        let opts = options_with_read_file_gate(Capabilities::default());
        let mut loader = MapLoader::new();
        let ws = build_with_options(
            "/abs/entry",
            r#"{ f(): false ? read_file("a.txt") : 0 }"#,
            &mut loader,
            &opts,
        );
        assert!(
            cap_diags(&ws).is_empty(),
            "{:?}",
            cap_diags(&ws).iter().collect::<Vec<_>>()
        );
    }

    // v1.1 — `false && gated()` short-circuits at fold time → rhs is dead
    // and the gated call inside it is silenced.
    #[test]
    fn dead_and_rhs_is_not_flagged() {
        let opts = options_with_read_file_gate(Capabilities::default());
        let mut loader = MapLoader::new();
        let ws = build_with_options(
            "/abs/entry",
            r#"{ x: false && (read_file("a.txt") == "") }"#,
            &mut loader,
            &opts,
        );
        assert!(cap_diags(&ws).is_empty());
    }

    // v1.1 — `true || gated()` short-circuits at fold time → rhs dead.
    #[test]
    fn dead_or_rhs_is_not_flagged() {
        let opts = options_with_read_file_gate(Capabilities::default());
        let mut loader = MapLoader::new();
        let ws = build_with_options(
            "/abs/entry",
            r#"{ x: true || (read_file("a.txt") == "") }"#,
            &mut loader,
            &opts,
        );
        assert!(cap_diags(&ws).is_empty());
    }

    // v1.1 negative — the live branch of a constant-cond ternary is still
    // walked. `true ? read_file() : 0` keeps `read_file()` reachable.
    #[test]
    fn live_ternary_branch_is_still_flagged() {
        let opts = options_with_read_file_gate(Capabilities::default());
        let mut loader = MapLoader::new();
        let ws = build_with_options(
            "/abs/entry",
            r#"{ f(): true ? read_file("a.txt") : 0 }"#,
            &mut loader,
            &opts,
        );
        let diags = cap_diags(&ws);
        assert_eq!(diags.len(), 1, "{diags:#?}");
    }

    // v1.1 negative — non-constant cond keeps both branches live, so the
    // gated call inside either branch still flags.
    #[test]
    fn variable_cond_keeps_both_branches_live() {
        let opts = options_with_read_file_gate(Capabilities::default());
        let mut loader = MapLoader::new();
        let ws = build_with_options(
            "/abs/entry",
            r#"{ flag: true, f(): flag ? read_file("a.txt") : 0 }"#,
            &mut loader,
            &opts,
        );
        let diags = cap_diags(&ws);
        assert_eq!(diags.len(), 1, "{diags:#?}");
    }

    // v1.1 — pruning is recursive: a gated call buried inside a list /
    // closure / nested expression sitting on the dead side is also
    // silenced.
    #[test]
    fn nested_call_in_dead_branch_is_silenced() {
        let opts = options_with_read_file_gate(Capabilities::default());
        let mut loader = MapLoader::new();
        let ws = build_with_options(
            "/abs/entry",
            r#"{ f(): false ? [read_file("a.txt"), read_file("b.txt")] : [] }"#,
            &mut loader,
            &opts,
        );
        assert!(cap_diags(&ws).is_empty());
    }

    // -------------------------------------------------------------------
    // 11. Each new capability bit is flagged independently — same
    //     diagnostic shape as the original `reads_fs` test, just with a
    //     different `capability` string. Drives the table-driven check
    //     in `capability_check::check_node`.
    #[test]
    fn each_new_capability_bit_flagged_independently() {
        let cases: Vec<(&str, NativeFnGate)> = vec![
            (
                "writes_fs",
                NativeFnGate {
                    writes_fs: true,
                    ..NativeFnGate::default()
                },
            ),
            (
                "network",
                NativeFnGate {
                    network: true,
                    ..NativeFnGate::default()
                },
            ),
            (
                "reads_clock",
                NativeFnGate {
                    reads_clock: true,
                    ..NativeFnGate::default()
                },
            ),
            (
                "reads_env",
                NativeFnGate {
                    reads_env: true,
                    ..NativeFnGate::default()
                },
            ),
            (
                "uses_rng",
                NativeFnGate {
                    uses_rng: true,
                    ..NativeFnGate::default()
                },
            ),
        ];
        for (bit, gate) in cases {
            let opts = options_with_gate("f", gate, Capabilities::default());
            let mut loader = MapLoader::new();
            let ws = build_with_options("/abs/entry", r#"{ x: f() }"#, &mut loader, &opts);
            let diags = cap_diags(&ws);
            assert_eq!(diags.len(), 1, "bit `{bit}`: {diags:#?}");
            let Diagnostic::CapabilityRequired { capability, .. } = diags[0] else {
                panic!(
                    "bit `{bit}`: expected CapabilityRequired, got {:?}",
                    diags[0]
                );
            };
            assert_eq!(capability, bit, "diagnostic bit name");
        }
    }

    // -------------------------------------------------------------------
    // 12. A fn declaring multiple bits with none granted produces one
    //     diagnostic per missing bit. Order is the field-declaration
    //     order in `NativeFnGate`.
    #[test]
    fn multiple_missing_bits_emit_multiple_diagnostics() {
        let gate = NativeFnGate {
            reads_fs: true,
            network: true,
            ..NativeFnGate::default()
        };
        let opts = options_with_gate("fetch", gate, Capabilities::default());
        let mut loader = MapLoader::new();
        let ws = build_with_options("/abs/entry", r#"{ x: fetch() }"#, &mut loader, &opts);
        let diags = cap_diags(&ws);
        assert_eq!(diags.len(), 2, "{diags:#?}");
        let names: Vec<&str> = diags
            .iter()
            .filter_map(|d| match d {
                Diagnostic::CapabilityRequired { capability, .. } => Some(capability.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(names, vec!["reads_fs", "network"]);
    }

    // -------------------------------------------------------------------
    // 13. Granting every bit silences every diagnostic. Exercises the
    //     only grant path — there is no global short-circuit to fall
    //     back on.
    #[test]
    fn explicit_per_bit_grants_silence_check() {
        let caps = Capabilities::all_granted();
        let gate = NativeFnGate {
            reads_fs: true,
            writes_fs: true,
            network: true,
            reads_clock: true,
            reads_env: true,
            uses_rng: true,
        };
        let opts = options_with_gate("everything", gate, caps);
        let mut loader = MapLoader::new();
        let ws = build_with_options("/abs/entry", r#"{ x: everything() }"#, &mut loader, &opts);
        let diags = cap_diags(&ws);
        assert!(diags.is_empty(), "{diags:#?}");
    }
}
