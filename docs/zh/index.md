---
layout: home

hero:
  name: "Relon"
  text: "Build typed business-config DSLs on top of JSON"
  tagline: "可嵌入 Rust 的工具集，用来搭建「类型化业务配置 DSL」。平台团队定义 schema、装饰器与原生函数；业务团队用 JSON 形态的配置组合它们，最终编译为纯净 JSON。From JSON-like, to JSON, for JSON。"
  image:
    src: /logo.svg
    alt: Relon
  actions:
    - theme: brand
      text: 快速开始
      link: /zh/guide/introduction
    - theme: alt
      text: GitHub 仓库
      link: https://github.com/kookyleo/relon

features:
  - title: 双层作者模型
    details: "平台团队提供 schema、装饰器、原生函数与 .relon 库；业务团队写薄薄一层 entry 配置去拼装。语言级 `@library` 标记区分「库」与「入口」，杜绝错把库当 entry 跑。"
  - title: 类型化业务 schema
    details: "内建 sum-type tagged enum、递归 schema、`@expect` 自定义校验消息、必填/可选/默认值/计算默认值并存——业务领域的契约不必再退化成松散字典。"
  - title: 沙箱默认安全
    details: "`Capabilities` 控制文件读白名单、求值步数、Value 元素水位、原生函数白名单。`Context::sandboxed()` 默认拒绝一切，宿主显式授权后再放行。"
  - title: JSON 闭环
    details: "From JSON-like, to JSON, for JSON——输入语法贴近 JSON，输出永远是普通 JSON。`Projector` trait 让你微调输出形态（如 sum-type 编码风格），但永远落在 JSON 上。"
---

<figure style="margin: 3rem auto; max-width: 760px; text-align: center;">
  <img src="/relon/positioning.svg" alt="Relon two-tier authoring: Platform Team writes schemas/fns/decorators, Business Team imports them and authors entry configs that the relon-evaluator turns into plain JSON for downstream services." style="width: 100%; height: auto;" />
  <figcaption style="margin-top: 0.75rem; font-size: 0.9rem; color: #64748b; font-style: italic;">双层作者模型：平台团队产出词表，业务团队组合词表，evaluator 输出 JSON。</figcaption>
</figure>
