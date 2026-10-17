# FerrisJson TUI 接入记录（Linux / macOS）

当前收录版本：**0.8.0**（2026-10-17，本次版本提交）。TUI 系列基于 0.7.9+3 实施，已通过 `12d791a` 合入 main；本轮只同步版本与文档，按用户明确要求提交，未运行测试、构建、程序、打包或基准，不创建新标签／推送，功能与跨平台验收仍待用户执行。

下文保留各阶段实施时的版本、未提交及验证状态描述，不改写历史结果；现行版本与本轮状态以上述说明、README 和 TODO 为准。

## 当前工作区：macOS 可选 TUI 编译

项目内 `.cargo/config.toml` 提供两个 Cargo alias：`cargo tui` 等价于下方纯 TUI Release 编译命令，`cargo tui-pack` 等价于终端打包命令。需在项目目录（或其子目录）执行，不修改全局配置；alias 本轮仅写入并静态阅读，未运行构建或测试、未提交。

增加 Cargo `desktop`（默认）和 `tui` feature。Linux 按目标自动使用终端界面；macOS 仅在启用 `tui` 时选择同一终端前端，默认 GUI 保留；Windows 维持默认 GUI，不支持 `tui` feature。TUI 的刷新适配、stdin、无固定内存配额、剪贴板和键盘／分页行为由两种 Unix 目标共用。

以下命令由用户在 Mac 上执行，本轮未编译或运行：

```sh
cargo build --release --locked --no-default-features --features tui --bin FerrisJson
./target/release/FerrisJson /path/to/data.json
cargo run --release --locked --no-default-features --features tui --example package
```

直接编译产物为 `target/release/FerrisJson`，package 复制到 `target/release/bundle/FerrisJson` 并附带 README/LICENSE，权限 0755，不创建 `.app`。打包的内部 `cargo build` 同步传递 feature 选择，防止错误打成 GUI；只生成当前 Rust 主机架构。GUI 与 TUI 的可执行文件同名，需要同时保留时，直接 `cargo build` 可分别传入 `--target-dir target/macos-tui` 等目录；package 仍使用项目固定的 target/bundle 路径并更新同名产物。`--features tui` 单独使用也会选择 TUI，但推荐加 `--no-default-features` 排除默认 GUI 依赖。

macOS TUI 使用 Ctrl／Vim 快捷键；⌘ 快捷键通常由终端应用处理。终端复制仍需 OSC 52 支持，无法承诺所有 Mac 终端均接受；stdin、路径与无终端 CLI 规则沿用现有实现。

仅运行离线、锁定的 Cargo metadata 依赖解析：Apple Silicon macOS 默认模式根依赖含 eframe/rfd、不含终端库；macOS 纯 TUI 根依赖含 Ratatui/Crossterm 等、不含 eframe/rfd；Linux 关闭默认 feature 时仍有终端根依赖。Linux 开启默认 feature 的完整 metadata 因本地缺少 as-raw-xcb-connection 源码缓存而未完成，此结果不是编译失败或通过记录。不运行构建、check、测试、Clippy、格式检查、打包、TUI／GUI 或基准；Mac 编译、交互、信号／退出恢复与默认 GUI 回归待用户执行。依赖版本和 Cargo.lock 无变化，版本仍为 0.7.9+3，尚未提交。

## 当前工作区：快捷键、布局与分页一致性

路径定位提示修订：`.`／p 的输入标题明确只定位一个节点；遇到旧引擎的多节点错误时，仅在 TUI 显示“按 F2 或 : 执行 JSONPath，Alt+4 查看结果”，不再展示命令行 `--query` 建议。CLI／GUI 的既有错误契约不变，不自动重跑查询。未运行构建／测试，尚未提交。

后续编译修复：用户报告 E0451，TUI 曾尝试直接构造库的 Clipboard，无法访问其 `pub(crate)` 预留字段。现改用终端本地的 ClipboardTransfer，分别持有完整库剪贴板对象或路径字符串和原 Reservation，发送结束后再释放；不扩大库字段可见性。未运行构建／测试，待用户重编译，尚未提交。

本轮对照 `app.rs`／`tree_app.rs` 的快捷键处理，修复 TUI 缺失的 `.`、搜索面板中无效的 n/N、逆向搜索方向丢失、F5 冲突、Ctrl+F、Vim 翻页／复制和数字前缀缺口。仅修改 Linux 前端及恢复共用 Vim 辅助方法在 Linux 的编译，Windows/macOS 的 GUI 处理不变。版本保持 0.7.9+3，未提交；按约定未运行测试、构建、Clippy、格式检查、程序或基准，交互与布局均待用户实机验收。

| 操作 | 当前 TUI 快捷键 | 适配说明 |
| --- | --- | --- |
| 打开／重载 | Ctrl+O（兼容 o）／F5 | 重载需要文件，管道／粘贴需重新提供输入 |
| 单路径定位 | `.`（兼容 p） | 与桌面路径输入入口一致 |
| 文本搜索 | `/`、`?`、Ctrl+F | n/N 在所有面板可用，遵循 `/` 或 `?` 的方向；数字前缀可用 |
| 搜索下一／上一项 | F3／Shift+F3 | 固定正／反向，首尾循环 |
| JSONPath 输入／结果导航 | `:`／F2；F4／Shift+F4 | 查询独立于搜索；首批结果自动选中第一项 |
| 搜索大小写／正则／范围 | F6／F7／F8 | F5 还给重载；修改选项清空旧搜索，需重新执行 |
| 移动与首尾 | hjkl、方向键、gg/G、Home/End | 树／结果／预览使用各自焦点；恢复数字前缀与组合命令 |
| 树容器／历史 | za/zo/zc/zR/zM、空格、Enter、`[`／`]` | 历史支持数字前缀；H/M/L 为树屏幕位置 |
| 整页／半页 | PgUp/PgDn、Ctrl+B/E；Ctrl+U/D | 作用于聚焦面板；结果整页为逻辑 16 条，半页按可见行计算 |
| 复制值／路径 | yy、Ctrl+C／yp、Ctrl+P | 通过 OSC 52 向终端发送，最多 100 KiB，不截断；终端／复用器需支持，不返回“已复制”确认；大值用导出 |
| 粘贴 | 终端粘贴（通常 Ctrl+Shift+V／Shift+Insert） | 输入框内编辑；普通模式加载 JSON；不主动读取远端剪贴板 |
| 离开输入／返回树 | Esc | 只退出输入、帮助或取消待导航；不会同时取消搜索和查询 |
| 取消当前任务 | Ctrl+X／Stop 按钮 | 作用于当前面板，加载期间取消加载，保留已提交结果 |
| 退出 | q／Ctrl+Q | Ctrl+C 恢复为复制；外部 SIGINT 仍退出 |

### 小窗口和 UI 结构

- 内容区至少 110 列且 18 行：左侧树和搜索，右侧选中值与 JSONPath；没有搜索内容时左侧仅树。
- 内容区至少 72 列且 10 行：树与当前详情双栏，Tab、Alt+1/2/3/4 或点击切换焦点；详情随最后选择的非树面板保留。
- 更小或 F9 最大化时：当前面板占满内容区，最小 24×8；F9 恢复多面板。短窗口／不足 40 列收起工具栏，入口仍可通过快捷键和 F1 使用。
- 工具栏按钮与搜索选项按终端宽度换行，标题／路径／错误按字符显示宽度省略，不再把所有提示挤在一行。输入框保持 UTF-8 光标可见；错误输入保留供修正。
- 支持点击面板、树行、结果行和分页按钮，以及鼠标滚轮；终端自身选择文本通常可按住 Shift。退出会关闭鼠标捕获。
- resize 时按树实际可见高度更新导航窗口并调整锚点，保留待定位请求；预览和帮助按实际宽度重新折行，滚动位置钳制在内容范围内。
- 加载转 Ready 后清除过时的 Opening 提示。空查询／搜索提交清空对应结果，与桌面清空行为衔接。

### 预览、搜索、JSONPath 怎样翻页

先通过 Tab、Alt+2/3/4，或点击面板选择预览／搜索／JSONPath。面板青色边框表示焦点，F9 可放大查看。

| 面板 | 整页 | 细移／首尾 | 页面提示 |
| --- | --- | --- | --- |
| 选中值预览 | PgUp/PgDn 或 Ctrl+B/E，按实际可见高度滚动 | j/k 一行、Ctrl+U/D 半页、gg/G 或 Home/End 首尾；w 切换换行，关闭后 h/l 横移 | 已显示行范围／总显示行数 |
| 搜索结果 | PgUp/PgDn、Ctrl+B/E、h/l，或底部 `< PgUp`／`PgDn >` 按钮 | j/k 逐项，gg/G 或 Home/End 首尾，Enter 返回树；n/N 按搜索方向逐项循环 | Page 当前／总页数及本页结果序号范围 |
| JSONPath 结果 | 同搜索结果，每页 16 条 | j/k 逐项；F4／Shift+F4 独立循环查询结果，Enter 返回树 | 同搜索结果；另显示状态和查询耗时 |

翻页在第一页／最后一页停止，不会像逐项匹配导航那样绕回。小面板显示不下整页时，逐项选择会滚动页内显示范围；预览滚动不会越过末尾出现空白页。结果数量在扫描中按已提交前缀显示，并非最终总数。

终端复制依据 [XTerm OSC 52 选择区协议](https://invisible-island.net/xterm/ctlseqs/ctlseqs.pdf)；发送成功只表示序列写入终端，不保证终端已接受剪贴板操作。配置编辑器、拖放、彩色 emoji 和原生剪贴板读取仍为终端适配边界，不宣称与 GUI 完全相同。

## 后续工作区：Linux 编译警告清理

用户提供的 Release 日志显示编译完成，同时有 7 条未使用项警告。此前针对这些项隔离桌面专用接口、处理剪贴板传输、接入查询表达式和耗时展示，已收录于 `b913ba6`。本轮 TUI 实际使用了 Vim 辅助方法和剪贴板传输，因此恢复对应方法编译并消费复制结果；没有全局关闭 dead_code 警告。未运行测试、构建或其他检查，是否清零等待用户重编译确认，版本保持 0.7.9+3。

## 后续工作区：移除 Linux TUI 固定内存配额

`fee325d` 按用户要求移除 Task 的 256 MiB 内存配额，Linux 使用 `usize::MAX` 作为预算账本边界，继续记录内存占用并检查计数溢出；Memory、Indexed、Sparse 及其搜索／查询共享此设置。实际分配受系统可用内存约束。Windows/macOS 仍为原 256 MiB，磁盘配额和内部缓存、单值／语法／结构边界不变；CLI 的独立默认预算不在此次 TUI 改动范围。未运行测试、构建、Clippy、格式检查或基准，版本保持 0.7.9+3。

## 平台与构建

- Linux 目标默认编译 Ratatui/Crossterm TUI，不需要额外 feature，不编译 eframe、rfd、窗口或字体模块。
- Windows/macOS 默认使用桌面入口和原 GUI 模块；macOS 可通过上述 feature 选择 TUI，纯 TUI 模式不启用 GUI 依赖，共享 worker 的桌面刷新通知仍为 `egui::Context`。
- Linux 的 `terminal_wake::Context` 用原子标记通知刷新，TUI 每 50 ms 检查终端事件和后台消息。不复制搜索、查询、取消、会话隔离或输出引擎。
- 共用 Cargo.lock；新增终端依赖，原有锁定包的名称和版本均保留。平台变化修改依赖位置和入口，不代表已有两系统通过回归。
- 原 `package` 示例增加 Linux 分支，按主机 target 构建并复制可执行文件及 README/LICENSE，执行权限设为 0755。原 Windows/macOS 分支保留。

以下命令供用户手动构建和验收，本次没有执行：

```sh
cargo build --release --locked --bin FerrisJson
./target/release/FerrisJson
./target/release/FerrisJson /path/to/data.json
./target/release/FerrisJson --indexed /path/to/data.json
./target/release/FerrisJson --sparse /path/to/large.json
./target/release/FerrisJson --sparse --cache-dir /path/to/cache /path/to/large.json
cargo run --release --locked --example package
```

直接构建的产物为 `target/release/FerrisJson`；package 产物为 `target/release/bundle/FerrisJson`。只构建当前工具链架构，不生成跨架构、跨发行版通用或静态链接承诺。Linux 需 Rust 工具链及系统 C 链接工具，无需 GUI 开发包；新增 Ratatui 声明最低 Rust 1.88，项目现有开发环境使用 Rust 1.99。

## 交互范围

- 根据内容区宽高切换四面板、双栏或单面板；具体阈值与小窗口行为见上节。
- Tab / Shift+Tab、Alt+1/2/3/4 或点击选择 Tree、Preview、Search、JSONPath。树与结果页分别保留选中项，F9 最大化当前面板。
- 树使用共用 Navigator/PagedTree，最多保存一帧 64 行，不生成全部可见行列表；显示相对行号、类型颜色、展开状态、路径、字节进度与状态。
- 方向键／hjkl、数字前缀、gg/G、za/zo/zc/zR/zM、历史、翻页及屏幕位置导航。预览支持滚动，长预览沿用引擎截断边界。
- o / Ctrl+O 输入文件路径；无参数启动直接输入路径。路径按字面解释，不执行 shell、波浪号或环境变量展开。
- /、?、Ctrl+F 输入搜索，F6 大小写、F7 正则、F8 All/Keys/Values；改变条件清空旧搜索，需要再次提交。F3/Shift+F3、所有面板中的 n/N 导航搜索结果，F5 重载文件。
- : / F2 输入 JSONPath 或 Pointer，F4/Shift+F4 导航结果。结果每页 16 条，方向键选中并定位，Enter 返回树；查询沿用各后端现有语法限制。
- e 导出当前节点，或聚焦结果面板的完整结果；E 显式导出已提交结果前缀。完整输出受任务完成和源验证状态约束，失败／取消后可选择部分输出；目标文件成功时原子替换。复制使用可选的终端 OSC 52，不依赖 GUI 剪贴板服务。
- Esc 关闭输入／帮助，或返回树并取消待导航；Ctrl+X 取消当前任务，保留已有结果。Delete 清空当前结果面板。
- F1 帮助支持滚动；输入支持 UTF-8 光标编辑和 bracketed paste，粘贴不会自动提交。

## 无图形环境、输入和退出

TUI 从 `/dev/tty` 绘制；Crossterm 在 stdin 非终端时从控制终端读取按键，所以读取 JSON 管道后仍可交互。JSON 管道只支持 Memory，并完整读入后解析，不是渐进式管道解析：

```sh
cat /path/to/data.json | ./target/release/FerrisJson -
```

没有控制终端或 TERM=dumb 时返回错误并提示使用 CLI。现有 `--get`、`--query`、`--search`、`--pretty` 在进入 TUI 前处理，保留 stdout 的机器可读输出：

```sh
./target/release/FerrisJson /path/to/data.json --query '$.items[*].name'
cat /path/to/data.json | ./target/release/FerrisJson --pretty
```

退出／错误的 RAII guard 恢复 raw mode、备用屏幕、鼠标捕获和光标；panic hook 在输出 panic 前恢复终端。SIGINT/SIGTERM/SIGHUP 设置退出标记，事件循环退出后恢复终端，再等待后台临时文件清理；SIGKILL 无法拦截。JSON、路径、文件名及错误中的控制字符在绘制前转义，复制内容仅在用户主动复制时编码为 OSC 52。Ctrl+Q 或普通模式 q 退出，Ctrl+C 为复制。

## 既有边界与验收

- --sparse 保留实验性、渐进验证、目录密度／首屏延迟及缓存限制；--indexed 仍建立完整结构索引；Linux TUI 不再设置固定内存配额，Memory 会保留完整 Value，大文件可显式选择 --sparse。
- --paged-tree 在 Linux 使用同一 TUI 树；--sparse-pages 是旧桌面有限页回退，在 Linux 明确拒绝。
- `tree_bench` / `search_bench` 是复用桌面 Viewer 的 CPU harness，在 Linux 提示不适用，避免拉入 GUI；它们不是 TUI 基准。其他纯引擎示例保持可用范围。
- 终端颜色、宽字符、中文、emoji、功能键、OSC 52 和 SSH 交互依赖实际终端；没有 GUI 配色编辑器、拖放或原生系统剪贴板读取。
- 本次只静态阅读源码／依赖 API 和锁文件，执行依赖解析及下载；未运行测试、构建、check、Clippy、格式检查、TUI、GUI、打包或基准。没有新增实测通过记录、性能数据或发布产物。
- 待用户验收：Linux 本地终端／SSH、无 DISPLAY/WAYLAND_DISPLAY、无控制终端 CLI、stdin 与重定向、UTF-8 编辑、窄屏与 resize、搜索选项、查询分页及取消、导出完整／部分／覆盖失败、换文件时旧任务隔离、退出恢复，以及 Windows/macOS GUI 和打包回归。
- 大文件专项继续默认十进制 100 MB / 300 MB / 500 MB，低命中与高命中；不得用历史 GUI/引擎基准冒充 TUI 数据。样本／测试 JSON 和基准 JSON 报告仅本地保存，不加入提交。S0 保持暂停。
