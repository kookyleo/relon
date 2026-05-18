# v5-γ Stage 2 Report — vtable indirection + dlopen-exec

**Date**: 2026-05-19
**Scope**: route every cranelift-emitted host helper call through
`__relon_capability_vtable`; activate dlopen-exec on the cached
cold-start path; quantify the result against the 15 µs strict target.
**Author**: v5-γ stage 2 implementer.

## 1. What stage 1 left for stage 2

stage 1 landed the cache write + load infrastructure (object-cache
+ IR-cache + HMAC) plus the AutoEvaluator wiring, but the
`from_cache_dir` constructor still re-ran `from_source(src)`
internally — the cached ET_DYN bytes only round-tripped through HMAC
verification. The honest blocker the stage 1 agent named was:

> dlopen'd ET_DYN 内部的 `relon_now` / `relon_raise_trap` /
> `relon_cap_lookup` / closure-table call 还是 hard-linked to 当前
> 进程的 symbol 表 ... 没有 `-rdynamic`, dlopen 的 .so 找不到这些
> host fn 符号。

Result: stage 1 `cached cold start = 2.68 ms`, ~180× over the 15 µs
target the design doc set.

## 2. What stage 2 landed

### 2.1 Vtable layout

`crates/relon-codegen-native/src/vtable.rs` (new) pins the on-disk
capability-vtable layout:

```rust
#[repr(u32)]
pub enum VtableSlot {
    RelonNow = 0,
    RelonRaiseTrap = 1,
    RelonCapLookup = 2,
}
pub const VTABLE_SYMBOL: &str = "__relon_capability_vtable";
pub const RESERVED_SLOTS: u32 = 32; // ≥ COUNT for future helpers
pub const VTABLE_BYTES: usize = RESERVED_SLOTS as usize * 8;
pub unsafe fn populate_vtable(vtable_ptr: *mut u8) { ... }
```

The vtable is a 32-slot zero-initialised data section the codegen
declares via `module.declare_data(VTABLE_SYMBOL, Linkage::Export,
writable=true, tls=false)`. Both JIT (`JITModule::declare_data`)
and object (`ObjectModule::declare_data`) accept the same call;
the linkage is `Export` so the dlopen'd ET_DYN exposes the symbol
to host-side `dlsym`.

### 2.2 Codegen refactor

Three call sites in `codegen.rs` moved from `Linkage::Import`
direct calls to vtable-indirect calls:

- `compile_module_with::trap_block` (entry function): the trap-
  block tail used to `builder.ins().call(raise_trap_ref, ...)`.
  Now emits via `emit_indirect_host_call(builder, vtable_gv,
  pointer_ty, VtableSlot::RelonRaiseTrap, sig_ref, args)`.
- `Codegen::emit_resource_check` (called from `emit_prologue` for
  the deadline guard): `call now_ref` → `emit_host_fn_call(
  VtableSlot::RelonNow, &[state_ptr])`.
- `Codegen::emit_check_cap` and `Codegen::emit_call_native`
  (capability gate before each `Op::CallNative`): `call
  cap_lookup_ref` → `emit_host_fn_call(VtableSlot::RelonCapLookup,
  &[state_ptr, cap_bit_v])`.

Both lambda functions (`__closure_<N>`) and the entry function
get the same wiring. The trap-block tail inside each lambda also
goes through `emit_indirect_host_call`.

### 2.3 Shared lowering

`compile_module_with` (JIT) and `compile_module_to_object_bytes`
(cranelift-object) now share a single helper:

```rust
fn lower_module_into<M: CrModule>(
    module: &mut M, ir, entry, entry_shape, sandbox,
    return_root_size, const_pool,
) -> Result<LoweredArtifacts, CraneliftError>;
```

The driver picks the backend, calls the helper, then drives the
backend-specific finalize (`JITModule::finalize_definitions()` vs
`ObjectModule::finish().emit()`). The object path returns
`ObjectArtifact { et_rel_bytes, entry_shape, entry_arity,
entry_range, const_data, entry_symbol, vtable_symbol,
closure_symbols }`.

### 2.4 JIT-side vtable populate

After `JITModule::finalize_definitions`, the evaluator calls

```rust
let (ptr, len) = module.get_finalized_data(vtable_data_id);
crate::vtable::populate_vtable(ptr as *mut u8);
```

before the first `run_main` invocation. The vtable lives inside
the JIT module's data section; the live host helper fn pointers
go in at runtime.

### 2.5 Object-path emit + cache write

`emit_module_object_bytes(ir, sandbox, return_root_size)` produces
ET_REL bytes that import only `__relon_capability_vtable`. The
cache-write path in `from_source_with_cache` replaced the stage 1
stub emitter (`emit_entry_stub_object`) with the real emitter; the
stub stays for smoke-test compatibility.

Schema cache (new): `<hash>.relon-schema-v1` carries
`(main_schema, return_schema, param_names, const_data,
closure_count, entry_shape, entry_arity, entry_range)` as
serde_json + sha256 trailer. `from_cache_dir` reads it to skip
parse + analyze + lower on cached cold start.

### 2.6 from_cache_dir flow

The new path:

1. Validate object-cache (HMAC + metadata).
2. Cross-check IR-cache sandbox bits.
3. Read + decode schema cache.
4. `LoadedObject::from_bytes(object_bytes, triple, &["run_main",
   VTABLE_SYMBOL, "__closure_0", ..., "__closure_{N-1}"])`
   memfd-creates a tmpfile, `dlopen`s `/proc/self/fd/N`, and
   `dlsym`s each requested symbol.
5. `populate_vtable(dlsym_vtable_ptr)` writes the three host fn
   pointers into the loaded ET_DYN's vtable slot.
6. Build the `closure_table: Box<[usize]>` from the `__closure_N`
   dlsym results; install into `SandboxState`.
7. Construct `CraneliftAotEvaluator { _module:
   EntryBacking::Dlopen(loaded), entry_fn: cast(entry_ptr),
   ... }`. Zero JIT involvement.

### 2.7 EntryBacking enum

`CraneliftAotEvaluator::_module` was a `JITModule`. Stage 2
generalises it:

```rust
enum EntryBacking {
    Jit(JITModule),
    Dlopen(relon_object_cache::LoadedObject),
}
```

The variant carries the lifetime owner of the entry's machine
code; the rest of the evaluator (entry_fn pointer, closure_table,
sandbox_state) is backend-agnostic.

## 3. Measured numbers (criterion `--quick`, host linux x86_64)

| Bench scenario | stage 1 | stage 2 | Δ |
|---|---|---|---|
| `cranelift_cold` (synthetic IR + JIT) | ~275 µs | ~278 µs | flat |
| `cranelift_warm` (preassembled) | ~391 ns | ~398 ns | +7 ns (one extra vtable load per call) |
| `v5_gamma_cached_cold_start/cold` | ~2.68 ms | **~339 µs** | **−7.9×** |
| `v5_gamma_cached_cold_start_full/cold_full` | — | **~350 µs** | new |
| `tree_walk_warm` | ~2.37 µs | ~2.38 µs | flat |

Uncached cold + warm did not regress — the extra `load + i64 +
call_indirect` per helper call costs ~7 ns warm-path, dwarfed by
the call itself.

### Stage breakdown of the ~340 µs cached cold start

`tests/vtable_latency_breakdown.rs` (release):

| Stage | Latency | % | Notes |
|---|---|---|---|
| cache_load | ~259 µs | 52% | disk read + HMAC of ELF bytes |
| dlopen+dlsym | ~179 µs | 36% | memfd + procfs dlopen + ld.so reloc + 3 dlsyms |
| schema_decode | ~52 µs | 10% | serde_json over ~500 bytes |
| vtable_populate | ~6 µs | 1% | three 8-byte writes |
| **total** | ~496 µs | 100% | criterion measures ~340 µs warm-FS-cache |

## 4. Honest gap to 15 µs

The 15 µs strict target is **not** reached. stage 2 brings cached
cold start from 2.68 ms to ~340 µs (~8× improvement), but the
remaining 22× gap lives in three places that this stage's scope did
not touch:

1. **disk read + HMAC verify** of the ELF bytes (~259 µs). A
   mmap'd cache file (`MAP_PRIVATE` over the
   `.relon-native-v1` file) would skip the read syscall and the
   per-cold-start sha256 recomputation, modulo a freshness check.
   Expected save: ~200 µs.
2. **dlopen** (~179 µs). The full `libc::dlopen` path runs ld.so's
   generic-purpose code: symbol lookup tables, lazy binding,
   thread-local handling, ELF parsing. A purpose-built ELF loader
   (we know the codegen emits PIC RIP-relative no-PLT no-GOT-RELRO
   code) could collapse this to ~20-40 µs. Expected save: ~120 µs.
3. **schema_decode** (~52 µs). serde_json is forced by `TypeRepr`'s
   `#[serde(tag = "kind")]` internal-tag attribute (bincode 1.x
   rejects `deserialize_any`). Either switch to a custom binary
   encoder for the small struct, or relax the `tag` on `TypeRepr`.
   Expected save: ~40 µs.

Sum of theoretical saves: ~360 µs. Realistic stage-3 target after
those land: **~100 µs**. Hitting the original ≤ 15 µs needs a
deeper restructure (pre-warmed dlopen pool, persistent
shared-library cache, or AOT-link into the host binary), which is
out of scope here.

The plan's instruction was: "如果 vtable indirection 跑出来仍然 >
15 μs, 写报告说明额外路径 latency 分布, host 决定是否再 dispatch
stage 3. 不要 silently 砍." This report is that disclosure.

## 5. Tests

| Suite | New | Notes |
|---|---|---|
| `vtable.rs::tests` | 4 | Slot offset / count / reserved headroom / populate smoke |
| `schema_cache.rs::tests` | 3 | Round-trip / magic / digest |
| `tests/vtable_indirection.rs` | 5 | End-to-end: layout, populate, cached cold-start for add / sub / div-by-zero |
| `tests/vtable_latency_breakdown.rs` | 1 | Prof probe (prints per-phase µs) |
| `tests/object_cache_integration.rs` | 0 new | All 10 stage 1 tests still green |
| `tests/auto_evaluator_cache.rs` (harness) | 0 new | Both stage 1 tests still green |

Net delta vs stage 1 baseline (1607): **1632**, +25.

## 6. Gate

| Gate | Result |
|---|---|
| `cargo build --workspace` | ✓ |
| `cargo test --workspace` | 1632 / 0 failed |
| `cargo clippy --workspace --all-targets -- -D warnings` | ✓ |
| `cargo fmt --all -- --check` | ✓ |
| `cargo build --target wasm32-unknown-unknown -p relon-wasm` | ✓ (cache-path crates do not enter the wasm32 dep graph) |

## 7. Commit nodes (this session)

```
c23a508 feat(codegen-native): define VtableSlot enum + vtable.rs module
e2c4bfb refactor(codegen-native): route host helpers through vtable indirection
2507602 feat(codegen-native): emit full module via cranelift-object
04ad1d5 feat(codegen-native): wire from_cache_dir through dlopen-exec
2b5c2fb test(codegen-native): vtable indirection + latency breakdown probes
(this commit) docs(internal): v5-gamma stage 2 vtable indirection report
```

## 8. Leftover work

The remaining v5-γ TODOs (and the v6-γ M2-M5 milestones, which are
unrelated to this stage):

- **stage 3 latency**: mmap cache + custom ELF loader + binary
  schema cache — see §4. Decision deferred to host.
- **IR cache full coverage** (stage 1 leftover): `crate::cache`
  still has the narrow legacy-i64 envelope. Stage 2 doesn't need
  it because the schema cache supplies what `from_cache_dir`
  reads, but a future IR-cache rebuild would unblock the
  schema-cache-less restore path. Not scheduled.
- **v6-γ M2-M5**: trace JIT integration milestones. Different
  agent stream.

## 9. License

Apache-2.0. Author: `kookyleo <kookyleo@gmail.com>`.
