# Internal Docs

本目录收纳维护者用的内部文档，不进 VitePress sidebar。三类文档的
职责要分开，避免同一件事在多处漂移：

| 文件 | 职责 | 维护规则 |
| --- | --- | --- |
| [`roadmap.md`](./roadmap.md) | 当前优先级和已完成阶段的活文档 | 计划变化时更新这里 |
| [`relon-self-consistency-review-2026-05-10.md`](./relon-self-consistency-review-2026-05-10.md) | 一次批判性审视的时间点快照（P0 capability hardening 已折叠进 roadmap） | 不要求长期同步；若结论进入计划，折叠进 roadmap |
| [`relon-self-consistency-review-2026-05-11.md`](./relon-self-consistency-review-2026-05-11.md) | schema-rooted Phase A-D 落地后的第二次批判性审视：沙箱实测语义、quickstart 门面、英文文档承诺 | 同上 |
| [`type-constraints-spec.md`](./type-constraints-spec.md) | future feature 草案：Constraint / schema method / host method 对照 | 作为设计草案维护；实现前必须重新核对当前语法和 capability 模型。已被 `schema-rooted-model-2026-05-11.md` 包含 |
| [`schema-rooted-model-2026-05-11.md`](./schema-rooted-model-2026-05-11.md) | Schema-rooted 调用模型的设计冻结草案：合并命名空间全局函数与值方法两条 dispatch 路径，统一为「每个可调用都有 schema 根」 | 实施过程中持续维护；新决策落地后更新对应章节 |

公开规范和用户文档仍以 `docs/zh/guide/spec.md`、`docs/zh/guide/*`
以及对应英文文档为准。
