# Mino

Mino 是基于 Rust + egui 的轻量级终端与 SFTP 可视化工具。

**许可证：[MIT](LICENSE)** · 纯开源免费

## 产品定位

主打**轻量、快速、简洁**的终端体验：本地 / 远程（SSH）终端 + 可视化 SFTP 文件管理，交互参照 Termius / WindTerm 的双栏分屏，视觉采用低干扰的深色界面与紫青渐变标记。核心功能集已收敛，后续迭代围绕性能、流畅度与视觉细节持续打磨。

## 功能概览

- **本地终端**：基于 [alacritty_terminal](https://github.com/alacritty/alacritty) 生产级内核（VT 仿真 + PTY），256 色、宽字符、滚动、括号粘贴、闪烁光标
- **SSH 远程终端**：密码 / 私钥认证（支持口令），xterm-256color PTY
- **SFTP 可视化**：终端 | SFTP 双栏分屏（可拖拽），目录导航、右键菜单操作、上传 / 下载带进度条、删除 / 重命名 / 新建目录（带确认对话框）
- **主机管理**：主机配置持久化（`~/.config/mino/hosts.toml`），单击选中、双击连接
- **三套主题**：`深色`（默认）/ `深蓝` / `霓虹`，每套含独立终端调色板（Catppuccin Mocha 16 色 + xterm 256 色表）
- **品牌**：Mino `>_` 标记、低对比层级、紧凑标签与无持续重绘的轻量动效
- **快捷键**：⌘N 新建连接、⌘T 新建本地终端、⌘W 关闭标签、⌘1-⌘9 切换标签、⌘, 打开设置、⌘+ / ⌘- 缩放终端字号、⌘0 重置字号、⌥1-⌥3 切换主题
- **全 Rust 无 C 依赖**：单二进制、启动 < 300ms

## 环境要求

| 工具 | 要求 |
|---|---|
| Rust | stable（推荐 1.80+） |
| 系统 | macOS 12+（Apple Silicon / Intel） |

## 平台支持

**仅支持 macOS**（Apple Silicon + Intel），发布产物为 `.dmg` 安装包。

## 自动更新

- 启动后自动检查 GitHub Releases 最新版本（后台，不阻塞）
- 设置 → 关于 →「检查更新」可手动检查
- 发现新版本时弹出提示（版本号 + 更新说明），**一键在应用内下载并安装**（带进度条），自动挂载 dmg、替换安装到「应用程序」并重启，无需打开浏览器

## 快速开始

```bash
cargo build --release
./target/release/mino-app
```

开发模式：

```bash
cargo run
```

测试与静态检查：

```bash
cargo test --workspace
cargo clippy --workspace --all-targets
cargo fmt --all --check
```

集成测试（SSH / SFTP 链路）需要本地测试 sshd，一键脚本：

```bash
./scripts/test-sshd.sh start
cargo test --workspace
```

### macOS 安装

Release 提供的 dmg 内含 `Mino.app` 与 **Applications 快捷方式**，与常规 macOS 应用一致：打开 dmg 后把 Mino 拖入「应用程序」即可。

从 Release 安装后若提示「Mino 已损坏，无法打开」，在终端执行：

```bash
xattr -cr "/Applications/Mino.app"
```

然后右键 → **打开** 即可。原因：产物未经 Apple 公证（开源项目未配置开发者签名）。

## 文档

| 文档 | 说明 |
|---|---|
| [使用说明](docs/使用说明.md) | 功能与快捷键全览 |
| [技术架构](docs/技术架构.md) | 选型、分层、数据流设计 |
| [多平台发布](docs/多平台发布.md) | GitHub Actions 打包 macOS 安装包 |
| [贡献指南](CONTRIBUTING.md) | 开发环境与 PR 约定 |
| [安全政策](SECURITY.md) | 漏洞报告方式 |
| [更新日志](CHANGELOG.md) | 版本变更 |
| [开源许可](LICENSE) | MIT |
| [第三方声明](THIRD-PARTY-NOTICES.md) | 依赖许可证聚合 |

## 参与贡献

欢迎 Issue 与 PR，详见 [CONTRIBUTING.md](CONTRIBUTING.md)。参与即同意 [行为准则](CODE_OF_CONDUCT.md)。

## 许可证

[MIT](LICENSE) © yqstart
