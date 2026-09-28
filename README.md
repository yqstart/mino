# Mino

Mino 是基于 Rust + egui 的轻量级终端与 SFTP 可视化工具。

**许可证：[MIT](LICENSE)** · 纯开源免费

## 产品定位

主打**轻量、快速、简洁**的终端体验：本地 / 远程（SSH）终端 + 可视化 SFTP 文件管理 + 项目快捷打开；交互是「终端 | SFTP」可拖拽双栏分屏，视觉参照 Tabby 终端的分层深色风格（三套主题：石墨绿 / 深蓝 / 霓虹），品牌标记为 `>_`。核心功能集已收敛，后续迭代围绕性能、流畅度与视觉细节持续打磨。

## 功能概览

- **本地终端**：基于 [alacritty_terminal](https://github.com/alacritty/alacritty) 生产级内核（VT 仿真 + PTY），256 色、宽字符、滚动、括号粘贴、闪烁光标
- **SSH 远程终端**：密码 / 私钥认证（支持口令），xterm-256color PTY
- **SFTP 可视化**：终端 | SFTP 双栏分屏（可拖拽、默认收起），目录导航（单击选中、双击进入）、Shift / ⌘ 多选、右键菜单（上传 / 下载 / 重命名 / 删除 / 新建文件夹 / 刷新 / 定位到终端目录 `⌘⇧L`）、上传下载进度与结果徽标（带确认对话框）
- **主机管理**：主机配置持久化（`~/.config/mino/hosts.toml`，0600 权限、原子写入、损坏自动备份），单击选中、双击连接；标签栏 `>_` 快捷入口单击主机即连
- **项目收藏**：`⌘D` 把当前终端目录收藏为项目，`⌘O` 打开项目面板（搜索 + ↑↓ 选择 + 回车在新标签打开）
- **三套主题**：`深色`（默认，石墨绿）/ `深蓝` / `霓虹`，每套含独立终端调色板（16 色 + xterm 256 色表），切换即持久化
- **品牌**：黑底圆角 `>_` 标记（白色 `>` + 磷光绿光标块），与应用图标同构图；低对比层级、紧凑标签与无持续重绘的轻量动效
- **字体**：打包 JetBrains Mono 四字重（Regular / Italic / Bold / BoldItalic）+ 系统 CJK fallback（PingFang / STHeiti），中文与提示符符号开箱即用
- **快捷键**：⌘N 新建连接、⌘T 新建本地终端、⌘O 项目面板、⌘D 收藏当前目录、⌘W 关闭标签、⌘1-⌘9 切换标签、⌘, 打开设置、⌘+ / ⌘- 缩放终端字号、⌘0 重置字号、⌥1-⌥3 切换主题
- **全 Rust 无 C 依赖**：单二进制（终端内核 alacritty_terminal，SSH / SFTP 为 russh + russh-sftp）

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
cargo test --workspace -- --test-threads=1   # UI 测试必须串行（kittest 争抢全局状态）
cargo clippy --workspace --all-targets
cargo fmt --all --check
```

集成测试（SSH / SFTP 链路）需要本地测试 sshd，一键脚本：

```bash
./scripts/test-sshd.sh start     # 127.0.0.1:2222，公钥认证，含 sftp subsystem
cargo test --workspace -- --test-threads=1
./scripts/test-sshd.sh stop
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
