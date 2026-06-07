//! Resolve sub-module: in-document scope-walk reference resolution.
//!
//! Hosts the `Walker` extension methods that look a reference's head
//! up against the active scope stack:
//!
//! * `resolve` — `&sibling.X` / `&root.X` / `&uncle.X` base lookups.
//!   Each base picks a different frame off the stack; only the *head*
//!   path segment is resolved (multi-segment tails like
//!   `&sibling.foo.bar` rely on runtime walking inside the value).
//! * `resolve_variable` — bare `Variable(path)` / `FnCall` head lookup.
//!   Walks the scope stack from innermost outward; closure params
//!   shadow enclosing dict siblings, mirroring the evaluator's
//!   `resolve_variable` semantics.
//!
//! Co-located because both methods consume the same `scope_stack`
//! field and share `path_head` from the parent module.

use super::{path_head, ResolvedRef, Walker};
use relon_parser::{RefBase, TokenKey, TokenRange};

impl<'a> Walker<'a> {
    /// Look up `path[0]` against the active scope chain. `Root` jumps
    /// straight to the bottom-most frame (the document root); `Sibling`
    /// uses the top frame; `Uncle` skips one frame up.
    pub(super) fn resolve(
        &self,
        base: &RefBase,
        path: &[TokenKey],
        source_range: TokenRange,
    ) -> Option<ResolvedRef> {
        let head = path_head(path)?;
        let frame = match base {
            // `&root` targets the document-root dict. When the entry
            // declares `#main(...)` params, a synthetic param-only frame
            // (empty `fields`, populated `closure_params`) sits *below*
            // the root dict on the stack — `first()` would land on it
            // and miss the root dict's fields. Skip leading param-only
            // frames so `&root.x` resolves to the actual root field even
            // in an entry program. (Without params the first frame IS
            // the root dict, so this is a no-op for library files.)
            RefBase::Root => self.scope_stack.iter().find(|f| !f.is_main_param_frame())?,
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
    pub(super) fn resolve_variable(
        &self,
        path: &[TokenKey],
        source_range: TokenRange,
    ) -> Option<ResolvedRef> {
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
