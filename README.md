# Rust JSON Viewer

使用 Rust + egui/eframe 实现的本地 JSON 桌面查看器，版本 **0.1.0**。
界面参考 [JSONBolt](https://jsonbolt.com/docs-gui) 的树视图、类型配色、搜索栏和路径栏，键盘操作参考 [fx](https://fx.wtf/how-to/using-fx-as-a-tui-for-json-data)。

## 运行

使用 Rust 1.99 验证构建；需要对应平台的编译工具链，Windows 推荐 MSVC 工具链。

```powershell
cargo run
cargo run -- examples/sample.json
```

构建独立可执行文件：

```powershell
cargo build --release
.\target\release\rust-json-viewer.exe
```

## 功能

- 文件选择、拖放、粘贴 JSON，以及命令行文件/标准输入。
- 对象和数组树形浏览，展开/折叠，类型配色，选中节点详情。
- 全文检索：忽略大小写或区分大小写、正则表达式、仅键/仅值搜索；覆盖折叠节点，自动展开并跳到结果。
- JSONPath 查询：使用 [serde_json_path](https://docs.rs/serde_json_path/0.7.2/serde_json_path/) 实现 RFC 9535 查询，支持通配符、递归查找、数组切片、负索引、多个选择器及过滤条件，同时兼容 JSON Pointer。
- 点击 **Go** 或按 Enter 执行 JSONPath，右侧显示查询结果数组、匹配数量，支持完整复制与导出。执行过查询后，选择树节点会保留查询结果；点击 **Reset**、**Selection preview** 或清空表达式后恢复选中值预览。零匹配显示 `[]`，无效表达式显示错误。
- Windows 桌面启动不创建命令行窗口。命令行调用会使用已有终端或管道。
- 复制选中节点的完整 JSON、复制路径、导出选中节点、F5 重新加载文件。
- 深色/浅色主题；内置示例；错误提示包含 JSON 行列信息。
- 保留对象键顺序和大整数精度。为 UI 和 JSON 文本设置相同的系统字体回退：Windows 使用微软雅黑、Malgun Gothic、游ゴシック等支持中日韩，并补充拉丁、俄语、希腊语、阿拉伯语、希伯来语、印地语和泰语字形。其他平台尝试加载常见系统字体/Noto 字体，覆盖范围取决于安装的字体；不随程序分发系统字体。
- 右下角显示最近一次打开 JSON 与查询的耗时：`Open` 包含文件读取（文件输入）、解析、树索引及初始视图准备，`Search` / `Path` 包含检索和结果定位。耗时不包含文件选择对话框及屏幕绘制；小于一秒显示毫秒，否则显示秒。打开时显示 `Opening...`，成功加载新文档后清除旧查询耗时。

点击 **Paste JSON** 粘贴并加载；在树视图中也可以直接 Ctrl+V。输入框内 Ctrl+V 仅粘贴文本。
导出内容是完整的选中 JSON，预览中截断的长字符串不会影响搜索、复制或导出。

## 快捷键

| 按键 | 操作 |
| --- | --- |
| Ctrl+O | 打开文件 |
| Ctrl+V | 加载剪贴板 JSON（输入框外） |
| Ctrl+F 或 `/` | 搜索 |
| F3 / Shift+F3、n / Shift+n | 下一条 / 上一条匹配 |
| ↑↓ 或 k/j | 选择上一个 / 下一个可见节点 |
| → 或 l | 展开节点 / 进入第一个子节点 |
| ← 或 h | 折叠节点 / 返回父节点 |
| Space | 展开/折叠选中节点 |
| Home / End | 首个 / 最后一个可见节点 |
| gg / G | 首个 / 最后一个可见节点（gg 连续按两次，间隔不超过一秒） |
| [ / ] | 浏览历史中的上一个 / 下一个位置 |
| Ctrl+C | 复制选中 JSON |
| Ctrl+P | 复制路径 |
| F5 | 重新加载源文件 |
| F1 | 帮助 |
| Escape | 退出搜索输入框焦点 |

## 命令行

不带处理选项时打开桌面窗口；带处理选项时输出到标准输出，错误返回退出码 1。
四个处理选项互斥。Windows PowerShell 直接调用 GUI 可执行文件时可能不会等待退出；使用管道或 `Start-Process -Wait` 可等待完成。

```powershell
cargo run -- examples/sample.json --get '$.users[0].name'
cargo run -- examples/sample.json --get '/special.keys/a~1b'
cargo run -- examples/sample.json --query '$.users[*].name'
cargo run -- examples/sample.json --query '$.users[?(@.active == true)].name'
cargo run -- examples/sample.json --query '$..email'
cargo run -- examples/sample.json --search email
cargo run -- examples/sample.json --pretty
Get-Content examples/sample.json -Raw | cargo run -- --get '$.users[1]'
```

`--search` 忽略大小写，检索键及标量值，输出匹配节点路径。`--get` 要求恰好一个匹配并输出该值；`--query` 始终输出所有匹配值组成的 JSON 数组，零匹配输出 `[]`。`--pretty` 输出格式化文档。
`-` 可显式指定标准输入，`--help` 查看参数，`--version` 查看版本。

## 验证

```powershell
cargo fmt --check
cargo test --offline
cargo clippy --offline --all-targets -- -D warnings
```

## v0.1.0 范围

此版本实现 TODO 的基本查看与检索，并借鉴两款工具的交互，不是完整复刻。
不提供 fx JavaScript 执行、JSONL、URL 下载或全局热键。JSONPath 支持 RFC 9535 语法；不执行 JavaScript 表达式。
仅接受一个 UTF-8 JSON 文档，支持 UTF-8 BOM。解析使用 serde_json 默认嵌套深度限制。
文件在后台读取与解析，整份数据和节点索引保存在内存中，尚未实现超大文件流式解析。
节点值及查询结果的预览最多输出约 200 KB，超出时明确标记截断；小节点预览不受整个文件大小影响。复制和导出始终保留完整值。主题设置仅在当前会话生效。
