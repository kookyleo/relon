# Sandbox & Capabilities

Relon's load-bearing positioning is **Logic as Data** — business
logic stored and shipped like JSON, deterministically evaluated by an
embedded runtime. That means scripts **cannot** rely on any implicit
trust from the host: FS access, network, host-registered native
functions, evaluation budget — all are **explicitly granted** by the
host.

`Capabilities` is that grant channel. The spec explicitly forbids a
"trusted-mode" bypass constructor (see [Language spec](./spec) §4.2)
— scripts must explicitly declare what they need, and the host
explicitly decides what to grant.

## What the sandbox guards against

What harm can an untrusted script do?

- **Read files**: `#import x from "/etc/passwd"` would siphon any
  file the host process can read.
- **Eval explosion**: an infinitely / exponentially recursive closure
  eats the process.
- **Oversized values**: a million-element list / dict eats memory.
- **Calling dangerous host-registered functions**: the host registers
  `secret.read`, `db.query`, but doesn't want arbitrary scripts to
  reach them.

Relon provides a capability knob for each — **all off by default**;
they take effect only when the host explicitly turns them on.

## The only constructor: `Context::sandboxed()`

For real-world use, `Context` has a single constructor —
`Context::sandboxed()`: zero privilege by default; the host must
grant explicitly to permit anything.

```rust
use relon_evaluator::module::FilesystemModuleResolver;
use relon_evaluator::{Capabilities, Context};
use std::sync::Arc;

let mut ctx = Context::sandboxed();

// 1. Grant a filesystem read root (if the script needs to #import other .relon files)
ctx.capabilities.reads_fs = true;
ctx.prepend_module_resolver(Arc::new(
    FilesystemModuleResolver::with_root_dir("/var/relon-userscripts"),
));

// 2. Set a step budget (guards against recursion / comprehension explosions)
ctx.capabilities.max_steps = Some(1_000_000);

// 3. Set a value-element water mark (guards against giant list/dict)
ctx.capabilities.max_value_elements = Some(10_000);

// 4. Grant only the bits you actually need (e.g. clock read)
ctx.capabilities.reads_clock = true;
```

### "I just want everything on" — `Capabilities::all_granted()`

For host-authored scripts (CLI, build-time, host-owned config files)
it's perfectly legitimate to access every capability. The spec
requires that **this grant be explicitly visible** — it must not hide
behind a constructor named `trusted()`. So write it out:

```rust
let mut ctx = Context::sandboxed();
ctx.capabilities = Capabilities::all_granted();
ctx.prepend_module_resolver(Arc::new(FilesystemModuleResolver::trusted()));
```

Those three lines = the old `Context::trusted()`. The difference is
that a code review can see at a glance "FS unrestricted + all six
capability bits on (any gate passes) + no step budget" — delete the
lines you don't want.

## `Capabilities` fields

The full struct (`crates/relon-evaluator/src/eval.rs::Capabilities`):

```rust
#[non_exhaustive]
pub struct Capabilities {
    pub reads_fs: bool,
    pub writes_fs: bool,
    pub network: bool,
    pub reads_clock: bool,
    pub reads_env: bool,
    pub uses_rng: bool,
    pub max_steps: Option<u64>,
    pub max_value_elements: Option<usize>,
}
```

`#[non_exhaustive]` means adding new capability bits later is not a
breaking change — don't construct it from host code with an
exhaustive struct literal; pull a baseline from
`Capabilities::default()` / `Capabilities::all_granted()` and adjust,
or end a struct literal with `..Capabilities::default()`. The same
applies to `NativeFnGate`.

Field by field:

### Six capability bits: `reads_fs` / `writes_fs` / `network` / `reads_clock` / `reads_env` / `uses_rng`

Each bit is the master switch for "is the host granting this class of
side effect?". `false` (the default) means deny; `true` means
allow. Semantics:

| Bit | Meaning | Typical side-effect sources |
| --- | --- | --- |
| `reads_fs` | File read | `#import "./local.relon"`, host-registered `fs.read`, `std::fs::read*` |
| `writes_fs` | File write | Host-registered `fs.write`, `std::fs::write*` / `OpenOptions::write` / `create_dir*` / `remove_*` |
| `network` | Network | Host-registered `http.get`, sockets, HTTP clients, DNS |
| `reads_clock` | Clock read | `SystemTime::now`, `Instant::now`, and similar non-deterministic time sources |
| `reads_env` | Process environment read | `std::env::var`, `std::env::args` |
| `uses_rng` | Non-deterministic randomness | Host-registered `rand.*`, anything calling `OsRng` / `thread_rng` |

Each bit appears in two places:

- **`Capabilities`** (host grant): `ctx.capabilities.network = true`
  means "this context allows networking".
- **`NativeFnGate`** (function declaration): when registering a
  native fn, declare "I need the `network` bit to run".

Every native-fn call goes through the same gate check: every bit the
function declares must be granted in `Capabilities`. Any missing bit
raises `RuntimeError::CapabilityDenied`, with `reason` shaped like
``"function declared `<bit>` but caller did not grant it"`` —
`<bit>` is the first missing capability name. The analyzer's static
reachability check is more aggressive: it emits one
`Diagnostic::CapabilityRequired` per missing bit (a function needing
`reads_fs + network` with neither granted produces two diagnostics).

`reads_fs` has one additional enforcement layer in
`FilesystemModuleResolver` (see below) — the bit is the policy switch
and the resolver is the actual enforcement point. Other bits have no
built-in resolver counterpart: whether a function "really" reads the
clock / sends network packets is up to the host's own native fn; the
capability layer just gates declared bits against grants.

`std/...` virtual modules do **not** consume `reads_fs` — they go
through `StdModuleResolver` rather than the filesystem, and are part
of the spec.

### No effectful language builtins

The capability bits gate only **host-registered `#native` fns**, never
language builtins: Relon has **no** effectful builtins (`clock()`,
`random()`, `read_file()`, `read_dir()`, `stat()` do not exist). The
language is a pure function — effectful values are taken by the host and
fed in as inputs. See
[the ADR](https://github.com/kookyleo/relon/blob/main/docs/internal/adr-effectful-io-builtins-2026-06-04.md) and
[Standard library → No effectful language builtins](./stdlib).

### `max_steps: Option<u64>`

The evaluator carries an internal step counter — each `eval_internal`
entry adds 1. `max_steps = Some(N)` caps it at N; exceeding it raises
`RuntimeError::StepLimitExceeded`.

```rust
ctx.capabilities.max_steps = Some(100);
// Running `loop(): loop()` (infinite recursion) gets cut off at step 101
```

`None` (default) means unlimited. Note: a step is a dispatch event,
not CPU time — one dispatch may invoke a slow built-in (a big
`string.join`, say), adding 1 step but consuming far more time. For
strict CPU control, add a wall-clock timer on the host side (see
[Things outside the sandbox's design](#things-outside-the-sandbox-s-design)).

### `max_value_elements: Option<usize>`

The `_bytes` part of the name leaves room for future extension; the
**current measurement is "Value element count"** — a list's element
count, or a dict's key/value pair count. Check points cover every
language-level entry where a list / dict is produced:

- Literal construction (`[...]` / `{ k: v }`).
- Dict `+` merge.
- Comprehensions (`[for x in xs: ...]`).
- Standard-library built-ins (`range`, `string.split`,
  `list.map` / `filter` / `reduce`, dict-`merge` method form,
  `dict.keys` / `values`, the `iter()` family, …). All these flow
  through the common exit at `call_function` /
  `try_call_native_method`, regardless of whether the call is a free
  function or `xs.method(...)` dispatch.
- `range` also **pre-checks before allocation**:
  `range(0, 10_000_000_000)` won't allocate a 10G `Vec` first and
  then trip the cap — it compares `end - start` against the cap and
  rejects immediately.

```rust
ctx.capabilities.max_value_elements = Some(3);
// `[1, 2, 3, 4, 5]` triggers ValueTooLarge { limit: 3, actual: 5 }
// `range(0, 1_000_000)` is rejected at the stdlib entry too
```

Scope: the check only inspects the **outermost** container's element
count. A `List<List<T>>` with a small outer length but huge inner
lists slips through — recursive size checking is a separate design
decision and isn't part of the current cap semantics.

Native functions the host registers via `register_fn` also pass
through this cap when they return list/dict values — the previous
documentation's "that's the host's domain" statement was the pre-fix
state; today it's unified to "any runtime-emitted list/dict goes
through the check". To opt out completely, set
`max_value_elements = None`.

Note: pure functions registered through `register_pure_fn` (`len`,
`range`, `string.*`, `math.*`, and other stdlib intrinsics) declare
an empty gate (`NativeFnGate::default()`), trivially satisfiable by
any `Capabilities` — they run even without any bit granted. Spec §4.3
classifies them as "part of the spec", not host trust decisions.

## `FilesystemModuleResolver` behavior

The real enforcement of file-read restrictions lives in the resolver,
not in `Capabilities`. Three common variants:

| Constructor | Behavior |
|---|---|
| `FilesystemModuleResolver::default()` | Rejects **all** real paths (`std/...` is unaffected because it goes through `StdModuleResolver`) |
| `FilesystemModuleResolver::with_root_dir("/path")` | Allows only files under `/path` (recursively); canonical paths must have the root as a prefix; auto-blocks `../` and symlink escapes |
| `FilesystemModuleResolver::trusted()` | Allows arbitrary paths — **only for host-owned scripts** (CLI, build-time) |

`with_root_dir`'s exact security semantics:

1. At construction, the root path is run through
   `std::fs::canonicalize` to resolve every `..` and symlink, giving
   a clean absolute path.
2. On every `#import`:
   - Join the target path to the scope's `current_dir`, then
     canonicalize again (also resolving symlinks).
   - Verify the canonical target has the root as a prefix; otherwise
     return
     `RuntimeError::CapabilityDenied { reason: "path escapes filesystem root ..." }`.

This blocks two common attacks:

- **`../` path escape**: `#import x from "../../etc/passwd"`
  canonicalizes to somewhere clearly outside the root.
- **Symlink escape**: a symlink under root pointing outside also gets
  canonicalized; the prefix check still catches it.

## Recipe: run a user script + expose a readonly fn

Putting all the building blocks together — the typical scenario: a
user submits a `.relon` snippet as a feature-flag rule; the host
provides a "read the current user ID" function; everything else is
locked down.

```rust
use relon_evaluator::module::StdModuleResolver;
use relon_evaluator::{
    Context, NativeArgs, NativeFnGate, RelonFunction, RuntimeError, Value,
};
use relon_parser::{parse_document, TokenRange};
use std::sync::Arc;

struct CurrentUserId(String);

impl RelonFunction for CurrentUserId {
    fn call(&self, _args: NativeArgs, _range: TokenRange) -> Result<Value, RuntimeError> {
        Ok(Value::String(self.0.clone()))
    }
}

fn run_user_rule(
    rule_src: &str,
    current_user: &str,
) -> Result<serde_json::Value, RuntimeError> {
    let mut ctx = Context::sandboxed();

    // Disable filesystem reads (user scripts can't #import files, only std/)
    ctx.module_resolvers = vec![Arc::new(StdModuleResolver)];

    // Evaluation budget
    ctx.capabilities.max_steps = Some(100_000);
    ctx.capabilities.max_value_elements = Some(10_000);

    // Expose a read-only, side-effect-free function. Pure fns use
    // register_pure_fn, declaring an empty gate. The sandbox's default
    // Capabilities can call it without granting any bit.
    ctx.register_pure_fn(
        "user.current_id",
        Arc::new(CurrentUserId(current_user.to_string())),
    );

    // Evaluate
    let node = parse_document(rule_src)
        .map_err(|e| RuntimeError::IoError(e.to_string()))?;
    let ctx = ctx.with_root(node);
    let scope = Arc::new(relon_evaluator::Scope::default());
    let value = relon_evaluator::Evaluator::new(&ctx).eval_root(&scope)?;

    // Project to JSON (details elided — see host-integration)
    Ok(relon::JsonProjector.project(&value).expect("…"))
}
```

In this setup, the user script:

- ✅ Can use `std/list` / `std/string` / `std/dict` and other pure-
  compute modules.
- ✅ Can call `user.current_id()` to get the current user ID.
- ❌ Can't `#import` files.
- ❌ Can't call any native fn declaring a capability bit
  (`reads_fs` / `network` / …) the host didn't grant → `CapabilityDenied`.
- ❌ Runs over 100k steps → `StepLimitExceeded`.
- ❌ Constructs a list/dict with more than 10k elements → `ValueTooLarge`.

## Error list

Runtime errors triggered by the sandbox (the relevant variants of
`RuntimeError`):

| Error | Trigger |
| --- | --- |
| `CapabilityDenied { name, reason, range }` | `#import` reaches a default-reject resolver; or `#import` path escapes the root; or a call to a native fn declaring an ungranted bit (`reason` shaped like ``"function declared `<bit>` but caller did not grant it"``) |
| `StepLimitExceeded { limit, range }` | `eval_internal` calls exceed `max_steps` |
| `ValueTooLarge { limit, actual, range }` | A single list/dict's element count exceeds `max_value_elements` |

Each carries `TokenRange` — feed it straight to miette for readable
output with source-code context.

## Things outside the sandbox's design

To avoid overestimating the guarantees, here's what Relon's sandbox
**does not** undertake:

- ❌ **CPU wall-clock limit**: Relon has no built-in wall-clock
  budget. If you need "this script runs at most 100 ms", implement
  it on the host side with `tokio::time::timeout` or a separate
  thread + timeout channel.
- ❌ **Exact heap-byte accounting**: `max_value_elements` only
  counts list/dict element counts; it doesn't count String bytes or
  closure-captured ref-counted references. For a strict in-process
  memory ceiling, layer OS-level `setrlimit` / cgroup on top.
- ❌ **Cross-process isolation**: Relon runs inside your process; if
  it crashes it takes you with it (though Rust has no segfault risk
  — RuntimeError unwinds cleanly). For strong isolation, run Relon
  in a subprocess or WASM sandbox.
- ❌ **Network / IPC isolation**: Relon itself has no network
  primitives, so by default it's network-free. But! If you
  **register** a native fn that makes network requests, that's host-
  layer territory — remember to set `NativeFnGate.network = true`
  when calling `register_fn` and to leave `Capabilities::network`
  ungranted by default, so the capability layer keeps the gate
  closed.

## Next

- The complete capability model and implementation contract:
  [Language spec](./spec).
- The full host integration flow: [Host integration](./host-integration).
