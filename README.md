# FerrisJson

当前版本：**0.5.1**。

实验分支 `codex/memory-on-demand` 更新（不发版）：新增内存优先的按需文件后端，提供容器页缓存、虚拟可见行、查找下一处、分批查找全部、压缩结果及常用过滤条件。不生成临时索引表或自动落盘结果，使用方式见下方“按需文件模式”。

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
- **Vim 操作**：数字前缀、`hjkl`、`gg/G`、`n/N`、浏览历史、翻页、展开折叠与复制；状态栏显示待完成的按键、最近执行的命令、当前可见树行号/总行数、Top/Bot 或位置百分比，以及选中节点的 JSON 路径。长路径省略显示，悬停可查看完整路径。行号随展开/折叠变化，是树中的可见行号，不是源文件行号。
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

### 按需文件模式（实验分支，不发版）

工具栏 **On-demand files**（原 Indexed files）默认关闭，开关会保存，只影响之后的打开、拖放或重载；当前文档不会自动转换。粘贴和标准输入继续使用原内存 Value 后端。文件模式先严格校验整个 JSON/UTF-8，只保留稀疏容器信息和数组检查点，不创建完整 Value 或完整节点/可见行数组。

不生成临时索引表、源文件磁盘快照或结果暂存文件。最多 **512 MiB** 的文件只读取一份原始字节到内存，打开后不受原文件修改影响；更大文件保持原文件的只读句柄并分块读取。Windows 会限制原文件的写入、截断和删除，直到文档及后台引用释放；Unix 检查句柄长度和修改时间，发现变化报错，但不能保证检测所有并发原地改写。首次仍需完整读取、校验，内存源预算是明确的存储上限，不是经过证明的性能分界线。

展开时加载直接子节点页，数组每 256 项保存一个检查点，并按滚动区域读取页面。容器信息和页面使用合计 **16 MiB / 最多 1,024 项**载荷缓存；折叠后的页面可按 LRU 淘汰。可见行采用虚拟映射，Expand all / zR 不生成全量行编号数组；gg/G、行号、浏览历史及折叠定位保留。默认只展开根节点，显式展开/折叠状态最多 16,384 项。

搜索默认按 Enter **查找全部**，后台从已校验源文本直接扫描解码后的键和值，按批次返回。界面显示“已找到”和扫描进度，完成前不将数量当作最终总数；可 **Cancel search**。**Find next** 找到目标就停止，之后 n/N 和数字前缀继续按需查找，支持方向与循环；反向查找按正向扫描确定前一个命中，不倒读 UTF-8。结果按 65,536 个节点分块，稀疏命中保存 u16 偏移，密集命中转为位图，内存预算 **64 MiB**；超限报错并保留明确标记的部分结果，不自动写盘或静默截断。

完整查找全部完成后可 **Export matches**，按文档顺序流式导出匹配节点的完整值组成的 JSON 数组，不展开为完整结果编号 Vec；部分结果及“下一处”模式不作为完整结果导出。用户主动导出仍使用原有临时输出文件/原子替换与取消机制。

查询支持 Pointer、字段、通配符、递归、多选择器、负索引和切片，以及字段存在性、`== != < <= > >=`、`! && ||` 和 `$` 根引用过滤。比较仅按需解析所需值（原始范围最多 256 KiB、最多 16,384 节点），不物化整条大记录或全文。数字比较保持整数/十进制精度，极端指数超出精确比较范围会报错；超出 IEEE-754 可互操作范围的大整数结果可能优于原 Value 查询库的浮点比较，与它不保证一致。函数暂不支持，可关闭 On-demand files 后重新打开以使用原完整 JSONPath 引擎。重复键保持首个键位置和最后值，联合选择保留重复结果。

普通文本值搜索支持跨块 Unicode/转义及原有大小写语义；字符串正则和数字完整匹配限单值原始范围 256 KiB，超限明确报错。流式 JSON 输出可能保留源标量的转义与数字拼写，不保证与重新序列化文本逐字一致。

稀疏元数据/活动检查点预算 **64 MiB**，活动对象键另以 64 MiB 和单键 1 MiB 限制；每个查询中间结果数组最多 64 MiB；查询计划缓存最多 256 KiB / 16 项，单表达式最多 16 KiB。以上为组件预算，源字节、临时解析、缓存元数据、系统文件缓存及 GUI 另计，不能解释为总 RSS 上限。超过预算须收窄操作或使用其他模式，不做隐藏完整物化回退。

```powershell
./target/release/FerrisJson.exe large.json --indexed --query '$.users[?(@.active == true)].name'
./target/release/FerrisJson.exe large.json --indexed --get '/users/0'
./target/release/FerrisJson.exe large.json --indexed --search '中文'
```

`--indexed` 使用新按需后端；CLI 的 `--resident-index` 保留旧常驻范围索引用于对照（仅适用于输出操作，会创建源快照）。详见 [优化记录](docs/performance-optimization.md#19-实验分支--内存稀疏索引与按需浏览)。Low memory load 可同时使用，提前释放旧文档；开启后加载失败或取消需要重新打开旧文件。

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
