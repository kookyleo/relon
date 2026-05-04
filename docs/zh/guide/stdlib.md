# 标准库 (Standard Library)

Relon 旨在成为配置领域的确定性超集。因此，它的标准库故意移除了所有不确定的副产品特性（例如网络请求、文件 I/O、当前时间等）。标准库的所有函数在给定相同的输入时，始终会产生完全一致的数据输出。

## 数据断言 (is)

`std/is` 命名空间提供了一系列纯函数，用于判定数据的运行时类型：

- `is.null(value)`
- `is.bool(value)`
- `is.int(value)`
- `is.float(value)`
- `is.number(value)`: 匹配 Int 和 Float。
- `is.string(value)`
- `is.list(value)`
- `is.dict(value)`
- `is.empty(value)`: 判断字符串、列表或字典是否为空。

## 契约守卫装饰器 (ensure)

`std/ensure` 下包含了一系列强大的校验拦截器，通常与 `@` 装饰器搭配在 Schema 中使用。如果断言失败，整个配置构建将会崩溃，从而在早期暴露问题：

- `@ensure.required_fields(["host", "port"])`: 确保当前字典包含指定的键。
- `@ensure.at_least(1024, "msg?")`: 确保数字大于或等于特定值。
- `@ensure.requires("tls", "cert", "msg?")`: 如果当前字典有 `tls` 字段，则必须同时有 `cert` 字段。
- `@ensure.fields_equal("password", "confirm")`: 确保两个字段的值严格相等。

## 字典操作 (dict)

- `dict.merge(base, patch)`: 类似于使用 `+` 算子，执行深度合并。
- `dict.keys(dict)`: 返回字典包含的所有键列表（由于是 `BTreeMap` 驱动，输出通常是有序的）。
- `dict.values(dict)`: 提取字典的所有值。
- `dict.has_key(dict, key)`: 判断键是否存在。

## 列表与字符串 (list & string)

### 列表操作 (list)
- `list.len(list)`: 数组长度。
- `list.first(list)`: 获取首个元素。
- `list.contains(list, item)`: 是否包含指定元素。
- `list.compact(list)`: 返回一个新数组，其中去除了所有的 `null` 元素。

### 字符串操作 (string)
- `string.len(str)`: 字符串长度。
- `string.split(str, separator)`: 切割字符串为列表。
- `string.join(list, separator)`: 把列表拼接为字符串。
- `string.replace(str, old, new)`: 替换子串。
- `string.upper(str)` / `string.lower(str)`: 大小写转换。
- `string.contains(str, substr)`: 是否包含片段。

## 数学操作 (math)

- `math.abs(val)`: 绝对值。
- `math.min(a, b)` / `math.max(a, b)`: 最大最小值。
- `math.clamp(val, min_val, max_val)`: 将数字限制在指定范围内。

通过组合这套小巧而紧凑的库，你能够在不借助外部脚本的情况下，利用纯 Relon 代码完成极其复杂的格式化、校验以及组件聚合工作。
