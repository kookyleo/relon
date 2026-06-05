# Performance & execution tiers

> This page **targets end users**: a plain-language tour of how Relon stays safe *and* fast, no compiler-theory required.
> For implementation details, see [Architecture overview](./architecture.md).

## 1. What Relon is

Relon is a **programmable configuration language**: you write a JS / Lua-style snippet that decides "can this user log in?", "should this order get a discount?", "should this log line be dropped?". The host program **executes** your snippet and gets back yes / no or a number.

Faster is better. LuaJIT is the speed champion of this class — Relon's goal is to reach the same tier **while keeping the sandbox**.

## 2. Four execution tiers (the speed ladder)

Think of an execution backend as a **restaurant kitchen**:

| Tier | What it does | Analogy | Startup | Steady-state |
|---|---|---|---|---|
| **Tree-walk** | Re-reads the source every call | Roadside diner, flips the cookbook each order | Instant | Slow |
| **Bytecode VM** | Pre-translates to an "operations list" | Casual restaurant with step-cards | ms | Medium |
| **Cranelift AOT** | Compiles to machine instructions | Central kitchen, common dishes pre-made | hundreds of μs | Fast |
| **Trace JIT** | Watches the hottest path, generates dedicated machine code for it | Fast-food line, top-selling combo has its own station | After heat-up | **Top tier** |

The further down, the faster — but the higher the prep cost. Relon **picks a tier automatically**: cold code → tree-walk (fast startup), hot code → trace JIT (fast execution).

You **don't have to choose**: the SDK entry `Backend::Auto` switches dynamically based on IR-op-count thresholds.

`Backend::Auto` is also **adaptive on capability**: if a program uses a `#main` shape the compiled (Cranelift AOT) backend can't express yet (for example a `#main() -> List<P>` return), auto transparently falls back to the tree-walk interpreter and still produces the correct result — it logs the fallback (so you know AOT acceleration was skipped for that run) rather than failing. Genuine source errors and host faults are **not** swallowed: they surface as usual.

## 3. The trace JIT trick

Trace JIT is the top tier. The principle is like **record + edit**:

1. **Observe** — run with tree-walk, attach a counter to every branch
2. **Trigger** — when a path runs 10,000 times ("hot"), mark it for promotion
3. **Record** — next time that path executes, log every operation step-by-step (this is the *trace*)
4. **Edit** — drop redundancy, fold neighbouring ops, pre-compute constants
5. **Emit native code** — compile the edited trace into CPU instructions
6. **Guard** — every recording bakes in assumptions (input is an integer, the array is long enough, …). The compiled trace inserts **guards** — if any assumption breaks at runtime, jump back to the slow path (called *deopt*) and retry from there

Compare to a short-video app's **preloading**: it learns which channel you watch most and optimises that one specifically; if you suddenly switch, it falls back to standard playback.

## 4. The sandbox — and why it's worth the cost

Relon runs user-supplied configuration, so **user code must not corrupt the host**. Every operation passes through 4 checks:

| Check | Prevents | Analogy |
|---|---|---|
| **Bounds check** | Reading / writing outside an array | No peeking over the neighbour's fence |
| **Trap** | Integer overflow / divide-by-zero must error | Dashboard red light, engine cuts out |
| **Capability gate** | Calling un-authorised host functions | Only the keys you've been issued |
| **Resource limit** | Infinite loops eating CPU / RAM | Hard timeout of 1 minute |

LuaJIT has **none of these**, which is why its 1 ns/iter is the "naked" baseline. Relon aims for the same tier *while keeping all four*. This is the core design tradeoff.

See [Sandbox & capabilities](./sandbox.md) for full details.

## 5. Where performance stands today (v6-δ M1)

Measured hot loop numbers (tight `acc += i` integer loop, lower = faster):

| Tier | Time / iter | vs LuaJIT 1-3 ns/iter |
|---|---|---|
| Tree-walk | 2245 ns | 750-2245× slower |
| Cranelift AOT (warm) | 390 ns | 130-390× slower |
| Trace JIT (warm, v6-δ M1) | **9.52 ns** | 3-9× slower |
| LuaJIT reference | 1-3 ns | baseline |

Concretely:

- Short scripts on cold start → tree-walk handles it, µs response is fine
- Steady-state moderate complexity → AOT compile once, sub-µs response
- High-frequency stream processing (millions of calls / sec) → trace JIT reaches LuaJIT trace-tier *class*, viable for ETL workloads

## 6. Cached cold start — restart doesn't recompile

Relon caches compile artefacts (ELF object files) locally; next launch `dlopen`s them directly instead of recompiling. Current **339 μs cached cold start**, on par with similar architectures.

The cache uses HMAC-SHA256 integrity tags (anti-tampering + binds to the local machine), key stored at `$XDG_DATA_HOME/relon/cache-key` mode 0600.

## 7. Why this approach is sound

LuaJIT validated the **"observe hot path → record → compile → guard back-off"** path over 20 years. Relon's differences:

- **Sandbox is mandatory** (LuaJIT skips it) → extra design cost for guard hoisting
- **Not pure JIT** (must support AOT cache + serialisable artefacts) → trace JIT sits *on top of* AOT
- **Correctness is non-negotiable** (production user code) → the deopt slow path must be 100% correct; the trace fast path only needs to be fast

A wrong fast path is fine — the slow path catches it. A wrong slow path is a bug. So the **slow path** (bytecode VM + true partial-resume) is the *foundation* of perf work — only once it's bulletproof can the fast path be aggressive.

## 8. One-sentence summary

> **Watch the hottest path; build a dedicated express lane for it; when the express lane's assumptions break, fall back to the safe lane — the safe lane is always correct.**

That's the entire recipe for getting from tree-walk to LuaJIT trace-tier class.
