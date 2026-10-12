# Mino

Mino 是基于 Rust + egui 的轻量级终端与 SFTP 可视化工具。

**许可证：[MIT](LICENSE)** · 纯开源免费

## 产品定位

主打**轻量、快速、简洁**的终端体验：本地 / 远程（SSH）终端 + 可视化 SFTP 文件管理 + 统一工作区（SSH 与目录收藏融合）；视觉与交互基准是 [Kaku](https://github.com/tw93/Kaku)（WezTerm 系）——Kaku Dark / Kaku Light 两套配色跟随系统外观，底栏式标签、分屏窗格、搜索、标签导航、命令面板与 Kaku 键位族。品牌标记为 `>_`。

## 功能概览

- **本地终端**：基于 [alacritty_terminal](https://github.com/alacritty/alacritty) 生产级内核（VT 仿真 + PTY），Kaku 同款配色（Kaku Dark / Kaku Light 跟随系统）、真彩、宽字符、滚动、括号粘贴、闪烁竖条光标、对比度下限与覆写表
- **分屏窗格**：⌘D 左右 / ⌘⇧D 上下拆分，⌘⌥ 方向键移焦、⌘⌃ 方向键调整、⌘⇧Enter 放大、右键菜单拆分/关闭
- **搜索与导航**：⌘F 搜索滚动缓冲（高亮 + 上下导航 + 大小写开关）、⌘⇧O 标签导航器、⌘⇧P 命令面板（fuzzy 过滤）
- **SSH 远程终端**：密码 / 私钥认证（支持口令），xterm-256color PTY；标签标题显示主机名
- **SFTP 可视化**：终端 | SFTP 双栏分屏（可拖拽、默认收起），目录导航（单击选中、再次单击进入）、Shift / ⌘ 多选、右键菜单（上传 / 下载 / 重命名 / 删除 / 新建文件夹 / 刷新 / 定位到终端目录 `⌘⇧L`）、上传下载进度与结果徽标（带确认对话框）
- **统一工作区（SSH 与项目收藏融合）**：本地目录与 SSH 主机同一个列表（`~/.config/mino/hosts.toml` 的 `[[workspaces]]`，0600 权限、原子写入、损坏自动备份），标签栏「工作区」按钮单击即开、⌘O 搜索面板、设置单卡片管理；`⌘B` 收藏当前会话（本地→目录、远程→主机档案）
- **AI 助手**：`# 自然语言` 回车生成命令回填提示符；失败命令自动给修复建议（⌘⇧E 填入，不自动执行）；⌘L 流式聊天面板；OpenAI 兼容连接（api_key 或跟随 Codex）
- **Shell 集成套件**：首次运行向 `~/.zshrc` 写入可一键卸载的受管片段；zsh 自动补全 / 语法高亮 / 自动建议 / z 目录跳转 / Smart Tab，starship 走预设配置
- **窗口快照**：退出自动保存标签布局，⌘⌥⇧T 恢复上次窗口
- **字体**：打包 JetBrains Mono 三字重（Regular / Medium / SemiBold）+ Symbols Nerd Font Mono + 系统 CJK fallback（PingFang / STHeiti），中文与提示符/图标符号开箱即用
- **快捷键**：⌘T 新建标签、⌘W 智能关闭（窗格→标签→最小化）、⌘⇧W 关标签、⌘⇧T 重开、⌘1-⌘9 与 ⌘⇧[ ] 切换标签、⌘D / ⌘⇧D 分屏、⌘F 搜索、⌘K 清屏、⌘O 工作区、⌘B 收藏、⌘N 新增工作区、⌘L AI 聊天、⌘, 设置、⌘+ / ⌘- / ⌘0 字号、⌘⌃F 全屏
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
