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
- 路径定位：`$.users[0].name`、`.users[0]`、`$["special.keys"]["a/b"]`、JSON Pointer `/users/0/name`。
- 复制选中节点的完整 JSON、复制路径、导出选中节点、F5 重新加载文件。
- 深色/浅色主题；内置示例；错误提示包含 JSON 行列信息。
- 保留对象键顺序和大整数精度。Windows 自动使用微软雅黑作为中文字体后备。

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
| Ctrl+C | 复制选中 JSON |
| Ctrl+P | 复制路径 |
| F5 | 重新加载源文件 |
| F1 | 帮助 |
| Escape | 退出搜索输入框焦点 |

## 命令行

不带处理选项时打开桌面窗口；带处理选项时输出到标准输出，错误返回退出码 1。
三个处理选项互斥。

```powershell
cargo run -- examples/sample.json --get '$.users[0].name'
cargo run -- examples/sample.json --get '/special.keys/a~1b'
cargo run -- examples/sample.json --search email
cargo run -- examples/sample.json --pretty
Get-Content examples/sample.json -Raw | cargo run -- --get '$.users[1]'
```

`--search` 忽略大小写，检索键及标量值，输出匹配节点路径。`--get` 和 `--pretty` 输出格式化 JSON。
`-` 可显式指定标准输入，`--help` 查看参数，`--version` 查看版本。

## 验证

```powershell
cargo fmt --check
cargo test --offline
cargo clippy --offline --all-targets -- -D warnings
```

## v0.1.0 范围

此版本实现 TODO 的基本查看与检索，并借鉴两款工具的交互，不是完整复刻。
不提供 fx JavaScript 执行、JSONPath 通配符/过滤表达式、JSONL、URL 下载或全局热键。
仅接受一个 UTF-8 JSON 文档，支持 UTF-8 BOM。解析使用 serde_json 默认嵌套深度限制。
文件在后台读取与解析，整份数据和节点索引保存在内存中，尚未实现超大文件流式解析。
大选中子树会省略详情预览，仍可复制和导出。主题设置仅在当前会话生效。
