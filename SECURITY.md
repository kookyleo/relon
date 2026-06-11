# Security Policy

## Supported Versions

Pre-release; all changes land on `main`. Once the first crates.io
release is cut, this section will list which minor versions receive
security fixes.

## Reporting a Vulnerability

Please report suspected vulnerabilities privately via [GitHub Security
Advisories](https://github.com/kookyleo/relon/security/advisories/new).
If that channel is unavailable, fall back to email at
`kookyleo@gmail.com`.

- Target acknowledgement: within 7 days of receipt.
- A PGP key is not required; include a minimal reproducer if possible.
- Please do not open public issues or PRs for unfixed vulnerabilities.

See "Coordinated Disclosure" below for the disclosure window.

## Threat Model & Sandbox Guarantees

Relon is an embeddable evaluator for untrusted scripts. The sandbox
boundary sits between the script (data) and the host process (Rust
code that builds a `Context` and registers native fns).

### What Relon guarantees by default

- **No ambient capabilities.** Scripts cannot read or write the
  filesystem, open sockets, read the wall / monotonic clock, read
  process environment, or consume randomness unless the host
  explicitly grants the corresponding capability bit on
  `Capabilities` (`reads_fs`, `writes_fs`, `network`, `reads_clock`,
  `reads_env`, `uses_rng`). See `crates/relon-evaluator/src/eval.rs`
  for the `Capabilities` and `NativeFnGate` definitions; the
  `missing_bits` check is the single chokepoint.
- **No self-elevation.** Capability bits are stored on the host-owned
  `Context` and consulted on every gated native-fn call. There is no
  intrinsic and no syntax that lets a script flip a bit on itself.
- **Pure stdlib.** Stdlib intrinsics are compile-time gated to be
  pure. A `purity_guard` test in `crates/relon-evaluator/src/stdlib.rs`
  greps the stdlib source for `std::fs`, `std::env`, `std::net`,
  `std::process`, `SystemTime`, `Instant::now`, `rand::`, `chrono::`,
  `tokio::`, `reqwest`, and fails the build if any appears. Any
  ambient capability must therefore arrive through a host-registered
  native fn behind a `NativeFnGate`.
- **Determinism.** Same source + same input produces byte-identical
  output, subject to host-registered native fns being themselves
  deterministic. Iteration order, dict ordering, and float formatting
  are pinned by the spec.
- **DoS budgets.** `Capabilities::max_steps` ticks once per AST
  dispatch and once per inner-loop iteration inside the 11 looping
  stdlib intrinsics (`range`; `list.map` / `filter` / `reduce` /
  `contains`; `string.split` / `replace` / `join`; `dict.merge` /
  `keys` / `values`). `Capabilities::max_value_elements` bounds
  literal allocation, comprehension output, and every `List` / `Dict`
  returned by a native fn (`range` pre-flights its requested size
  before allocating; the post-call check is a catch-all on the
  intrinsic return path — see `NativeFnCaps::max_value_elements` in
  `crates/relon-evaluator/src/native_fn.rs`).
- **Safe core, audited unsafe islands.** The parser, analyzer,
  tree-walking evaluator, facade, CLI, LSP, formatter, and browser wasm
  bindings are intended to remain safe Rust. The native codegen backends,
  object cache/loader, wasm host integration, and compact string
  representation contain explicit unsafe islands for FFI, JIT entry calls,
  raw object loading, and layout-sensitive fast paths. Treat those crates
  as part of the host trust boundary and keep changes behind focused tests
  and review.

### What is *not* guaranteed

- **Host-registered native fns are trusted code.** Every fn registered
  via `Context::register_fn` / `Context::register_method` runs with
  full host process authority once its gate bits are granted. A buggy,
  non-deterministic, or vulnerable native fn breaks isolation,
  determinism, and budget guarantees regardless of what the script
  does. Audit your registration call sites the same way you would
  audit FFI.
- **Side channels are out of scope.** Timing differences, cache
  effects, memory-pressure signals observable by co-tenants, and any
  other side channel are not mitigated. If your threat model includes
  side channels (e.g. multi-tenant evaluation with mutually
  distrusting scripts in the same process), run each tenant in a
  separate process and apply OS-level resource limits.
- **Parser stack depth.** Deeply nested source is bounded only by
  Rust's call stack; a deliberately pathological input can abort the
  thread with a stack overflow before `max_steps` fires. Hosts
  processing untrusted source should pre-flight input size and / or
  run the parse on a thread with a bounded stack.
- **`--trust` and `Capabilities::all_granted()` are explicit
  opt-outs.** The CLI flag `--trust` and the library helper
  `Capabilities::all_granted()` deliberately disable every sandbox
  bit. They are designed to be auditable at the call site — do not
  pass untrusted scripts under either.
- **Module resolution under `FilesystemModuleResolver::trusted()`.**
  The trusted resolver follows local `#import "./..."` paths without
  a root-directory check. Hosts are responsible for ensuring those
  paths are not attacker-controlled (e.g. via symlinks pointing
  outside the intended tree). For untrusted scripts, prefer
  `FilesystemModuleResolver::with_root_dir(...)`.
- **Memory accounting outside `max_value_elements`.** String length,
  closure-capture chains, and module-cache size are not currently
  metered. The `max_value_elements` budget covers `List` and `Dict`
  element counts only.

### Capability bits at a glance

| Bit | What it gates | Typical use |
| --- | --- | --- |
| `reads_fs` | Native fns that call `std::fs::read*`; also the policy bit consulted by `FilesystemModuleResolver` when resolving `#import "./..."`. | Reading config / template files; loading sibling `.relon` modules. |
| `writes_fs` | Native fns that call `std::fs::write*`, `OpenOptions::write`, `create_dir*`, `remove_*`. | Persisting derived artefacts from script output. |
| `network` | Native fns that open sockets, run HTTP clients, or perform DNS. | Calling out to a registry or remote API from a host fn. |
| `reads_clock` | Native fns that call `SystemTime::now` or `Instant::now`. | Stamping evaluation time into output; rate-limit logic. |
| `reads_env` | Native fns that call `std::env::var` / `args`. | Threading deployment context into a script. |
| `uses_rng` | Native fns that draw from a non-deterministic randomness source. | Sampling, jitter, nonce generation. |

Field semantics are defined in
`crates/relon-evaluator/src/eval.rs::Capabilities`. The struct is
`#[non_exhaustive]`; new bits may be added in a non-breaking release,
so hosts should construct via `Capabilities::default()` /
`Capabilities::all_granted()` and mutate fields rather than using
struct literals.

## Coordinated Disclosure

Please allow 7 days for acknowledgement and 90 days before public
disclosure. We may shorten the window if a fix ships sooner; we may
ask to extend it for issues that require coordinated downstream
updates. Credit will be given in release notes unless the reporter
prefers otherwise.
