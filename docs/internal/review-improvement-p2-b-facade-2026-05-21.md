# P2-B: Facade re-export shrink + EvaluatorBuilder

Date: 2026-05-21
Base: `726ff7ed74bd456a5ce78407724a2b89adad47e2`

## Re-export audit

### Before

`crates/relon/src/lib.rs` exposed three wildcard module re-exports that
leaked every public item of the downstream crates:

- `pub use relon_eval_api;`
- `pub use relon_evaluator;`
- `pub use relon_parser;`

Consumers could reach `relon::relon_evaluator::TreeWalkEvaluator`,
`relon::relon_parser::parse_document`, etc. — bypassing the typed
`from_str` / `value_from_str` entry points entirely.

### After

Three wildcards dropped. Curated typed surface stays:

- `EvaluatorBuilder`, `TrustLevel` (new, from `builder` module).
- `Backend`, `BackendError` (now with `UnsupportedFeature` variant).
- `Evaluator`, `Value`, `Scope`, `RuntimeError` re-exported flat from
  `relon-eval-api` so hosts driving a `Box<dyn Evaluator>` don't need
  a second crate dep just to spell the return type.
- `AutoEvaluator`, `Projector`, `JsonProjector`, `ResolverChainLoader`,
  `is_trivial_scalar_main*` — already curated entry points, kept.
- `relon_analyzer` re-export retained: LSP / wasm playground consume
  analyzer-only types (`AnalyzedTree`, `Diagnostic`,
  `WorkspaceDiagnostic`) with no runtime equivalent, and threading
  them through the facade would duplicate the analyzer API for no
  gain.

## EvaluatorBuilder design

```
pub struct EvaluatorBuilder { source, backend, trust, pending_fns }

impl EvaluatorBuilder {
    pub fn from_str(src: impl Into<String>) -> Self
    pub fn from_file(path: impl AsRef<Path>) -> Self
    pub fn backend(mut self, Backend) -> Self
    pub fn trust(mut self, TrustLevel) -> Self
    pub fn register_native_fn(mut self, name, gate, Arc<dyn RelonFunction>) -> Self
    pub fn register_pure_native_fn(mut self, name, Arc<dyn RelonFunction>) -> Self
    pub fn build(self) -> Result<Box<dyn Evaluator>, BackendError>
}
```

Sandboxed by default; trust posture mirrors `from_str` / `from_str_trusted`.
File reads happen at `build` time, not construction.

Backend coverage: Auto + TreeWalk dispatch host native fns;
CraneliftAot + Bytecode reject registration with `BackendError::
UnsupportedFeature` at `build` time (loud failure over silent drop).

## Upstream crate adjustments

| Crate         | Change                                                                                                            |
|---------------|-------------------------------------------------------------------------------------------------------------------|
| `relon-cli`   | Routes `Evaluator` / `Scope` / `Value` through `relon::`. Kept direct reach into `relon-evaluator` for `Context` / `TreeWalkEvaluator` / `Capabilities` / `FilesystemModuleResolver` because the `--lite` / trivial-`#main` fast paths + cache probing demand the lower-level surface the facade deliberately doesn't expose. |
| `relon-wasm`  | Routes `RuntimeError` / `Scope` / `Value` through `relon::`. Kept direct reach into `relon-evaluator::module` because the playground installs a custom in-memory `ModuleResolver` chain.                              |
| `relon-lsp`   | Routes `RuntimeError` / `Scope` through `relon::` (added `relon = { default-features = false }` to its Cargo.toml). Kept direct reach into `FilesystemModuleResolver::with_root_dir` — the LSP's root-constrained resolver chain has no facade equivalent. Heavy `relon-analyzer` reach is intentional and legitimate. |
| `relon-bench` | No change — bench multi-backend comparisons require observing backend internals (`Context`, `TreeWalkEvaluator`, IR ops, trace JIT). Direct reach is the right tool here. |

## LoC delta

```
 crates/relon/src/builder.rs       | +405 (new file)
 crates/relon/src/lib.rs           | +44 -8
 crates/relon-cli/src/main.rs      | +11 -3
 crates/relon-lsp/Cargo.toml       | +6
 crates/relon-lsp/src/workspace.rs | +10 -1
 crates/relon-wasm/src/lib.rs      | +9 -2
 Cargo.lock                        | +1
 6 files changed, 487 insertions(+), 14 deletions(-)
```

Net: +473 LoC, dominated by the new builder module (with five
integration-style unit tests covering default `eval_root`, TreeWalk
`run_main`, native-fn registration on a tree-walker build, and the
UnsupportedFeature rejection on the Bytecode backend).

## Reach kept (rationale)

- **bench**: multi-backend benchmarks need backend internals
  (`Context`, raw `TreeWalkEvaluator`, IR ops). Facade is the wrong
  granularity for what bench measures.
- **lsp**: root-constrained `FilesystemModuleResolver::with_root_dir`
  is an LSP-only resolver shape; analyzer reach (`AnalyzedTree`,
  `WorkspaceTree`, `goto_def`, `complete`) drives IDE features the
  facade doesn't model and shouldn't.
- **cli**: cold-start lite-mode shortcuts (`prepare_in_place_lite`,
  cache-probe via `CraneliftAotEvaluator::from_cache_dir`) need the
  lower-level surface. Builder would over-abstract this hot path.

## Compatibility

External consumers depending on `relon::relon_evaluator::Foo` (or any
other wildcard re-export) break. v0.x facade — acceptable per task
spec; CHANGELOG note added inline at `relon/src/lib.rs` module-doc.

## Verification

- `cargo fmt --all --check`: clean
- `cargo clippy --workspace --all-targets -- -D warnings`: clean
- `cargo test --workspace`: 2012 passed / 0 failed (2007 baseline + 5
  new builder tests)
- `cargo check --target wasm32-unknown-unknown -p relon-wasm`: clean
