// Wired up in the follow-up commit that threads `ReachabilityPlan`
// through `compile_module_with_host_fns`. This module ships first
// (with its own unit tests) so the BFS implementation can be
// reviewed in isolation before the codegen integration lands.
#![allow(dead_code)]

//! Phase v3+ a-2 stdlib dead-code elimination.
//!
//! Walks the combined `[stdlib | user]` function table starting from
//! the entry function plus every lambda referenced by
//! [`relon_ir::Module::closure_table`], marking every callee transitively
//! reached through `Op::Call { fn_index }`. The unreachable stdlib
//! slots are then dropped from the wasm module so a user that never
//! invokes e.g. `length` does not pay for `length` in the JIT'd binary.
//!
//! ## Index spaces
//!
//! Three index spaces show up around DCE; keep them straight:
//!
//! * **IR combined index** — `0..stdlib_count + user_count`. This is
//!   what [`relon_ir::Op::Call::fn_index`] stores. The lower
//!   `stdlib_count` slots are bundled stdlib bodies; the higher slots
//!   are user functions (`#main`, schema methods, lambdas).
//! * **Wasm function index (pre-DCE)** — same as the IR combined
//!   index, but shifted up by `import_count` to account for host
//!   imports occupying `0..import_count`.
//! * **Wasm function index (post-DCE)** — what we actually emit. Only
//!   reachable stdlib bodies appear; user functions stay at the same
//!   IR-combined ordering (we do not prune user funcs). The remap
//!   produced here is from IR-combined → new-IR-combined; the
//!   `import_count` shift still happens on the emit path.
//!
//! ## Why only stdlib
//!
//! Phase v3+ a-2 scope only covers stdlib pruning. User functions
//! (including schema methods) are kept whole. Trimming user funcs
//! would require coupling closure_table rewrites against schema
//! method dispatch sites; that is left for a later phase.

use relon_ir::{Module as IrModule, Op, TaggedOp};

/// Result of the reachability sweep.
#[derive(Debug, Clone)]
pub(crate) struct ReachabilityPlan {
    /// Total stdlib slots before pruning. Equal to
    /// `relon_ir::stdlib::stdlib_function_count()`. Retained on the
    /// plan so diagnostic tooling and benches can report the size
    /// reduction without re-running [`relon_ir::stdlib::builtin_stdlib`].
    #[allow(dead_code)]
    pub(crate) stdlib_count_before: usize,
    /// Stdlib slots kept after pruning (count of reachable bundled
    /// functions). User funcs are always kept whole. Read by the
    /// in-tree tests and externalised through future bench wiring
    /// (Phase v3+ a-2 bench v10).
    #[allow(dead_code)]
    pub(crate) stdlib_count_after: usize,
    /// Maps IR-combined index (pre-DCE) to the new IR-combined index
    /// (post-DCE). Length = `stdlib_count_before + user_count`.
    /// Unreachable stdlib entries map to `u32::MAX` and must never be
    /// looked up by the emit path (they correspond to functions that
    /// were also pruned from the wasm module).
    pub(crate) remap: Vec<u32>,
    /// IR-combined indices of the reachable stdlib slots, in the
    /// original order. Length = `stdlib_count_after`. The combined
    /// emit path iterates this list to assemble the new
    /// `combined_funcs` vector.
    pub(crate) reachable_stdlib: Vec<usize>,
}

impl ReachabilityPlan {
    /// Look up the post-DCE IR-combined index for a pre-DCE index.
    /// Panics in debug builds when the slot is unreachable; release
    /// builds return `u32::MAX` and callers are expected to never
    /// emit a `call` against an unreachable slot (the BFS guarantees
    /// no reachable function references one).
    #[inline]
    pub(crate) fn translate(&self, pre_idx: u32) -> u32 {
        debug_assert!(
            (pre_idx as usize) < self.remap.len(),
            "fn_index {} out of range (table size {})",
            pre_idx,
            self.remap.len()
        );
        let mapped = self.remap[pre_idx as usize];
        debug_assert_ne!(
            mapped,
            u32::MAX,
            "fn_index {} pointed at a pruned stdlib slot",
            pre_idx
        );
        mapped
    }
}

/// Compute the reachable-stdlib plan over the combined `[stdlib | user]`
/// IR function table.
///
/// `combined_funcs` must be the same vector codegen would build
/// without DCE: stdlib functions first (`0..stdlib_count`), then
/// user functions (`stdlib_count..stdlib_count + user_count`). The
/// returned plan is consumed by the codegen path to (a) skip
/// unreachable stdlib bodies when emitting the function + code
/// sections and (b) translate every `Op::Call { fn_index }` and
/// `closure_table` entry to the post-DCE index.
///
/// Entry roots:
///
/// * `entry_func_index` — the IR-combined index of `#main`.
/// * Every entry of `closure_table` (user-visible lambdas). These
///   are funcref-table targets and are reachable through
///   `call_indirect`, which we cannot statically resolve, so we
///   conservatively mark them live.
///
/// User functions reachable from those roots stay reachable (and
/// we already keep all user funcs unconditionally, so this is mostly
/// for ensuring their callees are also kept). Stdlib-to-stdlib calls
/// are handled transitively in case future bodies grow them.
pub(crate) fn compute_plan<F>(
    combined_funcs: &[F],
    stdlib_count: usize,
    entry_combined_index: Option<usize>,
    closure_table_user_indices: &[u32],
) -> ReachabilityPlan
where
    F: AsBody,
{
    let total = combined_funcs.len();
    let mut visited = vec![false; total];
    let mut work: Vec<usize> = Vec::new();

    // Root 1: the entry function (#main). When the module has no
    // entry (library shape), nothing else is reachable from the
    // top — but we still keep every user func and treat lambda
    // entries as roots.
    if let Some(idx) = entry_combined_index {
        if idx < total && !visited[idx] {
            visited[idx] = true;
            work.push(idx);
        }
    }

    // Root 2: every lambda registered in the closure_table. The
    // table entries are user-IR indices; shift by stdlib_count to
    // get the combined index.
    for &ir_user_idx in closure_table_user_indices {
        let combined = stdlib_count + ir_user_idx as usize;
        if combined < total && !visited[combined] {
            visited[combined] = true;
            work.push(combined);
        }
    }

    // Root 3: every user function. We do not prune user funcs in
    // this phase, so all of `stdlib_count..total` are roots. This
    // also ensures schema methods (callable through Op::Call from
    // #main or sibling methods) keep their stdlib callees alive
    // even if #main itself never reaches that callee.
    for idx in stdlib_count..total {
        if !visited[idx] {
            visited[idx] = true;
            work.push(idx);
        }
    }

    // BFS / worklist sweep. Stdlib-to-stdlib calls are transitively
    // followed; today none exist in the bundled bodies, but the
    // walker is shape-agnostic so a future stdlib body that wraps
    // another stdlib (e.g. `trim` calling `substring`) stays
    // correct without any DCE-side changes.
    while let Some(fn_idx) = work.pop() {
        let body = combined_funcs[fn_idx].body();
        for tagged in body {
            if let Op::Call { fn_index, .. } = &tagged.op {
                let callee = *fn_index as usize;
                if callee < total && !visited[callee] {
                    visited[callee] = true;
                    work.push(callee);
                }
            }
        }
    }

    // Build the remap. Stdlib slots collapse into a dense prefix
    // matching `reachable_stdlib`; user slots shift down by the
    // pruned count.
    let mut remap = vec![u32::MAX; total];
    let mut reachable_stdlib: Vec<usize> = Vec::new();
    let mut new_idx: u32 = 0;
    for old_idx in 0..stdlib_count {
        if visited[old_idx] {
            remap[old_idx] = new_idx;
            reachable_stdlib.push(old_idx);
            new_idx += 1;
        }
    }
    let stdlib_count_after = reachable_stdlib.len();
    // User slots are always kept; remap by appending after the
    // reachable stdlib prefix.
    for old_idx in stdlib_count..total {
        remap[old_idx] = new_idx;
        new_idx += 1;
    }

    ReachabilityPlan {
        stdlib_count_before: stdlib_count,
        stdlib_count_after,
        remap,
        reachable_stdlib,
    }
}

/// Trait used by [`compute_plan`] so the test suite can feed in
/// synthetic IR-shaped bodies without dragging in every field of
/// `relon_ir::Func`. The codegen path uses `relon_ir::Func` directly
/// via the blanket impl below.
pub(crate) trait AsBody {
    fn body(&self) -> &[TaggedOp];
}

impl AsBody for relon_ir::Func {
    fn body(&self) -> &[TaggedOp] {
        &self.body
    }
}

/// Convenience: compute the plan from an [`IrModule`] + the already-
/// built combined function table. Wraps the entry-index shift so the
/// caller does not have to repeat the `+ stdlib_count` math.
pub(crate) fn compute_plan_for_module(
    ir: &IrModule,
    combined_funcs: &[relon_ir::Func],
    stdlib_count: usize,
) -> ReachabilityPlan {
    let entry = ir.entry_func_index.map(|i| i + stdlib_count);
    compute_plan(combined_funcs, stdlib_count, entry, &ir.closure_table)
}

#[cfg(test)]
mod tests {
    use super::*;
    use relon_ir::{IrType, TaggedOp};
    use relon_parser::TokenRange;

    /// Minimal stand-in body used by the unit tests below.
    struct FakeFn {
        body: Vec<TaggedOp>,
    }
    impl AsBody for FakeFn {
        fn body(&self) -> &[TaggedOp] {
            &self.body
        }
    }

    fn call(fn_index: u32) -> TaggedOp {
        TaggedOp {
            op: Op::Call {
                fn_index,
                arg_count: 0,
                param_tys: vec![],
                ret_ty: IrType::I64,
            },
            range: TokenRange::default(),
        }
    }

    fn empty() -> FakeFn {
        FakeFn { body: vec![] }
    }

    fn with_calls(callees: &[u32]) -> FakeFn {
        FakeFn {
            body: callees.iter().map(|&c| call(c)).collect(),
        }
    }

    #[test]
    fn unused_stdlib_pruned_user_only() {
        // 3 stdlib + 1 user; user never calls any stdlib slot.
        let funcs = vec![empty(), empty(), empty(), empty()];
        let plan = compute_plan(&funcs, 3, Some(3), &[]);
        assert_eq!(plan.stdlib_count_after, 0);
        assert_eq!(plan.reachable_stdlib, Vec::<usize>::new());
        // User slot 3 collapses down to new index 0.
        assert_eq!(plan.remap[3], 0);
    }

    #[test]
    fn used_stdlib_kept_unused_pruned() {
        // 3 stdlib + 1 user; user calls stdlib slot 1 only.
        let funcs = vec![empty(), empty(), empty(), with_calls(&[1])];
        let plan = compute_plan(&funcs, 3, Some(3), &[]);
        assert_eq!(plan.stdlib_count_after, 1);
        assert_eq!(plan.reachable_stdlib, vec![1]);
        assert_eq!(plan.remap[0], u32::MAX);
        assert_eq!(plan.remap[1], 0);
        assert_eq!(plan.remap[2], u32::MAX);
        assert_eq!(plan.remap[3], 1);
    }

    #[test]
    fn transitive_stdlib_kept() {
        // stdlib 0 calls stdlib 2; user calls only stdlib 0 -> both
        // must be retained.
        let funcs = vec![
            with_calls(&[2]),
            empty(),
            empty(),
            with_calls(&[0]), // user
        ];
        let plan = compute_plan(&funcs, 3, Some(3), &[]);
        assert_eq!(plan.stdlib_count_after, 2);
        assert_eq!(plan.reachable_stdlib, vec![0, 2]);
        assert_eq!(plan.remap[0], 0);
        assert_eq!(plan.remap[1], u32::MAX);
        assert_eq!(plan.remap[2], 1);
        assert_eq!(plan.remap[3], 2);
    }

    #[test]
    fn lambda_root_kept_with_its_stdlib_callee() {
        // 2 stdlib + 3 user. User 0 = #main (empty); user 1 = lambda
        // calling stdlib 1; user 2 = unused user fn.
        // closure_table = [1] (the lambda's user index).
        let funcs = vec![
            empty(),         // stdlib 0
            empty(),         // stdlib 1
            empty(),         // user 0 (#main)
            with_calls(&[1]), // user 1 (lambda)
            empty(),         // user 2
        ];
        let plan = compute_plan(&funcs, 2, Some(2), &[1]);
        // stdlib 1 reachable through lambda; stdlib 0 not.
        assert_eq!(plan.stdlib_count_after, 1);
        assert_eq!(plan.reachable_stdlib, vec![1]);
        assert_eq!(plan.remap[0], u32::MAX);
        assert_eq!(plan.remap[1], 0);
        // User funcs preserved.
        assert_eq!(plan.remap[2], 1);
        assert_eq!(plan.remap[3], 2);
        assert_eq!(plan.remap[4], 3);
    }
}
