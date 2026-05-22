# #177 — `IntegrityMode::TrustOnWrite` removal (Option A)

## 选择 + 理由

**Option A**：直接 remove `IntegrityMode::TrustOnWrite` variant。

- workspace 当前版本 `0.1.0`，v0.x semver 明确允许 breaking。
- audit 显示**没有 workspace 内 production caller**使用此 variant：
  唯一非 test 引用是 `relon-codegen-native/src/object_cache_integration.rs`
  里两处 doc 注释，本身就在解释 #171 为什么把 production 路径切到
  `HmacRequired`。
- 此 variant 是 #171 关停的 bypass 的"残骸"：skip SHA-256 + skip HMAC
  enforce，刚好就是 attacker drop unsigned blob 后能被 dlopen 进
  host 的那条路径。即便 doc 标 deprecated，only-attribute fix
  仍是 "诱导降级" — Option A 彻底没了。
- legacy tests 全部能用 `HmacRequired` + fixture HMAC key 等价覆盖：
  HMAC tag 对 header + object + metadata 全段做 authentication，比
  `TrustOnWrite` 的 "什么都不查" 严格得多。

## 改动列表

`crates/relon-object-cache/src/integrity.rs`
- 删除 `IntegrityMode::TrustOnWrite` variant。
- module-level doc 由 "Three modes" 改 "Two modes"，新增 `## Removed`
  section 解释 footgun 性质 + migration path。

`crates/relon-object-cache/src/storage.rs`
- `load()` 的 doc 删掉 `TrustOnWrite` bullet。

`crates/relon-object-cache/src/lib.rs`
- crate-level doc 改述 integrity module。

`crates/relon-object-cache/tests/`
- `concurrent_load.rs`：`concurrent_writers_atomic_rename` /
  `reads_during_overwrite_never_observe_partial_blob` 切换到
  `Some(&hmac_key)` 写、`IntegrityMode::HmacRequired` 读。
- `sha256_strict.rs`：`trust_on_write_skips_recompute` 改名重写为
  `hmac_required_skips_recompute_when_filename_is_source_derived`，
  保留 "filename stem 是 source-derived key 时 SHA-256 必须跳过" 的
  覆盖。
- `hmac_verify.rs`：`hmac_rejects_tampered_object_byte` 切到
  `HmacRequired`。
- `storage_roundtrip.rs`：`overwrite_replaces_previous_entry` 切到
  `HmacRequired`。

`crates/relon-codegen-native/`
- `src/object_cache_integration.rs` / `tests/cache_hmac_absence.rs`
  里两处历史叙述性 doc 注释把 `TrustOnWrite` 改为 "permissive
  (now-removed) integrity mode"。

`CHANGELOG.md`
- 顶部 `[Unreleased]` 加 Breaking note，列 migration path。

## Migration path（external caller）

`IntegrityMode::TrustOnWrite` → `IntegrityMode::HmacRequired` +
`relon_object_cache::ensure_key()` 提供的 per-installation HMAC key。
写端同步把 `hmac_key = None` 改为 `Some(&key)`。HMAC tag 覆盖
header + object + metadata 全段，对原本 "trust writer" 的场景给出
等价或更强的 tamper detection。

## Gate

- `cargo fmt --all --check`：pass
- `cargo clippy --workspace --all-targets -- -D warnings`：pass
- `cargo test --workspace`：**2316 passed / 0 failed / 6 ignored**
- `cargo check -p relon-wasm --target wasm32-unknown-unknown`：pass
- `relon-object-cache` 套件：31 passed / 0 failed
