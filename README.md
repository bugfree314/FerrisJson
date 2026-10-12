# FerrisJson

当前版本：**0.3.2**。

一个使用 **Rust 编写并编译为原生可执行程序**的本地 JSON 查看器，提供树形浏览、JSONPath 查询和 Vim 风格导航。

本项目由作者与 **OpenAI Codex 协作开发**，共同完成需求迭代、代码实现、测试与文档编写。

打开文件或粘贴 JSON，在左侧浏览结构，右侧同时查看选中值和查询结果。数据解析、搜索与查询在本地完成，无需账号。

## 功能

- **树形浏览**：对象、数组展开与折叠，键和值按字符串、数字、布尔、null、对象、数组类型配色。
- **全文搜索**：支持正反向查找、大小写切换、正则表达式，以及仅搜索键或值；匹配折叠节点时自动展开并定位。
- **JSONPath 查询**：支持通配符、递归查找、数组切片、负索引、多选择器和过滤条件，同时兼容 JSON Pointer。
- **双预览区**：右侧上方始终显示选中值，下方保留 JSONPath 结果。两区独立滚动，可拖动上下分隔条和面板左边缘调整大小。
- **Vim 操作**：数字前缀、`hjkl`、`gg/G`、`n/N`、浏览历史、翻页、展开折叠与复制；状态栏显示待完成的按键和最近执行的命令。
- **快捷键速查页**：F1 打开，Esc 关闭，输入框内保持正常文字输入。
- **输入与输出**：文件选择、拖放、粘贴、标准输入；复制完整 JSON、复制路径、导出选中值或所有查询结果。
- **多语种与主题**：中日韩及其他常用文字的系统字体回退，深浅色主题，保留对象键顺序和大整数精度。
- **可自定义配色**：内置 Ferris、Monokai、Solarized、Claude 风格、VS Code 风格，以及 Custom 配色；每套均有深浅两种模式，统一应用到树、值预览与 JSONPath 结果。
- **耗时显示**：右下角显示最近一次打开文档、搜索或 JSONPath 查询的耗时。
- **Windows 桌面启动**：不创建命令行窗口；命令行处理可以使用已有终端或管道。

## 从源码运行

开发环境使用 Rust 1.99。Windows 为当前主要验证平台，需要 Rust MSVC 工具链及对应的 C++ 构建工具。

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

## 桌面操作

1. 点击 **Open file**、拖放文件，或点击 **Paste JSON** 粘贴内容。树视图中 Ctrl+V 也可加载剪贴板 JSON。
2. 点击键或值选择节点；点击箭头、双击容器键名，或使用键盘展开与折叠。
3. 在搜索栏输入文字，使用 **Aa** 切换大小写、`.*` 切换正则模式、**All / Keys / Values** 限定范围。
4. 在 **JSONPath** 栏输入表达式，点击 **Go** 或按 Enter 执行；下方预览区显示结果数量和 JSON 数组。
5. 点击 **Copy JSON / Copy path / Export** 操作选中值；查询区的 **Copy / Export** 操作完整结果数组。
6. **Reset**、查询区的 **Clear** 或清空表达式会清除查询结果，上方选中值预览继续保留。

输入框里的 Ctrl+V 只粘贴文本。切换树节点不会清除已经执行的 JSONPath 结果。F5 可重新读取当前源文件。

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

也可省略起始 `$`，例如 `.users[0].name`。JSONPath 采用 [serde_json_path](https://docs.rs/serde_json_path/0.7.2/serde_json_path/) 的 RFC 9535 查询语法，不执行 JavaScript。零匹配显示 `[]`；无效表达式显示错误。

## 配色设置

在顶部配色下拉框选择 **Ferris / Monokai / Solarized / Claude / VS Code / Custom**，使用 **Light / Dark** 切换深浅模式。顶部的方案和模式选择会自动保存，下次启动时恢复。

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

首次使用 `--offline` 前需要下载依赖。测试覆盖特殊键名、Unicode、大整数、JSONPath、搜索方向、Vim 计数与历史、预览截断，以及界面分隔条等交互。

## 当前限制

- 一次读取一个 UTF-8 JSON 文档，支持 UTF-8 BOM；不支持 JSONL、URL 加载或 JavaScript 处理。
- 文件读取与解析在后台执行，但文档和节点索引完整保存在内存中，尚未实现超大文件流式解析。
- 使用 serde_json 默认嵌套深度限制。
- 选中值与查询结果的预览最多输出约 200 KB，截断时显示提示；复制与导出仍保留完整内容。
- 多语种字形覆盖取决于系统安装的字体。Windows 使用微软雅黑、Malgun Gothic、游ゴシック及其他后备字体；其他平台尝试常见系统字体或 Noto 字体。系统字体不随项目分发。
- 配色和深浅模式可保存到用户配置目录；面板分隔位置仅在当前会话生效。
- 打开耗时包含文件读取、解析、树索引和初始预览准备；查询耗时包含检索与结果定位，不包含文件选择对话框及屏幕绘制。

## 致谢

界面与交互借鉴 [JSONBolt](https://jsonbolt.com/docs-gui) 和 [fx](https://fx.wtf/how-to/using-fx-as-a-tui-for-json-data)。GUI 使用 egui/eframe，JSON 解析使用 serde_json，查询使用 serde_json_path。

## 许可证

本项目采用 **GNU General Public License v3.0 或后续版本（GPL-3.0-or-later）**。允许使用、修改和分发，包括商业用途；分发受 GPL 覆盖的程序或修改版本时，应遵守 GPL 的源码提供、许可证保留及相同许可等要求。软件按原样提供，不提供担保。完整条款以 [LICENSE](LICENSE) 与 [GNU 官方许可证](https://www.gnu.org/licenses/gpl-3.0.html) 为准。

历史版本的许可请以相应 Git 标签中的 LICENSE 文件为准。

第三方依赖及系统字体分别遵循其自身许可证。
