# Mino

以 [Kaku](https://github.com/tw93/Kaku)（WezTerm 系终端）为视觉与交互基准的 macOS 轻量终端：在 Kaku 的终端体验之上，加入**可视化面板**（SFTP 文件管理、统一工作区、AI 助手）与完整的 **SSH / SFTP** 支持；同时规划接入用户本地的 AI Agent。

**许可证：[MIT](LICENSE)** · 纯开源免费

## 项目定位

- **Kaku 基准**：配色（Kaku Dark / Kaku Light，跟随系统外观）、底栏标签、分屏窗格、搜索、标签导航、命令面板与整套键位族，均照搬 [Kaku](https://github.com/tw93/Kaku)
- **可视化面板**：不止于纯命令行——SFTP 文件面板（可拖拽分栏）、统一工作区（本地目录与 SSH 主机同一列表）、AI 助手面板与浮动启动器
- **SSH / SFTP**：密码 / 私钥认证（含口令）、TOFU 主机密钥校验、连接保活、传输进度与失败清理，纯 Rust 实现
- **本地 Agent（规划中）**：支持直接使用用户本机已安装的 AI Agent，首批 5 种：pi、oh my pi（omp）、Claude Code、Codex CLI、opencode

## 功能概览

- **本地终端**：基于 [alacritty_terminal](https://github.com/alacritty/alacritty) 生产级内核（VT 仿真 + PTY）；Kaku 同款配色与调色板、真彩、宽字符、回滚缓冲、括号粘贴、闪烁竖条光标、对比度下限与覆写表；会话在后台创建，启动不阻塞首帧
- **分屏窗格**：⌘D 左右 / ⌘⇧D 上下拆分，⌘⌥ 方向键移焦、⌘⌃ 方向键调比例、⌘⇧S 翻转方向、⌘⇧Enter 放大还原
- **搜索与导航**：⌘F 搜索回滚缓冲（命中高亮、上下导航、大小写开关）、⌘⇧O 标签导航器、⌘⇧P 命令面板（fuzzy 过滤）
- **SSH 远程终端**：密码 / 私钥认证、TOFU 主机密钥校验（`~/.config/mino/known_hosts.toml`）、30s keepalive、标签标题显示主机名
- **SFTP 可视化**：终端 | SFTP 双栏分屏（可拖拽、默认收起）；目录导航、Shift / ⌘ 多选、右键菜单（上传 / 下载 / 重命名 / 删除 / 新建文件夹 / 刷新 / 定位到终端目录）、传输进度徽标与失败清理
- **统一工作区**：本地目录与 SSH 主机融合为同一列表（`~/.config/mino/hosts.toml` 的 `[[workspaces]]`，0600 权限、原子写入、损坏自动备份）；标签栏「工作区」按钮单击即开、⌘O 搜索面板、设置内单卡片管理；⌘B 收藏当前会话
- **AI 助手**：`# 自然语言` 回车生成命令并回填提示符（不自动执行）；失败命令自动给修复建议（⌘⇧E 填入）；⌘L 流式聊天面板；OpenAI 兼容连接（api_key 或跟随 Codex 登录态）
- **Shell 集成套件**：首次运行向 `~/.zshrc` 写入可一键卸载的受管片段；zsh 自动建议 / 语法高亮 / 补全 / z 跳转 / Smart Tab，starship 走预设配置
- **窗口快照**：退出自动保存标签布局（含 SSH 档案），⌘⌥⇧T 恢复上次窗口
- **字体**：打包 JetBrains Mono 三字重（Regular / Medium / SemiBold）+ Symbols Nerd Font Mono + 系统 CJK fallback，中文与提示符图标开箱即用
- **全 Rust 无 C 依赖**：单二进制（终端内核 alacritty_terminal，SSH / SFTP 为 russh + russh-sftp）

## 本地 Agent（规划中）

Mino 的下一步方向：**直接使用用户本机已安装的 AI Agent**，而不是内置一套封闭的 AI 能力。首批计划支持 5 种终端 Agent：

| Agent | 说明 |
|---|---|
| [pi](https://github.com/badlogic/pi-mono) | Mario Zechner 的 coding agent CLI |
| [oh my pi（omp）](https://github.com/can1357/oh-my-pi) | 基于 pi 的增强发行版（"batteries included"） |
| [Claude Code](https://github.com/anthropics/claude-code) | Anthropic 官方 agentic coding 工具 |
| [Codex CLI](https://github.com/openai/codex) | OpenAI 官方终端 coding agent |
| [opencode](https://github.com/sst/opencode) | 开源终端 AI coding agent |

> **状态说明**：该能力尚未实现，当前版本（v2.0.0）提供的是上文的「AI 助手」（远程 API 形态）。已有终端功能已针对这些 Agent 的 TUI 输出做了兼容打磨（kitty 键盘协议、同步更新应答、鼠标上报、渲染一致性等），在 Mino 中运行它们可获得良好体验。

## 环境要求

| 工具 | 要求 |
|---|---|
| Rust | stable（推荐 1.80+） |
| 系统 | macOS 12+（Apple Silicon / Intel） |

## 平台支持

**仅支持 macOS**（Apple Silicon + Intel），发布产物为 `.dmg` 安装包。

## 自动更新

- 启动后自动检查 GitHub Releases 最新版本（后台，不阻塞；走 releases.atom，不受 API 限流）
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
| [多平台发布](docs/多平台发布.md) | GitHub Actions 打包 macOS 安装包与发布门禁 |
| [贡献指南](CONTRIBUTING.md) | 开发环境与 PR 约定 |
| [安全政策](SECURITY.md) | 漏洞报告方式 |
| [更新日志](CHANGELOG.md) | 版本变更 |
| [开源许可](LICENSE) | MIT |
| [第三方声明](THIRD-PARTY-NOTICES.md) | 依赖许可证聚合 |

## 参与贡献

欢迎 Issue 与 PR，详见 [CONTRIBUTING.md](CONTRIBUTING.md)。参与即同意 [行为准则](CODE_OF_CONDUCT.md)。

## 许可证

[MIT](LICENSE) © yqstart
