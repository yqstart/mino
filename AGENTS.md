# Mino 项目架构

以 [Kaku](https://github.com/tw93/Kaku)（WezTerm 系）为视觉与交互基准的 macOS 终端：在 Kaku 的终端体验之上，提供**可视化面板**（SFTP 文件面板、统一工作区、AI 助手）与完整的 **SSH / SFTP** 支持。Rust workspace，两个 crate。

**产品方向（规划中）**：接入用户本机已安装的 AI agent，首批 pi、oh my pi（omp）、Claude Code、Codex CLI、opencode（见 README）。当前 AI 助手为 OpenAI 兼容 API 形态。

**工程规范总纲见 `~/AGENTS.md`；本文件只放项目级事实与硬约束。**

## 技术选型

| 组件 | 选型 |
|---|---|
| GUI | eframe / egui 0.36 + wgpu（终端行网格自管 GPU 提交） |
| 终端内核 | alacritty_terminal 0.26（VT 仿真 + PTY，vte 解析） |
| SSH / SFTP | russh 0.62 + russh-sftp 2.4（ring 后端，纯 Rust 无 C 依赖） |
| HTTP | ureq 3（更新检查 releases.atom；AI 客户端 SSE 流式） |
| 配置 | serde + toml（`hosts.toml` / `known_hosts.toml` / `assistant.toml` / `snapshot.toml`） |
| 剪贴板 / 对话框 | arboard 3.6 + image 0.25（仅 PNG）；rfd 0.15 |
| macOS 原生 | objc2-app-kit（窗口圆角、毛玻璃） |
| UI 测试 | egui_kittest 0.36 + kittest（accesskit） |

## 目录结构

```
crates/
├── mino-core/                          # 纯引擎，禁止依赖 egui
│   ├── src/terminal/mod.rs            # Session 统一封装（本地/远程）、Listener、PtyWrite 队列
│   ├── src/terminal/sniffer.rs        # OSC 7/133/9;4 嗅探（本地 sniffing pty / 远程读循环共用）
│   ├── src/terminal/keys.rs           # 键盘 → 字节序列（xterm 修饰键 + kitty 键盘协议）
│   ├── src/ssh/mod.rs                 # connect_remote / connect_verified（TOFU）、共享 runtime、keepalive
│   ├── src/ssh/known_hosts.rs         # 主机密钥校验 + known_hosts.toml（0600、原子写）
│   ├── src/ssh/sftp.rs                # connect_sftp、SftpHandle、SftpEvent
│   ├── src/config/mod.rs              # Workspace（local/ssh 融合）+ HostConfig + hosts.toml
│   ├── src/ai/                        # AI 客户端：config / client（SSE）/ prompt
│   ├── src/shell_integration.rs       # 受管 zsh 片段 + 插件 bootstrap
│   ├── assets/shell-integration/mino.zsh
│   ├── src/updater.rs                 # releases.atom 检查 + dmg 下载 + 安装脚本
│   └── tests/                         # ssh/sftp 集成测试（需本地测试 sshd: 127.0.0.1:2222）
└── mino-app/                            # egui 应用
    ├── src/main.rs                    # 入口 + 字体装配（同步主字体 + 后台 CJK）+ 圆角 + 崩溃日志
    ├── src/app.rs                     # MinoApp：标签/分屏/设置/工作区/快捷键/更新状态机
    ├── src/native.rs                  # macOS 原生窗口（圆角、毛玻璃）
    ├── src/theme.rs                   # Kaku Dark/Light 主题 + 终端调色板 + 覆写表
    ├── src/anim.rs / dialog.rs / clip_image.rs / workdir.rs
    └── src/views/
        ├── terminal_view.rs           # cell 渲染、输入、滚动、搜索、右键、链接、IME
        ├── terminal_gpu.rs (+ .wgsl)  # 自管 GPU 行网格渲染
        ├── pane_tree.rs               # 分屏布局树（纯函数）
        ├── link_detect.rs             # 裸文本链接识别
        ├── ai_panel.rs                # ⌘L AI 聊天面板
        ├── launcher.rs                # 标签导航（⌘⇧O）/ 命令面板（⌘⇧P）
        └── sftp_view.rs               # 文件面板（虚拟化列表/导航/传输进度）
```

## 核心架构

### 终端会话（本地/远程统一）

- 本地：`tty::new` 建 PTY → alacritty `EventLoop` 线程读 → vte → `Term`；**会话在后台线程创建**（`pending_local` mpsc + 帧内 `poll_local_spawn` 挂载），不堵首帧与 ⌘T；测试构建走同步路径
- 远程：共享 tokio runtime → SSH channel 读 → `Processor::advance` → 同一 `Term`
- 统一接口 `Session { term, writer, resizer, shuttor, is_remote }`（闭包抽象）；写操作本地走 `EventLoopSender`、远程走命令队列，UI 线程非阻塞
- **shell 集成标记**：`mino.zsh` 的 OSC 133 A/B/C/D、OSC 7、OSC 9;4 在本地（SniffingPty 包装读取路径）与远程（读循环内 `OscSniffer`）嗅探 → `SessionEvent::{Cwd, PromptReady, CommandStart, CommandFinished, Progress}`；用途：权威工作目录、AI 失败建议、标签注意力点、shell 空闲判定
- `Session::drop` 发 Shutdown；本地会话先 SIGHUP、守护线程延时 300ms 补 SIGKILL（shell 不响应 SIGHUP 时兜底，否则 UI 线程阻塞）

### 终端渲染

- **行级增量**：每帧同一持锁内 `Term::damage()` + `reset_damage()`，再 `renderable_content()`；**damage 行号 = 显示行**（网格行号 + display_offset）
- **行缓存按网格行号索引**（`HashMap<i32, RowCache{hash, galley, backgrounds}>`）：滚动后同一网格行直接命中，只重建滚入的新行；hash 指纹 = fg+bg+样式+字符（不含光标效果）
- **两条渲染路径共用行缓存**：
  - 自管 GPU（`terminal_gpu.rs`）：逐行顶点常驻显存，行不变不重传、滚动只改 uniform、空闲零上传；区间分配器 `v_off/v_bytes/free_*`；`MAX_ROWS = 256`
  - egui 回退（kittest / 无 wgpu）：行内容合成单个 `Mesh` + `shapes_scratch` 一次 `Painter::extend`
- **宽字符逐 cell 成段**：CJK 每个 cell 单独定位（`start_col * cell_width`），**不得合并成段**——字体 advance（1em）与终端双列宽不一致，合并会累积漂移（光标离文字越来越远）
- resize：网格立即更新（字不越界），后台通知 50ms 合并 + trailing 补发；**网格最小值钳制 20×4**；远端尺寸在 `REMOTE_RESIZE_SYNC_DURATION`（2s）时间窗口内每渲染帧补发
- `pixels_per_point` 变化清空全部缓存；`rows_cache.len() > rows*4` 时只保留可见网格行

### 终端能力应答与输入（`terminal/mod.rs` + `terminal_view.rs` + `keys.rs`）

- 程序查询必须应答：OSC 颜色（索引边界检查，`COUNT = 269`；OSC 动态覆盖 > 主题调色板）、文本区尺寸、焦点上报（DECSET 1004）、鼠标上报（1000/1002/1003 + SGR 1006）、OSC52、OSC8
- **DECRQM 2026 应答「不支持」**：在 `Listener` 的 `PtyWrite` 出口按**任意包内容**改写（程序会把多个查询混在同一次写入；本地/远程/测试三条路径都要覆盖），否则程序用 BSU/ESU 包裹每次重绘导致输出错乱
- `PtyWrite` 队列 64KB 上限，超限丢最旧（程序可触发的应答，防止后台标签无界增长）
- **kitty 键盘**：`term_config()` 在本地 spawn / 远程连接 / 测试**三处同源**（`kitty_keyboard = true` + `default_cursor_style = BlinkingBar`）；订阅后只有 Escape、带修饰的 Enter/Tab/Backspace 与 ctrl/alt 文本键走 CSI-u，方向/Home/End/F1-F12 与无修饰键保持 legacy；Shift+Enter = `ESC[13;2~`、Ctrl+Enter = `\n`
- `COLORTERM=truecolor` 注入本地与远程会话（本地 `local_session_options_at` + 远程 `set_env`，sshd 不放行则静默忽略）
- 输入合并：一帧内的按键/粘贴攒进 `pending_writes` 一次写入

### 工作目录跟踪（`workdir.rs`）

- 根据 PTY 可见输入维护尽力而为的目录（本地 `canonicalize` 校验，远程词法归一化）；shell 集成标记提供权威值（`SessionEvent::Cwd`）
- 底栏等高频读取走 `effective_local_directory`（300ms TTL）；需要即时真值走 `fresh_local_directory`

### SFTP

- `connect_sftp` 独立 SSH 连接（共享 runtime，任务存活到 `Shutdown`）；UI 持 `SftpHandle` 发命令，后台经 `SftpEvent`（`Listed/Progress/Done/Error/Closed`）回报
- 失败清理半成品（下载删本地、上传删远端）；主机名绑定在连接上（多远程标签不串台）
- 列表 `show_rows` 虚拟化；面板 `poll_events` 收到新事件才 `request_repaint`（事件驱动）

### 连接生命周期

- **TOFU fail-closed**：仅 `NotFound` 视为首次连接；读取/解析/保存失败一律拒绝（不降级重新信任）；终端与 SFTP 都经 `connect_verified`
- 30s keepalive + 3 次无响应断开；私钥路径自动展开 `~`
- **进程级共享 tokio runtime**（`ssh::shared_runtime()`，上限 8 worker）；runtime 不可用时失败事件也必须回传（否则 SFTP 面板永久停留「连接中」）

### 配置与持久化

- `~/.config/mino/hosts.toml`：统一 `[[workspaces]]`（`kind = "local" | "ssh"`）；旧 `hosts`/`projects` 为只读迁移字段；偏好字段（`appearance`、`font_size` 等）同文件
- 全部配置（hosts / known_hosts / assistant / snapshot）**0600 + 原子写**（临时文件 + fsync + rename + 父目录同步）
- `hosts.toml` 解析失败：先备份 `.bak` 再按空配置启动 + toast，**绝不静默清空**
- `snapshot.toml`（窗口快照，⌘⌥⇧T 恢复）**含明文认证信息**，权限与 hosts 同级

### AI 助手（现形态）

- `mino-core/src/ai/`：纯客户端层（config 解析、SSE 流式、提示词）；UI 在 `mino-app`（`ai_panel.rs` + app 层任务）
- `assistant.toml` 0600；认证 `api_key` 或 `codex`（读 `~/.codex` 的 config.toml + auth.json；ChatGPT 登录态不支持）；错误不拼接 api_key
- 触发：`133;D` 非 0 → 修复建议（⌘⇧E 填入不执行）；`# 描述` 回车 → 生成命令回填；⌘L 聊天面板（流式）

### 更新器与崩溃日志

- 更新：releases.atom（不受 API 限流）→ 版本比较 → dmg 直链下载（进度）→ 后台脚本 `hdiutil attach` / 等待退出 / `ditto` 替换（失败回退 `~/Applications`）/ 重启
- 崩溃日志 `~/.config/mino/crash.log`（**追加**写，写失败回落 `/tmp`）；下次启动归档为 `crash.log.1` 并 toast 路径（测试构建跳过；行为有单测覆盖）

### 窗口与外观

- 布局：`Panel::top`（28pt 拖拽条，红绿灯悬浮）+ `Panel::bottom`（底栏标签）+ 右侧 SFTP 面板 + CentralPanel；无独立状态栏
- 主题：Kaku Dark / Kaku Light（`THEMES = [深, 浅]`），`resolve_appearance(auto|dark|light, system_dark)`；每帧检查系统外观变化
- Kaku token：应用/终端/标签栏同底 `#15141b`、surface `#1f1d28`、surface_active `#29263c`、紫 `#8e6ad9`、琥珀 `#daae76`、`RADIUS_SM = 6` / `RADIUS_ITEM = 4`
- 终端调色板原样搬 Kaku + `color_overrides`/`foreground_color_overrides` 覆写表 + `text_min_contrast_ratio = 3.0`；粗体不增亮（换字重族）
- 字体：JetBrains Mono 三字重（命名族 `mino-mono-dark` 等，按主题深浅 × 粗体选择；裸 Context 测试回退 Monospace）+ Symbols Nerd Font Mono + Menlo + CJK 后台加载
- 分屏：布局树 `pane_tree`（纯函数）；间隙 = 5 字宽（`pane_gap()` 实测字宽换算）

## 工程铁律（必须遵守）

1. **`mino-core` 禁止依赖 egui**；UI 与引擎分层不可打破
2. **锁序唯一合法顺序：先终端 `FairMutex<Term>`、后 egui `Context`**；反向嵌套死锁
3. **自管 GPU 回调内绝不触碰 `egui_wgpu::Renderer`**（渲染期持锁，回调内取锁必死锁）；图集绑定只允许在 UI 线程帧内刷新
4. **GPU 行槽位按顶点尺寸对齐**：容量一律走 `slot_capacity`（`align_up` 到 `VERTEX_BYTES`/`INDEX_BYTES`）——`draw_indexed` 的 `base_vertex` 是顶点索引，偏移不是网格步长的整数倍会整行错读（历史事故：除首行外全部不显示）
5. **`screen_size` 必须是整个渲染目标的点尺寸**（窗口），不是终端内容区（否则文字整体拉伸错位）
6. **图集换代看门狗双条件**：尺寸变化 **或** 填充率骤降（epaint 重建字体系统时尺寸可能相同但字形位置全变）；并在下一帧开头再比一次
7. **宽字符逐 cell 成段**（见渲染节），不得合并优化
8. **`term_config()` 三处同源**（本地/远程/测试），改一处必须三处同步
9. **焦点铁律**：任何程序化切换活动窗格必须同时 `request_pane_focus`（否则下一帧焦点跟随拉回旧窗格）；`TerminalView::focus_id` 每实例唯一
10. **测试绝不能读写用户真实配置**：涉及配置的测试用 `MinoApp::new_with_config(test_config_path("标签"))`（`/tmp` 隔离）；历史事故：测试曾覆盖并删除用户真实 hosts.toml
11. **UI 测试必须串行**（`-- --test-threads=1`）；主题/外观测试必须持 `THEME_TEST_LOCK`（`CURRENT_THEME` 是进程级静态）
12. **测试构建跳过崩溃日志检查**（`#[cfg(not(test))]`）；shell 集成测试用隔离 zshrc 路径
13. **配置写入统一 0600 + 原子写**；解析失败先备份再空启动，绝不静默清空
14. **TOFU 只对 `NotFound` fail-open**，其余失败一律拒绝
15. **CJK fallback 用 `ctx.add_font`（`FontPriority::Lowest`）增量追加**，禁止「克隆定义 + `set_fonts`」（跨帧比较吞并）；渲染侧按字体定义指纹在帧开头比对、命中才清行缓存；**禁止加载 Apple Color Emoji**（ab_glyph 无法解析）
16. `resolve_color` 优先级：**OSC 动态覆盖 > 主题调色板**；`text_min_contrast_ratio = 3.0` 在 `resolve_cell` 内强制
17. 尺寸/时间常量集中在来源处定义（如 `MIN_TERMINAL_COLS/ROWS`、`REMOTE_RESIZE_SYNC_DURATION`），不要散落魔法数
18. egui 0.36 特定坑：`App::ui` 替代 `update`；`Fonts` 需 `fonts_mut`；`Event::Key` 无 `text` 字段；事件帧后焦点可能被 dead-man-switch 清除（`TerminalView` 用 `had_focus` 每帧补 `request_focus`）；`consume_key` 是子集匹配，带 Shift 的组合必须先于裸组合消费（如 ⌘⇧T 先于 ⌘T）
19. **提示符里的 `?➜` 是 zsh 主题（robbyrussell `%1{➜%}`）的真实输出，不是渲染 bug，不要"修复"**

## Kaku 对齐注记

- 键位族、标签标题（本地：父目录/当前目录；远程：主机名；多窗格 `∙` 连接；放大态 `[Z]`）、窗口标题 `[i/n]`、`COLORFGBG` 注入、`OSC 9;4` 注意力点、拖选滚轮扩展、文件链接编辑器（`file_link_editor` 解析顺序：配置 → `code`/`cursor` → `$VISUAL`/`$EDITOR` → 系统默认）、右 Option 组合字符、窗口不透明度/毛玻璃等行为细节以**回归测试**为准（`cargo test` 中「Kaku 对齐 B 组」相关用例）
- 与 Kaku 的已知差异：单标签也保留底栏（Mino 无菜单栏）；⌘W 最后一个标签为最小化而非隐藏；替代屏滚轮维持「映射方向键」（alacritty 引擎下替代屏无回滚历史）

## 常用命令

```bash
cargo run                                     # 开发运行
cargo test --workspace -- --test-threads=1    # 全量测试（UI 必须串行）
cargo clippy --workspace --all-targets        # 零警告
cargo fmt --all
./scripts/test-sshd.sh start                  # 集成测试前置（127.0.0.1:2222）
./scripts/package-macos.sh 2.0.0 arm64        # 本地打包预览
```

## 发布流程

- 三件套一致：`Cargo.toml` 版本 == tag 去 `v` == `CHANGELOG.md` 顶部 `## [版本]` 段落（Release 工作流强制校验）
- **先在 `main` 合并代码再打 tag**；禁止在功能/诊断分支直接打 tag
- UI 测试（kittest）只在 Intel runner 跑；详见 `docs/多平台发布.md`

## 提交与协作

- 提交信息：`type(scope): English past-tense summary`，正文中文（`omp commit` 生成）
- 功能改动附回归测试；测试断言行为与边界，不测实现细节
- 自管 GPU 路径 kittest 覆盖不到（无 wgpu 后端），需实机 `cargo run -p mino-app` 或临时 `.wgpu()` harness 验证
