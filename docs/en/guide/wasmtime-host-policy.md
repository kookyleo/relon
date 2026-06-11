# Wasmtime Host Policy

Relon source files do not carry runtime budgets. For CLI execution use
`relon run` flags; for VM execution install limits in the host runtime
where they can actually be enforced.

This page is the Wasmtime wiring template. The shared trust and
resource-boundary model is documented in [Threat model](./threat-model).

Use the policy generator as a starting point:

```bash
relon host-policy --target wasmtime --profile untrusted
relon host-policy --target wasmtime --profile untrusted --format rust
```

This is deliberately not a `config.relon` file. The output is operator
policy for CI, deployment code, or the Rust host that creates a
Wasmtime `Engine` / `Store`.

## Profiles

| Profile | Intended use | Fuel | Memory | Output |
| --- | --- | ---: | ---: | ---: |
| `dev` | Local VM development and debugging | 5,000,000 | 64 MiB | 16 MiB |
| `untrusted` | Externally supplied scripts in a VM boundary | 1,000,000 | 16 MiB | 8 MiB |

Fuel is Wasmtime's instruction-cost budget, not Relon's tree-walker
step counter. The numbers are aligned on purpose so operators get one
mental model, but the enforcement mechanisms are different.

## What the Policy Wires

The generated Wasmtime policy maps to these enforcement points:

| Policy field | Wasmtime / host hook | What it controls |
| --- | --- | --- |
| `engine.consume_fuel` + `store.fuel` | `Config::consume_fuel(true)` + `Store::set_fuel(...)` | CPU-ish wasm instruction cost |
| `engine.epoch_interruption` + `store.epoch_deadline_ticks` | `Config::epoch_interruption(true)` + `Store::set_epoch_deadline(...)` | Wall-clock interruption checkpoints |
| `host.wall_clock_timeout_ms` | Host timer/task calling `Engine::increment_epoch()` | Actual elapsed-time deadline |
| `store.limits.memory_size_bytes` | `StoreLimitsBuilder::memory_size(...)` + `Store::limiter(...)` | Linear memory growth |
| `store.limits.table_elements` | `StoreLimitsBuilder::table_elements(...)` | Table growth |
| `store.limits.instances` / `tables` / `memories` | `StoreLimitsBuilder` resource counts | Store resource creation |
| `host.output_bytes` | Host-side serialized output check | JSON/result size at the boundary |
| `host.wasi` / `host.imports` | Host linker policy | Ambient authority and native imports |

Epoch interruption is a two-part mechanism: enabling it instruments
wasm execution, but a host timer still has to increment the engine
epoch after the deadline. Without that host task, the wall-clock limit
does not fire.

## Minimal Host Shape

`relon host-policy --format rust` emits the current template. The
important shape is:

```rust
use wasmtime::{Config, Engine, Store, StoreLimits, StoreLimitsBuilder};

pub struct RelonVmState {
    limits: StoreLimits,
}

pub fn build_relon_store() -> Result<(Engine, Store<RelonVmState>), wasmtime::Error> {
    let mut config = Config::new();
    config.consume_fuel(true);
    config.epoch_interruption(true);

    let engine = Engine::new(&config)?;
    let state = RelonVmState {
        limits: StoreLimitsBuilder::new()
            .memory_size(16 * 1024 * 1024)
            .table_elements(4096)
            .instances(1)
            .tables(4)
            .memories(1)
            .trap_on_grow_failure(true)
            .build(),
    };

    let mut store = Store::new(&engine, state);
    store.limiter(|state| &mut state.limits);
    store.set_fuel(1_000_000)?;

    #[cfg(target_has_atomic = "64")]
    store.set_epoch_deadline(1);

    Ok((engine, store))
}
```

Then keep WASI denied by default, expose only audited imports, run a
host timer for the wall-clock deadline, and reject serialized output
above `host.output_bytes`.

## Host Runner Template

The template below starts after your build pipeline has produced a wasm
module. It is intentionally a host-side skeleton, not a Relon source
configuration file and not a new Relon runtime crate.

`Cargo.toml`:

```toml
[dependencies]
anyhow = "1"
serde_json = "1"
wasmtime = "45"
```

Runner skeleton:

```rust
use anyhow::{bail, Context, Result};
use std::time::Duration;
use wasmtime::{Config, Engine, Instance, Linker, Module, Store, StoreLimits, StoreLimitsBuilder};

pub struct RelonVmPolicy {
    pub fuel: u64,
    pub memory_size_bytes: usize,
    pub table_elements: u32,
    pub wall_clock_timeout_ms: u64,
    pub output_bytes: usize,
}

pub struct RelonVmState {
    limits: StoreLimits,
}

pub fn untrusted_policy() -> RelonVmPolicy {
    RelonVmPolicy {
        fuel: 1_000_000,
        memory_size_bytes: 16 * 1024 * 1024,
        table_elements: 4096,
        wall_clock_timeout_ms: 250,
        output_bytes: 8 * 1024 * 1024,
    }
}

pub fn build_engine() -> Result<Engine> {
    let mut config = Config::new();
    config.consume_fuel(true);
    config.epoch_interruption(true);
    Engine::new(&config).context("create Wasmtime engine")
}

pub fn build_store(engine: &Engine, policy: &RelonVmPolicy) -> Result<Store<RelonVmState>> {
    let state = RelonVmState {
        limits: StoreLimitsBuilder::new()
            .memory_size(policy.memory_size_bytes)
            .table_elements(policy.table_elements)
            .instances(1)
            .tables(4)
            .memories(1)
            .trap_on_grow_failure(true)
            .build(),
    };

    let mut store = Store::new(engine, state);
    store.limiter(|state| &mut state.limits);
    store.set_fuel(policy.fuel)?;

    #[cfg(target_has_atomic = "64")]
    store.set_epoch_deadline(1);

    Ok(store)
}

pub fn build_linker(engine: &Engine) -> Result<Linker<RelonVmState>> {
    let linker = Linker::new(engine);

    // Deny by default:
    // - do not add WASI unless the script is meant to have WASI;
    // - do not wildcard-export host functions;
    // - add only audited imports here, one by one.
    //
    // If the module needs compiler-runtime/libc shims or Relon host
    // imports, define them explicitly:
    //
    // linker.func_wrap("env", "__multi3", your_multi3_shim)?;
    // linker.func_wrap("env", "clock_add", your_audited_clock_add)?;

    Ok(linker)
}

fn arm_wall_clock_deadline(engine: Engine, timeout: Duration) {
    std::thread::spawn(move || {
        std::thread::sleep(timeout);
        engine.increment_epoch();
    });
}

pub fn instantiate(bytes: &[u8], policy: &RelonVmPolicy) -> Result<(Store<RelonVmState>, Instance)> {
    let engine = build_engine()?;
    let mut store = build_store(&engine, policy)?;
    let linker = build_linker(&engine)?;
    let module = Module::new(&engine, bytes).context("compile wasm module")?;

    arm_wall_clock_deadline(
        engine.clone(),
        Duration::from_millis(policy.wall_clock_timeout_ms),
    );

    let instance = linker
        .instantiate(&mut store, &module)
        .context("instantiate Relon wasm module")?;

    Ok((store, instance))
}

pub fn run_i64_entry(bytes: &[u8], export: &str, arg: i64) -> Result<i64> {
    let policy = untrusted_policy();
    let (mut store, instance) = instantiate(bytes, &policy)?;
    let entry = instance
        .get_typed_func::<i64, i64>(&mut store, export)
        .with_context(|| format!("lookup export `{export}`"))?;

    let value = entry.call(&mut store, arg).context("call Relon wasm entry")?;

    // Boundary output check. Replace this with your real Relon result
    // encoder/decoder for buffer-protocol entries.
    let encoded = serde_json::to_vec(&value)?;
    if encoded.len() > policy.output_bytes {
        bail!(
            "Relon output too large: {} bytes exceeds {}",
            encoded.len(),
            policy.output_bytes
        );
    }

    Ok(value)
}
```

For buffer-protocol modules, keep the same `Engine` / `Store` / `Linker`
shape, but replace `run_i64_entry` with the generated entry signature and
the verifier-backed buffer decoder used by your embedding. The important
boundary rules stay the same: pre-plan input size, bound linear memory,
deny ambient imports, run the epoch timer, and check serialized output.

If you intentionally grant WASI, make it an explicit profile and document
which directories, environment variables, clocks, and stdio handles are
available. The default untrusted profile should have no WASI.

## Relationship to Other Backends

- `tree-walk`: use `ResourceBudget` or `relon run --budget ...`;
  these limits are evaluator-side guardrails.
- `cranelift-aot`: CLI evaluator budgets are rejected instead of being
  silently ignored; embedding hosts should enforce their own deadlines
  around the call.
- `llvm-aot` / co-compiled host programs: treat limits as host code and
  tested infrastructure, similar to other compiled Rust components.
- `wasm` / VM: rely on the runtime boundary. The Wasmtime policy
  generator exists for this path.

For the boundary model, see [Threat model](./threat-model). For
capability grants and evaluator-side limits, see
[Sandbox & capabilities](./sandbox).
