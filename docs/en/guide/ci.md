# CI Integration

Relon should enter CI as a small set of explicit command-line gates:
formatting, static/backend compatibility checks, golden output checks,
and host-runtime policy generation for untrusted VM deployments.

## Minimal Pipeline

```bash
relon fmt --check examples/*.relon
relon check --backend auto examples/validation.relon
relon run --backend tree-walk examples/validation.relon > actual.json
diff -u fixtures/golden/success/examples/validation.json actual.json
relon host-policy --target wasmtime --profile untrusted --format rust > relon_wasmtime_policy.rs
```

If your CI runs from this repository rather than an installed `relon`
binary, use the cargo form:

```bash
cargo run -q -p relon-cli -- fmt --check examples/*.relon
cargo run -q -p relon-cli -- check --backend auto examples/validation.relon
cargo run -q -p relon-cli -- run --backend tree-walk examples/validation.relon > actual.json
diff -u fixtures/golden/success/examples/validation.json actual.json
cargo run -q -p relon-cli -- host-policy --target wasmtime --profile untrusted --format rust > relon_wasmtime_policy.rs
```

## Backend Pinning

Use `auto` for ordinary source compatibility. Use explicit
`cranelift-aot` only when the file must stay on the native performance
path:

```bash
relon check --backend cranelift-aot path/to/program.relon
```

`relon check` does not run the program. It parses, analyzes, and reports
whether the selected backend can accept the source. Unsupported compiled
shapes must fail loudly or route through an explicit `auto` fallback.

## Golden Outputs

For entry programs, keep the host inputs and expected JSON together:

```bash
relon run --backend tree-walk examples/feature_flag.relon \
  --args '{"user":{"id":"alice-42","region":"eu","plan":"pro","rollout_bucket":17}}' \
  > actual.json
diff -u fixtures/golden/examples_main/feature_flag.json actual.json
```

Tree-walk is the full-surface oracle. Native backend parity belongs in
backend/fixture tests; user CI should pin `cranelift-aot` only when
native execution is part of the product contract.

## Untrusted VM Deployment

For externally supplied source, CI should also pin the host runtime
policy template:

```bash
relon host-policy --target wasmtime --profile untrusted --format rust
```

Keep the generated limits under review with your deployment code. Relon
does not infer container/process/Wasmtime limits from source files.

## Repository Gate

Inside this repository, the release gate is:

```bash
./scripts/verify.sh
npm run docs:build
```

`verify.sh` runs format checking, workspace build, clippy, workspace
tests, and fixture/example formatting.
