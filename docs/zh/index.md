---
layout: home

hero:
  name: "Relon"
  text: "下一代强类型配置语言"
  tagline: "一个专为现代 Web 和工业级应用设计的配置语言与 UI 模板引擎。它融合了 JSON 的极简美学、完备的表达式能力，以及名义类型系统（Nominal Types）。"
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
  - title: 面向表达式 (Expression-Oriented)
    details: 没有语句，没有 return 关键字。从简单的算术运算到复杂的列表推导式，代码中的一切最终都会求值为一个数据。
  - title: 强类型契约 (Type Contracts)
    details: 原生支持前缀类型标记、泛型与身份守卫（Identity Guard）。使用 @schema 定义契约，确保数据在历经复杂的合并后依然合法。
  - title: 动态寻址与上下文 (Dynamic Pathing)
    details: 内置了强大的相对引用（&sibling, &root, &prev, &next），支持动态键名解析，为高度复用的配置模板提供了无限可能。
  - title: Schema 混入组合 (Mixins)
    details: 像处理普通数据一样对 Schema 进行加法组合，轻而易举地实现 UI 组件属性的继承与覆盖。
---
