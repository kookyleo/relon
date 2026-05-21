//! Resolve sub-module: cross-module pending-reference queue.
//!
//! When the in-document scope walk fails to bind a reference head, the
//! head may still resolve through an `#import` directive on the
//! current file. This module owns `queue_cross_module`, which records
//! a [`PendingCrossModuleRef`] keyed by import-binding form (alias /
//! destructure / spread). The workspace post-pass later walks the
//! `WorkspaceTree::import_graph` to bind each pending entry to a real
//! target NodeId in the imported module's tree.
//!
//! Carved out so the per-doc resolution loop in `mod.rs` doesn't have
//! to carry the import-index matching logic inline.

use super::{path_head, PendingCrossModuleRef, PendingCrossModuleVia, Walker};
use relon_parser::{NodeId, TokenKey, TokenRange};

impl<'a> Walker<'a> {
    /// Record a pending cross-module reference if `path[0]` matches a
    /// `#import` binding visible to this importer. No-op when the head
    /// doesn't match any import — the typecheck pass will report it as
    /// `UnresolvedReference` later if it stays unbound. The function
    /// is deliberately a strict superset of the in-document scope walk:
    /// callers invoke it only after the in-document lookup has failed.
    pub(super) fn queue_cross_module(
        &mut self,
        node_id: NodeId,
        path: &[TokenKey],
        source_range: TokenRange,
    ) {
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
}
