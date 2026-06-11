# Diagnostics Contract

Relon reports parse, analysis, workspace, runtime, backend, format, and
compatibility failures through human-readable CLI output backed by
`miette` diagnostics where the underlying error carries a source span.

The first public release does **not** promise machine-readable JSON
diagnostics. If that surface is added later, it will be designed as a
separate CLI option.

## CLI Exit Code

| Exit code | Meaning |
| --- | --- |
| `0` | The command succeeded. |
| non-zero | Parse, analysis, runtime, backend, format, check, or host-policy generation failed. |

The first release does not promise distinct exit codes for each error
class.

## Diagnostic Namespaces

The stable namespace shape is:

| Namespace | Source |
| --- | --- |
| `relon::parse::*` | Parser failures. Parser errors may currently be wrapped by CLI text rather than a miette code. |
| `relon::analyze::*` | Single-file semantic analysis: types, schemas, references, function calls, capability diagnostics. |
| `relon::workspace::*` | Multi-file loading and import graph diagnostics. |
| `relon::eval::*` | Runtime evaluation, resource limits, capability denial, validation, import hash, and `#main` invocation errors. |

Individual code names may grow as the language grows, but new names must
stay inside these namespaces unless a new subsystem is deliberately
introduced and documented here.

## Locations

Diagnostics should point at user source when a source span is available.
For cross-module diagnostics, the entry module reports the import site
and the diagnostic text names the imported path where relevant.

Backend compatibility errors must name the selected backend and whether
the source was rejected, routed to tree-walk by `auto`, or unsupported by
the compiled backend.

## Resource Errors

Resource-limit diagnostics carry structured values where the enforcing
backend has them:

| Error | Required detail |
| --- | --- |
| `relon::eval::step_limit_exceeded` | `limit` when the enforcing path knows it. |
| `relon::eval::value_too_large` | `limit` and `actual`. |
| CLI output-byte rejection | serialized byte count and configured limit. |
| Wasmtime fuel / epoch / memory traps | host/runtime context should map the trap into an operator-readable error. |

Compiled backends may only know that a guard trapped. In that case the
diagnostic should still identify the trap class, even if exact consumed
amount is unavailable.

## Common Failure Examples

The CLI contract tests run these fixture shapes and compare normalized
output against goldens. The examples below show the stable intent without
promising JSON output.

### Parse Error

```relon
{ a: }
```

```sh
relon check parse.relon
```

Expected class: analyzer-wrapped parse failure, with text naming
`expected expression`.

### Static Type Mismatch

```relon
{ Int port: "oops" }
```

```sh
relon check type.relon
```

Expected class: `relon::analyze::*` or analyzer text that points at
`port` and reports `expected Int, value is String`.

### Schema Validation Failure

```relon
#schema C { #expect "n positive" Int n: (Int n) -> Bool => n > 0 }
#main(C c) -> C
c
```

```sh
relon run --backend tree-walk schema.relon --args '{"c":{"n":0}}'
```

Expected class: `relon::eval::main_arg_type_mismatch`, with the `#main`
argument name, expected schema constraint, actual value, and a source span
on the argument declaration.

### Capability / Import Policy Denied

```relon
#import x from "https://example.com/a.relon"
{ y: 1 }
```

```sh
relon check remote_import.relon
```

Expected class: analyzer/workspace text explaining that remote
`#import` requires `--trust` or `Capabilities::network`. This is a
capability posture failure, not an OS sandbox claim.

### Step Limit Exceeded

```relon
#relaxed
{ loop(): loop(), x: loop() }
```

```sh
relon run --backend tree-walk --max-steps 10 steps.relon
```

Expected class: `relon::eval::step_limit_exceeded`, with a source span
near the exhausted recursive call and help text naming `max_steps`.

### Backend Unsupported

```relon
{ x: 1 }
```

```sh
relon check --backend cranelift-aot backend_unsupported.relon
```

Expected class: backend compatibility failure naming `cranelift-aot` and
the reason: Cranelift AOT requires `#main(...)`.

### Missing `#main` Argument

```relon
#main(Int x) -> Int
x
```

```sh
relon run missing_arg.relon
```

Expected class: invocation failure explaining that files declaring
`#main(...)` require `--args '<json>'` or `--args -`.

## Recovery Semantics

- Parse errors stop the current command before analysis.
- Analysis errors stop evaluation; scripts with diagnostics should not be
  run by normal hosts.
- Runtime/backend errors terminate the current invocation.
- `relon check` never evaluates the program; it only parses, analyzes,
  and reports backend compatibility.
- Formatter check failures do not rewrite files.
