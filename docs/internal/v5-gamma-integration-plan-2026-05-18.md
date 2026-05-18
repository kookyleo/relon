# v5-γ Cranelift-Object Cache Integration Plan

Author: kookyleo <kookyleo@gmail.com>
Date: 2026-05-18
Status: 草案 v1，整合 v5-γ phase 剩余工作
Supersedes (high-level only): `docs/internal/v5-gamma-cranelift-object-cache-design.md` §5 milestone
Companion: `docs/internal/v6-gamma-integration-plan-2026-05-18.md`（同期 prep agent 风格参考）

---

## 0. 背景

`v5-gamma-cranelift-object-cache-design.md` 是 prep 阶段产出，写在
`relon-object-cache` / `relon-object-link` 任何代码落地之前。如今两个独立 crate
都已合进 main，且 `relon-codegen-native` 已经在 v5-β 系列里全链路通了
parse → analyze → IR lower → cranelift codegen → JIT。v5-γ phase 剩下的工作
就是把这三段拼起来：

| Crate | Tests | 关键模块 |
| --- | ---: | --- |
| `relon-object-cache` | 28 | `storage`（RLNC 文件格式 + atomic rename）/ `hmac`（per-installation key + 0600）/ `integrity`（Strict vs TrustOnWrite）/ `loader`（memfd + /proc/self/fd dlopen）/ `error` |
| `relon-object-link` | 20 | `elf_check`（手卷 ELF header parser）/ `linker_subproc`（默认走 `ld -shared`）/ optional `linker_lld`（feature `lld-inproc`，目前 stub）/ `error` |
| `relon-codegen-native` | 既有 v5-β 测试 | `evaluator::CraneliftAotEvaluator::{from_source, from_cache}` 已存在但 cache 是 v5-β-1 简化版（IR bincode + 重 JIT） |

本文档 supersede 设计稿 §5 的 M1-M2 估算（原"γ 第 1 周 + γ 第 2 周"，~2 周
体感），把已落地两个 crate 的真实 API 形状映射到剩余整合工作，**给出
~5 天可拆 dispatch 的 M1-M3**。

---

## 1. 当前状态 vs 目标

| 路径 | 当前（post v5-β-2 stage 4） | 目标（post v5-γ） |
| --- | ---: | ---: |
| Cold (uncached) — parse + analyze + IR lower + cranelift codegen + JIT finalize | 245-275 μs | ~120-150 μs（cranelift compile 仍占大头，但 IR bincode 路径让位给 object emit；总时长不退步） |
| Cold (cached) — 当前是 IR bincode 反序列化 + 重 JIT | ~80 μs | **~10-15 μs（mmap + HMAC verify + memfd_create + dlopen + dlsym）** |
| Warm invoke（per-call dispatch） | 415 ns | unchanged |

**cached cold start 5-8× 提升是 γ phase 唯一关键指标**。其余两条不能退步，
warm path 完全不动（dlopen 出来的 fn pointer 跟 JIT finalize 出来的形状一致，
都走同一个 `EntryPtr::Buffer / Legacy` 分支）。

未达成时的 fallback 路径：每一项都要降级到 v5-β-1 的 IR bincode + 重 JIT，
不允许直接 fail evaluator（详见 §5 风险与缓解）。

---

## 2. 已落地两 crate 的 API surface 速查

下面的清单来自 `relon-object-cache/src/lib.rs` + `relon-object-link/src/lib.rs`
的 `pub use`，**不是猜测**。每条 50-100 字符简介，整合阶段查询用。

### 2.1 `relon-object-cache`

**`storage` 模块**（cache 文件格式 + 落盘 / 加载）：

- `pub const CACHE_MAGIC: [u8; 4] = *b"RLNC"` — 文件首 4 字节。
- `pub const CACHE_VERSION: u32 = 1` — 当前 layout 版本；任何变更要 bump，旧文件以 `CacheError::VersionMismatch` 拒绝。
- `pub const HMAC_TAG_LEN: usize = 32` — trailing HMAC-SHA256 tag 长度（即便 HMAC 关也保留零填充）。
- `pub const CACHE_FILE_SUFFIX: &str = ".relon-native-v1"` — 文件名后缀。
- `struct HostFnImport { name: String, cap_bit: u32, params_hash: [u8;32], returns_hash: [u8;32] }` — 单条 host fn 导入 metadata（symbol + 能力位 + ABI sig 指纹）。
- `struct SignatureHash(pub [u8; 32])` — `#main` 签名的不透明 32 字节摘要。
- `struct Metadata { host_fn_imports, cap_bitmap: u64, main_signature, created_at_unix, generator_version: String }` — bincode-serialize 进 trailer。
- `struct CacheEntry { target_triple: String, object_bytes: Vec<u8>, metadata: Metadata }` — load 返回的内存视图（object bytes 已 eager-copy）。
- `pub fn cache_path_for(cache_dir: &Path, source_sha256: [u8; 32]) -> PathBuf` — 文件名 = `<sha_hex>.relon-native-v1`。
- `pub fn store(cache_dir, source_sha256, target_triple, object_bytes, metadata, hmac_key: Option<&[u8;32]>) -> Result<PathBuf, CacheError>` — 原子 rename 写入；temp 文件名带 PID + 纳秒 nonce 解决并发冲突。
- `pub fn load(cache_dir, source_sha256, expected_triple, hmac_key, integrity: IntegrityMode) -> Result<Option<CacheEntry>, CacheError>` — `Ok(None)` 表示文件不在；其余错误（HMAC / SHA-256 / 截断 / triple 不匹配）以 typed error 返回，caller 应当 log 后回退到 from_source 重生成。

**`integrity` 模块**：

- `enum IntegrityMode { Strict (default), TrustOnWrite }` — Strict 每次 load 重算 SHA-256（~1 μs/MB）；TrustOnWrite 跳过，与 HMAC 配合使用时仍安全。

**`hmac` 模块**（per-installation key）：

- `pub const KEY_LEN: usize = 32` — 密钥长度。
- `pub fn hmac_key_path() -> PathBuf` — `$XDG_DATA_HOME/relon/cache-key` 或 `$HOME/.local/share/relon/cache-key`，最后 fallback 到 cwd。
- `pub fn ensure_key() -> Result<[u8; 32], HmacError>` — 不在就创建（mode `0600`），错的 size / mode 一律拒绝。
- `pub fn compute_hmac(bytes, key) -> [u8;32]` / `pub fn verify_hmac(bytes, key, expected) -> bool` — HMAC-SHA256 计算 / 常时间比较。

**`loader` 模块**（Linux memfd dlopen）：

- `struct ObjectHandle` — 持 memfd + dlopen handle；drop 时先 `dlclose` 再 `close`。`Send + Sync`。
- `struct LoadedObject { fn_pointers: HashMap<String, *const u8>, _handle: ObjectHandle }` — 解析后的 symbol 表 + handle 一体。
- `LoadedObject::from_bytes(object_bytes, target_triple, expected_symbols: &[&str]) -> Result<Self, LoaderError>` — 流程：`memfd_create(MFD_CLOEXEC)` → `write` → `dlopen("/proc/self/fd/<n>", RTLD_NOW|RTLD_LOCAL)` → `dlsym(*expected_symbols)` 全部成功才返回。非 Linux 返回 `LoaderError::UnsupportedPlatform`。
- `LoadedObject::resolve(name) -> Option<*const u8>` — 查 expected_symbols 内的指针；不在表内一律 None（防 typo）。
- `LoadedObject::iter_symbols()` — 调试用枚举。

**`error` 模块**：

- `enum CacheError { Io, MagicMismatch, VersionMismatch{file, runtime}, TripleMismatch{file, runtime}, HmacMismatch, Sha256Mismatch, Metadata(String), Truncated(usize) }`。
- `enum LoaderError { Memfd, Write, Dlopen(String), SymbolNotFound(String), UnsupportedPlatform }`。
- `enum HmacError { Io, Random(String), BadSize(usize), InsecureMode(u32) }`。

### 2.2 `relon-object-link`

**顶层入口**：

- `pub fn link_to_dyn(et_rel_bytes: &[u8], target_triple: &str) -> Result<Vec<u8>, LinkError>` — 默认用 `SubprocLinker::new()?` 做 `ET_REL → ET_DYN`。
- `pub fn parse_elf_type(bytes) -> Result<ElfType, LinkError>` / `pub fn is_et_rel(bytes) -> bool` / `pub fn is_et_dyn(bytes) -> bool` — 手卷 64-bit ELF header parse。
- `enum ElfType { Rel, Dyn, Exec, Other }` — `e_type` 字段映射。

**`linker_subproc` 模块**（默认 backend）：

- `struct SubprocLinker { ld_path: PathBuf, is_cc_frontend: bool, extra_flags: Vec<String> }`。
- `SubprocLinker::new()` — 解析 ld：`$RELON_LD` → `/usr/bin/ld` → `$PATH/ld` → `$PATH/cc`，全失败回 `LinkError::LinkerNotFound`。
- `SubprocLinker::link(et_rel, target_triple) -> Result<Vec<u8>, LinkError>` — `-shared -z noexecstack`（cc-driver 加 `-nostdlib -Wl,-z,noexecstack`），通过 tempfile.NamedTempFile 物化两端 IO（OS temp 一般在 tmpfs）。仅 `x86_64-*-linux-*` 接受。
- `SubprocLinker::with_extra_flag(flag)` — 测试用钩子（如 `--build-id`）。
- `SubprocLinker::from_path_for_tests(path)` — 注入任意 binary 用于负路径测试。

**可选 `linker_lld`**（feature `lld-inproc`）：

- `struct LldLinker` / `LldLinker::link` — 目前 stub，返回 `LinkError::FeatureNotImplemented`。整合阶段**不依赖**这条路径。

**`error` 模块**：

- `enum LinkError { InvalidElf(String), NotEtRel(ElfType), NotEtDyn(ElfType), LinkerNotFound, LinkerFailed(String), Io, UnsupportedTriple(String), FeatureNotImplemented }`。

---

## 3. `from_source` 整合

### 3.1 现状

`CraneliftAotEvaluator::from_source(src: &str) -> Result<Self, CraneliftError>`
（`crates/relon-codegen-native/src/evaluator.rs:127`）当前流程：

```
parse → analyze → IR lower (lower_workspace_single)
      → buffer-schema 提取（main_schema / return_schema / layouts）
      → from_ir_inner: codegen::compile_module_with → CompiledModule
      → JITModule::get_finalized_function → EntryPtr::{Legacy, Buffer}
      → 返回 CraneliftAotEvaluator
```

`from_cache(entry: CacheEntry)` 走的是简化路径：直接拿
`entry.ir + entry.sandbox`，跳过 parse / analyze / lower，但**仍重跑
cranelift codegen + JIT finalize**。

### 3.2 v5-γ 后

新增 `from_source_with_cache(src, cache_dir) -> Result<Self, ...>`，
在既有 `from_source` 之上**串两个 crate**：codegen 完成后把 ELF bytes
经过 `relon-object-link` 转 `ET_DYN`，再用 `relon-object-cache::store`
落盘。当前 invoke 仍跑 in-memory JIT（不做 dlopen 切换），保证不破坏
warm path。

```
parse → analyze → IR lower → cranelift codegen
      ├─ JIT finalize (in-mem)     → 当前 invoke 路径（不动）
      └─ ObjectModule::finish().emit() (ET_REL bytes)
              │
              ▼
        relon_object_link::link_to_dyn(et_rel, TARGET_TRIPLE)  → ET_DYN bytes
              │
              ▼
        sha256(canonical src + caps + sig) → cache_key
              │
              ▼
        relon_object_cache::store(cache_dir, key, dyn_bytes, metadata, hmac_key)
              │  ← best-effort：失败仅 log warn，不影响 evaluator 返回
              ▼
        CraneliftAotEvaluator 仍由 in-mem JIT 构造（cache 是 next-time-fast）
```

伪代码（~30 行）：

```rust
pub fn from_source_with_cache(
    source: &str,
    cache_dir: &Path,
) -> Result<Self, CraneliftError> {
    // 1. 既有 parse + analyze + IR lower + buffer schema 提取
    let (ir, main_schema, return_schema) = Self::lower_source(source)?;
    let metadata = build_cache_metadata(&ir, &main_schema, &return_schema);

    // 2. cranelift codegen 双输出：JIT module 给当前 invoke；ObjectModule 给 cache
    let compiled = codegen::compile_module_with(&ir, &SandboxConfig::default(), root_size)?;
    let et_rel_bytes = match codegen::compile_module_to_object(&ir, &SandboxConfig::default(), root_size) {
        Ok(bytes) => bytes,
        Err(e) => {
            tracing::warn!(error = ?e, "object emit failed, cache skipped this run");
            return Self::from_compiled(compiled, main_schema, return_schema);
        }
    };

    // 3. ET_REL → ET_DYN（subproc ld -shared）
    let dyn_bytes = match relon_object_link::link_to_dyn(&et_rel_bytes, TARGET_TRIPLE) {
        Ok(b) => Some(b),
        Err(e) => {
            tracing::warn!(error = ?e, "ld link failed, cache skipped this run");
            None
        }
    };

    // 4. cache key = sha256(canonical IR + caps + sig)。canonical 算法
    //    复用 v5-β-1 既有 helper（cache::source_sha256_for_ir）。
    let cache_key = canonical_source_sha256(source, &metadata);

    // 5. store cache：best-effort，失败 warn 即可
    if let Some(dyn_bytes) = dyn_bytes {
        let hmac_key = relon_object_cache::ensure_key().ok();
        match relon_object_cache::store(
            cache_dir,
            cache_key,
            TARGET_TRIPLE,
            &dyn_bytes,
            &storage_metadata_from(&metadata),
            hmac_key.as_ref(),
        ) {
            Ok(path) => tracing::debug!(?path, "v5-γ cache stored"),
            Err(e) => tracing::warn!(error = ?e, "cache store failed"),
        }
    }

    // 6. 当前 invoke 仍跑 in-mem JIT
    Self::from_compiled(compiled, main_schema, return_schema)
}
```

补充：

- `codegen::compile_module_to_object` 是 v5-γ M1 在
  `crates/relon-codegen-native/src/codegen.rs` 内新增的并行入口。**与既有
  `compile_module_with` 共享 IR lowering 路径**，仅 `Module` trait 的
  backend 不同：JIT → `JITModule`；γ → `cranelift_object::ObjectModule`，
  最后调 `module.finish().emit()` 拿 `Vec<u8>` ET_REL bytes。
  设计稿 §5 M1 描述的就是这个差异（"Module trait 接口几乎一致，差异在
  finalize 时 JIT 是 finalize-and-execute，Object 是 emit-to-bytes"）。
- `TARGET_TRIPLE` 暂时硬编码 `"x86_64-unknown-linux-gnu"`，由
  `cfg(target_arch / target_os)` 在编译期决定；非 Linux x86_64 走分支
  直接跳过 cache（同设计稿 §4 平台表）。

### 3.3 cache key canonicalization

`canonical_source_sha256(source, &metadata)` 的输入：

1. canonical source bytes — 剥注释 + normalize newlines + strip BOM。
   v5-β-1 已有 `cache::canonicalize_source` helper（若不存在则 M1 顺手加）。
2. `metadata.cap_bitmap` little-endian 8 字节。
3. `metadata.main_signature.0`（32 字节）。
4. `metadata.host_fn_imports` 按 `name` 字典序排序后逐条
   `name_len_u32_le | name_bytes | cap_bit_le | params_hash | returns_hash`。
5. `metadata.generator_version` `len_u32_le | bytes`。
6. `TARGET_TRIPLE` `len_u32_le | bytes`。

把上面 6 段拼成一条 `Vec<u8>` 再 `Sha256::digest`，得到 `[u8; 32]`。
**注意**：raw source bytes 不直接 hash —— 仅注释 / 空白差异不应该让
cache miss。

---

## 4. `from_cache` 整合

### 4.1 新签名

```rust
pub fn from_cache_v2(
    source_hash: [u8; 32],
    cache_dir: &Path,
    integrity: IntegrityMode,
) -> Result<Option<Self>, CraneliftError> { ... }
```

返回 `Ok(None)` 表示 cache miss（caller 应回退到 `from_source_with_cache`）。
错误（HMAC 失败 / SHA-256 不匹配 / metadata 不兼容 / dlopen 失败）一律
typed `CraneliftError`，caller log + 回退即可。

旧 `from_cache(entry: CacheEntry)` 保留，作为 IR bincode 路径的 fallback
（cache 文件损坏或 ld 缺失时仍能跑），**不删**。

### 4.2 实现 sketch

```rust
pub fn from_cache_v2(
    source_hash: [u8; 32],
    cache_dir: &Path,
    integrity: IntegrityMode,
) -> Result<Option<Self>, CraneliftError> {
    // 1. HMAC key 优先尝试加载；失败时降级为 no-HMAC（设计稿 §3.1 允许）。
    let hmac_key = match relon_object_cache::ensure_key() {
        Ok(k) => Some(k),
        Err(e) => {
            tracing::warn!(error = ?e, "hmac key unavailable, cache loaded without auth");
            None
        }
    };

    // 2. relon_object_cache::load
    let entry = match relon_object_cache::load(
        cache_dir,
        source_hash,
        TARGET_TRIPLE,
        hmac_key.as_ref(),
        integrity,
    ) {
        Ok(Some(e)) => e,
        Ok(None) => return Ok(None),                              // miss
        Err(CacheError::HmacMismatch) | Err(CacheError::Sha256Mismatch) => {
            // 损坏 / 篡改 → 主动 unlink，让 caller 回 from_source 重生成
            let path = relon_object_cache::storage::cache_path_for(cache_dir, source_hash);
            let _ = std::fs::remove_file(&path);
            tracing::warn!(?path, "cache corrupt, removed");
            return Ok(None);
        }
        Err(e) => return Err(CraneliftError::Cache(e.to_string())),
    };

    // 3. metadata 兼容性 — host_fn_imports 必须 ⊆ 当前注册的 host fn 表，
    //    cap_bitmap 必须 ⊆ 当前进程允许的能力位。任一不匹配 → miss
    //    （视为版本漂移，caller 重新生成 cache）。
    if !validate_metadata(&entry.metadata) {
        tracing::info!("cache metadata incompatible with current runtime, regenerating");
        return Ok(None);
    }

    // 4. LoadedObject::from_bytes —— memfd + dlopen + dlsym
    let expected = &["relon_main_entry", "__relon_capability_vtable"];
    let loaded = match relon_object_cache::LoadedObject::from_bytes(
        &entry.object_bytes,
        &entry.target_triple,
        expected,
    ) {
        Ok(l) => l,
        Err(LoaderError::UnsupportedPlatform) => return Ok(None),
        Err(e) => return Err(CraneliftError::Cache(format!("dlopen: {e}"))),
    };

    // 5. 解析 fn pointer
    let main_fn = loaded.resolve("relon_main_entry").ok_or_else(|| {
        CraneliftError::Cache("symbol relon_main_entry missing".into())
    })?;
    let cap_vt_ptr = loaded.resolve("__relon_capability_vtable").ok_or_else(|| {
        CraneliftError::Cache("symbol __relon_capability_vtable missing".into())
    })?;

    // 6. 包装：cap_vt_ptr 由 host fill；main_fn 按 EntryShape 选 transmute
    //    （Legacy vs Buffer 由 metadata 携带）。
    Ok(Some(Self::from_loaded_object(loaded, main_fn, cap_vt_ptr, &entry.metadata)?))
}
```

### 4.3 Cold-path 时间预算

| 步骤 | 估算 |
| --- | ---: |
| `ensure_key()`（一次性，进程级 lazy_static cache）| ~1 μs（已加载后 0 μs） |
| `relon_object_cache::load` — `fs::read` mmap 等效（< 500 KB）| ~2-3 μs |
| HMAC verify（覆盖整个 blob）| ~3 μs |
| SHA-256 strict（重算 object bytes）— **TrustOnWrite 模式可省** | ~5 μs |
| metadata bincode deserialize | ~0.5 μs |
| `memfd_create(MFD_CLOEXEC)` | ~0.5 μs |
| `write` object bytes 进 memfd | ~1 μs |
| `dlopen("/proc/self/fd/<n>", RTLD_NOW \| RTLD_LOCAL)` | ~5-8 μs |
| `dlsym` × 2 | ~0.5 μs |
| capability vtable populate | < 0.5 μs |
| **合计 strict** | **~15-22 μs** |
| **合计 trust-on-write** | **~10-15 μs** ✓ 达 γ 目标 |

设计稿 §1.6 / §2.1 估算 10-15 μs，与本实现一致。Strict 模式略超
（22 μs 上界），TrustOnWrite 在 §7 的 bench 中应当作 fast-mode 跑。

---

## 5. 风险与缓解

| 风险 | Severity | 缓解 |
| --- | :-: | --- |
| **AutoEvaluator 没指定 cache_dir** | Medium | M2 给 `AutoEvaluator` 加 `cache_dir: Option<PathBuf>`，默认 `dirs::cache_dir().join("relon/native-aot")`（Linux: `$XDG_CACHE_HOME/relon/native-aot` 或 `~/.cache/relon/native-aot`）。初始化时 `fs::create_dir_all`；权限错则 cache 关，evaluator 仍可跑。 |
| **HMAC key 缺失或权限不对** | Low | `ensure_key()` 失败时 fallback no-HMAC（log warn 一行）。与 SHA-256 strict 配合仍能防 bit-rot；只是失去 authentication 抗第三方投放。设计稿 §3.1 明示允许。 |
| **ld 缺失（容器 minimal 镜像）** | Medium | `SubprocLinker::new()` 失败时降级到"只 in-mem JIT，不写 cache"。下次启动同源仍走 from_source。Log `error!`。 |
| **cache 文件 corrupt（bit-rot / 意外）** | Medium | load 时 `Sha256Mismatch` / `HmacMismatch` / `Truncated` / `MagicMismatch` 任一 → best-effort `remove_file` corrupt 文件 + 回 `Ok(None)`。Caller 回 `from_source_with_cache` 重新生成。 |
| **多进程并发写同 cache key** | Low | `relon_object_cache::store` 已用 `<name>.tmp.<pid>.<nanos>` + `fs::rename`，rename(2) 在同 filesystem 内 POSIX atomic。后写覆盖前写，无 corruption。 |
| **stale cache（cranelift / generator 升级）** | Medium | `Metadata.generator_version` 比对（M2 加；包含 `relon-codegen-native` crate version + cranelift version stamp）；任一不匹配 → miss。**注**：当前 `Metadata` struct 没有 `generator_version` 比对函数，M2 加 `validate_metadata` helper 时一起做。 |
| **`Metadata.host_fn_imports` 与运行时 host fn 不匹配** | High | `validate_metadata` 检查每条 `name + params_hash + returns_hash` 必须在当前 `CapabilityVtable` 注册表内；任一缺失或 hash 不同 → miss。Log `info!`。 |
| **dlopen 出来的 fn 触发 SIGSEGV 撞 host 自己的 handler** | High | 设计稿 §3.3 已固化：进程级 trap handler 仅在第一次 evaluator 构造前 install，多 evaluator 共享。M1 复用 v5-β-2 的 `relon_runtime::install_trap_handlers()`，γ phase **不动**。 |
| **`memfd_create` 不可用（< 3.17 内核）** | Low | `LoaderError::Memfd` 返回时降级到"只 in-mem JIT"。γ phase 不为旧内核加 tempfile fallback —— 等用户报再说。设计稿 §2.2 的 "fallback：临时文件 dlopen" 留给 v5-δ。 |
| **`__relon_capability_vtable` symbol 没在 cranelift-object emit 出来** | Medium | M1 的 `compile_module_to_object` 必须 `module.declare_data("__relon_capability_vtable", Linkage::Export, true, false)`，且 emit 时占位 64-byte 全零 data section。dlopen 后 host 用 dlsym 拿地址再写入。 |
| **TARGET_TRIPLE 与 host 真实 triple 不一致**（cross-compile） | Low | `validate_metadata` 比对 `entry.target_triple == TARGET_TRIPLE`。`relon_object_cache::load` 内部已比对，重复一次防漏。 |

---

## 6. M1-M3 milestone breakdown

| M | 工作 | 估算 | 验证 |
| --- | --- | :-: | --- |
| M1 | `CraneliftAotEvaluator::from_source_with_cache` + `from_cache_v2` 实现，新增 `codegen::compile_module_to_object`（与 `compile_module_with` 共享 IR lowering，仅 backend 不同），`canonical_source_sha256` helper，`validate_metadata` helper | **2 天** | 既有 1568 tests 全绿；新加 ≥ 8 个 integration test 覆盖 (a) from_source_with_cache 写出 cache 文件；(b) from_cache_v2 命中；(c) cache miss 返 Ok(None)；(d) HMAC 失败 unlink；(e) SHA-256 失败 unlink；(f) metadata 不兼容 miss；(g) ld 缺失降级；(h) memfd 不可用降级 |
| M2 | `AutoEvaluator` 加 `cache_dir: Option<PathBuf>` 选项 + `ensure_cache_dir()` + default `$XDG_CACHE_HOME/relon/native-aot`；`Metadata.generator_version` populate + 比对 | **1 天** | smoke test：源跑一次 → cache 文件存在 → 再跑同源应走 cache 路径（用 tracing event 或 timing assertion 验证）；`generator_version` mismatch 时正确 miss |
| M3 | bench `cached_cold_start` strict vs trust-on-write；append 新 section 进 `docs/internal/relon-perf-report-2026-05.md`（或新建 `v5-gamma-bench-2026-05-XX.md`） | **2 天** | strict 模式 ≤ 15 μs；trust-on-write 模式 ≤ 8 μs；uncached cold 保持 ~275 μs（±10%）；warm invoke 保持 ~415 ns（±10%）；cache 文件平均大小 < 500 KB |

**总计 ~5 天**，远低于设计稿 §5 的 ~2 周原始估算（prep agent 已把 ~70%
工作前置完成：HMAC / storage / linker / loader 这四个 crate-level 模块
都是 prep 阶段产物）。

---

## 7. 验收标准

v5-γ phase 完成判定（**全部满足**）：

1. **回归**：既有 1568 tests 全绿；既有 cranelift-aot warm path 测试不退步。
2. **新覆盖**：≥ 8 个新 integration test（参 M1 列表），覆盖 cache hit /
   miss / 损坏 / HMAC 失败 / SHA-256 失败 / metadata 不兼容 / ld 缺失 /
   memfd 失败八条路径。
3. **Cached cold start**：strict 模式 ≤ 15 μs；trust-on-write 模式 ≤ 8 μs
   （bench `cached_cold_start_strict` + `cached_cold_start_trust_on_write`）。
4. **Uncached cold start 不退步**：保持 ~275 μs（±10%）。新引入的
   `compile_module_to_object` 旁路执行不应让原 JIT 路径变慢
   （JIT module + ObjectModule 共享 IR lowering，差异仅在 backend
   finalize 一步）。
5. **Warm invoke 不退步**：保持 ~415 ns（±10%）。
6. **Cache 文件平均大小 < 500 KB**：用既有 corpus 跑一遍统计。HelloWorld
   级 ~30 KB；最大 stdlib-heavy case 应当 < 500 KB。
7. **Perf report 新 section**：`v5-γ cached cold start` 至少含
   `strict / trust_on_write / uncached / warm` 四组数据 + 与 v5-β-2 baseline
   对照。
8. **零 unsafe 增量**：所有 unsafe 必须在 `relon-object-cache::loader`
   crate 内（已有），整合层不引入新 unsafe block。

---

## 8. Dispatch 顺序建议

v5-γ phase 真启动时建议 **1 个 fresh agent 一口气做 M1+M2+M3**（总 5 天）。
原因：

1. 三个 milestone 都改 `relon-codegen-native`，连续做减少 context-switch。
2. M2 / M3 都依赖 M1 的两个新 entry point；拆开会浪费 hand-off 成本。
3. ~5 天工作量在单 agent 上下文 budget 内；用单 agent 节省 ~1.5 天
   hand-off + 重建上下文时间。
4. 与 v6-γ integration plan（3 周，4 agents）不同 —— v5-γ prep 阶段已经
   把 ~70% 工作前置完成，剩下的整合是窄面收口工作，不适合再拆。

> 严禁让单 agent 把 `relon-object-cache` 或 `relon-object-link` 任一改
> 公共 API —— 这两个 crate 是 sealed prep 产物，γ phase 只 consume，
> 不 modify。如发现 API gap，应当 stop 然后另开 agent 做 crate-level
> 修订。

---

## 9. 与其它文档的接口

- 与 [`v5-gamma-cranelift-object-cache-design.md`](./v5-gamma-cranelift-object-cache-design.md)：本文 supersede 其 §5 milestone，其余章节（§1 文件格式 / §2 dlopen 流程 / §3 安全模型 / §4 平台覆盖）作为底层契约不变。
- 与 [`v5-beta-2-stdlib-relower-plan.md`](./v5-beta-2-stdlib-relower-plan.md)：v5-γ cache 是 β-2 之上的层，β-2 stdlib 必须先全 cranelift 化（其 ABI symbol = `relon_stdlib_<name>`），M1 的 `compile_module_to_object` 直接复用 β-2 的 `codegen::compile_module_with` 路径。
- 与 [`v6-gamma-integration-plan-2026-05-18.md`](./v6-gamma-integration-plan-2026-05-18.md)：trace JIT 不**写** native cache（trace specialized code per-process），但**读** generic code 仍走 v5-γ cache。v6-γ phase 启动时本计划应当已经合入 main。
- 与 [`wasm-aot-v4-roadmap-sandbox-safe.md`](./wasm-aot-v4-roadmap-sandbox-safe.md)：v5-γ 是该 roadmap §v5-γ 实施收口。

---

## 10. 修订记录

- 2026-05-18 草案 v1：基于 `relon-object-cache`（28 tests）+ `relon-object-link`
  （20 tests）两个独立 crate 已合入 main 的现状，supersede 设计稿 §5 的
  2 周估算为 ~5 天 M1-M3 dispatch 计划；HMAC fallback / SHA-256 双模式 /
  ld 缺失降级 / memfd 失败降级四条 fallback 路径全部 typed error 化。
