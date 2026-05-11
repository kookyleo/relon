# Use Cases & Positioning

> This page helps you decide **whether to bring Relon into your
> project**. We start with the load-bearing axis, then list eight
> killer scenarios. If your need fits one of these, Relon is worth
> considering; otherwise plain JSON or an existing scripting language
> is probably a better choice.

## 1. Top-level positioning: Logic as Data

Relon's payload **is** executable logic. Its reason to exist is to let
business logic — validation rules, billing formulas, workflows, risk
policies — be **written once, stored once, shipped once**, and
**deterministically** evaluated by an embedded Relon runtime.

This is the load-bearing axis that governs every other design choice:

- Same source + same input → byte-identical output. No floating-point
  ambiguity, no hash-iteration-order leakage, no implicit influence
  from environment variables. The same `.relon` run twice always
  produces the same result — replayable, hashable, cacheable.
- Every capability that crosses a process boundary (FS, network,
  native fns) must be **explicitly declared** by the script and
  **explicitly granted** by the host. There is no "implicit trust"
  escape hatch.
- Business logic is **data**, not a binary: it lives in the database
  for hot updates, travels in RPC payloads, and can be audited via
  diff — without redeploying the service that embeds it.

## 2. Three supporting capabilities

### A. Logic-as-Data

- **Essence**: logic is not compiled into binary code; it can be
  stored and shipped like JSON.
- **Pain point**: changing a business rule means walking the full
  "edit code → release → roll out" pipeline. Rules scattered across
  services drift apart.
- **Relon advantage**: write the logic once, store it in the DB or
  config center, load and evaluate at runtime. Business-rule release
  cycles decouple from service deployment. Multiple services share
  the same `.relon` library; type contracts backed by `#schema`
  prevent the data shape from warping in transit.

### B. Capability-Granted sandbox

- **Essence**: a pure-function VM with a step counter and zero default
  I/O privileges.
- **Pain point**: SaaS platforms need to let users author custom
  rules. Running JS is risky; modeling logic as JSON is too clunky.
- **Relon advantage**: sandboxed by default and very lightweight.
  Step counts, value sizes, and native-fn allowlists are all
  controlled explicitly through `Capabilities`. Scripts have **no**
  fallback to bypass the sandbox.

See [Sandbox & capabilities](./sandbox) for details.

### C. Context-aware references

- **Essence**: semantic paths like `&prev`, `&parent`, `&root` give
  data "gravity".
- **Pain point**: in deeply nested configs where fields depend on
  neighbors, manually maintaining indices and paths in JS is
  error-prone.
- **Relon advantage**: data carries relative references natively.
  Wherever a fragment is moved, it finds its context and recomputes
  (think Excel formulas). References are declarative; evaluation
  stays deterministic.

## 3. Eight killer scenarios

| # | Scenario | One-line summary |
|---|---|---|
| 1 | **Template (amplifier)** | Backend ships minimal data; Relon renders complex UI descriptions |
| 2 | **Validation (I/O offload)** | Strong contract validation guarantees incoming data is internally consistent |
| 3 | **VM (SaaS plugin)** | Zero-risk execution of tenant-defined rules |
| 4 | **Game (design-team agility)** | Designers describe polymorphic level behaviors as Relon fragments |
| 5 | **Sequential (self-healing pipelines)** | Video editing / job scheduling — delete an intermediate item and downstream timing auto-aligns |
| 6 | **Hot-update (financial risk)** | Risk formulas live in the config center; edit one line and it takes effect without restarting |
| 7 | **Reactive (self-computing assets)** | Nested balance-sheet hierarchies recompute automatically |
| 8 | **Edge (edge policy)** | IoT nodes update control policies on the fly |

## 4. Benchmark insights

Stress-testing has validated Relon's profile as an embedded language:

### A. Performance: microsecond response and functional acceleration

- **Simple eval**: ~12.7 μs (warm state) — 80k requests per second.
- **Heavy logic**: 1,000-iteration list computation in ~1.3 ms.
- **Architecture upgrade**: a Core/Std split. Core (Rust) provides
  high-performance primitives (`_list_map`, `_list_reduce`, …); Std
  (Relon) provides elegant business wrappers. Native code can call
  Relon closures back, giving first-class functional support.

### B. Footprint: under the megabyte threshold

After tuning IR-level optimization (LTO + Opt-Z + Panic-Abort):

- **Core library**: ~589 KB (>50% reduction).
- **Full CLI**: ~898 KB (first time under 1 MB).
- **WASM projection**: stripping CLI deps should drop us to ~250 KB,
  on the order of Lua.

> Note: actual WASM size pending CI landing.

## 5. Bottom line

Relon balances **high performance**, **functional expressiveness**,
and **sub-megabyte footprint**. It targets scenarios where you need
to manage business logic as data and require auditable, deterministic
execution. If a static config or your host's native scripting suffices,
plain JSON / Lua / JS is the better fit.
