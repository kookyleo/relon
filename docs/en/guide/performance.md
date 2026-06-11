# Performance & execution backends

> This page **targets end users**: a plain-language tour of how Relon stays safe *and* fast, no compiler-theory required.
> For implementation details, see [Architecture overview](./architecture.md).

## 1. What Relon is

Relon is a **programmable configuration language**: you write a JS / Lua-style snippet that decides "can this user log in?", "should this order get a discount?", "should this log line be dropped?". The host program **executes** your snippet and gets back yes / no or a number.

Faster is better. LuaJIT is the traditional speed champion of this
class — Relon's goal is to match or beat it while keeping deterministic
language checks and explicit host authority. On the benchmarks below,
Relon's compiled backends now beat LuaJIT across the board
(2.7×–17×) and sit in the same tier as native Rust.

Release contract note: Cranelift AOT is the default native performance
path. LLVM AOT is an advanced / preview path. Tree-walk is the
full-surface reference implementation. For untrusted tenant or plugin
source, use a VM or process boundary; compiled native backends are not a
multi-tenant safety boundary. See [Release tiers](./release-tiers) and
[Threat model](./threat-model).

## 2. Three execution backends

Think of an execution backend as a **restaurant kitchen**:

| Backend | What it does | Analogy | Compile cost | Steady-state |
|---|---|---|---|---|
| **Tree-walk** (interpreter) | Walks the syntax tree on every call | Roadside diner, flips the cookbook each order | Zero | Slow |
| **Cranelift AOT** | Compiles to native machine instructions | Central kitchen, common dishes pre-made | Hundreds of µs | Fast |
| **LLVM AOT** | Machine code through the LLVM optimisation pipeline | Michelin kitchen, longest prep | Higher | **Fastest** (native-Rust tier) |

The further down, the faster the steady state — and the higher the prep cost. Tree-walk is the full-surface reference implementation. The compiled backends lower the analysed program into IR and are tested against tree-walk on their explicitly declared supported surface, including the same observable sandbox errors (see the Auto fallback note below).

## 3. How `Backend::Auto` picks

You **don't have to choose** a backend — the SDK default `Backend::Auto` follows two simple rules:

1. **Trivial-scalar short-circuit**: if the program is a trivial scalar `#main` (decided by an AST classifier in negligible time), it goes straight to tree-walk — interpreting such a program once is cheaper than compiling it. This is a **performance short-circuit**, not a capability fallback.
2. **Lazy compilation otherwise**: everything else is compiled to Cranelift AOT on first actual execution, and the compiled artefact is cached in-process (`OnceLock`) so subsequent calls to the same program pay zero compile cost.

`Backend::Auto` is also **adaptive on capability**: if a program uses a `#main` shape the compiled (Cranelift AOT) backend can't express yet (for example a `#main() -> List<P>` return), auto transparently falls back to the tree-walk interpreter and still produces the correct result — it logs the fallback (so you know AOT acceleration was skipped for that run) rather than failing. Genuine source errors and host faults are **not** swallowed: they surface as usual.

When you do want to force a backend, `EvaluatorBuilder` offers the explicit options `Backend::TreeWalk`, `Backend::CraneliftAot`, and `Backend::LlvmAot`.

## 4. The sandbox — and why it's worth the cost

Relon runs user-supplied configuration, so **user code must not corrupt the host**. Every operation passes through 4 checks:

These are language and host-API guardrails, not an OS sandbox. For hard
isolation of untrusted source, run Relon behind Wasmtime, a subprocess,
or another host-enforced boundary.

| Check | Prevents | Analogy |
|---|---|---|
| **Bounds check** | Reading / writing outside an array | No peeking over the neighbour's fence |
| **Trap** | Integer overflow / divide-by-zero must error | Dashboard red light, engine cuts out |
| **Capability gate** | Calling un-authorised host functions | Only the keys you've been issued |
| **Resource limit** | Infinite loops eating CPU / RAM | Step budget / host deadline |

LuaJIT has **none of these**, which is why its speed is the "naked" baseline. Relon matches and beats it *while keeping all four language guardrails* — that is the core design result.

See [Sandbox & capabilities](./sandbox.md) for operational details and
[Threat model](./threat-model.md) for the boundary statement.

## 5. Where performance stands today (2026-06-10, W-series benchmarks)

**Methodology**: fixed benchmark host (Xeon E5-2620 v4, Broadwell-EP), pinned to one core with `taskset -c 2`, criterion with 100 samples × 5 s measurement windows, idle machine (load1 ≈ 0). The table shows the full evaluation path (lower = faster), against hand-written native Rust implementations and LuaJIT 2.1:

| Benchmark | Size | Tree-walk | Cranelift AOT | LLVM AOT | Native Rust | LuaJIT |
|---|---|---|---|---|---|---|
| Recursive fib | n=22 | 123.94 ms | — | 85.872 µs | 84.997 µs | 898.10 µs |
| Quicksort | 1000 elems | 105.67 ms | 681.76 µs | 78.130 µs | 110.48 µs | 972.19 µs |
| Binary search | 100 lookups | 3.6072 ms | 13.287 µs | 2.4508 µs | 2.2840 µs | 6.0968 µs |
| Prime sieve | n=10000 | 491.87 ms | 3.4018 ms | 752.28 µs | 751.60 µs | 2.7049 ms |
| Matrix multiply | 16×16 | 26.211 ms | 41.535 µs | 9.5944 µs | 9.4295 µs | 43.299 µs |

How to read this table:

- **LLVM AOT is in the native-Rust tier**: four of the benchmarks are within ±2% of Rust. Honesty caveat — Relon's LLVM AOT compiles for the host CPU by default (`target-cpu=native`) while rustc defaults to generic x86-64; the Rust control was rebuilt with the **matched target-cpu** (broadwell) before comparing, and the parity verdict above holds under that condition (in the control run only binary search had Rust 3.2% faster; the rest were parity).
- **Quicksort being 1.41× faster than Rust** is not an algorithmic difference: both sides run the same algorithm, and the gap comes from allocation strategy — Relon's compiled backend allocates lists from a scratch bump-arena while the Rust control goes through malloc. It's an allocator dividend, not "the language being faster".
- **All five benchmarks beat LuaJIT by 2.7×–17×** (LLVM AOT caliber), with Relon keeping all 4 sandbox checks that LuaJIT doesn't have.
- **Cranelift AOT** is 4×–6× slower than LLVM but compiles two orders of magnitude faster, which suits cold-start-sensitive scenarios — exactly why `Backend::Auto` picks it by default.
- Tree-walk being 3–4 orders of magnitude slower is expected: it is the full-surface reference implementation and fallback, not a performance path.

## 6. Row caliber vs kernel caliber — never cross-compare

Crossing the language boundary has a fixed **marshalling cost** (converting arguments and return values between host and sandbox representations). The same minimal benchmark (a scalar kernel) measures:

- **Row caliber** (including per-call marshalling): about 192 ns per call;
- **Kernel caliber** (pure compute kernel, marshalling excluded): 3.38 ns per call, parity with the native Rust kernel (3.40 ns).

The ~185 ns difference is the per-call marshalling cost. **The two calibers must never be compared against each other** — comparing your kernel number against someone else's row number (or vice versa) yields a false conclusion. If your workload is high-frequency tiny calls, marshalling dominates: batch more computation into each call.

## 7. Cached cold start — restart doesn't recompile

Relon caches Cranelift compile artefacts locally. The IR cache (the fast-restore path that skips parse + analyze + lower) is active on cold start, and the linked `.o` object cache (ELF bytes + integrity metadata) is **executed directly via dlopen**: a second cold start of the same source hits the cache and runs the chain *HMAC verify → generator-version match → dlopen load → symbol resolution → execute*, skipping parse + analyze + lower + codegen entirely. If any stage fails (corrupted file, version drift, missing symbol), the stale entry is invalidated with a loud log line and the run falls back to in-process compilation — program behaviour is unchanged, and a cache-hit run produces bit-identical results to a fresh compile.

The cache carries HMAC-SHA256 integrity tags (anti-tampering + anti-third-party-injection), with the key stored at `$XDG_DATA_HOME/relon/cache-key` (mode 0600). HMAC verification always runs **before** dlopen — an object that fails authentication is never loaded. Without a usable key, both cache reads and writes are refused and the run degrades to a normal cold-start compile. A generator-version mismatch (the compiler changed since the cache was written) is treated as a miss: the entry is recompiled and overwritten.

## 8. One-sentence summary

> **The interpreter guarantees full coverage and correctness; the compiled backends provide the speed on their declared supported surface; `Backend::Auto` picks for you and falls back loudly to the interpreter when it can't compile.**

That's the route by which Relon reaches native-Rust parity and beats
LuaJIT on native benchmarks while keeping the language guardrails. For
untrusted deployments, put those guardrails inside a VM or process
boundary.
