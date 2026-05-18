# v5-γ cranelift-object cache + dlopen 设计稿

> 状态：**设计稿**（2026-05-18）。撰写时 v5-β-1 agent 正在主仓建 `crates/relon-codegen-native` + 4 项 sandbox + IR lowering 基础；β-1 cache 是简化版（IR bincode，每次重 JIT compile）。本文档定 γ phase 把 cache 升级为 cranelift-object `.o` 落盘 + dlopen 加载的接口契约 + 安全模型 + 实现 milestone。
>
> 上游：[`wasm-aot-v4-roadmap-sandbox-safe.md`](./wasm-aot-v4-roadmap-sandbox-safe.md) §v5-γ + [`v5-beta-2-stdlib-relower-plan.md`](./v5-beta-2-stdlib-relower-plan.md)。
>
> 性能目标：cached cold start **80 μs → 10 μs**（β-1 的 IR re-JIT vs γ 的 dlopen-only）。

## Section 1：cache 文件格式

### 1.1 文件名 + 路径

- 路径：`<cache_dir>/<source_hash_hex>.relon-native-v1`
- `<cache_dir>` 由 host 配置，默认 `$XDG_CACHE_HOME/relon/native-aot/`（Linux）或 `~/Library/Caches/com.relon.native-aot/`（macOS）。
- `<source_hash_hex>` 是源码 + workspace context 的 sha256。具体 hash 输入见 §1.4。
- 文件名后缀 `relon-native-v1` 是版本标签。后续 incompatible 改动 → `v2`，旧 `v1` 文件被视为 miss 不删（host 可以 GC，γ phase 不实现）。

### 1.2 文件 byte 布局

```text
offset  size       field
------  ----       -----
  0      4         magic = b"RLNC"          ("Relon Native Cache")
  4      4         version u32 LE           = 1
  8      4         target_triple_len u32 LE
 12      <var>     target_triple_str        ("x86_64-unknown-linux-gnu" / "aarch64-apple-darwin" / ...)
  ?      8         object_size u64 LE
  ?      <var>     object_bytes             (cranelift-object emit 的 ELF / Mach-O bytes)
  ?      8         metadata_size u64 LE
  ?      <var>     metadata_bincode         (bincode-serialized CacheMetadata)
  ?      32        sha256_of_above          (cover bytes [0 .. metadata end])
```

总开销：header + metadata 大约 < 1 KB；object body 范围 10 KB - 5 MB（取决于程序大小 + stdlib link 范围）。

### 1.3 `CacheMetadata` 定义（bincode-serializable）

```rust
#[derive(serde::Serialize, serde::Deserialize)]
struct CacheMetadata {
    /// codegen-native crate semver
    codegen_version: u32,

    /// cranelift crate version stamp (sha256 of resolved Cargo.lock entry)
    cranelift_version_stamp: [u8; 32],

    /// stdlib body version (bump when ANY stdlib body changes)
    stdlib_version: u32,

    /// main fn signature (params + return)
    main_signature: MainSignature,

    /// imports the object file references (host fn + stdlib fn)
    imports: Vec<ImportSpec>,

    /// capability bitmap declared in source (read_fs / net / ...)
    capabilities: u64,

    /// IR lowering layout hint (struct layouts, enum tags)
    layout_hints: LayoutFingerprint,

    /// 创建时戳 (Unix epoch seconds), 仅作 GC 用
    created_at: u64,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct ImportSpec {
    /// symbol name as emitted by cranelift-object (e.g., "relon_stdlib_upper" / "relon_host_log")
    symbol: String,
    /// expected ABI signature checksum (sha256 of canonical ABI string)
    abi_sig: [u8; 32],
    /// import kind: stdlib | host-fn | constant-table
    kind: ImportKind,
}
```

### 1.4 sha256 cache key 组成

`source_hash_hex = sha256(`
1. canonical source bytes（剥去 comments + normalize newlines + strip BOM）
2. resolved import 列表（remote 文件按 path + hash）
3. capability declaration
4. main signature canonicalized
`)`

**注意**：不是 raw source bytes 直接 hash。canonicalization 步骤要在 β-1 时已经定，γ 沿用。如果 canonical 算法变 → 加 `canonical_version` 进 hash 输入，旧 hash 自动失效。

### 1.5 Invalidation 规则（任一不匹配 → miss）

| trigger | 检查方式 |
|---|---|
| 源码改 | `source_hash_hex` 不匹配 → 文件名找不到 |
| cranelift 升级 | `cranelift_version_stamp` 比对 |
| stdlib body 改 | `stdlib_version` 比对 |
| codegen-native 改 | `codegen_version` 比对 |
| target arch / OS 改 | `target_triple_str` 比对 |
| ABI signature 改 | 任一 `imports[i].abi_sig` 比对 |
| 文件损坏 | trailing sha256 重算不一致 → 当 miss + 主动 unlink |
| 用户改 capability | `capabilities` bitmap 比对 |

任一 miss → host 回 codegen 路径重生成 → 覆盖旧文件。**γ phase 不做版本兼容性 fallback**（保持简单）；future v5-δ / v6 phase 再考虑跨 cranelift minor 版本复用。

### 1.6 完整性验证（关键，安全相关）

加载时**两次**校验：

1. **cheap check（load 时）**：读 header + metadata，比 magic / version / target_triple / metadata fields。任一不匹配 → miss。
2. **expensive check（load 时强制做）**：sha256 重算文件 bytes [0 .. metadata_end]，与 trailing 32 bytes 比对。不匹配 → 视为损坏，从 disk 删除该文件，回 codegen 路径。

`expensive check` 用 SHA-256 而非 BLAKE3 / 弱 hash 的理由：

- 抗第三方投放（攻击者无法构造 collision 跑任意代码）。
- 与 cache key 一致，复用 sha2 crate（已在依赖）。
- ~1 μs / 1 MB on modern x86_64，单次加载开销可接受（fall in 10 μs 总预算之外的 amortized 部分；详见 §5 milestone）。

## Section 2：dlopen + relocation 流程

### 2.1 整体 cold-start 步骤（cache hit 路径）

目标：cache 命中时 ~10 μs 完成 evaluator ready。

```
T0 ──► open + mmap cache file                    (~ 2 μs, syscall + page fault)
T1 ──► parse header + metadata + cheap check     (~ 1 μs, in-memory parse)
T2 ──► expensive sha256 check (object bytes)     (~ 3-5 μs, ≤ 100 KB object)
T3 ──► extract object section → memfd_create     (~ 1 μs)
T4 ──► dlopen via /proc/self/fd/<n>              (~ 2-3 μs, dynamic linker)
T5 ──► dlsym main + N stdlib + host imports      (~ 1-2 μs, hash table lookup * N)
T6 ──► capability vtable populate                (~ < 1 μs)
T7 ──► evaluator ready
```

预算合计 ~10-15 μs，落在 roadmap 预期。

如果 T2 占比过高（大 program），可改为：sha256 check 移到**写入时**保证（每次 store cache 强制写正确 hash），load 时仅 trust file metadata（POSIX mtime + size unchanged）。但**这条路是 weaker security**，γ phase **默认 strict check**，performance flag 留待 host 评估后开。**TODO（待 host 决策）：strict sha256 on each load (~5 μs hit) vs trust-on-first-write (~0.5 μs hit but no tamper detection)？默认 strict，留 flag。**

### 2.2 平台 dlopen 策略

#### Linux (β-1 + γ M2)

主路径：**memfd_create + fdlopen** —— 无临时文件，object bytes 不接触 disk。

```rust
let fd = libc::memfd_create("relon-aot\0".as_ptr() as *const _, libc::MFD_CLOEXEC);
libc::write(fd, object_bytes.as_ptr() as _, object_bytes.len());
let proc_path = format!("/proc/self/fd/{fd}");
let handle = libc::dlopen(proc_path.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL);
```

或更现代：glibc `dlmopen` + LM_ID_NEWLM 隔离 namespace，避免符号冲突（多 evaluator 共存场景）。**TODO（待 host 决策）：是否启用 LM_ID_NEWLM？优点：每个 evaluator 独立 namespace，stdlib 不共享；缺点：内存浪费（每个 evaluator 重新 link 一份 stdlib）。建议 default 共享，每 evaluator 用 unique symbol prefix（如 `relon_main_<hash>`）规避冲突。**

fallback：**临时文件 dlopen** —— 在 memfd 不可用（旧内核 < 3.17）时退化。`mkstemp` 写 + dlopen + `unlink`（unlink 后 fd 仍持有 inode）。开销 +20-50 μs。

#### macOS（后续 phase，γ 不强求）

- `NSCreateObjectFileImageFromMemory` 已弃用（macOS 12+），不可用。
- 替代：写临时 `.dylib` 文件 → `dlopen`。
- 签名问题：未签名 .dylib 在 hardened runtime / Gatekeeper 下加载会失败。需要 ad-hoc codesign（`codesign --sign - --force <file>`）— 加 ~10-20 ms 一次。
- **结论**：macOS 走 cache 后端**主线不开**；β-2 / γ phase macOS 用 JIT 路径。后续 v5-δ 单独处理。

#### Windows（deferred 单立 phase）

- PE 格式不同（cranelift-object 需 PE backend）。
- `LoadLibrary` 等价 dlopen，但 in-memory 加载需 `MemoryLoadLibrary` 第三方库 or 自实现 PE relocation。
- SEH 替 sigsetjmp 处理 trap unwind。
- **结论**：Windows γ phase **不支持**，文档明示。Roadmap v6+ 单立。

### 2.3 Symbol 绑定细节

cranelift-object emit 时，调用 stdlib / host fn 走 unresolved external symbol：

```
.text
relon_main_<hash>:
    ... 
    call relon_stdlib_upper      ; PLT entry, unresolved
    ...
    call relon_host_log          ; PLT entry, unresolved
```

dlopen 时 dynamic linker 试图 resolve 这些 symbol。两种来源：

**（a）stdlib / runtime fn**：由 Rust binary 自身 export（即 `relon-runtime` crate 用 `#[no_mangle] pub extern "C" fn relon_stdlib_upper(...)` 暴露）。dlopen 走 RTLD_GLOBAL（或 main program 默认 export）resolve 到 host 进程内的 Rust 实现。

**（b）host fn（用户注册的回调）**：由 host 在 dlopen 后用 `dlsym` 反向取出符号？**不行**：dlopen 期间符号必须 ready，否则报 `undefined symbol`。

正确做法：

1. cranelift-object 把 host fn 调用**不**走 PLT，改走**间接 call vtable**：

   ```
   relon_main_<hash>:
       movq capability_vtable@GOTPCREL(%rip), %rax
       call *vtable_offset(%rax)
   ```

2. `capability_vtable` 是个 thread-local 或 evaluator-local 结构，dlopen 后由 host 主动填充：

   ```rust
   #[no_mangle]
   pub static mut RELON_CAPABILITY_VTABLE: CapVtable = CapVtable::null();

   // dlopen 后：
   let vtable_ptr = dlsym(handle, "RELON_CAPABILITY_VTABLE\0");
   let vt = &mut *(vtable_ptr as *mut CapVtable);
   vt.host_log = user_provided_log_fn;
   vt.host_fetch = user_provided_fetch_fn;
   ```

3. 每次 host fn 调用走 vtable 验 capability 后 dispatch。这与 β-1 sandbox spec 一致。

### 2.4 Cache write 路径

```
runtime 编译完一份 cranelift module 后：
1. cranelift-object emit ELF / Mach-O bytes
2. 构建 CacheMetadata（imports / capabilities / signature 由 codegen 阶段记录）
3. 写文件按 §1.2 layout：header + object + metadata + sha256
4. 原子 rename：先写到 <name>.tmp，再 rename 到 <name>
5. fsync（可选；TODO（待 host 决策）：fsync 加 1-3 ms cold path 开销，是否值得？默认 off，崩溃时丢 cache 接受。）
```

**并发**：多个 evaluator 并行 compile 同一 source。用 `<name>.tmp.<pid>.<nonce>` 避免冲突 + rename 是 atomic。两个 producer 同时写 → 后者覆盖前者，无 corruption。

## Section 3：安全（4-prong sandbox 保留）

dlopen 加载 = 执行 unverified native code，比 wasmtime 沙箱**显著危险**。Mitigation 分层。

### 3.1 文件完整性（防外部投放）

- **strict sha256 on each load**（§1.6）防止任何篡改。
- cache 文件**只**由当前进程 produce 才接受。**TODO（待 host 决策）：加 per-installation HMAC key 吗？key 存 `$XDG_DATA_HOME/relon/cache-key`（mode 0600），cache 文件尾加 HMAC-SHA256(key, object_bytes)。这样攻击者即便能写 cache_dir 也不能投放 valid 文件。**Default 建议**加**，HMAC 计算 < 1 μs 不影响 budget。
- `from_cache_file` API 接受 path 参数时，仍跑同样验证流程（不区分 cache_dir 内文件 vs 外部 path）。

### 3.2 加载 sandbox（cranelift emit 时已嵌入）

dlopen 出的 fn 仍跑在 Rust 进程内，**但 sandbox 4 项已经在 cranelift IR 阶段 emit 进 object code**：

1. **Bounds check**：每次 memory access 前 emit `trapnz` 指令。dlopen 不剥离它们。
2. **Trap handler**：cranelift emit 的 trap 指令是 `ud2`（x86）/ `brk #0`（aarch64）等明确 ISA trap。host 进程一次性 install SIGSEGV / SIGILL / SIGFPE handler（**进程级，不 per-evaluator**）—— β-1 引入，γ 复用，**不能 per-cache load 重装**（重装会破坏其它 evaluator）。
3. **Capability vtable**：见 §2.3，host fn 调用走间接 vtable。dlopen 时 vtable 未填 → 默认 null function → 调用立即 trap → 等于"未授权"。
4. **Deadline / fuel**：cranelift emit 的 prologue 含 `cmp + brif deadline_block`，对照线程局部 deadline timestamp。dlopen 不剥。

**关键点**：β-1 在 cranelift codegen 时**必须**保证每条 sandbox 指令是**不可裁剪**的 codegen pattern（不依赖 user-provided optimizer pass）。γ 加 dlopen 时只验"cranelift-object emit 的 bytes 包含这些 pattern" —— 通过 ABI signature checksum 间接验（codegen 算 `imports[i].abi_sig` 时把 sandbox 嵌入级别写入）。

### 3.3 Trap handler 进程级安装

```rust
// 在 host 启动时（main 第一次构造 evaluator 前）：
relon_runtime::install_trap_handlers();

// install_trap_handlers 内部：
//  1. sigaction(SIGSEGV, ...)
//  2. sigaction(SIGILL, ...)
//  3. sigaction(SIGFPE, ...)
//  4. sigaction(SIGBUS, ...)
//  handler 内：siglongjmp 到 per-thread jump buffer，回到 Rust evaluator caller
```

**重要约束**：

- 只装一次（idempotent，多次调用安全）。
- 不能与 host 进程的其它 SIGSEGV handler 冲突。文档明示：host 应用如自带 SIGSEGV 处理（如 backtrace crate），必须**先**让 relon install，relon 内部 chain 到旧 handler。
- 装 handler 时保存 old `struct sigaction`，trap 时如果 PC 不在 cranelift-emit code range 内 → chain old handler。
- code range 由 dlopen 时记录 `Dl_info::dli_text_start..dli_text_end`，多 evaluator 时维护 range list。

**TODO（待 host 决策）：要不要支持 host 端"我自己接管 SIGSEGV"？如果是，relon trap handler 改 opt-in，default 不装，host responsibility 调 relon-provided check fn 决定是否走 relon longjmp。default 建议自动装。**

### 3.4 多线程 / 重入

- 每个 evaluator 有独立的 thread-local jump buffer + deadline + scratch arena。dlopen 出的 code 是 reentrant（cranelift emit 默认是 PIC）。
- dlopen handle 在 evaluator drop 时 `dlclose`。**注意**：trap handler 仍持有该 code range 引用 —— `dlclose` 后 PC 可能落入已 unmap 区域。Mitigation：trap handler 在 PC 不在已知 range 内时走 fallback path（chain old handler 或 abort）；evaluator drop 时**先**从 trap handler range list 中摘除该 range，再 dlclose。

## Section 4：平台覆盖

| 平台 | β-1 (JIT) | γ M1 | γ M2 | 后续 |
|---|---|---|---|---|
| Linux x86_64 | ✓ | ✓ (tmpfile) | ✓ (memfd) | — |
| Linux aarch64 | ✓ | ✓ (tmpfile) | ✓ (memfd) | — |
| macOS x86_64 | ✓ (JIT only) | — | — | v5-δ 评估 |
| macOS aarch64 | ✓ (JIT only) | — | — | v5-δ 评估 |
| Windows x86_64 | ✓ (JIT only) | — | — | 单立 phase |
| WASM browser | tree-walk only | — | — | — |

γ phase 主线 = Linux x86_64 + aarch64 cache，其它平台 JIT-only（cache 文件不写不读）。

## Section 5：实现 milestone

### M1 — γ 第 1 周：tempfile dlopen 通路

目标：跑通 "cranelift-object emit ELF → 临时文件 dlopen → call main" 全链路。

任务：

1. `crates/relon-codegen-native/Cargo.toml` 加 `cranelift-object` 依赖（β-1 应已有 `cranelift-jit`，γ 起平行多一个 backend）。
2. 新增 `codegen-native` 内 `ObjectModuleBuilder`，与 `JITModuleBuilder` 共享 IR lowering 路径，仅 backend trait impl 不同。
3. emit `.o` 文件到 `tempfile::NamedTempFile`。
4. dlopen + dlsym + 调 main 测试。差错 propagate 通过 trap handler（M1 用 β-1 现成的 trap handler，不改）。
5. Diff test：对 §[v6-γ trace JIT design] §4 corpus 的前 10 个用例，cranelift JIT 输出 == cranelift object dlopen 输出。

完工标志：

- HelloWorld 等价程序 cold start（含 codegen + dlopen）< 5 ms。
- main 上有 `cargo test --features native-aot-object` 通过。
- 不写 cache 文件（M2 才加）。

### M2 — γ 第 2 周：cache file + memfd path

目标：完整 cache 落盘 + load 路径 + memfd 无临时文件。

任务：

1. 按 §1 实现 `CacheStore::write(source_hash, object_bytes, metadata) -> io::Result<PathBuf>`。
2. 实现 `CacheStore::load(source_hash) -> Option<LoadedModule>`：
   - 走完整 §1.5 invalidation check
   - 走 §1.6 sha256 strict check
   - memfd_create + write + fdlopen
   - dlsym 所有 imports
   - 填 capability vtable
   - 返回 `LoadedModule { handle, main_fn, imports_resolved }`
3. tempfile fallback（memfd 不可用时）。
4. HMAC key 创建 / 加载 / 校验流程（如果 §3.1 决定加）。
5. concurrent producer atomic rename 测试。
6. cache 命中 cold start bench：目标 ≤ 15 μs（含 sha256 check），不含 sha256 check 时 ≤ 8 μs。

完工标志：

- bench `cranelift_object_cache_hit_cold_start` 上线，结果落在 roadmap 预期。
- cache miss → cache hit 二次跑能复现 15→3 倍加速。
- 安全 test：手动篡改 cache 文件 1 byte → load 返回 None 并 unlink 文件。

### 后续（M3+，γ 之外）

- macOS dlopen 路径（v5-δ phase 起头）。
- Windows PE backend（独立 phase）。
- cache GC（按 mtime + 总大小上限，可能在 v6 phase）。
- 跨 cranelift minor 版本 fallback（v6+）。

## Section 6：与其它文档的接口

- 与 [`v5-beta-2-stdlib-relower-plan.md`](./v5-beta-2-stdlib-relower-plan.md)：γ cache 是 β-2 之上的层，β-2 stdlib 必须先全 cranelift 化（其 ABI symbol = `relon_stdlib_<name>`）。
- 与 [`v6-gamma-trace-jit-design.md`](./v6-gamma-trace-jit-design.md)：trace JIT 不**写** native cache（trace specialized code per-process），但**读** generic code 仍走 γ cache。
- 与 [`wasm-aot-v4-roadmap-sandbox-safe.md`](./wasm-aot-v4-roadmap-sandbox-safe.md)：γ 是该 roadmap §v5-γ 实施。

## 附录 A：相关 cranelift 引用

| API | 用途 |
|---|---|
| `cranelift_object::ObjectBuilder` | 配置 ELF / Mach-O / PE 目标 |
| `cranelift_object::ObjectModule` | 累积 fn 定义 + data 定义 |
| `ObjectModule::finish()` | emit bytes |
| `cranelift_module::Module::declare_function` | 声明 fn signature |
| `cranelift_module::Module::declare_data` | 声明 data slot |
| `cranelift_module::Linkage::Import` | 标记 unresolved external（stdlib / host fn） |
| `cranelift_module::Linkage::Export` | 标记 host 可 dlsym 出来 |

γ M1 起手第一周大头是把 β-1 JIT path 的 `JITModule` operations rewire 到 `ObjectModule`。两者 trait 接口几乎一致（`Module` trait），主要差异在 `define_function` 完成后 JIT 是 finalize-and-execute，Object 是 emit-to-bytes。

---

**作者**：Relon perf 直路并行 prep 设计稿撰稿 agent
**日期**：2026-05-18
**License**：Apache-2
