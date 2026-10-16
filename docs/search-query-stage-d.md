# 搜索与 JSONPath 分离：阶段 D 实施记录

日期：2026-10-16。基线：0.7.5+1 / `5bfeb23`。D 按用户明确要求收录为 0.7.6（本次版本提交）。按用户要求未运行测试、构建、Clippy、格式检查或基准，本次只核对源码差异、版本文本与暂存文件清单，不将提交授权记为实测通过；S0 保持暂停。

## 1. 本阶段边界

D 将现有结构查询职责移入独立引擎，保持已有语法、顺序和错误约定；不增加过滤语法、不实现大文件流式求值，也不迁移 JSONPath 结果区到磁盘分页。

| 文件 | 职责 |
| --- | --- |
| `src/engine/query/mod.rs` | QueryEngine，Document／单节点定位／Sparse Pointer 入口，后端能力矩阵 |
| `src/engine/query/request.rs` | 公共 QueryRequest，只包含表达式，不含 case／regex／搜索范围 |
| `src/engine/query/plan.rs` | CompiledQuery：规范化、语法校验、选择后端计划、源会话绑定及执行分派 |
| `src/engine/query/adapters/memory.rs` | serde_json_path 求值及精确位置到节点编号的转换 |
| `src/engine/query/adapters/indexed.rs` | 迁移原文件后端的 Selector／Step 解析和执行，沿用完整 NodeIds 中间结果 |
| `src/engine/query/adapters/pointer.rs` | 完整索引后端的 Pointer 查找，非法／缺失路径保持零结果语义 |
| `src/engine/query/adapters/sparse.rs` | 原有 Sparse 单 Pointer 导航，不开放一般 JSONPath |
| `src/query_worker.rs` | 独立任务调度适配与有界预览生成；请求类型再导出以兼容已有调用方 |

Document 不再解析表达式、调用 JSONPath 库或做查询结果位置映射；原 `find_path`／`query_path`／`query_path_cancellable` 只转发到 QueryEngine。Indexed 存储模块不再保存选择器解析／执行逻辑。SparseIndex.pointer 也变为兼容包装。

为了移除跨模块对存储数组的依赖，Document 的键查找、数组下标查找和直接子节点迭代仅向 crate 内开放；查询适配器通过这些导航方法和 node_info 读取结构，不直接访问 nodes／storage。Sparse 仅将既有节点校验方法向 crate 内开放，仍执行原会话、范围与目录检查。

## 2. 请求与编译执行路径

```text
GUI QueryWorker / 主 CLI --query
  → 公共 QueryRequest
  → QueryEngine
  → CompiledQuery：规范化、语法检查、后端计划
  → Memory 或 Indexed 适配器
  → NodeIds（顺序及重复项保留）
  → UI 预览 / CLI 输出

旧 Document 接口 → QueryEngine（兼容入口）
旧 SparseIndex.pointer → QueryEngine::sparse_pointer → Sparse 适配器
```

主 CLI 的 `--get` 进入 QueryEngine::find，要求恰好一个结果；`--query` 使用公共请求执行。GUI Worker 也直接调用 QueryEngine，不通过 Document 查询包装回调。旧内部无 GUI 分支同样使用引擎入口。

CompiledQuery 保留原表达式的规范化形式，对外提供 normalized／backend；执行前校验会话和后端，拒绝将旧文档编译对象用于新文档。新的任务仍由 B 的 Worker 会话／代次隔离控制，文本搜索沿用自己的入口，两者不调用对方的匹配器或执行队列。

规范化字符串先检查长度溢出、预留共享预算、使用可失败分配，再将实际容量计入账本。库 AST 和迁移的 Indexed 选择器解析临时分配仍未全部精确计量；不能将这个新增预留写成“所有查询内存都已受硬限制”。原 NodeIds 预算、扩容重叠预留及 0.7.4 取消固定结果上限的行为继续保留。

## 3. 保持的语义与能力矩阵

| 能力 | Memory | Indexed | Sparse |
| --- | --- | --- | --- |
| Pointer | 已有查找语义 | 已有查找语义 | 现有单节点接口 |
| 字段／下标／通配符／联合／递归／切片 | 现有库定义 | 原有子集，迁移为有名字的 Selector／Step 计划 | 一般 JSONPath 未实现 |
| 过滤／函数 | LibraryDefined，只承诺实际库支持的语法 | 明确不支持，无整份 Value 回退 | 不支持 |
| 求值中断 | 库求值前后及位置转换时检查；库内部整次求值不可强停 | 解析阶段及结构遍历中检查 | 导航／读取中检查 |
| 增量发布查询结果 | 未实现 | 未实现 | 未实现 |

能力通过 Backend::capabilities 显式提供，Memory 的函数支持不等于所有 JSONPath 扩展函数。Indexed 编译仍先经现有库校验合法 JSONPath，再由原有子集解析器检查能力；本次只是迁移并显式组织计划，不将原字符串解析器写成完整 RFC AST。通用 AST／流式选择／过滤执行器属于 E。

兼容规则：

- 完整索引入口先 trim；`.` 变为 `$`，`.`／`[` 开头的简写补 `$`，`/` 开头进入 Pointer。
- 合法零命中返回空列表；非法 JSONPath 返回原有 Invalid JSONPath 错误；find 的零结果和多结果分别报错。
- 联合重复选择、递归顺序、反向切片、Unicode／特殊键和数字语义沿用原执行器，不排序、不去重、不转成浮点近似。
- 完整索引 Pointer 非法转义／非法数组下标／路径缺失仍形成空结果，空字符串仍走 JSONPath 解析。Sparse 保留空 Pointer 指根、非法 Pointer 明确报错、不 trim 的差异。
- Sparse 保留 16 KiB Pointer 和 32 KiB 解码工作区约束；其余语法不自动转入 Memory。新引擎入口增加执行前取消检查，取消时可直接返回 Query cancelled。
- 预览仍由 Worker 调用有界输出生成，CLI 仍完整收集后输出；查询结果磁盘分页尚未接入。

## 4. 验收与后续

本次仅核对源码和调用关系，没有添加、运行测试或执行构建。原 Document／Indexed／Sparse 用例保留在原文件，通过兼容包装经过新引擎，但这只是源码路径变化，不是通过记录。

待用户验收：

1. 编译、主 CLI `--get`／`--query`、默认 GUI 查询及原有功能回归。
2. Memory／Indexed 公共子集的顺序、重复项、正反切片、零结果和特殊键对照；Memory 过滤与 Indexed 不支持错误。
3. Pointer 根／空字符串、非法转义／数组下标、缺失节点；Sparse 与完整索引的既有差异。
4. 编译后在原会话重复执行；换文档后执行旧 CompiledQuery 必须明确拒绝。
5. 查询取消、快速替换、与常规搜索并行、错误与预览隔离；资源不足时错误和预算归还。
6. 大整数、缺失／null、现有库实际支持函数的原行为；本次不声明新增兼容范围。

E 再实现真正的有界查询执行、公共 ResultStore 发布及大文件过滤，逐项开放明确支持的语法。大文件专项仍采用十进制 100／300／500 MB，低／高命中分别记录；本次未执行，S0 不恢复。样本、测试 JSON 和基准 JSON 报告继续只保存在本地。
