# Release Tiers

Relon has several execution-related crates, but the release promise is
smaller than the repository layout. Treat this page as the support
contract for the first public release.

## Tier 1: stable core

Stable core is the portable language surface:

- Parser, analyzer, type checking, strict-by-default diagnostics.
- Tree-walk evaluator as the full-surface reference implementation.
- `relon` facade APIs, including sandboxed defaults and explicit
  trusted variants.
- CLI `run`, `check`, `fmt`, and `host-policy`.
- Documentation, formatter, LSP diagnostics/completion basics, bundled `std/...`
  modules documented in [Standard library](./stdlib).

This is the surface user scripts should target by default.

## Tier 2: default native performance path

Cranelift AOT is the native performance path used by `Backend::Auto`
and `relon run --backend auto` for non-trivial `#main(...)` programs.

Promise:

- Supported shapes are checked against the tree-walk oracle.
- Unsupported shapes must fail loudly or route through an explicit Auto
  fallback message; silent miscompilation is a release blocker.
- `Backend::Auto + TrustLevel::Trusted` is not implemented in the first
  public release. Host-owned trusted imports or staged host fns should
  use `Backend::TreeWalk`; compiled performance paths should be explicit
  and should not rely on staged host fns.
- CLI evaluator budgets are not silently ignored: step/value budgets
  force tree-walk under `auto` and are rejected under explicit
  `cranelift-aot`.

Use `relon check --backend cranelift-aot path.relon` when CI must pin
a file to the compiled backend.

## Tier 3: advanced / preview

These crates are real and tested, but they are not the default first
release surface:

- LLVM AOT: opt-in through the `llvm-aot` cargo feature and LLVM 18
  host toolchain. Suitable for host-owned compiled deployments and
  ongoing AOT work, not a universal replacement for the core path.
- Rust build-time AOT (`relon-rs-*`): host-owned, closed-world
  integration path.
- Object cache/link internals: native performance infrastructure, not
  a language feature.
- Browser wasm bindings / playground: supported as product surface for
  the docs playground, but untrusted server-side VM deployment should
  follow the Wasmtime host-policy guide.

## Untrusted VM deployment

For plugin, tenant, or otherwise externally supplied source, use a VM
or process boundary. Relon defines the language-side capability and
budget model; the hard runtime controls belong to the host
infrastructure. For Wasmtime, start with:

```bash
relon host-policy --target wasmtime --profile untrusted --format rust
```

See [Threat model](./threat-model) for the boundary model and
[Wasmtime host policy](./wasmtime-host-policy) for the Wasmtime wiring
template.
