# Linux TUI 接入记录

2026-10-17，基于 0.7.9+3，按用户明确要求提交到 `tui` 分支；版本号保持不变，功能与跨平台验收仍待用户执行。

## 后续工作区：Linux 编译警告清理

用户提供的 Release 日志显示编译完成，同时有 7 条未使用项警告。针对这些项：桌面／测试专用的队列峰值快照、预算访问、搜索相对导航和 Vim 辅助方法按目标条件编译；TUI 明确释放不使用的桌面剪贴板传输，查询表达式与耗时接入结果展示。Windows/macOS 及测试所需接口保留，没有全局关闭 dead_code 警告。此轮未运行测试、构建或其他检查，是否清零等待用户重编译确认；改动尚未提交，版本保持 0.7.9+3。

## 平台与构建

- Linux 目标默认编译 Ratatui/Crossterm TUI，不需要额外 feature，不编译 eframe、rfd、窗口或字体模块。
- Windows/macOS 继续使用桌面入口、原 GUI 模块和原依赖配置；共享 worker 的桌面刷新通知仍为 `egui::Context`。
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

- 宽终端显示树与预览／当前结果面板；宽度不足 100 列时切换单面板。最小显示提示为 24×8。
- Tab / Shift+Tab 循环 Tree、Preview、Search、JSONPath。树与结果页分别保留选中项。
- 树使用共用 Navigator/PagedTree，最多保存一帧 64 行，不生成全部可见行列表；显示相对行号、类型颜色、展开状态、路径、字节进度与状态。
- 方向键／hjkl、数字前缀、gg/G、za/zo/zc/zR/zM、历史、翻页及屏幕位置导航。预览支持滚动，长预览沿用引擎截断边界。
- o / Ctrl+O 输入文件路径；无参数启动直接输入路径。路径按字面解释，不执行 shell、波浪号或环境变量展开。
- / 和 ? 输入正反向搜索，F5 大小写、F6 正则、F7 All/Keys/Values；改变条件清空旧搜索，需要再次提交。F3/Shift+F3、树中 n/N 导航搜索结果。
- : / F2 输入 JSONPath 或 Pointer，F4/Shift+F4 导航结果。结果每页 16 条，方向键选中并定位，Enter 返回树；查询沿用各后端现有语法限制。
- e 导出当前节点，或聚焦结果面板的完整结果；E 显式导出已提交结果前缀。完整输出受任务完成和源验证状态约束，失败／取消后可选择部分输出；目标文件成功时原子替换。没有系统剪贴板依赖。
- Esc 先关闭输入／帮助；普通模式取消加载或当前导航、搜索、查询，保留已有查询／搜索结果。Delete 清空当前结果面板。
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

退出／错误的 RAII guard 恢复 raw mode、备用屏幕和光标；panic hook 在输出 panic 前恢复终端。SIGINT/SIGTERM/SIGHUP 设置退出标记，事件循环退出后恢复终端，再等待后台临时文件清理；SIGKILL 无法拦截。JSON、路径、文件名及错误中的控制字符在绘制前转义，避免把文档数据当终端控制序列。Ctrl+C 或普通模式 q 退出。

## 既有边界与验收

- --sparse 保留实验性、渐进验证、目录密度／首屏延迟及缓存限制；--indexed 仍建立完整结构索引；Memory 使用共用分页任务的 256 MiB 管理预算，该数字不是进程 RSS 上限，大文件应显式选择 --sparse。
- --paged-tree 在 Linux 使用同一 TUI 树；--sparse-pages 是旧桌面有限页回退，在 Linux 明确拒绝。
- `tree_bench` / `search_bench` 是复用桌面 Viewer 的 CPU harness，在 Linux 提示不适用，避免拉入 GUI；它们不是 TUI 基准。其他纯引擎示例保持可用范围。
- 终端颜色、宽字符、中文、emoji、功能键和 SSH 交互依赖实际终端；没有 GUI 配色编辑器、拖放、桌面粘贴或系统剪贴板集成。
- 本次只静态阅读源码／依赖 API 和锁文件，执行依赖解析及下载；未运行测试、构建、check、Clippy、格式检查、TUI、GUI、打包或基准。没有新增实测通过记录、性能数据或发布产物。
- 待用户验收：Linux 本地终端／SSH、无 DISPLAY/WAYLAND_DISPLAY、无控制终端 CLI、stdin 与重定向、UTF-8 编辑、窄屏与 resize、搜索选项、查询分页及取消、导出完整／部分／覆盖失败、换文件时旧任务隔离、退出恢复，以及 Windows/macOS GUI 和打包回归。
- 大文件专项继续默认十进制 100 MB / 300 MB / 500 MB，低命中与高命中；不得用历史 GUI/引擎基准冒充 TUI 数据。样本／测试 JSON 和基准 JSON 报告仅本地保存，不加入提交。S0 保持暂停。
