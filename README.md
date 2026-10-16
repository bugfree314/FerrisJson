# FerrisJson

当前版本：**0.7.8**。

0.7.8（本次版本提交）收录阶段 F：[分页树查询与兼容入口收口](docs/search-query-stage-f.md) 已实现。`--sparse`／`--paged-tree [--indexed]` 增加独立 JSONPath 输入、16 条结果分页、点击定位、取消及完整／部分结果复制和导出；源验证完成前等待最新请求。分页树查询复制最多 8 MiB，较大结果用导出；`--sparse-pages` 仍为旧有限页浏览器。新增显式的新语义 collect 与旧 compatibility 入口，没有扩展 E 的文件后端语法范围。按用户明确要求提交，未运行测试、构建或性能测量，不将提交授权写为实测通过。

0.7.7 / `b730729` 收录阶段 E：[流式 JSONPath 与结果分页](docs/search-query-stage-e.md) 已接入默认 GUI、主 CLI 和 `sparse_index --query`。查询逐条写入磁盘结果，GUI 每页 16 条，支持取消及完整／部分结果复制和导出；Indexed／Sparse 新增存在性、标量比较及逻辑过滤。Memory 的过滤／高级语法仍走原库完整收集，文件后端不支持函数或容器比较；旧 NodeIds 兼容接口仍保留 D 的范围，分页树 GUI 查询面板留待 F。按用户明确要求提交，未运行测试、构建或性能测量，不将提交授权写为实测通过。

0.7.6 / `f21db89` 收录阶段 D：[独立查询引擎与后端适配](docs/search-query-stage-d.md) 已实现。GUI／CLI 查询经 QueryEngine 编译和分派，Document 保留兼容包装；Memory／Indexed 原有语义与 Sparse 单 Pointer 范围保持，JSONPath 仍完整收集结果，流式查询和新过滤语法留给 E。按用户要求提交，未运行测试、构建或性能测量，不将提交授权写为实测通过。

0.7.5+1 / `5bfeb23` 的 UI 调整：默认界面的搜索栏改为一行，输入框内 × 清空，× 右侧转圈表示检索中且可点击取消；Export matches 位于 Value preview 的 Export 旁。保留完整／部分导出语义；按用户要求提交，未运行测试、构建或性能测量。取消保留已提交部分结果，清空会清除搜索状态；没有新的性能结论。

0.7.5 / `0764d03` 收录拆分阶段 C：公共结果存储、分页与前缀游标，默认 GUI 搜索接入 SearchHub 和后台结果读取，支持按序号导航、取消后部分结果及完整／部分值和路径导出。按用户要求提交，未运行测试、构建或性能测量。JSONPath 和主 CLI 仍使用完整结果兼容接口，旧 GUI 树／行数组仍驻留内存；不宣称整个应用固定内存或性能提升。

0.7.4 / `2ff76c0`完成搜索／JSONPath 拆分 A 的行为基线，取消兼容搜索 65,536 条命中及 Indexed 查询 256 MiB 结果 ID 固定上限，保留共享资源预算。按用户指示直接提交，不运行测试、构建或性能测量；这不代表专项验收已通过。

**0.7.3 / `ce30083`** 基于 **0.7.2 / `a8f2980`**，为非 JSONPath 检索增加树形区和预览／结果区的命中文本高亮，支持大小写、正则及 All / Keys / Values 范围；修改检索条件时清除旧高亮，预览保留 JSON 语法配色，完整 JSON 字符串中的转义映射回显示位置。同时移除未使用的 `json_preview` 包装函数。高亮仅覆盖当前显示的文本，截断部分不会补读或展开；零宽正则命中不着色。按用户要求，本次未运行测试、格式检查、Clippy、构建或 GUI 验收，没有新增性能测量，不宣称性能提升。

**0.7.2 / `a8f2980`** 基于 **0.7.1 / `47fa5ff`** 收录 S6 实现与自动验证：`--sparse` / `--paged-tree [--indexed]` 支持增量搜索、磁盘结果分页、匹配跳转和完整/部分导出。本次及以后的专项基准默认 **100 MB / 300 MB / 500 MB**，保留历史样本口径。S0 保持暂停。

0.7.0 / `46b149c` 已提交 S4 渐进打开、持久缓存与恢复，并创建本地 tag。0.7.1 / `47fa5ff` 收录 S5 分页树与异步预览：`--sparse` 使用真正分页的树，原 S4 有限页界面移至 `--sparse-pages`；`--paged-tree` 可用同一树模型查看旧后端。S5 自动及专项验收、剩余限制见 [S5 记录](docs/performance-optimization.md#25-s5--分页树与异步预览工作区)。S0 保持暂停。

已提交的 0.6.2 新增 `.` 快捷键：输入框外按下时聚焦 JSONPath 栏，保留查询内容且不插入句点；输入框内正常输入，弹窗打开时不触发。按 Esc 仍可取消待完成的 Vim 按键序列。帮助速查页同步更新；该提交无新的性能优化或性能测量，S0 保持暂停。

0.6.0 实施 **S2：后台文件读取与异步预览**。选中值、显示路径和可见行摘要在独立后台任务中准备，未就绪时显示加载占位；快速切换只保留最新请求。文档块 I/O 采用有界准入，交互任务优先。树行键名超过 160 字符时缩略显示，显示路径最多 16 KiB，**Copy path** 仍复制完整路径。S0 性能观测按用户要求暂停，直到用户明确恢复；本阶段不宣称新的加速比或内存成绩。

0.6.1 完成 **S3 独立稀疏索引 CLI 原型**：容器、数组检查点和对象字段目录落盘，提供 JSON Pointer 与有界子节点页。用户将本阶段验收改为 50/100/500 MB；两种数组结构在固定 1 MiB 受管理内存预算下通过扫描与首/中/末定位，账本数据不等于进程 RSS。0.6.1 提交时原型尚未接入 GUI、持久缓存、搜索或 JSONPath；使用及历史限制见 [S3 优化记录](docs/performance-optimization.md#22-s3--固定内存扫描与稀疏索引-cli-原型)。S0 保持暂停。

大文件架构与验收计划见 [大文件分步重构方案](docs/large-file-refactor-plan.md)。0.5.2 完成 S1 的公共引擎接口、会话标识和共享资源预算；默认两个后端仍采用完整索引。S3–S5 的稀疏引擎采用独立实验入口；S5 已接入分页树，默认策略调整仍属 S8。

0.5.2 将 GUI、CLI 和基准统一到公共引擎库，新增会话/节点/任务标识及子节点分页接口；共享预算在超限时明确报错，并跟随结果/子树引用释放。默认过渡配额为受管理内存 8 GiB、临时磁盘 64 GiB，尚无 GUI 配额控件；预算含估算及预留池，不是总进程 RSS 上限，部分 serde/JSONPath/GUI 内部分配仍需实测观察。

0.5.1 默认关闭实时检索，按 Enter 执行；搜索栏 **Live search** 可开启 200 ms 防抖检索，开关自动保存。旧配置默认关闭。

0.5.0 新增可选的 **Indexed files** 文件索引模式，不为整个文件创建完整 `Value`。300 MiB / 约 2,141 万节点的三次文档引擎对照中，加载后工作集约 4,637 → 985 MiB，峰值约 4,637 → 1,516 MiB；打开耗时约 5.41 → 5.57 秒，未测出加速。使用范围见下方“文件索引模式”，详细测量见 [优化记录](docs/performance-optimization.md#17-050--文件范围索引与按需子树)。

0.4.3 新增后台流式导出（临时文件成功后替换，可取消），超过 8 MiB 的完整 JSON 复制提示，1 MiB 字符串容量预算的 LRU 树行缓存，以及转移粘贴缓冲区；可见行、搜索及查询结果编号改为 u32，可见行重建复用缓冲区。超大复制可选择继续完整复制、改为导出或取消；粘贴语法错误会恢复原文本供修正。

0.4.2 的内存优化：节点由 64 B 压缩为 28 B，子节点编号由 8 B 压缩为 4 B，并增加工具栏 **Low memory load** 开关（默认关闭，设置会保存）。开启后加载新文件前关闭旧文档，等待旧后台任务释放引用，再销毁旧文档；等待期间显示状态，加载失败后需要重新打开旧文件。关闭时仍可在新文件加载期间浏览旧文档，失败后保留旧文档。

<img src="assets/FerrisJson.png" alt="FerrisJson 图标：花括号中的橙色螃蟹" width="96" height="96">

一个使用 **Rust 编写并编译为原生可执行程序**的本地 JSON 查看器，提供树形浏览、JSONPath 查询和 Vim 风格导航。

本项目由作者与 **OpenAI Codex 协作开发**，共同完成需求迭代、代码实现、测试与文档编写。

打开文件或粘贴 JSON，在左侧浏览结构，右侧同时查看选中值和查询结果。数据解析、搜索与查询在本地完成，无需账号。

## 功能

0.4.1 在解析后、建索引前释放源文本，降低大文件加载峰值，并新增可重复的 Windows 文档引擎内存基准。300 MiB / 约 2,141 万节点文件的三次对照中，峰值工作集约从 5,753 MiB 降至 5,454 MiB（减少 299 MiB / 5.2%）；加载完成后的占用基本不变，未测出打开速度提升。这是文档引擎测量，不包含完整 GUI。详细结果见 [优化记录](docs/performance-optimization.md)，版本历史和后续计划见 [TODO](todo.md)。

- **树形浏览**：对象、数组展开与折叠，键和值按字符串、数字、布尔、null、对象、数组类型配色。
- **全文搜索**：支持正反向查找、大小写切换、正则表达式，以及仅搜索键或值；匹配折叠节点时自动展开并定位。默认按 Enter 执行；搜索栏 **Live search** 可开启输入停止 200 ms 后的后台检索，开关会保存。修改搜索文本或条件会取消旧搜索并清除旧匹配；实时检索关闭时等待再次按 Enter。
- **JSONPath 查询**：支持通配符、递归查找、数组切片、负索引、多选择器和过滤条件，同时兼容 JSON Pointer。
- **双预览区**：右侧上方始终显示选中值，下方保留 JSONPath 结果。默认自动换行，两区各有 **Wrap** 开关，关闭后可水平滚动；开关会保存。两区独立滚动，可拖动上下分隔条和面板左边缘调整大小。
- **Vim 操作**：数字前缀、`hjkl`、`gg/G`、`n/N`、浏览历史、翻页、展开折叠与复制；状态栏显示待完成的按键、最近执行的命令、当前可见树行号/总行数、Top/Bot 或位置百分比，以及选中节点的 JSON 路径。长路径省略显示，悬停查看当前显示路径；超过 16 KiB 的路径显示截断标记，Copy path 可复制完整路径。行号随展开/折叠变化，是树中的可见行号，不是源文件行号。
- **快捷键速查页**：F1 打开，Esc 关闭，输入框内保持正常文字输入。
- **输入与输出**：文件选择、拖放、粘贴、标准输入；复制完整 JSON、复制路径、导出选中值或所有查询结果。
- **多语种与主题**：中日韩及其他常用文字的系统字体回退，深浅色主题，保留对象键顺序和大整数精度。
- **可自定义配色**：内置 Ferris、Monokai、Solarized、Claude 风格、VS Code 风格，以及 Custom 配色；每套均有深浅两种模式，统一应用到树、值预览与 JSONPath 结果。
- **耗时显示**：右下角显示最近一次打开文档、搜索或 JSONPath 查询的耗时。
- **Windows 桌面启动**：不创建命令行窗口；命令行处理可以使用已有终端或管道。

## 随机生成指定大小的 JSON

生成器包含全部 JSON 数据类型、多语种和转义字符串、嵌套对象及混合数组。随机记录采用流式写入，末尾用少量字符串填充，使文件字节数精确匹配目标大小，无需将整个文件保存在内存中。

```powershell
cargo run --release --example generate_json -- 37kb random-37kb.json
cargo run --release --example generate_json -- 1.06GB random-1.06gb.json
cargo run --release --example generate_json -- 64MiB random-64mib.json 42
```

大小单位不区分大小写：`KB/MB/GB/TB` 使用 1000 进制，`KiB/MiB/GiB/TiB` 使用 1024 进制；无单位表示字节。支持最多 9 位小数，非整数的字节数四舍五入。文件太小而无法容纳所有类型时会报错，`--help` 显示最小字节数。最后一个可选参数是 `u64` 随机种子，同一种子和大小可复现相同内容；随机数据不用于密码学。输出路径已存在时会报错，避免覆盖文件。

## 从源码运行

开发环境使用 Rust 1.99。Windows 需要 Rust MSVC 工具链及对应的 C++ 构建工具；macOS 需要 Rust 和 Xcode Command Line Tools（`xcode-select --install`）。

```powershell
cargo run --bin FerrisJson
cargo run --bin FerrisJson -- examples/sample.json
cargo run --bin FerrisJson -- examples/multilingual.json
```

### 构建产物

Cargo 包名与可执行文件名均为 **FerrisJson**。

```powershell
cargo build --release --bin FerrisJson
```

默认输出：

| 平台 | 文件 |
| --- | --- |
| Windows | `target/release/FerrisJson.exe` |
| Linux / macOS | `target/release/FerrisJson` |

Windows 构建后启动：

```powershell
.\target\release\FerrisJson.exe
.\target\release\FerrisJson.exe examples/sample.json
```

图标采用原创的“橙色螃蟹 + JSON 花括号”，窗口使用 PNG，Windows EXE 在构建时嵌入多尺寸 ICO。Windows 构建需要包含资源编译器的 Windows SDK。图标文件位于 `assets/FerrisJson.svg`、`assets/FerrisJson.png` 和 `assets/FerrisJson.ico`；SVG 是可编辑源文件。

图标可通过 SVG 源文件继续编辑，再导出 PNG 和多尺寸 ICO。正常构建直接使用仓库中的图标文件。

### 自动编译与打包（Windows / macOS）

在项目目录执行同一个命令，程序会自动识别当前系统和 Rust 工具链架构，编译 Release 版本并打包：

```sh
cargo run --release --locked --example package
```

产物位于 `target/release/bundle/`：

| 当前系统 | 产物 |
| --- | --- |
| Windows | `FerrisJson.exe`，同目录附带 README 和 LICENSE |
| macOS | `FerrisJson.app`，文档位于 App 的 `Contents/Resources/` |

macOS App 可在 Finder 中双击启动，或拖入「应用程序」目录。图标会从现有 PNG 自动生成 ICNS，版本号使用 `Cargo.toml` 中的版本。macOS 最低版本为 11.0；Apple Silicon 使用原生 Rust 工具链时生成 `aarch64` 产物，Intel Mac 生成 `x86_64` 产物。每次只打包当前平台与架构，不生成通用二进制，也不跨系统编译。

macOS 打包使用系统自带的 `sips`、`iconutil`、`plutil` 和 `codesign`，进行无需 Apple 开发者账号的本地 ad-hoc 签名；向其他用户公开分发时，Developer ID 签名与 Apple 公证需另行完成。Windows 沿用现有 EXE 图标资源编译配置。两种平台均不生成 ZIP；重复运行会更新同名产物。

## 桌面操作

1. 点击 **Open file**、拖放文件，或点击 **Paste JSON** 粘贴内容。树视图中 Ctrl+V 也可加载剪贴板 JSON。
2. 点击键或值选择节点；点击箭头、双击容器键名，或使用键盘展开与折叠。
3. 在搜索栏输入文字，使用 **Aa** 切换大小写、`.*` 切换正则模式、**All / Keys / Values** 限定范围。
4. 在 **JSONPath** 栏输入表达式，点击 **Go** 或按 Enter 执行；下方预览区显示结果数量和 JSON 数组。
5. 点击 **Copy JSON / Copy path / Export** 操作选中值；查询区的 **Copy / Export** 操作完整结果数组。
6. **Reset**、查询区的 **Clear** 或清空表达式会清除查询结果，上方选中值预览继续保留。

输入框里的 Ctrl+V 只粘贴文本。切换树节点不会清除已经执行的 JSONPath 结果。F5 可重新读取当前源文件。

搜索和 JSONPath 查询在后台执行，进行中显示加载指示。清空查询、Reset 或打开新文档会丢弃旧任务的结果；连续提交只保留最新的待执行任务。现有 JSONPath 引擎无法在表达式求值过程中强制中断，过期结果会被丢弃，后续结果定位可取消。

### JSONPath 示例

| 表达式 | 含义 |
| --- | --- |
| `$.users[0].name` | 第一个用户的名字 |
| `$.users[*].name` | 所有用户的名字 |
| `$..email` | 任意层级中的 email |
| `$.users[0:5]` | 前五个用户 |
| `$.users[-1]` | 最后一个用户 |
| `$.users[?(@.active == true)].name` | active 为 true 的用户名字 |
| `$["special.keys"]["a/b"]` | 含特殊字符的键 |
| `/users/0/name` | 使用 JSON Pointer 定位 |

也可省略起始 `$`，例如 `.users[0].name`。内存模式的 JSONPath 采用 [serde_json_path](https://docs.rs/serde_json_path/0.7.2/serde_json_path/) 的 RFC 9535 查询语法，不执行 JavaScript。零匹配显示 `[]`；无效表达式显示错误。文件索引模式的查询范围见下节。

### 文件索引模式（0.5.0）

工具栏 **Indexed files** 默认关闭，设置会保存；开启后，下一次文件打开、拖放或重载使用索引后端，当前文档不会自动转换。粘贴和标准输入继续使用内存模式。命令行可显式选择：

```powershell
./target/release/FerrisJson.exe large.json --indexed --query '$.users[*].name'
./target/release/FerrisJson.exe large.json --indexed --get '/users/0'
./target/release/FerrisJson.exe large.json --indexed --pretty
```

索引模式先在系统临时目录创建与输入大小相当的磁盘快照，再严格扫描 JSON/UTF-8 并建立节点和字节范围索引。打开时显示快照、扫描进度，可用 **Cancel open** 取消。快照在文档及后台引用释放后删除；原文件之后被修改不会改变当前文档。初次打开仍需完整读取、校验文件，需要额外磁盘空间，不能保证更快。**Low memory load** 可同时开启以减少切换时两份文档重叠；开启后失败或取消需要重新打开旧文件。

支持 JSON Pointer、字段、通配符、递归、多选择器、负索引和切片，保留顺序、重复结果、重复键的最后值及大整数精度。已提交的 0.7.6 / D 不支持过滤和函数。0.7.7 / E 的默认 GUI／主 CLI 流式入口增加存在性、标量比较和逻辑过滤，函数、多节点过滤操作数及容器比较仍明确报错；需要更广语法时关闭 **Indexed files** 后重新打开文件。不会自动完整物化文件文档作为兼容回退，详细边界见 [E 记录](docs/search-query-stage-e.md)。

普通文本搜索按解码后的字符串流式检索，保留 Unicode 大小写语义。字符串正则仅支持原始范围不超过 256 KiB 的单值，超限明确报错；超过该范围的数字也不能完整检索。小子树按需解析（原始范围最多 256 KiB、最多 16,384 节点），缓存按估算载荷 8 MiB / 最多 512 项淘汰；大子树直接限量预览或流式输出。输出保持 JSON 值语义，可能保留源标量的转义和数字拼写，与内存模式的格式化文本不保证逐字一致。

完整结构索引仍驻留内存：结构数组容量预算 1.5 GiB，共享键解码字节预算 64 MiB，单键最多 1 MiB、最多一百万种键。0.7.4 已取消索引查询单个中间结果数组的 256 MiB 固定上限，扩容仍受共享资源预算保护，自动验证未运行。缓存、索引和结果预算都不是总进程内存上限；数组扩容、旧/新结果、后台引用和系统文件缓存另占空间。E 的新流式查询入口使用磁盘结果分页，旧 NodeIds 兼容入口仍完整收集；结构索引未分页，也未自动按文件大小切换。

### 实验性分页树与持久缓存（S5，0.7.1）

```powershell
cargo run --release --offline --locked --bin FerrisJson -- your-file.json --sparse --cache-dir target/my-s5-cache
# 旧 S4 有限页界面
cargo run --release --offline --locked --bin FerrisJson -- your-file.json --sparse-pages
# 同一分页树查看旧内存 / 完整文件索引后端
cargo run --release --offline --locked --bin FerrisJson -- your-file.json --paged-tree
cargo run --release --offline --locked --bin FerrisJson -- your-file.json --paged-tree --indexed
cargo run --release --offline --locked --example sparse_index -- your-file.json --cache-dir target/my-s5-cache --events --pointer /items/0 --raw
```

缓存目录应专用于一个当前输入；切换到内容不同的文件会安全重建。省略 `--cache-dir` 可关闭持久缓存，仍使用私有快照渐进扫描。默认查看器和 `--indexed` 的入口保持原样；`--sparse` 只接受文件并打开实验浏览器，不接受 stdin 或现有 CLI 查询选项。

新建会话先复制快照，再后台扫描。数组已提交的完整元素可在全量校验前导航；对象字段在容器闭合、重复键定稿后可读。总数未定时显示下界；G、超过已提交范围的行跳转和路径可等待 Ready，Esc 或新的跳转可取消等待。末尾错误转 Failed 并清空树，禁止继续输出。

每次交付最多 64 个真实节点，页缓存最多 8×64 个节点；不会建立完整 rows/expanded 数组或把范围概要当作 JSON 节点。展开例外和祖先权重保存在临时磁盘树，`zR/zM` 切换逻辑策略。`hjkl`/方向键、数字前缀、`gg/G`、H/M/L、翻页、历史和 Row 整数跳转可用；滚轮移动局部窗口，不将全局行数转换为 egui 浮点高度。稀疏模式的 Path 接受 JSON Pointer（例如 `/items/0`）；旧后端保留已有定位支持范围，`.` 聚焦 Path。S6 工作区支持 `/` 正向、`?` 反向及 Ctrl+F 聚焦搜索，`n/N` 沿/逆搜索方向跳转，可加数字计数；一般 JSONPath 待 S7。

预览、行摘要和路径由独立后台交互线程准备，内容、主题、宽度、Wrap 与 DPI 决定布局缓存。选中预览最多显示 16 KiB，可能不是完整 JSON；Ready 后复制/导出使用独立快照范围和完整值，复制最多 8 MiB，超限提示使用 Export。输出线程与导航分离，后续选择不改变已提交输出。旧后端使用相同 Provider，但仍承担旧完整 DOM/索引的存储成本。
缓存保存源快照、两代结构索引和解析栈检查点；严格复用逐字节比较整个源文件与快照，并检查索引记录。复用仍有完整读取成本，不能保证比重新扫描快。恢复只使用已经提交的结构边界，可退回最后检查点重扫；对象重复键处理状态一并恢复。目录由系统文件锁限制单会话访问，异常退出后可重新获取锁；不兼容或损坏的状态回退重建，配额不足明确报错。

当前只实现稳定快照和严格复用，没有采样快速复用或直接原文件读取。Windows 打开源时拒绝并发写入/删除；Unix 的复制/比较前后大小与时间检查不能排除绕过元数据检测的并发改写，应在源文件静止时操作。持久目录包含原始 JSON 内容；活动会话核算该目录的受管理文件，关闭后目录保留，下次重新计费。没有跨目录的全局缓存淘汰。GUI 配额为受管理内存 256 MiB、磁盘 64 GiB，均不是总 RSS 或整个磁盘空间上限。

磁盘索引/检查点格式升级为 v2，不兼容的旧缓存安全重建。数组检查点和对象序号由磁盘 AVL 目录定位，容器跳转不重扫整个容器；对象键哈希碰撞仍可能退化。检查点和后台只读视图目前复制完整索引，首次缓存构建还复制已有快照，额外 I/O/磁盘开销需要计入；高节点密度和单个巨大标量另有成本，不能保证任意 10 GB 文件即时打开。S5 测量使用过本地 `scripts/verify-s5.py`，该脚本目前不在工作区且未纳入本版本；历史结果保持实际测量版本 0.7.0。headless egui CPU 数据不代表原生窗口/GPU 帧率；人工及跨平台未验收。上方 CLI 可检查局部交付、缓存复用及 Pointer 输出。`--stop-after-page` 可检查取消恢复，`--crash-after-page` 可模拟异常退出；大输入和报告只保存本地。

### 增量搜索（S6，0.7.2）

分页树的搜索默认按 Enter / Search 执行；Live 开启 200 ms 防抖。支持 All / Keys / Values、大小写和 Regex；按解码后的键/标量值匹配，容器不把整个子树当作值搜索，重复键保持首次位置/末次值语义，每节点最多一次命中。

搜索等待源完整验证（Ready），首个命中立即交付，后续每批最多 128 条，事件队列最多 4 条。结果和祖先链保存在临时磁盘文件，热缓存最多 8×128 条；显示页最多 16 条，只按需生成截断路径与预览。扫描中显示至少多少命中；未到达的正向命中等待，逆向或循环需完整计数。Esc 取消待跳转，Stop search 保留已提交部分，Clear/编辑条件取消旧查询；完整导出等待完成，部分导出固定已提交前缀。磁盘超限/错误显式显示部分结果。

Export all values / paths 和 Export partial values / paths 分别输出 JSON 数组，完整路径不使用显示缩略字符串；通过临时文件成功替换目标。文件后端字符串/数字 Regex 原文范围限定 256 KiB，稀疏键 Regex 同样受限；旧后端已驻留的键直接匹配，字面量跨块解码；搜索词最多 64 KiB，完整导出路径最多 8 MiB，复制与其他既有输出限制不变。0.7.4 已取消兼容收集器的 65,536 条命中上限，扩容受共享资源预算保护，自动验证未运行；0.7.5 的默认 GUI 已接入磁盘结果，主 CLI 仍完整收集。默认后端策略仍待 S8；原生 GUI/GPU、跨平台及真实磁盘故障仍需验收。验证与专项测量见性能记录第 26 节。

搜索与 JSONPath 拆分：[A 行为基线](docs/search-query-separation.md) 已提交为 0.7.4 / `2ff76c0`，[B 独立请求／Worker](docs/search-query-stage-b.md) 已提交为 `bf5f69d`。[C 公共结果存储及默认 GUI 搜索迁移](docs/search-query-stage-c.md) 收录为 0.7.5（本次版本提交），未运行测试或构建。

0.7.5 中，默认 GUI 也通过 SearchHub 增量写入磁盘结果，按序号导航，后台读取当前视口的命中信息，不再完整收集搜索结果。扫描中显示至少发现的条数，取消／失败标注部分结果；新增 Cancel search 和 Export matches 菜单，完整值／路径仅完成后可导出，部分导出固定已提交前缀。主 CLI 的文本搜索仍使用完整收集兼容接口；0.7.7 / E 将默认 GUI 和主 CLI 的 JSONPath 接入流式／分页入口，Memory 高级语法保留完整收集例外。旧树与可见行数组仍驻留内存，不代表整个 GUI 已固定内存，也没有新的性能测量。

## 配色设置

主题与配色控件位于顶栏右侧。在配色下拉框选择 **Ferris / Monokai / Solarized / Claude / VS Code / Custom**，使用 **Light / Dark** 切换深浅模式。方案、模式与两个预览区的换行开关会自动保存，下次启动时恢复。

点击 **Colors** 打开配色编辑器，可独立调整字符串、数字、布尔、null、对象、数组、文字/标点、背景、交替行、选中行及搜索高亮颜色，并查看即时示例。修改内置方案的颜色会生成 **Custom**；深色和浅色的自定义颜色分别保存。**Use as custom** 以当前内置方案作为编辑起点，**Restore Ferris defaults** 恢复默认配色；点击 **Save settings** 保存自定义改动。Esc 可关闭编辑器。

内置配色是针对 JSON 类型设计的风格方案，不是对应产品的官方完整主题。Claude 风格使用暖色纸张、陶土色及柔和点缀；Monokai 为经典风格，不使用 Monokai Pro 主题资源。

设置保存在用户配置目录的 `FerrisJson/colors.json`：Windows 为 `%APPDATA%/FerrisJson/colors.json`，macOS 为 `~/Library/Application Support/FerrisJson/colors.json`，Linux 为 `$XDG_CONFIG_HOME/FerrisJson/colors.json`（未设置时使用 `~/.config`）。读取失败时使用 Ferris 默认方案并显示提示；保存失败时显示错误。

## 快捷键

Vim 操作仅在树视图中生效。数字表示重复次数，例如 `12j` 向下移动十二个可见节点，`3l` 连续展开或进入三级。

| 按键 | 操作 |
| --- | --- |
| `h j k l` / 方向键 | 折叠或回到父节点 / 下一个 / 上一个 / 展开或进入子节点 |
| 数字 + `h j k l n N` | 重复移动或搜索跳转，次数上限 10,000 |
| `gg` / `G`、Home / End | 首个 / 最后一个可见节点 |
| `25gg` / `25G` | 第 25 个可见节点 |
| `H` / `M` / `L` | 视口上方 / 中间 / 下方节点 |
| `[` / `]` | 浏览历史后退 / 前进 |
| `/` / `?` | 向前 / 向后搜索 |
| `n` / `N` | 沿上次搜索方向 / 反方向继续，支持数字前缀 |
| Enter | 确认搜索并回到树；JSONPath 输入框内执行查询 |
| F3 / Shift+F3 | 下一条 / 上一条匹配 |
| Ctrl+F | 聚焦搜索栏 |
| `.` | 输入框外聚焦 JSONPath 栏；输入框内正常输入句点 |
| Ctrl+D / Ctrl+U | 下 / 上半页 |
| PageDown / PageUp、Ctrl+E / Ctrl+B | 下 / 上一整页 |
| Space / `za` | 切换选中容器的展开状态 |
| `zo` / `zc` | 展开 / 折叠选中容器 |
| `zR` / `zM` | 展开 / 折叠整个文档 |
| `yy` / Ctrl+C | 复制完整选中 JSON |
| `yp` / Ctrl+P | 复制选中路径 |
| Ctrl+O | 打开文件 |
| Ctrl+V | 加载剪贴板 JSON，输入框外生效 |
| F5 | 重新加载源文件 |
| F1 | 打开 / 关闭快捷键速查页 |
| Esc | 关闭帮助或粘贴窗口、退出输入框、取消待完成的 Vim 命令 |

Ctrl+F 保留查找功能，整页向下翻动使用 PageDown 或 Ctrl+E。

## 命令行

不带处理选项时打开桌面窗口。处理选项输出到标准输出，发生错误时退出码为 1；四个处理选项互斥。

```powershell
cargo run --bin FerrisJson -- examples/sample.json --get '$.users[0].name'
cargo run --bin FerrisJson -- examples/sample.json --query '$.users[*].name'
cargo run --bin FerrisJson -- examples/sample.json --query '$.users[?(@.active == true)].name'
cargo run --bin FerrisJson -- examples/sample.json --search email
cargo run --bin FerrisJson -- examples/sample.json --pretty
Get-Content examples/sample.json -Raw | cargo run --bin FerrisJson -- --get '$.users[1]'
```

| 参数 | 输出 |
| --- | --- |
| `--get PATH` | 恰好一个匹配值的格式化 JSON；零个或多个匹配返回错误 |
| `--query PATH` | 所有匹配值组成的 JSON 数组；零匹配输出 `[]` |
| `--search TEXT` | 忽略大小写搜索键和标量值，逐行输出匹配路径 |
| `--pretty` | 格式化整个 JSON 文档 |
| `--help` | 参数帮助 |
| `--version` | 程序版本 |

`-` 可显式指定标准输入。Windows GUI 子系统程序在 PowerShell 中直接调用时可能不会等待退出；用管道或 `Start-Process -Wait` 可等待完成。

## 开发检查

```powershell
cargo fmt --check
cargo test --offline
cargo clippy --offline --all-targets -- -D warnings
```

首次使用 `--offline` 前需要下载依赖。测试覆盖特殊键名、Unicode、大整数、JSONPath、搜索方向、Vim 计数与历史、预览截断、后台任务替换与取消、搜索防抖，以及界面分隔条等交互。

Windows 文档引擎内存基准（Release、每种模式独立进程运行三次）：

```powershell
./scripts/measure-memory.ps1 -File random_300MiB.json -Runs 3
./scripts/measure-memory.ps1 -File target/memory-switch20MiB-seed42.json -Scenario switch -Runs 3 -Output target/memory-switch.json
./scripts/measure-memory.ps1 -File random_300MiB.json -Scenario tree -Runs 3 -Output target/memory-tree.json
./scripts/measure-memory.ps1 -File target/memory-switch20MiB-seed42.json -Scenario export -Runs 3 -Output target/memory-export.json
```

报告默认写入 `target/memory-benchmark.json`，包含阶段耗时、峰值/加载完成后的工作集和私有内存、中位数及最小/最大值。`retain` 模拟旧源文本生命周期，`release` 在解析后释放源文本再建索引；两者都使用当前代码的节点布局。`switch` 比较同一文件加载两次时保留/提前释放旧文档的峰值；`tree` 测量全展开、重建及匹配所有节点时的数组容量；`export` 比较完整格式化 String 与流式根节点导出。固定种子数据生成方式、测量范围和复现说明见 [大文件优化说明](docs/performance-optimization.md)。

每次带版本号的提交必须同步更新 Cargo.toml、Cargo.lock、README、优化记录和 TODO，注明实际变化、验证结果、测量范围及剩余事项；项目协作规则见 [AGENTS.md](AGENTS.md)。

样本、测试数据及基准 JSON 报告只保存在本地，不纳入本次及后续提交；复现命令和测量结论保留在文档中。

## 当前限制

- 一次读取一个 UTF-8 JSON 文档，支持 UTF-8 BOM；不支持 JSONL、URL 加载或 JavaScript 处理。
- 文件读取与解析在后台执行，节点使用紧凑索引和共享键名；完整路径和预览按需生成，树行标签最多缓存 512 行，字符串容量预算为 1 MiB，条目元数据和临时绘制分配另计。文档和节点索引仍完整保存在内存中，尚未实现超大文件流式解析。
- 使用 serde_json 默认嵌套深度限制。
- 紧凑索引最多支持 4,294,967,295 个节点；超限返回明确错误，不截断编号。完整 Value 仍需驻留，紧凑索引不构成总内存预算。
- 选中值与查询结果的预览最多输出约 200 KB，截断时显示提示；复制与导出仍保留完整内容。
- 流式导出失败或在替换前取消时保留原目标文件；最终替换完成后不会因迟到的取消回滚。继续超大复制仍需要完整输出内存。CLI 的 JSON 输出也逐项写入缓冲标准输出。
- 多语种字形覆盖取决于系统安装的字体。Windows 使用微软雅黑、Malgun Gothic、游ゴシック及其他后备字体；其他平台尝试常见系统字体或 Noto 字体。系统字体不随项目分发。
- 配色和深浅模式可保存到用户配置目录；面板分隔位置仅在当前会话生效。
- 打开耗时包含文件读取、解析、树索引和初始预览准备；查询耗时包含检索与结果定位，不包含文件选择对话框及屏幕绘制。

## 致谢

界面与交互借鉴 [JSONBolt](https://jsonbolt.com/docs-gui) 和 [fx](https://fx.wtf/how-to/using-fx-as-a-tui-for-json-data)。GUI 使用 egui/eframe，JSON 解析使用 serde_json，查询使用 serde_json_path。

## 许可证

本项目采用 **GNU General Public License v3.0 或后续版本（GPL-3.0-or-later）**。允许使用、修改和分发，包括商业用途；分发受 GPL 覆盖的程序或修改版本时，应遵守 GPL 的源码提供、许可证保留及相同许可等要求。软件按原样提供，不提供担保。完整条款以 [LICENSE](LICENSE) 与 [GNU 官方许可证](https://www.gnu.org/licenses/gpl-3.0.html) 为准。

历史版本的许可请以相应 Git 标签中的 LICENSE 文件为准。

第三方依赖及系统字体分别遵循其自身许可证。
