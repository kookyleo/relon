# LLVM-AOT 缓存键不变量(GENERATOR_VERSION)

## 背景

cranelift 后端有持久化 object cache:编译产物(`.relon-native-v1` 机器码 blob +
`.relon-ir-v1` IR)落盘后,下次同输入直接复用,跳过整条 codegen。为了防止旧产物
被新版 codegen 静默读用,cranelift 把一个生成器版本串
`relon_codegen_cranelift::GENERATOR_VERSION`(现 `v5-gamma 13`)纳入缓存完整性键
(见 `object_cache_integration::cache_signature` 的 HMAC):codegen 一旦发生不兼容
改动(op lowering / ABI / arena 布局 / 入口形状)就 bump 此串,旧缓存因键不匹配
自动失效。

## llvm 侧现状

llvm-AOT 后端**当前没有任何 object / ELF / bitcode 缓存**——每次 dispatch 都用
进程内 MCJIT 现编现跑。因此现在不存在「落盘字节流对不上新代码」的风险,也就**暂时
不出错**。

为对齐两后端、并给未来钉死规则,llvm 侧已加占位常量:

    relon_codegen_llvm::GENERATOR_VERSION  // crates/relon-codegen-llvm/src/lib.rs

它**今天不参与任何缓存键**,纯粹是前瞻占位 + 文档锚点。

## 铁律(给未来加 llvm 缓存的人)

> 若将来给 llvm-AOT 加 object / ELF / bitcode 缓存,**必须**把
> `relon_codegen_llvm::GENERATOR_VERSION` 折进该缓存的完整性键(HMAC / hash,
> 即决定命中与否的那把键),做法照搬 cranelift 的
> `object_cache_integration::cache_signature`。并且**每次** codegen 不兼容改动
> (op lowering、ABI / arena 布局、marshalling-seam、entry-shape)都要 bump 此串。

漏掉这条的后果:旧生成器产出的机器码会被新代码静默加载,并在**新的 host 侧解码
假设**下执行——典型表现是静默错值,严重时是越界 / 内存安全问题。这正是 cranelift
通过 `GENERATOR_VERSION` 一直在防的坑;llvm 加缓存时不可重蹈。
