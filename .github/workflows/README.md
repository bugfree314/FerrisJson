# GitHub Actions 发布

仓库提供 [Release 工作流](release.yml)。推送 `v*` 标签后，使用 stable Rust 和 `--locked` 分别构建 Windows x64 GUI、macOS Intel / Apple Silicon GUI、Linux x64 TUI，复用仓库中的 `examples/package.rs` 打包脚本。标签必须与 `Cargo.toml`、`Cargo.lock` 的项目版本一致（当前为 `v0.8.0`）。工作流不运行测试、Clippy、格式检查或基准。

使用步骤：

1. 按协作约定，由用户确认改动后提交并推送工作流和待发布代码；标签必须指向包含工作流的提交。
2. 在 GitHub 仓库的 Actions 页面确认允许运行工作流。默认分支包含工作流后，可选择 **Release → Run workflow** 手动构建分支，仅生成 Actions Artifacts，不创建 Release。
3. 准备发布时，在已推送且确认可发布的提交上执行：

   ```sh
   git tag -a v0.8.0 -m "FerrisJson v0.8.0"
   git push origin v0.8.0
   ```

4. 四个平台全部构建成功后，工作流创建草稿 Release，附带 ZIP / tar.gz 和 `SHA256SUMS.txt`。进入 Releases，下载验收、完善变化与限制说明后手动点击 **Publish release**。构建成功不代表功能或跨平台验收通过。

使用自动提供的 `GITHUB_TOKEN`，仅 Release 作业申请 `contents: write`，无需配置个人访问令牌；组织策略仍须允许工作流及该权限。相同标签重跑可更新草稿附件，已正式发布的 Release 会被拒绝覆盖。已存在的标签不会因推送分支再次触发；可在 Actions 中重跑原任务，或对现有标签手动运行工作流。不要为触发构建移动已有发布标签。

Windows 包为便携版，未进行证书签名；macOS App 仅 ad-hoc 签名，未进行 Developer ID 签名或公证。macOS 两种架构分别打包，不是 Universal Binary；Linux 在 Ubuntu 22.04 构建，仍受目标系统动态库兼容性限制。本次仅添加工作流与说明，未执行本地或远端构建、测试、签名分发验收或性能测量，首次 Actions 运行与各平台实际验收待用户安排。

