# Threat Model

Relon's first-release safety rule is simple: there is **no implicit
trust**. Every trust posture must be explicit at the host boundary and
auditable in review.

This page is the normative security boundary. Other pages show the API
knobs; this page says what they do and do not guarantee.

## Protected by Relon

Relon itself owns these language-level guarantees:

| Area | Guarantee |
| --- | --- |
| Determinism | No language builtin reads time, randomness, environment variables, files, network, or process state. |
| Explicit trust | `--trust`, `*_trusted`, and `TrustLevel::Trusted` are host-owned opt-ins; scripts cannot grant trust to themselves. |
| Capability vocabulary | `Capabilities` names language/host authority: `reads_fs`, `writes_fs`, `network`, `reads_clock`, `reads_env`, `uses_rng`, plus native functions gated on those bits. |
| Static capability diagnostics | The analyzer reports missing grants for statically visible gated calls. |
| Runtime capability denial | The evaluator/backends deny ungranted capability bits instead of silently calling host code. |
| Correctness traps | Divide-by-zero, numeric overflow, missing `#main` args, unsupported backend shapes, bounds errors, and validation failures surface as errors. |
| Budget integrity under concurrent reuse | Top-level tree-walk runs (`eval_root` / `run_main`) are serialized per `Context`, so a concurrent run cannot reset another run's step budget or per-run caches. |

## Not Protected by Relon Alone

Relon is not an operating-system sandbox.

| Area | Required boundary |
| --- | --- |
| Multi-tenant isolation | Use Wasmtime, another VM, a subprocess, or a container/process boundary. |
| Wall-clock deadline | Use Wasmtime epoch interruption or a host/process timeout. |
| Hard memory ceiling | Use Wasmtime `StoreLimits`, OS limits, cgroups, or a container. |
| Host import behavior | Audit and wrap each import; Relon only gates the call by capability. |
| WASI / filesystem / network ambient authority | Keep denied by default and grant only through the host runtime policy. |

## Backend Boundaries

| Backend | Intended use | Security boundary |
| --- | --- | --- |
| `tree-walk` | Reference/debug/developer execution; full language surface. | In-process guardrails only. Not a tenant boundary. |
| `cranelift-aot` | Default native performance path for supported `#main` shapes. | In-process native code with traps and capability gates. Not a tenant boundary. |
| `llvm-aot` | Advanced/preview host-owned AOT path. | Treat like Rust code linked into the host process. Resource control belongs to the host deployment. |
| wasm / Wasmtime | Recommended VM path for untrusted plugins, tenants, or uploaded scripts. | Wasmtime fuel, epoch interruption, `StoreLimits`, import/WASI policy, and host/process controls. |

## Capabilities

`Capabilities` is Relon's authority vocabulary. It says which operations
are allowed from the language/runtime point of view; it does not change
the operating system's permissions.

Examples:

- `reads_fs` lets a resolver read files only when the host also installs a
  filesystem resolver rooted where the host intends.
- A native function with a `NativeFnGate` is callable only if the required
  capability bit is granted.
- `Capabilities::all_granted()` is allowed for host-owned scripts, but it
  must be explicit and visible at the call site.

## Resource Budgets

`ResourceBudget` is Relon's standard budget model. It does not mean every
backend can enforce the same hard limit automatically.

| Budget | Enforced by |
| --- | --- |
| Source bytes | CLI/SDK preflight before read/parse where metadata is available. |
| Tree-walk steps | Tree-walk evaluator counters. |
| Value elements | Tree-walk value construction checks where implemented. |
| Output bytes | CLI/host boundary after serialization. |
| Wasm fuel | Wasmtime `Config::consume_fuel` + `Store::set_fuel`. |
| Wall-clock timeout | Host timer plus Wasmtime epoch interruption or a process timeout. |
| Memory/table limits | Wasmtime `StoreLimits` or OS/container controls. |

See [Wasmtime host policy](./wasmtime-host-policy) for the recommended
untrusted VM wiring.

### Concurrent evaluator reuse

The tree-walk step budget is a per-run limit, but the counter lives on
the shared `Context`. To keep `max_steps` enforceable, the tree-walk
backend serializes its top-level entry points (`eval_root` /
`run_main`) per `Context`:

- Concurrent top-level calls on one `Context` block until the active
  run finishes; they never interleave, so no run can zero another
  run's step accounting or clear its per-run caches (reference path
  cache, iter cursors).
- Re-entering `eval_root` / `run_main` from inside a run on the same
  thread (for example, from a native-function callback) panics with an
  explicit message instead of deadlocking. Nested work inside a run
  must use the non-resetting entry points (`eval`, `force_thunk`,
  `invoke_closure`, `NativeFnCaps::call_relon`).
- For parallel evaluation, give each thread its own `Context` and
  evaluator; a `Context` is intentionally cheap to construct per run.

## Operational Rule

For host-owned configuration, use the stable core and explicit trust
where necessary. For externally supplied or multi-tenant code, run Relon
behind a VM/process boundary and keep WASI/imports denied by default.
