---
layout: home

hero:
  name: "Relon"
  text: "Logic as data"
  tagline: "把业务逻辑像 JSON 一样写一次、存一次、传一次。Relon 是一种可执行的数据格式，载荷就是业务逻辑本身——校验规则、计费公式、工作流、风控策略。嵌入式 Rust 运行时提供显式 capability 与预算控制：同源 + 同输入 → 字节级一致的输出。"
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
    - theme: alt
      text: 威胁模型
      link: /zh/guide/threat-model

features:
  - title: 设计上的确定性
    details: "同源 + 同输入 → 字节级相同的输出。Dict 走 BTreeMap 保序、IEEE-754 f64 无歧义、不读环境变量、不依赖隐式上下文。把同一份 .relon 跑两次必然得到同一个结果，可重放、可 hash、可缓存。"
  - title: 默认沙箱，无隐式 trust
    details: "脚本零环境特权。`Capabilities` 显式授予文件读与宿主原生能力；`ResourceBudget` 定义预算模型。宿主可显式全开，但必须可审计；不可信部署应使用 VM 或进程边界。"
  - title: 自描述类型契约
    details: "`#schema`、sum-type tagged enum、递归 schema、品牌标记、计算默认值——契约信息和载荷一起传输，下游不需要带外文档就能校验。"
  - title: 上下文感知引用
    details: "`&root`、`&sibling`、`&prev`、`&next` 让逻辑声明式地引用周围数据，无硬编码路径——把片段移到结构里的另一个位置，引用会自动重新解析到新邻居。"
---

<RelonGallery />
