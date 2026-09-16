# 第三方开源软件声明

本产品（Mino）以 MIT 许可证开源，并包含来自以下开源社区的代码与资源。
完整版权与许可证文本以各上游项目仓库为准。

---

## 核心依赖

### alacritty_terminal（终端仿真内核：VT 解析 + PTY 管理）
- 仓库：https://github.com/alacritty/alacritty
- 许可证：Apache-2.0
- 用途：终端状态机、转义序列解析、PTY 会话

### vte（VT 转义序列解析器）
- 仓库：https://github.com/alacritty/vte
- 许可证：Apache-2.0 OR MIT
- 用途：ANSI 转义序列解析

### egui / eframe（GUI 框架）
- 仓库：https://github.com/emilk/egui
- 许可证：MIT OR Apache-2.0
- 用途：界面渲染、窗口管理（wgpu 后端）

### wgpu（GPU 渲染后端）
- 仓库：https://github.com/gfx-rs/wgpu
- 许可证：MIT OR Apache-2.0
- 用途：Metal / Vulkan / DX12 图形 API

### russh（SSH 协议客户端）
- 仓库：https://github.com/warp-tech/russh
- 许可证：Apache-2.0
- 用途：SSH 连接、认证、远程终端通道

### russh-sftp（SFTP 子系统）
- 仓库：https://github.com/AspectUnk/russh-sftp
- 许可证：Apache-2.0
- 用途：SFTP 文件传输

### ssh-key（密钥解析）
- 仓库：https://github.com/RustCrypto/SSH
- 许可证：Apache-2.0 OR MIT
- 用途：OpenSSH 私钥 / 公钥格式解析

### tokio（异步运行时）
- 仓库：https://github.com/tokio-rs/tokio
- 许可证：MIT
- 用途：远程会话异步 I/O

### rfd（原生文件对话框）
- 仓库：https://github.com/PolyMeilex/rfd
- 许可证：MIT
- 用途：SFTP 上传 / 下载的本地文件选择

---

## 其他依赖

| crate | 许可证 | 用途 |
|---|---|---|
| serde / serde_derive | MIT OR Apache-2.0 | 配置序列化 |
| toml | MIT OR Apache-2.0 | hosts.toml 解析 |
| thiserror | MIT OR Apache-2.0 | 错误类型 |
| log / env_logger | MIT OR Apache-2.0 | 日志 |
| parking_lot / lock_api | MIT OR Apache-2.0 | 锁（egui 依赖） |
| egui_kittest / kittest（测试） | MIT OR Apache-2.0 | UI 渲染测试 |
| image（测试） | MIT OR Apache-2.0 | 测试截图 |

---

## 打包字体

### JetBrains Mono（终端等宽主字体，随应用打包：`crates/mino-app/fonts/`）
- 仓库：https://github.com/JetBrains/JetBrainsMono
- 许可证：SIL Open Font License 1.1（许可证全文见 `crates/mino-app/fonts/OFL.txt`；OFL 允许随软件打包分发）
- 用途：终端 Monospace 族主字体（Regular / Italic / Bold / BoldItalic 四字重，`main.rs::setup_fonts` 经 `include_bytes!` 编译期嵌入）
- 版本：v2.304（静态 TTF，非 variable）

---

## 主题参考

- **Catppuccin Mocha**（终端调色板）：https://github.com/catppuccin/catppuccin（MIT）
- **MiroCode**（视觉体系参考）：https://github.com/yqstart/MiroCode（MIT）
