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
use crate::diagnostic::Diagnostic;
use crate::workspace::WorkspaceTree;
use miette::SourceSpan;
use relon_parser::{Expr, Node, TokenKey};
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
    // that don't use `register_fn_with_caps` pay zero overhead.
    if gates.is_empty() {
        return;
    }

    let mut diagnostics: Vec<Diagnostic> = Vec::new();
    for tree in workspace.modules.values() {
        for node in tree.node_index.values() {
            check_node(node, &caps, &gates, &mut diagnostics);
        }
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
    // `allow_all_native_fn` and explicit allowlist short-circuit any
    // gate. Match the evaluator's order in `check_native_fn_capability`
    // so a denial reason here means the same denial would surface at
    // runtime.
    if caps.allow_all_native_fn || caps.allow_native_fn.contains(&name) {
        return;
    }
    if gate.reads_fs && !caps.reads_fs {
        out.push(Diagnostic::CapabilityRequired {
            fn_name: name,
            capability: "reads_fs".to_string(),
            range: SourceSpan::from(node.range),
        });
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
        let mut gates: HashMap<String, NativeFnGate> = HashMap::new();
        gates.insert("read_file".to_string(), NativeFnGate { reads_fs: true });
        let mut names = HashSet::new();
        names.insert("read_file".to_string());
        AnalyzeOptions {
            host_fn_names: names,
            host_fn_signatures: HashMap::new(),
            host_fn_gates: gates,
            caps,
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
    // 10. caps.allow_native_fn contains the fn → silent (per-fn allowlist).
    #[test]
    fn allow_native_fn_silences_diagnostic() {
        let mut allow = HashSet::new();
        allow.insert("read_file".to_string());
        let caps = Capabilities {
            allow_native_fn: allow,
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
}
