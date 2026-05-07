---
layout: home

hero:
  name: "Relon"
  text: "Logic as portable data"
  tagline: "把业务逻辑像 JSON 一样写一次、存一次、传一次。任何符合规范的 Relon 运行时都给出相同的执行结果——Go、TS、Swift、浏览器、Rust、WASM 之间不再有「逻辑漂移」。仓库里这一份是参考运行时（Rust），语言规范与运行时无关。"
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
  - title: 跨端确定一致
    details: "同源 + 同输入 → 字节级相同的输出。Dict 走 BTreeMap 保序、IEEE-754 f64 无歧义、不读环境变量、不依赖隐式上下文。逻辑写在数据库里、跑在哪个 runtime 上，结果都一样。"
  - title: 默认沙箱，无逃生口
    details: "脚本零环境特权。`Capabilities` 显式控制文件读、求值步数、value 元素水位、原生函数白名单——没有「trusted 模式」让脚本绕过宿主授权。安全审计的边界明确。"
  - title: 自描述类型契约
    details: "`@schema`、sum-type tagged enum、递归 schema、品牌标记、计算默认值——契约信息和载荷一起传输，下游不需要带外文档就能校验。"
  - title: 上下文感知引用
    details: "`&root`、`&sibling`、`&prev`、`&next` 让逻辑声明式地引用周围数据，无硬编码路径。引用是声明式的，跨端执行结果仍然确定。"
---

<RelonGallery />
