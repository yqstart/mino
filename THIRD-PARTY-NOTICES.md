# 第三方开源软件声明

本产品（Mino）以 MIT 许可证开源，并包含来自以下开源社区的代码与资源。
完整版权与许可证文本以各上游项目仓库为准。

---

## 核心依赖

### alacritty_terminal（终端仿真内核：VT 解析 + PTY 管理）
- 仓库：https://github.com/alacritty/alacritty
- 版本：0.26
- 许可证：Apache-2.0
- 用途：终端状态机、转义序列解析、PTY 会话（附带的 `vte` 0.15 解析器为 Apache-2.0 OR MIT）

### egui / eframe / epaint（GUI 框架）
- 仓库：https://github.com/emilk/egui
- 版本：0.36
- 许可证：MIT OR Apache-2.0
- 用途：界面渲染、窗口管理；图形后端为 `wgpu` 30（MIT OR Apache-2.0）、窗口层为 `winit` 0.30（Apache-2.0）

### russh（SSH 协议客户端）
- 仓库：https://github.com/warp-tech/russh
- 版本：0.62（ring 后端）
- 许可证：Apache-2.0
- 用途：SSH 连接、认证、远程终端通道

### russh-sftp（SFTP 子系统）
- 仓库：https://github.com/AspectUnk/russh-sftp
- 版本：2.4
- 许可证：Apache-2.0
- 用途：SFTP 文件传输

### ssh-key（密钥解析，经 russh 使用）
- 仓库：https://github.com/RustCrypto/SSH
- 许可证：Apache-2.0 OR MIT
- 用途：OpenSSH 私钥 / 公钥格式解析

### tokio（异步运行时）
- 仓库：https://github.com/tokio-rs/tokio
- 版本：1
- 许可证：MIT
- 用途：远程会话与 SFTP 的异步 I/O

### ureq（HTTP 客户端）
- 仓库：https://github.com/algesten/ureq
- 版本：3
- 许可证：MIT OR Apache-2.0
- 用途：应用内更新（releases.atom 版本检查与 dmg 下载）

### arboard（剪贴板）
- 仓库：https://github.com/1Password/arboard
- 版本：3.6
- 许可证：MIT OR Apache-2.0
- 用途：终端选区复制、OSC52 剪贴板、图片粘贴

### rfd（原生文件对话框）
- 仓库：https://github.com/PolyMeilex/rfd
- 版本：0.15
- 许可证：MIT
- 用途：SFTP 上传 / 下载的本地文件选择

### image（图像编解码）
- 仓库：https://github.com/image-rs/image
- 版本：0.25（仅启用 PNG 特性）
- 许可证：MIT OR Apache-2.0
- 用途：应用图标解码、剪贴板截图编码

### objc2 / objc2-app-kit / objc2-foundation（macOS 平台绑定）
- 仓库：https://github.com/madsmtm/objc2
- 版本：0.6 / 0.3
- 许可证：MIT（`objc2-app-kit` 为 MIT 或 Zlib OR Apache-2.0 OR MIT）
- 用途：无边框窗口整体圆角（AppKit CALayer）、原生粘贴板

### raw-window-handle（窗口句柄抽象）
- 仓库：https://github.com/rust-windowing/raw-window-handle
- 许可证：MIT OR Apache-2.0 OR Zlib
- 用途：从 eframe 取 AppKit 窗口句柄

---

## 其他依赖

| crate | 许可证 | 用途 |
|---|---|---|
| serde / serde_derive | MIT OR Apache-2.0 | 配置序列化 |
| toml | MIT OR Apache-2.0 | hosts.toml 解析 |
| thiserror | MIT OR Apache-2.0 | 错误类型 |
| log / env_logger | MIT OR Apache-2.0 | 日志 |
| parking_lot / lock_api | MIT OR Apache-2.0 | 锁（egui 依赖） |
| bytemuck | Zlib OR Apache-2.0 OR MIT | GPU 顶点缓冲的 Pod 转换 |
| libc（仅 unix 目标） | MIT OR Apache-2.0 | 本地会话关闭的 SIGHUP / SIGKILL 兜底 |
| egui_kittest / kittest / accesskit（测试） | MIT OR Apache-2.0 | UI 渲染测试与无障碍树断言 |
| rand（测试） | MIT OR Apache-2.0 | known_hosts 测试生成临时密钥对 |

---

## 打包字体

### JetBrains Mono（终端等宽主字体，随应用打包：`crates/mino-app/fonts/`）
- 仓库：https://github.com/JetBrains/JetBrainsMono
- 版本：v2.304（静态 TTF，非 variable）
- 许可证：SIL Open Font License 1.1（许可证全文见 `crates/mino-app/fonts/OFL.txt`；OFL 允许随软件打包分发）
- 用途：终端 Monospace 族主字体，**Regular / Medium / SemiBold 三字重**（Kaku 字重体系：深色主题正文 Regular、粗体 Medium；浅色主题正文 Medium、粗体 SemiBold；`main.rs::setup_fonts` 经 `include_bytes!` 编译期嵌入，按命名族供终端渲染选择）

### Symbols Nerd Font Mono（Nerd Fonts 符号集，随应用打包）
- 仓库：https://github.com/ryanoasis/nerd-fonts（Symbols-only 子集）
- 许可证：MIT（许可证全文见 `crates/mino-app/fonts/SymbolsNerdFont-LICENSE.txt`）
- 用途：等宽符号兜底（私有区图标，如 SSH 锁形 U+F08C0 等），排在 JetBrains Mono 之后

其余字体（Menlo、PingFang / STHeiti）由 macOS 系统提供，不随应用分发。

---

## 设计参考

- **Tabby 终端**（主题变量 `tabby-core/theme.vars.scss`，MIT）：Mino 的深色分层界面、扁平控件与低对比 hover 的视觉参照
- **Catppuccin Mocha**（https://github.com/catppuccin/catppuccin，MIT）：早期版本终端调色板的取色参考；当前三套主题调色板为项目自定义
