# relon-aot-bench

Measures **relon's LLVM-AOT native object** against **literal, hand-written
Rust** on a handful of arithmetic kernels.

For each workload a `.relon` `#main` is compiled at build time (via
`relon-rs-build`) into a native `.o` and linked into this binary, next to a
hand-written Rust function whose body is a *byte-for-byte algorithm match*
(same loop, same `wrapping_*` arithmetic, same branch structure). The harness
first asserts the two produce identical results, then times both with
warmup + repeated samples and reports the median nanoseconds per call.

This is **not** the criterion-style `relon-bench`. That crate measures the
interpreter / JIT tiers end to end; this one isolates a single, narrow
question — *how close does the AOT-emitted machine code get to what `rustc`
emits for the same algorithm* — and so it is kept as a separate member crate.

## Workloads

| id | shape | `.relon` kernel |
|----|-------|-----------------|
| a  | vectorizable reduce | `range(n).reduce(0, (acc,i) => acc + (i%7)*(i%13))` |
| b  | scalar dependency chain (LCG) | `range(n).reduce(7, (acc,i) => acc*C1 + C2 + i)` |
| c  | branch-dense reduce | `range(n).reduce(0, (acc,i) => i%3==0 ? ... : ...)` |
| d  | memory-bound `List<Int>` sum | `list.sum(xs)` over a 100k-element list |
| e  | host `#native` fn in the hot loop | `range(n).reduce(0, (acc,i) => acc + mix(i))` |
| f  | float reduce | `range(n).reduce(x, (acc,i) => acc + 1.5)` |
| g  | fused `map` + `reduce` | `range(n).map(...).reduce(0, (acc,x) => acc + x)` |

Workload **e** exercises a host function: `mix` is registered as an ungated
`#native` fn in `build.rs` and co-compiled closed-world (inlined into the
`.o`). The Rust side carries two baselines — one where the equivalent `mix`
is `#[inline(always)]` (matching the closed-world inline) and one forced
behind a non-inlinable, `black_box`'d call boundary (an open-world / FFI
analogue) — so the cost the closed-world inline avoids is visible.

## Running it faithfully

Requires LLVM 18 development headers with `LLVM_SYS_181_PREFIX` pointing at
them (the `build.rs` pipeline pulls `inkwell` + `llvm-sys` to emit the
native objects):

```sh
LLVM_SYS_181_PREFIX=/usr/lib/llvm-18 \
  cargo run -p relon-aot-bench --release
```

The `--release` build matters: the workspace release profile is fat-LTO +
single codegen unit, so the linked `.o` and the surrounding Rust are
optimized as one program. For stable numbers, pin to an isolated core on an
otherwise quiet machine:

```sh
taskset -c 3 cargo run -p relon-aot-bench --release
```

## Honesty caveats

- **Matched `target-cpu` is mandatory.** relon's AOT defaults to
  `-C target-cpu=native`; `rustc`'s default is the generic baseline. Comparing
  those two as-is overstates the AOT result — the gap is an ISA-feature
  artifact, not a codegen win. Only compare with *both* sides built for the
  same `target-cpu`.
- **The Rust baselines are deliberately raw.** They use plain `while` loops
  and `wrapping_*` arithmetic, not iterators / `fold`. That is the point: the
  baseline must mirror the same scalar codegen the AOT path emits, so the
  comparison stays apples-to-apples. Do not "idiomatize" them.
- **Limited signal surface.** The native-object path is the AOT supported
  subset — it does not cover where-bound recursion, first-class closures, or
  open-world `#native` dynamic dispatch. These kernels stay inside that subset
  on purpose.
- **Numbers are a point-in-time reading, not a guarantee.** Results depend on
  host CPU, toolchain version, and ambient load. Re-measure on your own
  hardware; do not treat any figure as a committed performance contract.
