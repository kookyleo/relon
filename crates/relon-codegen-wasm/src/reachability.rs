//! Phase v3+ b-1 whole-program dead-code elimination.
//!
//! Walks the combined `[stdlib | user]` function table starting from
//! the entry function alone (`#main`), marking every callee reached
//! transitively through `Op::Call { fn_index }` (direct dispatch into
//! stdlib bodies or user schema methods) and `Op::MakeClosure {
//! fn_table_idx }` (indirect dispatch into lambdas registered in the
//! IR module's `closure_table`). Schema methods that nobody invokes
//! and lambdas that nobody constructs are pruned from the wasm
//! module so a user with five method declarations but only one
//! reachable call site does not pay for the other four in the JIT'd
//! binary.
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
//!   reachable stdlib **and** user bodies appear; both are compacted
//!   in their original ordering with stdlib slots first. The remap
//!   produced here is from IR-combined → new-IR-combined; the
//!   `import_count` shift still happens on the emit path.
//!
//! ## Closure-table slot remap
//!
//! Phase b-1 also prunes the wasm `funcref` table backing closure
//! dispatch. Each entry in [`relon_ir::Module::closure_table`] names a
//! lambda's IR user index; entries whose lambda is unreachable are
//! dropped, and the rest are compacted into a dense `0..k` range.
//! [`ReachabilityPlan::closure_slot_remap`] turns pre-DCE
//! `Op::MakeClosure::fn_table_idx` values into the post-DCE slot
//! before codegen materialises the handle's `fn_table_idx` field.
//!
//! ## Why entry-only roots
//!
//! Phase a-2 marked every user function (and every lambda in the
//! closure table) as a root, which kept the DCE strictly stdlib-only.
//! Phase b-1 narrows the root set to `#main` and walks edges through
//! both direct `Op::Call` and the closure-construction edge —
//! `MakeClosure` literally embeds the closure table slot in the
//! handle, so the lambda is reachable iff that slot is reachable.
//! `Op::CallClosure` itself goes through `call_indirect` against a
//! runtime-loaded slot and is therefore conservative: it requires no
//! extra edge, because every lambda that can land on the operand
//! stack at a `CallClosure` site must have been built by some
//! statically-visible `MakeClosure`.

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
    /// functions). Read by the in-tree tests and externalised through
    /// future bench wiring (Phase v3+ a-2 bench v10 / b-1 bench v13).
    #[allow(dead_code)]
    pub(crate) stdlib_count_after: usize,
    /// User function slots kept after pruning (count of reachable
    /// schema methods, the entry, and lambdas combined). The pre-DCE
    /// count is implicit — `remap.len() - stdlib_count_before`.
    #[allow(dead_code)]
    pub(crate) user_count_after: usize,
    /// Maps IR-combined index (pre-DCE) to the new IR-combined index
    /// (post-DCE). Length = `stdlib_count_before + user_count_before`.
    /// Unreachable entries map to `u32::MAX` and must never be looked
    /// up by the emit path (they correspond to functions that were
    /// also pruned from the wasm module).
    pub(crate) remap: Vec<u32>,
    /// IR-combined indices of every reachable function in the
    /// original (pre-DCE) order — stdlib slots first, then user
    /// slots. Length = `stdlib_count_after + user_count_after`. The
    /// codegen emit path iterates this list to assemble the new
    /// `combined_funcs` vector.
    pub(crate) reachable_funcs: Vec<usize>,
    /// Maps pre-DCE closure-table slot index to post-DCE slot index.
    /// Length matches the pre-DCE `Module::closure_table`. Entries
    /// whose lambda became unreachable map to `u32::MAX` — codegen
    /// must never see a `Op::MakeClosure` with such an index in a
    /// reachable body (the lambda being unreachable implies no
    /// reachable construction site, by induction on the BFS).
    pub(crate) closure_slot_remap: Vec<u32>,
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
            "fn_index {} pointed at a pruned slot",
            pre_idx
        );
        mapped
    }

    /// Look up the post-DCE closure-table slot for a pre-DCE
    /// `Op::MakeClosure::fn_table_idx`. Panics in debug builds when
    /// the slot is unreachable; release builds return `u32::MAX` for
    /// caller-side diagnostics.
    ///
    /// Kept for diagnostic / external-tooling use even though the
    /// codegen emit path reads the remap slice directly through
    /// `closure_slot_remap` — the helper keeps the panic discipline
    /// uniform with `translate`.
    #[inline]
    #[allow(dead_code)]
    pub(crate) fn translate_closure_slot(&self, pre_slot: u32) -> u32 {
        debug_assert!(
            (pre_slot as usize) < self.closure_slot_remap.len(),
            "fn_table_idx {} out of range (closure_table size {})",
            pre_slot,
            self.closure_slot_remap.len()
        );
        let mapped = self.closure_slot_remap[pre_slot as usize];
        debug_assert_ne!(
            mapped,
            u32::MAX,
            "fn_table_idx {} pointed at a pruned closure slot",
            pre_slot
        );
        mapped
    }
}

/// Compute the reachable-funcs plan over the combined `[stdlib | user]`
/// IR function table.
///
/// `combined_funcs` must be the same vector codegen would build
/// without DCE: stdlib functions first (`0..stdlib_count`), then
/// user functions (`stdlib_count..stdlib_count + user_count`). The
/// returned plan is consumed by the codegen path to (a) skip
/// unreachable bodies when emitting the function + code sections,
/// (b) translate every `Op::Call { fn_index }` to the post-DCE
/// IR-combined index, and (c) translate every
/// `Op::MakeClosure { fn_table_idx }` to the post-DCE closure-table
/// slot.
///
/// Roots:
///
/// * `entry_combined_index` — the IR-combined index of `#main`. When
///   the module is library-shaped (`None`), every user function stays
///   reachable for backward compatibility with the v3+ a-2 contract;
///   stdlib bodies are still pruned through the BFS sweep below.
///
/// Edges followed by the BFS:
///
/// * `Op::Call { fn_index }` — direct dispatch into a stdlib body
///   or user schema method. The combined index is added to the
///   worklist as-is.
/// * `Op::MakeClosure { fn_table_idx }` — closure construction;
///   resolves the slot through `closure_table_user_indices` to find
///   the lambda's user-IR-index, shifts by `stdlib_count`, then
///   adds the combined index. The slot itself is recorded so the
///   closure-table remap can pin down which entries survive.
/// * `Op::CallClosure` — no specific target; relies on the matching
///   construction site having been visited already.
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
    let mut closure_slot_visited = vec![false; closure_table_user_indices.len()];

    match entry_combined_index {
        Some(idx) if idx < total => {
            visited[idx] = true;
            work.push(idx);
        }
        Some(_) => {
            // Out-of-range entry — leave the visited set untouched
            // and fall through to an empty BFS. The plan will end up
            // with zero reachable funcs which the emit path treats
            // as a hard error elsewhere.
        }
        None => {
            // Library-shaped module: no entry, no statically-known
            // root. Keep every user function alive (matches the
            // v3+ a-2 behaviour). Stdlib slots are still pruned
            // because the BFS only reaches one if a kept user body
            // calls it. Closure-table entries are seeded as roots so
            // lambdas declared at library scope stay live.
            for (idx, slot) in visited.iter_mut().enumerate().skip(stdlib_count) {
                if !*slot {
                    *slot = true;
                    work.push(idx);
                }
            }
            for (slot_idx, &ir_user_idx) in closure_table_user_indices.iter().enumerate() {
                closure_slot_visited[slot_idx] = true;
                let combined = stdlib_count + ir_user_idx as usize;
                if combined < total && !visited[combined] {
                    visited[combined] = true;
                    work.push(combined);
                }
            }
        }
    }

    // BFS / worklist sweep. The visitor descends through structured
    // control-flow children (`Op::Block`, `Op::Loop`, `Op::If`) and
    // walks two distinct edge kinds — direct `Op::Call` targets and
    // `Op::MakeClosure` slots resolved through the closure table.
    while let Some(fn_idx) = work.pop() {
        let body = combined_funcs[fn_idx].body();
        visit_edges(
            body,
            total,
            stdlib_count,
            closure_table_user_indices,
            &mut visited,
            &mut closure_slot_visited,
            &mut work,
        );
    }

    // Build the function-index remap. Stdlib slots collapse into a
    // dense prefix matching the reachable stdlib slots; reachable
    // user slots follow in their original order.
    let mut remap = vec![u32::MAX; total];
    let mut reachable_funcs: Vec<usize> = Vec::new();
    let mut new_idx: u32 = 0;
    for (old_idx, slot) in visited.iter().enumerate().take(stdlib_count) {
        if *slot {
            remap[old_idx] = new_idx;
            reachable_funcs.push(old_idx);
            new_idx += 1;
        }
    }
    let stdlib_count_after = reachable_funcs.len();
    for (old_idx, slot) in visited.iter().enumerate().skip(stdlib_count) {
        if *slot {
            remap[old_idx] = new_idx;
            reachable_funcs.push(old_idx);
            new_idx += 1;
        }
    }
    let user_count_after = reachable_funcs.len() - stdlib_count_after;

    // Build the closure-slot remap. Entries whose lambda became
    // unreachable map to `u32::MAX`; surviving entries are compacted
    // in their original pre-DCE order so the funcref table emission
    // stays deterministic.
    let mut closure_slot_remap = vec![u32::MAX; closure_table_user_indices.len()];
    let mut next_slot: u32 = 0;
    for (slot_idx, &live) in closure_slot_visited.iter().enumerate() {
        if live {
            closure_slot_remap[slot_idx] = next_slot;
            next_slot += 1;
        }
    }

    ReachabilityPlan {
        stdlib_count_before: stdlib_count,
        stdlib_count_after,
        user_count_after,
        remap,
        reachable_funcs,
        closure_slot_remap,
    }
}

/// Recursively walk an op sequence, marking every `Op::Call` target
/// and every `Op::MakeClosure`-referenced lambda as reachable, then
/// pushing newly-reachable callees onto the worklist. Descends
/// through `Op::Block` / `Op::Loop` / `Op::If` bodies so callees
/// nested inside structured control flow (v3+ a-4 `upper` / `lower`
/// call `__casefold_lookup` from inside a `Op::Loop`) still get
/// picked up.
#[allow(clippy::too_many_arguments)]
fn visit_edges(
    body: &[TaggedOp],
    total: usize,
    stdlib_count: usize,
    closure_table_user_indices: &[u32],
    visited: &mut [bool],
    closure_slot_visited: &mut [bool],
    work: &mut Vec<usize>,
) {
    for tagged in body {
        match &tagged.op {
            Op::Call { fn_index, .. } => {
                let callee = *fn_index as usize;
                if callee < total && !visited[callee] {
                    visited[callee] = true;
                    work.push(callee);
                }
            }
            Op::MakeClosure { fn_table_idx, .. } => {
                let slot = *fn_table_idx as usize;
                if slot < closure_table_user_indices.len() && !closure_slot_visited[slot] {
                    closure_slot_visited[slot] = true;
                    let ir_user_idx = closure_table_user_indices[slot];
                    let combined = stdlib_count + ir_user_idx as usize;
                    if combined < total && !visited[combined] {
                        visited[combined] = true;
                        work.push(combined);
                    }
                }
            }
            Op::If {
                then_body,
                else_body,
                ..
            } => {
                visit_edges(
                    then_body,
                    total,
                    stdlib_count,
                    closure_table_user_indices,
                    visited,
                    closure_slot_visited,
                    work,
                );
                visit_edges(
                    else_body,
                    total,
                    stdlib_count,
                    closure_table_user_indices,
                    visited,
                    closure_slot_visited,
                    work,
                );
            }
            Op::Block { body, .. } | Op::Loop { body, .. } => {
                visit_edges(
                    body,
                    total,
                    stdlib_count,
                    closure_table_user_indices,
                    visited,
                    closure_slot_visited,
                    work,
                );
            }
            _ => {}
        }
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
    use relon_ir::{ClosureCapture, IrType, TaggedOp};
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

    fn make_closure(fn_table_idx: u32) -> TaggedOp {
        TaggedOp {
            op: Op::MakeClosure {
                fn_table_idx,
                captures: Vec::<ClosureCapture>::new(),
                captures_size: 0,
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
        assert_eq!(plan.user_count_after, 1);
        assert_eq!(plan.reachable_funcs, vec![3]);
        // User slot 3 collapses down to new index 0.
        assert_eq!(plan.remap[3], 0);
    }

    #[test]
    fn used_stdlib_kept_unused_pruned() {
        // 3 stdlib + 1 user; user calls stdlib slot 1 only.
        let funcs = vec![empty(), empty(), empty(), with_calls(&[1])];
        let plan = compute_plan(&funcs, 3, Some(3), &[]);
        assert_eq!(plan.stdlib_count_after, 1);
        assert_eq!(plan.user_count_after, 1);
        assert_eq!(plan.reachable_funcs, vec![1, 3]);
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
        assert_eq!(plan.reachable_funcs, vec![0, 2, 3]);
        assert_eq!(plan.remap[0], 0);
        assert_eq!(plan.remap[1], u32::MAX);
        assert_eq!(plan.remap[2], 1);
        assert_eq!(plan.remap[3], 2);
    }

    #[test]
    fn unused_user_method_pruned() {
        // 0 stdlib + 3 user. User 0 = #main, user 1 = unused method,
        // user 2 = another unused method. Entry is user 0.
        let funcs = vec![empty(), empty(), empty()];
        let plan = compute_plan(&funcs, 0, Some(0), &[]);
        assert_eq!(plan.stdlib_count_after, 0);
        assert_eq!(plan.user_count_after, 1);
        assert_eq!(plan.reachable_funcs, vec![0]);
        assert_eq!(plan.remap[0], 0);
        assert_eq!(plan.remap[1], u32::MAX);
        assert_eq!(plan.remap[2], u32::MAX);
    }

    #[test]
    fn transitive_method_call_chain_kept() {
        // 0 stdlib + 4 user. user 0 = #main calls user 1; user 1
        // calls user 2; user 2 calls user 3. Entire chain must
        // survive — pruning any breaks the dispatch.
        let funcs = vec![
            with_calls(&[1]),
            with_calls(&[2]),
            with_calls(&[3]),
            empty(),
        ];
        let plan = compute_plan(&funcs, 0, Some(0), &[]);
        assert_eq!(plan.user_count_after, 4);
        assert_eq!(plan.reachable_funcs, vec![0, 1, 2, 3]);
        for (i, slot) in plan.remap.iter().enumerate() {
            assert_eq!(
                *slot, i as u32,
                "remap[{}] should be {} (got {})",
                i, i, *slot
            );
        }
    }

    #[test]
    fn unreached_method_pruned_while_sibling_kept() {
        // 0 stdlib + 3 user. user 0 = #main calls user 1 only;
        // user 2 sits unreferenced. user 1 must survive, user 2 must
        // drop.
        let funcs = vec![with_calls(&[1]), empty(), empty()];
        let plan = compute_plan(&funcs, 0, Some(0), &[]);
        assert_eq!(plan.user_count_after, 2);
        assert_eq!(plan.reachable_funcs, vec![0, 1]);
        assert_eq!(plan.remap[0], 0);
        assert_eq!(plan.remap[1], 1);
        assert_eq!(plan.remap[2], u32::MAX);
    }

    #[test]
    fn lambda_root_dropped_when_no_make_closure() {
        // 2 stdlib + 3 user. User 0 = #main (empty); user 1 = lambda
        // body; user 2 = another lambda body. closure_table = [1, 2].
        // No MakeClosure construction anywhere -> both lambdas dead
        // and both closure slots dropped.
        let funcs = vec![empty(), empty(), empty(), empty(), empty()];
        let plan = compute_plan(&funcs, 2, Some(2), &[1, 2]);
        assert_eq!(plan.stdlib_count_after, 0);
        assert_eq!(plan.user_count_after, 1);
        assert_eq!(plan.reachable_funcs, vec![2]);
        assert_eq!(plan.closure_slot_remap, vec![u32::MAX, u32::MAX]);
    }

    #[test]
    fn lambda_kept_when_make_closure_in_entry() {
        // 2 stdlib + 3 user. Combined layout:
        //   funcs[0..2] = stdlib bystanders
        //   funcs[2]    = user 0 = lambda body (calls stdlib 1)
        //   funcs[3]    = user 1 = unused lambda
        //   funcs[4]    = user 2 = #main, MakeClosure slot 0
        // closure_table = [0, 1] (lambda IR user indices). Entry =
        // combined index 4. Only slot 0 is constructed -> lambda 0
        // and stdlib 1 survive, lambda 1 is pruned.
        let funcs = vec![
            empty(),          // stdlib 0
            empty(),          // stdlib 1
            with_calls(&[1]), // user 0 = lambda body, calls stdlib 1
            empty(),          // user 1 = unused lambda
            FakeFn {
                body: vec![make_closure(0)],
            }, // user 2 = #main constructs slot 0
        ];
        let plan = compute_plan(&funcs, 2, Some(4), &[0, 1]);
        // Lambda 0 (combined 2) reachable through MakeClosure(0);
        // stdlib 1 reachable through lambda 0's body.
        assert_eq!(plan.stdlib_count_after, 1);
        assert_eq!(plan.reachable_funcs, vec![1, 2, 4]);
        assert_eq!(plan.remap[0], u32::MAX);
        assert_eq!(plan.remap[1], 0);
        assert_eq!(plan.remap[2], 1);
        assert_eq!(plan.remap[3], u32::MAX);
        assert_eq!(plan.remap[4], 2);
        // Closure-slot remap compacts slot 0 to 0 and drops slot 1.
        assert_eq!(plan.closure_slot_remap, vec![0, u32::MAX]);
    }

    #[test]
    fn library_module_keeps_all_user_funcs() {
        // No entry — library shape. Every user func stays reachable
        // for backward compatibility; stdlib is still pruned through
        // the BFS (no roots reach it).
        let funcs = vec![empty(), empty(), empty(), empty()];
        let plan = compute_plan(&funcs, 2, None, &[]);
        assert_eq!(plan.user_count_after, 2);
        assert_eq!(plan.stdlib_count_after, 0);
        assert_eq!(plan.reachable_funcs, vec![2, 3]);
    }
}
