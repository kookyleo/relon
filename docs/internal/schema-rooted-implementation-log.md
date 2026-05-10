# Schema-Rooted Implementation Log

> 实施过程的决策日志。Phase A.1 / B / C / D / E 实施时遇到的、未在
> [`schema-rooted-model-2026-05-11.md`](./schema-rooted-model-2026-05-11.md)
> 决策清单里覆盖的细节，在这里登记。每条记录形态：上下文 + 选择 + 理由。

## 体例

每个条目：

```text
### N. 主题
日期 / 阶段 / 涉及文件 / 备注

**问题**：实施中冒出来的具体细节
**选择**：定下来的方案
**理由**：基于过往 Q&A / 倾向的推导
**回流**：是否要把它折叠回主设计文档（默认是；记录原因）
```

---

## A.1 阶段（parser body-less + #extend）

### A.1.1：body-less 时 body 字段如何表示
2026-05-11 / Phase A.1 / `crates/relon-parser/src/directive.rs`

**问题**：`#schema String with { ... }` 没有 body 表达式。
`DirectiveBody::NameBody.body` 类型是 `Box<Node>`（必须存在）。要么
改成 `Option<Box<Node>>`（破坏性，几个 destructure 站点要更新），
要么让 parser 在 body-less 场景合成一个空 Node。

**选择**：parser 合成一个 `Expr::Dict(vec![])` 节点占位；body 字段类
型保持 `Box<Node>` 不变。

**理由**：
- 决策 10 表面语法是「省 body」，但 AST 是 parser 自己的形状，让
  AST 层面始终有 body 不影响表层语义
- destructure 站点（lsp/cursor.rs / eval.rs / 其它）继续按
  `body, ..` 形态读，不需要做 `Option` 解包
- 「empty dict body = no fields」与「body absent」在分析阶段语义一
  致（schema 没有字段集），合成不引入歧义
- 反之改 `Option` 是 N+1 文件的破坏性改动，杠杆低

**回流**：是。已记录到 schema-rooted 主文档「实施细节」附录（待添加）。

### A.1.2：`#extend` 的 directive shape
2026-05-11 / Phase A.1 / `crates/relon-parser/src/directive.rs`

**问题**：`#extend X with { ... }` 表面看是「name + with-block」，
跟 `#schema X with { ... }` 形状完全一致 —— 是注册成 `NameBody` 复
用 `parse_name_body`，还是单独搞个 `DirectiveShape::Extend`？

**选择**：复用 `DirectiveShape::NameBody`。`#extend` 与 `#schema` 在
parser 层走同一条路径，AST 都是 `DirectiveBody::NameBody`，analyzer
通过 `dir.name == "schema" | "extend"` 区分语义。

**理由**：
- A.1.1 的合成空 body 让两者语法形态完全统一
- 无新 `DirectiveShape` 变体 = 无 N+1 处 match arm 调整
- analyzer 端按名字 dispatch 是已有惯用法（`SCHEMA` / `IMPORT` / `MAIN`
  常量已经作为名字字符串）

**回流**：否，是 parser 内部实现选择。

