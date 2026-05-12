# Internal Docs

本目录收纳维护者用的内部文档，不进 VitePress sidebar。三类文档的
职责要分开，避免同一件事在多处漂移：

| 文件 | 职责 | 维护规则 |
| --- | --- | --- |
| [`roadmap.md`](./roadmap.md) | 当前优先级和已完成阶段的活文档 | 计划变化时更新这里 |
| [`schema-rooted-model-2026-05-11.md`](./schema-rooted-model-2026-05-11.md) | Schema-rooted 调用模型的设计冻结草案：合并命名空间全局函数与值方法两条 dispatch 路径，统一为「每个可调用都有 schema 根」 | 实施过程中持续维护；新决策落地后更新对应章节 |
| [`schema-rooted-implementation-log.md`](./schema-rooted-implementation-log.md) | Phase A/B/C/D 落地日志，append-only。顶部带 "Reading guide" 索引 | 新决策回流时追加新章节；不重写历史条目 |
| [`relon-self-consistency-review-2026-05-10.md`](./relon-self-consistency-review-2026-05-10.md) | 一次批判性审视的时间点快照（P0 capability hardening 已折叠进 roadmap） | 不要求长期同步；遵循下方 retention policy |
| [`relon-self-consistency-review-2026-05-11.md`](./relon-self-consistency-review-2026-05-11.md) | schema-rooted Phase A-D 落地后的第二次批判性审视：沙箱实测语义、quickstart 门面、英文文档承诺 | 同上 |

公开规范和用户文档仍以 `docs/zh/guide/spec.md`、`docs/zh/guide/*`
以及对应英文文档为准。

## Retention policy

- self-consistency review 文件按 `relon-self-consistency-review-YYYY-MM-DD.md`
  命名。每份 review 是**当时**批判性视角的时间点快照，结论被折叠进
  `roadmap.md` 或实施完毕后即转为「历史快照」，**不再要求与现状同步**。
- 共存份数上限：本目录顶层最多保留 2 份 review。超过时把**最老的一份**
  `git mv` 进 `archive/`（与 `type-constraints-spec.md` 同处置），保留
  git 历史。
- 阅读 `archive/` 下文档的前提：理解它是当时的快照，**不是当前现状**。
  评估现状请回到 `roadmap.md` 与 `schema-rooted-implementation-log.md`。
- perf baseline 文件（如 [`perf-baseline-2026-05-12.md`](./perf-baseline-2026-05-12.md)）
  同样是 snapshot 性质，命名为 `perf-baseline-YYYY-MM-DD.md`，按需归档，
  遵循上述同一份 retention policy。

## Archived

| 文件 | 说明 |
| --- | --- |
| [`archive/type-constraints-spec.md`](./archive/type-constraints-spec.md) | future feature 草案：Constraint / schema method / host method 对照。已被 `schema-rooted-model-2026-05-11.md` 完整包含，保留作为历史草案 |
