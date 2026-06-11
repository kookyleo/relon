# Wasmtime 宿主策略

Relon 源文件不携带 runtime budget。CLI 执行用 `relon run` 参数；VM
执行则应在宿主 runtime 里安装限制，因为那里才是真正能 enforcement
的边界。

本页是 Wasmtime 接线模板。统一 trust 与资源边界模型见
[威胁模型](./threat-model.md)。

用策略生成器拿一份起点：

```bash
relon host-policy --target wasmtime --profile untrusted
relon host-policy --target wasmtime --profile untrusted --format rust
```

这不是 `config.relon`。输出是给运维、CI、部署脚本，或创建 Wasmtime
`Engine` / `Store` 的 Rust 宿主代码用的策略。

## Profile

| Profile | 适用场景 | Fuel | Memory | Output |
| --- | --- | ---: | ---: | ---: |
| `dev` | 本地 VM 开发和调试 | 5,000,000 | 64 MiB | 16 MiB |
| `untrusted` | 外部输入脚本，跑在 VM 边界内 | 1,000,000 | 16 MiB | 8 MiB |

Fuel 是 Wasmtime 的指令成本预算，不是 Relon tree-walker 的 step 计数。
数值刻意对齐，方便操作者形成同一套心智模型；但具体 enforcement
机制不同。

## 策略接到哪里

生成出的 Wasmtime 策略对应这些 enforcement 点：

| 策略字段 | Wasmtime / 宿主 hook | 控制什么 |
| --- | --- | --- |
| `engine.consume_fuel` + `store.fuel` | `Config::consume_fuel(true)` + `Store::set_fuel(...)` | 接近 CPU 的 wasm 指令成本 |
| `engine.epoch_interruption` + `store.epoch_deadline_ticks` | `Config::epoch_interruption(true)` + `Store::set_epoch_deadline(...)` | 挂钟中断检查点 |
| `host.wall_clock_timeout_ms` | 宿主定时任务调用 `Engine::increment_epoch()` | 真正的 elapsed-time 截止时间 |
| `store.limits.memory_size_bytes` | `StoreLimitsBuilder::memory_size(...)` + `Store::limiter(...)` | 线性内存增长 |
| `store.limits.table_elements` | `StoreLimitsBuilder::table_elements(...)` | table 增长 |
| `store.limits.instances` / `tables` / `memories` | `StoreLimitsBuilder` 资源数量 | Store 内资源创建 |
| `host.output_bytes` | 宿主序列化输出后检查 | 边界处 JSON / result 大小 |
| `host.wasi` / `host.imports` | 宿主 linker 策略 | 环境权限和 native import |

Epoch interruption 是两段式机制：打开它只是在 wasm 执行里插入检查点；
还需要宿主定时任务在截止时间后递增 engine epoch。没有这个宿主任务，
挂钟限制不会触发。

## 最小宿主形态

`relon host-policy --format rust` 会输出当前模板。关键形态如下：

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

然后保持 WASI 默认拒绝，只暴露审计过的 imports；为挂钟截止时间跑宿
主定时任务；并在序列化输出后按 `host.output_bytes` 拒绝超限结果。

## 宿主 Runner 模板

下面的模板从“你的构建流程已经产出 wasm module”开始。它刻意只是宿主侧
骨架，不是 Relon 源码配置文件，也不是新增 Relon runtime crate。

`Cargo.toml`：

```toml
[dependencies]
anyhow = "1"
serde_json = "1"
wasmtime = "45"
```

Runner 骨架：

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

    // 默认拒绝：
    // - 除非脚本确实需要 WASI，否则不要 add WASI；
    // - 不要把宿主函数 wildcard 暴露出去；
    // - 只在这里逐个添加审计过的 import。
    //
    // 如果 module 需要 compiler-runtime/libc shim 或 Relon 宿主 import，
    // 显式定义：
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

    // 边界输出检查。buffer-protocol entry 应替换成你自己的 Relon
    // result encoder/decoder。
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

对于 buffer-protocol module，保留同样的 `Engine` / `Store` / `Linker`
形态，但把 `run_i64_entry` 换成生成出的 entry signature，以及你的
embedding 使用的 verifier-backed buffer decoder。关键边界规则不变：预先
规划输入大小，限制 linear memory，拒绝环境 import，运行 epoch timer，并
检查序列化输出。

如果确实要授予 WASI，把它做成显式 profile，并记录开放了哪些目录、环境
变量、clock 和 stdio。默认 untrusted profile 应该没有 WASI。

## 和其它后端的关系

- `tree-walk`：用 `ResourceBudget` 或 `relon run --budget ...`；这是
  evaluator 侧 guardrail。
- `cranelift-aot`：CLI evaluator budget 会直接拒绝，而不是静默忽略；
  嵌入式宿主应在调用外层自己做 deadline。
- `llvm-aot` / 和宿主共同编译：把限制当成宿主代码和经过测试的基础设
  施，类似其它编译进 Rust 程序的组件。
- `wasm` / VM：依赖 runtime 边界。Wasmtime 策略生成器就是给这条路
  径准备的。

边界模型见 [威胁模型](./threat-model.md)。Capability 授权和 evaluator
侧限制见 [沙箱与权限](./sandbox)。
