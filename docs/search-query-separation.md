# 搜索与 JSONPath 引擎分离：阶段 A 行为基线

日期：2026-10-16。源码基线：FerrisJson 0.7.3；本文及取消固定结果上限的修改按用户明确要求提交为 0.7.4；没有新增测试或用户实测通过记录。本文是现有 [S6／S7 方案](large-file-refactor-plan.md) 的细化，不改变历史阶段成绩。

执行约定：后续不运行测试，除非用户明确指定；本次也不运行构建、基准、Clippy 或格式检查命令。由用户测试并确认 OK 后再提交。S0 保持暂停。以下“已有用例”仅表示读取过源码中的断言，不表示本次执行通过。

## 1. 阶段 A 范围与交付状态

- [x] 记录 Memory、Indexed、Sparse 的入口、语义和当前差异。
- [x] 记录常规搜索和结构查询分别应保留的行为。
- [x] 定位已有回归用例，列出用户验收步骤及迁移缺口。
- [x] 按用户追加要求取消兼容搜索 65,536 条命中上限、Indexed 查询 256 MiB 结果 ID 固定上限；保留共享资源预算。
- [x] 同步修改原有“超过 65,536 条必须报错”的用例断言，使其反映新行为；未运行。
- [x] 用户明确指示提交为 0.7.4 后开始 B；不等同于测试通过。
- [x] 按用户指示同步版本及文档为 0.7.4（本次版本提交）。

阶段 A 不创建新执行器、不迁移 Worker、不开放新语法。阶段 B 再开始职责拆分。固定上限取消是本阶段唯一追加的运行时行为变化。

## 2. 当前入口与数据流

| 入口 | 常规搜索 | 结构查询 | 当前结果承载 |
| --- | --- | --- | --- |
| 默认 GUI `src/app.rs`，Memory／Indexed | `search` → 独立 Worker 实例 → `Document::search_cancellable` | `go_to_path` → 另一 Worker 实例 → `Document::query_path_cancellable` | 搜索 NodeIds；查询 NodeIds + 有界预览 |
| 分页树 GUI `src/tree_app.rs`、`src/tree_worker.rs`，Memory／Indexed／Sparse | SearchHub → `engine::search::Input::run` | 一般 JSONPath 尚未接入 | 磁盘 ResultStore + 有限展示页 |
| S4 回退 GUI `src/sparse_app.rs` | 未接入完整搜索交互 | 未接入一般 JSONPath | 有限节点页 |
| 主 CLI `src/main.rs`，Memory／Indexed | `--search` → `Document::search` | `--get` → `find_path`；`--query` → `query_path` | 完整收集后输出 |
| `examples/sparse_index.rs` | 不作为完整主 CLI 的替代入口 | `SparseIndex::pointer` 的 CLI 原型 | 单个节点定位与输出 |

默认 GUI 的两个 Worker 实例已经各有队列和取消代次。共享的是 Worker 类型及其 Search／Query 枚举，不是同一个队列。拆分不能将已存在的并行能力记为新增成果。

主 CLI 的 `--get`、`--query`、`--search`、`--pretty` 互斥。`--search` 固定采用大小写不敏感的字面量、键和值全部范围；目前没有 CLI case／regex 开关。`--indexed` 不接受 stdin；`--sparse`、`--paged-tree` 是文件 GUI 入口，不能与主 CLI 查询参数组合。

## 3. 常规搜索契约

| 项目 | 当前行为／兼容要求 | 源码依据 |
| --- | --- | --- |
| 模式 | normal 即字面量；正则开关决定是否解释表达式；case 是大小写选项 | `engine/search.rs::Query`、`Matcher::new` |
| 字面量 | 对搜索词做正则转义；不是原始文件字节搜索 | `Matcher::new` |
| 大小写 | `case=false` 使用正则库的大小写不敏感匹配；不额外承诺语言地区规则 | `Matcher::new` |
| 搜索范围 | Keys／Values／All；对象键、数组下标文本、根 `$` 都属于键语义 | `Document::key`、`SearchScope`、Sparse `search_visit` |
| 值语义 | 解码后的字符串、数字文本、true／false／null；容器本身不通过序列化全文参与值匹配 | `Document::search_visit`、各文件遍历适配器 |
| 数字 | 沿用 serde_json arbitrary_precision 的表示；文件匹配器规范化指数标记和缺省正指数符号 | `Matcher::range`、已有数字用例 |
| 顺序 | 沿用语义树的前序遍历；对象重复键按现有后端的最终语义处理 | 三后端遍历及对照用例 |
| 结果单位 | 每个节点至多一次，即使键和值同时命中或同一文本多处命中 | `search_visit`、三后端对照用例 |
| 空词 | 不命中；默认 GUI 清除旧搜索状态，流式执行器可完成零结果任务 | `Matcher::text`、`Input::run`、`app.rs` |
| 错误 | 非法正则返回错误；不得解释为普通零命中 | `Matcher::new` |
| 结果上限 | 本次取消 65,536 条固定上限；旧接口仍完整收集 NodeIds，扩容受 BudgetManager 保护 | `Document::search_cancellable`、`engine/ids.rs` |

仍保留的限制：搜索词最多 64 KiB；文件 Regex 的每个键／字符串／数字原文范围限制 256 KiB（已驻留的旧后端键直接匹配）；流式遍历有深度限制。它们不属于本次取消范围。Sparse 搜索需要完整验证，未完成时返回 Pending 错误，不能视作已支持边加载边全文搜索。

S6 当前首条命中立即发布，后续按 128 条批次发布，搜索事件队列容量为 4。扫描计数是访问节点计数，不是扫描字节数，也不是已知准确百分比。高亮使用独立的展示匹配器；结果节点与屏幕中具体高亮片段不可混为一个结果模型。

## 4. JSONPath／Pointer 能力矩阵

| 能力 | Memory | Indexed | Sparse |
| --- | --- | --- | --- |
| 根、字段、数组下标 | 现有库求值 | 已有选择器执行 | Pointer 原型 |
| 通配符、联合选择、递归 | 现有库求值 | 已支持 | 一般 JSONPath 未实现 |
| 负下标、切片、反向切片 | 现有库求值 | 已支持 | 一般 JSONPath 未实现 |
| 过滤 | 委托现有库，具体语法以实际支持为准 | 明确不支持 | 未实现 |
| 函数 | 委托现有库，不承诺所有扩展函数 | 明确不支持 | 未实现 |
| 结果顺序与重复项 | 保留库返回顺序及重复项 | 保留选择顺序及重复项 | Pointer 单节点接口 |
| 中间结果 | 库的 located results + NodeIds | 每段构造完整 NodeIds，旧／新数组可能共存 | 无通用查询执行器 |
| 取消 | 求值前后、转换节点时检查；库整次求值无法中途强停 | 遍历期间检查 | Pointer 导航和读取期间检查 |

完整索引的入口先 trim；`.` 规范化为 `$`，`.` 或 `[` 开头的简写补 `$`；`/` 开头进入 Pointer 定位。其余表达式先经 JsonPath::parse 校验，再分派后端。Indexed 没有自动全量 Value 回退。

`query_path` 的合法零命中返回空列表；`find_path` 要求恰好一个结果，零结果和多结果均报错。顺序和重复项必须在未来分页存储中继续保留，不可改用集合去重。

Pointer 存在需要明确处理的差异：完整索引路径的非法转义、非法数组下标或缺失节点通常形成空结果；Sparse 原型返回明确定位错误。Sparse 接受空 Pointer 作为根；完整索引当前的空查询走 JSONPath 解析，不是 Pointer 根。阶段 B 不顺手统一这些语义，未来如调整需单独列为行为变化并验收。

本次取消 Indexed 单个结果数组的 256 MiB 固定上限。所有 NodeIds 扩容仍经过 reserve_vec 的容量／字节溢出检查、共享账本预留和可失败内存分配。扩容暂时预留旧／新缓冲重叠；失败明确返回错误。取消固定上限不代表无限内存或已经流式化。库内部的临时结果也不能声称已全部受账本精确约束。

## 5. UI、生命周期与输出基线

| 场景 | 应保留的行为／现有缺口 |
| --- | --- |
| 搜索输入 | 默认手动执行；Live 启用后 200 ms 防抖；编辑和清空取消旧搜索 |
| 搜索定位 | `/`、`?`、n/N 和计数维持已有方向及循环规则 |
| JSONPath 定位 | 输入框外 `.` 聚焦查询；执行后显示结果并定位首条；普通选中不丢失查询结果 |
| JSONPath 错误 | 完成失败后清除旧结果节点并显示查询错误；不能继续导出旧结果冒充新结果 |
| 独立状态 | 当前搜索列表与查询列表分开，但默认 GUI 共用 query_duration，查询错误还涉及通用 error；阶段 B 需显式拆出状态 |
| 文件切换 | 两类任务都取消；旧会话／旧任务完成消息不得回写新文档 |
| 高亮 | 常规搜索匹配选项控制高亮，不用 JSONPath 表达式生成高亮规则；查询结果预览仍可能显示已激活的搜索高亮 |
| 预览 | 预览有截断上限；完整复制／导出不可用截断预览代替 |
| 导出 | 旧界面使用选中值或查询节点集；S6 搜索结果有完整／部分游标导出，不能把两种入口当成已经统一 |
| 资源生命周期 | 清除任务不应误关闭另一引擎正在使用的会话；预算随最后引用释放；磁盘结果由持有者生命周期清理 |

## 6. 已有回归源码索引（本次均未运行）

| 文件 | 重点函数 | 覆盖意图 |
| --- | --- | --- |
| `src/document.rs` | `search_preserves_array_index_keys_regex_and_case_semantics`、`search_checks_full_values_and_scopes` | 数组键、范围、大小写、长值、正则错误 |
| `src/document.rs` | `pointer_lookup_rejects_invalid_escapes_and_array_indices`、`jsonpath_queries_support_selectors_filters_and_exact_locations` | Pointer、过滤、顺序重复、错误和零结果 |
| `src/indexed.rs` | `supported_queries_preserve_order_slices_recursion_and_duplicate_results` | Memory／Indexed 公共子集对照 |
| `src/engine/search/tests.rs` | `three_backends_keep_order_scopes_unicode_duplicates_and_one_hit_per_node`、`number_spelling_matches_old_semantics` | 三后端搜索语义对照 |
| `src/engine/search/tests.rs` | `streaming_literal_crosses_blocks_and_regex_failure_keeps_committed_prefix`、`million_hits_have_bounded_memory_and_random_result_pages` | 跨块、部分结果、百万命中分页 |
| `src/engine/search/tests.rs` | `compatibility_collector_accepts_more_than_65536_hits_within_budget` | 本次将旧拒绝断言改成 65,537 条结果的数量及首末节点断言 |
| `src/worker.rs` | `obsolete_queued_results_cannot_replace_the_current_result`、`full_result_channel_does_not_retain_documents` | 迟到消息与通道持有引用 |
| `src/app.rs` | `background_operations_update_navigation_and_loading_invalidates_old_results`、`jsonpath_results_survive_selection_and_invalid_queries_clear_old_results` | UI 导航、切换、查询结果保留及错误清理 |

缺口不能写为通过：两引擎同时运行时的取消隔离、拆分后错误和耗时互不覆盖、查询分页重复项／反向顺序、超 256 MiB Indexed 结果及预算耗尽，均需后续用户验收或明确授权测试。保留已有用例不代表已覆盖所有新边界。

## 7. 交给用户的验收清单

以下为待执行步骤，不是本次执行记录。样本／测试 JSON、基准 JSON 报告只存本地，不提交。

1. 同一文件分别用 Memory 与 Indexed 打开，比较 normal／case／regex、Keys／Values／All，包含 Unicode、转义、数字、容器键和数组下标；核对结果顺序及每节点一次。
2. 使用超过 65,536 个值命中的本地数据，检查默认 GUI 和主 CLI 返回全部结果；在较小共享预算下应明确失败，不静默截断。
3. 对公共 JSONPath 子集比较根、字段、联合重复选择、通配符、递归、正反切片、零结果；过滤在 Memory 执行，在 Indexed 明确拒绝。
4. 对足够大的 Indexed 中间结果，在可用预算足够时确认不再触发旧 256 MiB 固定错误；预算不足仍明确失败。该项尚未实测，不承诺当前机器必然能完成。
5. 搜索期间执行查询、反向操作、分别编辑／清空输入、快速切换文件，观察结果／错误是否串用；记录当前缺陷，作为 B 的迁移输入。
6. 验证普通选中保留查询结果、错误查询清除旧节点、n/N 导航、`.` 聚焦及预览截断后的完整导出。
7. 大文件专项如执行，采用十进制 100 MB／300 MB／500 MB，低命中和高命中分别记录；S0 不恢复。实际版本、样本大小、缓存口径及未测项目如实保存。

## 8. 进入阶段 B 的边界

阶段 A 的文档和追加实现完成后，先由用户验收。阶段 B 依次拆 QueryRequest／QueryWorker、整理 SearchHub 接入与独立状态；C 再提取公共结果存储，D 迁移查询适配器，E 增加大文件流式查询。没有用户明确指示，不在本次跨入 B。
