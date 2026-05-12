---
layout: page
title: Playground
---

# Playground

> 在浏览器里直接跑 Relon。代码改动后自动 evaluate，右侧实时显示 JSON
> 输出；错误会在底部面板列出，并在编辑器对应行打 marker。

<Playground />

## 几个例子

页面顶部的 **Example** 下拉可以在四个预置示例之间切换：

- **demo** — 首页示例，演示函数定义、`&sibling` 引用、装饰器、`f"..."`
  插值。沙箱内可直接跑。
- **pricing** — 阶梯折扣 + 税费的发票计算，签名是 `#main(Order order)`。
  浏览器沙箱没法传 args，会报错；本地用
  `cargo run -p relon-cli -- run examples/pricing.relon --args '{...}'`
  可正常跑。
- **feature_flag** — 运行时特性开关。除了 args 还要 host 注册一个
  `native_hash` 函数，浏览器沙箱里两者都没有。
- **workflow** — 状态机驱动的订单流转，签名是
  `#main(String state, String event)`，同样需要本地 CLI 才能跑。

切到 demo 之外的示例时，错误面板上方会有一条说明 banner，指引你用
CLI 跑。

## 沙箱说明

- 默认无 `fs` / `net` / `clock` / `env` / `rng` 任何 capability，参见
  仓库根 [`SECURITY.md`](https://github.com/kookyleo/relon/blob/main/SECURITY.md)。
- 多文件用 `#import` 指令；新建文件点编辑器顶部的 `+` ，删除点对应
  tab 上的 `×`。entry 文件用 `★` 标记，点击其他文件的 `★` 可切换。
- 顶部状态栏显示 wasm 模块版本与就绪状态。
- 错误点击可跳转到对应文件和位置。
