# #151 — String Tier 2a: compile-time intern (stage report)

## Audit — pre-#151 const_pool state

- `crates/relon-codegen-native/src/codegen/const_pool.rs` keys
  `string_offsets` / `list_*_offsets` by IR-level `idx: u32` and skips
  duplicate idxs (`contains_key` guard). Dedup is **by idx, not by
  bytes**, so two `Op::ConstString { idx: 0, value: "a" }` /
  `{ idx: 1, value: "a" }` produced two identical `[len][bytes]`
  records.
- `crates/relon-ir/src/lowering.rs` minted those idxs from
  `LowerCtx::next_string_idx`, **reset to 0 on every `LowerCtx::new`
  / `new_method`**. Each schema-method body + the entry body had
  their own counter, so module-wide `idx` uniqueness was never
  guaranteed. The const-pool's `HashMap<idx, offset>` would silently
  collapse a method's `idx 0` onto the entry's `idx 0` — latent
  cross-func wrong-bytes bug, masked only because no shipped corpus
  exercised `Op::ConstString` from a non-entry func.

## Design + LoC

`crates/relon-ir/src/intern.rs` (new, ~200 LoC incl. doctests +
unit tests): `ConstInternTables` holds `StringInternTable` (bytes ->
idx HashMap) plus per-list-variant `next_*` counters. Lives behind
`Rc<RefCell<...>>`, allocated once per `lower_workspace_*` call,
threaded through every `LowerCtx::new` / `new_method` (entry +
schema methods + lambdas). `Expr::String` lowering swaps from
`next_string_idx += 1` to `borrow_mut().strings.intern(s)`; list-
literal lowering swaps from `next_list_*_idx += 1` to per-variant
`alloc_list_*_idx`.

Lowering diff: ~25 lines net (signatures of `lower_schema_methods` /
`lower_one_method` grew one `Rc<RefCell<...>>` param; five literal-
allocation call sites switched from local field increments to
intern-table calls; one ctor in `lower_workspace_single_with_module`
threads the shared handle).

Three new lowering-level tests cover the contract end-to-end
through analyze + lower_workspace_single:
`intern_dedups_same_literal_in_one_func`,
`intern_keeps_distinct_literals_distinct`,
`module_wide_idx_uniqueness_across_methods_and_entry`. Plus four
unit tests in `intern.rs` for the table primitives.

Gate: `cargo fmt --check`, `clippy -D warnings`, full workspace
tests (2178 passed, baseline 2171; wasm32-unknown-unknown check
green).

## W5 / W6 bench

`cargo bench --bench cmp_lua -- W5_dict_str_key/relon_trace_jit
W6_dict_num_key/relon_trace_jit --warm-up-time 1 --measurement-time
3` on the worktree (`RELON_BENCH_FORCE_RUN=1`; machine non-
quiescent — schedutil governor, load1≈3.7).

| Row                              | Baseline (4e091de) | After #151        | criterion change          |
| -------------------------------- | ------------------ | ----------------- | ------------------------- |
| W5_dict_str_key, relon_trace_jit | 131.08 µs          | ~130.78 µs        | -0.23 % (p=0.19, n.s.)    |
| W6_dict_num_key, relon_trace_jit | 33.26 µs           | ~32.99 µs         | -0.81 % (p=0.33, n.s.)    |

Both within criterion's "No change in performance detected" band.

## Honest reading of zero-delta

W5 / W6 use **hand-built recorder IR bodies** (`build_w5_recorder_body`
in `crates/relon-bench/benches/cmp_lua.rs`) with pre-built dict-key
records (`build_string_record` once per fixture, hash cached into
bytes 4..12 by #149). The keys never travel through `Op::ConstString`
lowering — they arrive as runtime pointers via
`ListGetByIntIdx` off `keys_list`. Source-level intern simply
cannot reach this path; the entire savings would need Step 3 (ptr-
cmp shortcut in `dict_inline`), which the task explicitly marked
optional follow-up.

#149 already cached the hash on the key-record header so the
`dict_inline` scan does one `load.u64` + `icmp_eq` per entry — no
memcmp, no per-iter hashing left to amortise. The expected -5~10 %
in the task brief assumed there was still a memcmp tail; there
isn't. Tier 2a as scoped (intern only) lands the
**correctness** + **dedup-by-bytes** wins (latent cross-func
collision fixed, list-literal dedup arrives "for free" via the
shared id-allocator) at zero hot-path cost — the bench rows
correctly stay flat.

Follow-up: Tier 2a Step 3 (ptr-cmp inline cache in `dict_inline`)
needs a source-driven W5-equivalent bench whose keys actually come
from `Op::ConstString` for the optimisation to be observable —
worth filing as a separate phase when a source-level dict-key
fixture replaces the hand-built recorder body.
