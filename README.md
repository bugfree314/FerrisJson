# FerrisJson

<img src="assets/FerrisJson.png" alt="FerrisJson：花括号中的橙色螃蟹" width="96" height="96">

**本地 JSON 查看器，使用 Rust 构建，支持树形浏览、JSONPath 查询与 Vim 风格导航。**

在 Windows / macOS 上使用桌面窗口，在 Linux 或 macOS 可选终端版中使用 TUI，也可以通过 CLI 提取、搜索和格式化 JSON。数据解析、搜索和查询在本地完成，无需账号。

当前版本：**0.8.0** · [GPL-3.0-or-later](LICENSE) · [优化历史](docs/optimization-history.md) · [开发记录与 TODO](docs/todo.md)

## 功能

- **结构浏览**：展开与折叠对象、数组，按 JSON 类型着色；查看选中值、复制完整值或路径、导出子树。
- **搜索**：按键、值或全部内容查找，支持大小写与正则；默认按 Enter 执行，可开启 200 ms 防抖的 Live search，命中可定位与高亮。
- **JSONPath / JSON Pointer**：字段、通配符、递归、切片、多选择器和过滤；结果分页，支持跳转、取消、完整或部分结果输出。语法范围取决于后端，见下文。
- **键盘导航**：Vim 移动、数字前缀、浏览历史、相对行号、搜索与查询结果跳转，F1 查看帮助。
- **桌面体验**：文件选择、拖放和粘贴，独立值预览与查询结果区，自动换行、面板缩放、深浅主题和自定义配色。
- **终端体验**：自适应多面板、鼠标、结果分页、终端复制与粘贴，可用于本地终端或 SSH 会话。
- **数据保真**：保留对象键顺序和大整数精度，支持多语种字体回退。
- **大文件探索**：可选文件范围索引，以及实验性稀疏索引、分页树、持久缓存和增量结果存储。

0.8.0 收录 Linux TUI、macOS 可选 TUI、终端打包和 Cargo 快捷命令。该版本尚未完成完整构建、功能及跨平台验收；历史测量与具体限制见[优化历史](docs/optimization-history.md)。

## 快速开始

取得适合当前平台的可执行文件，或按下文从源码编译。在终端中运行：

```sh
# Windows 使用 FerrisJson.exe；下例假设程序位于当前目录
./FerrisJson data.json
./FerrisJson data.json --query '$.users[*].name'
./FerrisJson data.json --get '/users/0'
./FerrisJson data.json --search email
./FerrisJson data.json --pretty
```

不带处理参数时进入查看器：Windows / macOS 默认为 GUI，Linux 为 TUI；macOS 的 TUI 需编译时选择。Windows 也可双击 EXE，macOS 打包后可双击 App。

标准输入使用 Memory 后端，完整读入内存：

```sh
# macOS / Linux
cat data.json | ./FerrisJson -
cat data.json | ./FerrisJson - --query '$.users[*].name'
```

```powershell
# Windows PowerShell
Get-Content data.json -Raw | .\FerrisJson.exe - --query '$.users[*].name'
```

示例中的 `data.json` 为用户本地文件。Windows 程序使用 GUI 子系统；PowerShell 直接调用时可能不等待退出，捕获 CLI 输出时可使用管道或 `Start-Process -Wait`。

### 桌面使用

1. 点击 **Open file**、拖放文件或使用 **Paste JSON**。macOS 用 `⌘O` 打开文件，Windows 用 `Ctrl+O`。
2. 在树中选择节点，点击箭头或使用方向键展开、折叠；右侧查看选中值。
3. 搜索框输入关键词后按 Enter；使用大小写、Regex 与 All / Keys / Values 控件调整范围。
4. JSONPath 输入框按 Enter 执行，默认 GUI 框内的 `×` 清空并取消查询；分页树入口也提供 **Run query**。通过结果页或 `F4` / `Shift+F4` 定位查询命中。
5. **Copy JSON / Copy path / Export** 操作选中值，结果区提供完整或部分结果复制与导出。截断预览不用于生成完整输出。

F1 打开快捷键帮助；JSONPath 旁的 **Cheatsheet** 提供可复制的语法示例。搜索和查询分别保留状态，普通节点选择不会清空查询结果。

### 常用快捷键

| 按键 | 操作 |
| --- | --- |
| `h j k l` / 方向键 | 折叠或父节点 / 下一项 / 上一项 / 展开或子节点 |
| `12j`、`gg` / `G` | 数字计数移动、首项 / 末项 |
| `[` / `]` | 浏览历史后退 / 前进 |
| `/` / `?`、`n` / `N` | 正向 / 反向搜索、沿 / 逆上次搜索方向继续 |
| `F3` / `Shift+F3` | 下一条 / 上一条搜索命中 |
| `F4` / `Shift+F4` | 下一条 / 上一条 JSONPath 结果 |
| `Space` / `za`、`zR` / `zM` | 切换展开状态、全部展开 / 折叠 |
| `yy` / `Ctrl+C`、`yp` / `Ctrl+P` | 复制 JSON、复制路径 |
| `F5`、`F1`、`Esc` | 重载、帮助、退出输入 / 关闭弹窗 / 取消待完成命令 |

输入框有焦点时优先正常文字输入。默认 GUI 的 `.` 聚焦 JSONPath；分页树和 TUI 的 `.` 聚焦单路径定位，TUI 用 `F2` / `:` 进入 JSONPath。

### 终端使用

`Tab` / `Shift+Tab` 或 `Alt+1/2/3/4` 切换树、预览、搜索和 JSONPath，`F9` 最大化面板。结果每页 16 条；结果面板用 `h/l` 或 PgUp/PgDn 翻页，`j/k` 逐项选择。`e` 导出完整结果，`E` 导出当前已发布部分；`Ctrl+X` 取消任务，`q` / `Ctrl+Q` 退出。

TUI 需要交互式 ANSI 终端和 `/dev/tty`；无交互终端时使用 CLI 处理选项。复制依赖终端支持 OSC 52，发送上限 100 KiB，较大值使用导出；字体、emoji、鼠标和功能键效果取决于终端。完整操作及边界见 [TUI 说明](docs/linux-tui.md)。

### JSONPath 与命令行

| 表达式 | 含义 |
| --- | --- |
| `$.users[0].name` | 第一个用户的名字 |
| `$.users[*].name` | 所有用户的名字 |
| `$..email` | 递归选择 email |
| `$.users[0:5]` / `$.users[-1]` | 前五项 / 最后一项 |
| `$.users[?(@.active == true)].name` | 满足标量条件的名字 |
| `$["special.keys"]["a/b"]` | 包含特殊字符的键 |
| `/users/0/name` | JSON Pointer |

可省略 JSONPath 起始 `$`，例如 `.users[0].name`。JSONPath 不执行 JavaScript。

| 参数 | 行为 |
| --- | --- |
| `--get PATH` | 输出恰好一个匹配值；零个或多个匹配时报错 |
| `--query PATH` | 输出全部匹配值组成的 JSON 数组；零匹配为 `[]` |
| `--search TEXT` | 忽略大小写搜索键和标量值，逐行输出匹配路径 |
| `--pretty` | 输出格式化 JSON |
| `--indexed` | 为文件选择范围索引后端 |
| `--sparse` | 打开实验性稀疏索引分页树，仅接受文件 |
| `--paged-tree` | 用分页树查看 Memory / Indexed 文件 |
| `--cache-dir DIR` | 指定 Sparse 持久缓存目录，与 `--sparse` 配合 |
| `--help` / `--version` | 参数帮助 / 版本 |

四个处理选项 `--get`、`--query`、`--search`、`--pretty` 互斥；错误退出码为 1。主 CLI 查询完成后才输出数组，不逐批输出部分结果。`--sparse` 不与主 CLI 处理选项组合；稀疏查询另有 `sparse_index` example，见[阶段 E](docs/search-query-stage-e.md)。

## 大文件模式与限制

| 后端 | 适用方式 | 主要代价与边界 |
| --- | --- | --- |
| Memory（默认） | 一般文件、粘贴、stdin；较广 JSONPath 语法 | 完整 `serde_json::Value` 与结构索引驻留；高级查询 / 过滤仍可能完整收集，库求值内部不可强制取消 |
| Indexed（`--indexed`） | 文件快照、范围索引、按需解析子树 | 不构建完整 Value，但完整结构索引仍在内存；初次打开需复制和完整校验，额外占用磁盘 |
| Sparse（`--sparse`，实验性） | 磁盘稀疏索引、分页树、缓存与恢复 | 先复制快照再扫描；数组安全边界可渐进交付，对象等待定稿；搜索 / 查询等待完整验证，仍有索引复制与高密度目录成本 |

```sh
./FerrisJson large.json --indexed
./FerrisJson large.json --sparse --cache-dir ./local-cache
./FerrisJson data.json --paged-tree
./FerrisJson data.json --paged-tree --indexed
```

桌面 **Indexed files** 设置对下一次文件打开生效，粘贴和 stdin 始终使用 Memory。**Low memory load** 会先关闭旧文档再加载，减少新旧文档重叠，失败后需重新打开旧文件。`--sparse-pages` 保留旧有限页 GUI，TUI 不支持该入口。

Indexed / Sparse 新查询入口支持普通选择器、存在性、标量比较和逻辑过滤；函数、多节点过滤操作数、对象 / 数组比较尚不支持，超出范围会报错，不自动完整物化文件。具体语义与资源限制见[阶段 E](docs/search-query-stage-e.md)和[阶段 F](docs/search-query-stage-f.md)。

文件后端的部分正则与标量过滤受 256 KiB 单值范围约束。默认 GUI 大复制在 8 MiB 时提示；分页树复制有 8 MiB 硬边界，TUI OSC 52 上限 100 KiB。预览可能截断，完整值请复制或导出。

持久缓存包含原始 JSON，应使用专用目录；复用会完整比较源文件，尚无跨目录全局淘汰。Unix 上应在源文件静止时打开。预算账本不等于进程 RSS；TUI 已移除固定任务内存配额，Memory / stdin 仍受可用内存限制。没有任意大小文件即时打开或固定总内存的保证。

## 从源码编译

项目使用 Rust Edition 2024；历史开发记录使用 Rust 1.99，尚未声明或验证最低 Rust 版本（MSRV）。请准备可编译锁定依赖的 Rust 工具链与 Cargo。

| 平台 | 构建准备与默认界面 |
| --- | --- |
| Windows | Rust MSVC 工具链、C++ Build Tools 与含资源编译器的 Windows SDK；默认 GUI |
| macOS | Rust、Xcode Command Line Tools（`xcode-select --install`）；默认 GUI，可选 TUI；打包配置最低 macOS 11.0，实际兼容性待验收 |
| Linux | Rust、C 链接工具；默认 TUI，无需 X11 / Wayland / GTK 开发包 |

在仓库根目录运行：

```sh
cargo build --release --locked --bin FerrisJson
# 或直接从源码启动
cargo run --release --locked --bin FerrisJson -- /path/to/data.json
```

产物：Windows 为 `target/release/FerrisJson.exe`，macOS / Linux 为 `target/release/FerrisJson`。

macOS 可选纯 TUI；Linux 也可使用相同命令：

```sh
cargo tui
# 等价命令
cargo build --release --locked --no-default-features --features tui --bin FerrisJson
```

### 打包

```sh
# 当前平台默认界面
cargo run --release --locked --example package
# macOS / Linux 终端版
cargo tui-pack
```

| 平台 / 模式 | `target/release/bundle/` 内产物 |
| --- | --- |
| Windows GUI | `FerrisJson.exe`、README、LICENSE |
| macOS GUI | `FerrisJson.app`，README / LICENSE 位于 `Contents/Resources/` |
| Linux / macOS TUI | `FerrisJson`、README、LICENSE |

每次构建当前主机架构，不生成 Universal Binary 或跨系统产物。macOS 使用系统 `sips`、`iconutil`、`plutil`、`codesign`，执行本地 ad-hoc 签名；公开分发所需 Developer ID 与公证另行处理。Linux 不承诺跨发行版通用或静态链接，重复打包会更新同名产物。

## 文档与开发

- [完整优化历史](docs/optimization-history.md)：Git 版本索引、架构演进、原始测量、复现口径及剩余限制。
- [开发记录与 TODO](docs/todo.md)：里程碑、构建 / 验收状态与后续待办。
- [大文件重构方案](docs/large-file-refactor-plan.md)与[感知快开方案](docs/large-file-fast-open-plan.md)：阶段设计和尚未完成的目标。
- [搜索 / 查询分离基线](docs/search-query-separation.md)、[B](docs/search-query-stage-b.md)、[C](docs/search-query-stage-c.md)、[D](docs/search-query-stage-d.md)、[E](docs/search-query-stage-e.md)、[F](docs/search-query-stage-f.md)：接口契约与阶段边界。
- [字体来源与许可证](assets/fonts/README.md)。

仓库提供本地流式 JSON 生成工具，支持十进制 / 二进制单位和可选种子：

```sh
cargo run --release --locked --example generate_json -- 100MB target/local-100MB.json 42
```

生成器拒绝覆盖已有文件。样本、测试 JSON 与基准 JSON 报告只保存在本地，不加入提交。大文件专项默认十进制 **100 / 300 / 500 MB**，包含低 / 高命中，S0 完整基准继续暂停。按项目协作约定，测试及构建由用户安排，确认 OK 后再提交；本文中的命令是使用说明，本次文档整理未执行这些命令。

## 致谢

界面与交互参考了 [fx](https://github.com/antonmedv/fx) 的终端 JSON 浏览和键盘操作，以及 [JSONBolt](https://jsonbolt.com/) 的 JSON 树形浏览体验。感谢这两个项目带来的启发。

感谢以下直接依赖的开源组件及其贡献者；平台 / feature 不同，实际启用的组件也不同，完整依赖版本与传递依赖见 [Cargo.lock](Cargo.lock)。

| 组件 | 用途 |
| --- | --- |
| [egui / eframe](https://github.com/emilk/egui) | 桌面 GUI、绘制与应用框架 |
| [rfd](https://github.com/PolyMeilex/rfd) | 原生文件对话框 |
| [Serde](https://github.com/serde-rs/serde)、[serde_json](https://github.com/serde-rs/json) | 配置序列化、JSON 解析与输出 |
| [serde_json_path](https://github.com/hiltontj/serde_json_path) | JSONPath 校验及 Memory 兼容查询 |
| [regex](https://github.com/rust-lang/regex) | 正则搜索 |
| [Ratatui](https://github.com/ratatui/ratatui)、[Crossterm](https://github.com/crossterm-rs/crossterm) | 终端界面、输入和终端控制 |
| [signal-hook](https://github.com/vorner/signal-hook) | Unix 信号处理与终端退出恢复 |
| [unicode-width](https://github.com/unicode-rs/unicode-width) | 终端 Unicode 显示宽度 |
| [windows-sys](https://github.com/microsoft/windows-rs) | Windows 控制台 API |
| [winresource](https://github.com/BenjaminRi/winresource) | Windows EXE 图标与资源嵌入 |
| [Noto Emoji](https://github.com/google/fonts/tree/main/ofl/notoemoji) | 内置单色 emoji 字体，采用 [SIL OFL 1.1](assets/fonts/OFL-NotoEmoji.txt) |

本项目由作者与 OpenAI Codex 协作开发，完成需求迭代、代码实现与文档整理。

## License

FerrisJson 采用 **GNU General Public License v3.0 或后续版本（GPL-3.0-or-later）**，完整条款见 [LICENSE](LICENSE)。

第三方组件与字体保留各自的版权声明及许可证，详见相应上游项目和仓库中的字体许可文件。
