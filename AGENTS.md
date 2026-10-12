# Mino 项目架构

轻量终端 + SFTP 可视化工具。Rust workspace，两个 crate。
**视觉与交互基准：Kaku（tw93/Kaku，WezTerm 系）**——配色（Kaku Dark/Light 跟随系统）、底栏标签、分屏、搜索、标签导航、命令面板与键位族都照搬 Kaku；SSH 主机与目录收藏融合为统一「工作区」。

## 技术选型

| 组件 | 选型 | 理由 |
|---|---|---|
| GUI | eframe/egui 0.36 + wgpu | 原生轻量、macOS 走 Metal |
| 终端内核 | alacritty_terminal 0.26 | 生产级 VT 仿真 + PTY（Alacritty 自用） |
| SSH/SFTP | russh 0.62 + russh-sftp 2.4 | 纯 Rust 无 C 依赖 |
| 配置 | serde + toml | hosts.toml 持久化 |
| 应用内更新 | ureq 3（releases.atom 检查 + dmg 下载） | 轻量 HTTP、不受 GitHub API 限流 |
| 剪贴板 / 文件对话框 | arboard 3.6（图片）+ rfd 0.15 | 图片粘贴与原生选择面板 |
| AI 客户端 | ureq 3（SSE 流式，`mino-core/src/ai/`） | 与更新检查同一套轻量 HTTP |
| Shell 集成 | 内嵌 zsh 脚本（`mino-core/assets/shell-integration/mino.zsh`） | 受管片段写入 `~/.zshrc`；OSC 133/7 标记；插件 git clone 到 `~/.config/mino/shell/vendor` |
| 字体 | JetBrains Mono 三字重 + Symbols Nerd Font Mono | Kaku 同款字重体系（见「字体」节） |
| UI 测试 | egui_kittest | 渲染级断言 |

## 目录结构

```
crates/
├── mino-core/                 # 纯引擎，禁止依赖 egui
│   ├── src/lib.rs
│   ├── src/terminal/mod.rs   # Session 统一封装（本地/远程）、TermSize、Listener、shell 标记事件
│   ├── src/terminal/sniffer.rs # OSC 7/133 嗅探（SniffingPty 包装读取路径；远程在读循环内嗅探）
│   ├── src/terminal/keys.rs  # 键盘 → 字节序列编码（XTerm 修饰键）
│   ├── src/ssh/mod.rs        # connect_remote（后台线程 + tokio）、authenticate、connect_verified（TOFU + keepalive 配置）
│   ├── src/ssh/known_hosts.rs # HostKeyVerifier（TOFU 主机密钥校验）+ known_hosts.toml 持久化（0600）
│   ├── src/ssh/sftp.rs       # connect_sftp、SftpHandle（命令队列）、SftpEvent（事件流）
│   ├── src/config/mod.rs     # Workspace（local/ssh 融合）/HostConfig + hosts.toml；旧 hosts/projects 只读迁移
│   ├── src/ai/               # AI 助手客户端：config（assistant.toml 0600）/client（SSE 流式）/prompt（中文提示词）
│   ├── src/shell_integration.rs # 受管 zsh 片段安装/卸载、插件 bootstrap、starship 预设
│   ├── assets/shell-integration/mino.zsh # 内嵌 zsh 集成脚本（守卫 + OSC 133/7 + 插件 + Smart Tab）
│   ├── src/updater.rs        # 应用内更新（releases.atom 检查、dmg 下载、安装脚本）
│   └── tests/                # 集成测试（需本地测试 sshd: 127.0.0.1:2222）
└── mino-app/                  # egui 应用
    ├── src/main.rs           # 入口 + 字体装配（同步主字体，CJK fallback 走后台 CjkFontLoader）+ 圆角注入
    ├── src/native.rs         # macOS 原生窗口定制（无边框窗口整体圆角，AppKit layer）
    ├── src/app.rs            # MinoApp：tabs 列表 + 设置弹窗（show_settings）+ 布局/连接管理/对话框/快捷键；本地会话后台创建（poll_local_spawn 帧内挂载）
    ├── src/anim.rs           # 动效：指数平滑 / 扫光 / 圆角渐变 / 辉光（无第三方依赖）
    ├── src/clip_image.rs     # 剪贴板图片与 Finder 文件 → shell 转义路径 token（远程经 SFTP 中转）
    ├── src/dialog.rs         # 弹窗外壳 / 卡片 / 头部标题的共用样式
    ├── src/theme.rs          # Kaku Dark / Kaku Light 两套主题 + 终端调色板/覆写表
    ├── src/workdir.rs        # 根据 PTY 输入 + shell 集成标记跟踪本地/远程目录
    └── src/views/
        ├── terminal_view.rs  # cell 渲染（行级增量 + Galley 缓存 + 自管 GPU 提交）、输入转发、滚动、搜索/右键/链接
        ├── terminal_gpu.rs   # 自管 GPU 渲染：逐行顶点常驻显存 + 动态 uniform + 图集换代看门狗
        ├── terminal_gpu.wgsl # 行网格着色器（与 egui.wgsl 同源的颜色/抖动/gamma 语义）
        ├── pane_tree.rs      # 分屏布局树（几何分配/拆分/移除/比例/邻居；纯函数）
        ├── link_detect.rs    # 裸文本链接识别（URL/裸域名/文件路径，手写线性扫描）
        ├── ai_panel.rs       # ⌘L AI 聊天面板（流式渲染 + 极简 Markdown）
        ├── launcher.rs       # 标签导航（⌘⇧O）与命令面板（⌘⇧P）共用浮层
        └── sftp_view.rs      # 文件面板：列表（show_rows 虚拟化）/导航/传输进度/确认对话框
```

## 核心数据流

### 终端会话（本地与远程统一）

- 本地：`tty::new` 建 PTY → alacritty `EventLoop` 线程读 → vte 解析 → `Term` 网格
- 远程：后台线程 tokio runtime → SSH channel 读 → `Processor::advance` → 同一 `Term`
- 统一接口：`Session { term, writer, resizer, shuttor, is_remote }`（Writer/Resizer/Shuttor 闭包抽象）；本地与远程会话共用同一终端输入与渲染链路
- **shell 集成标记（v2.0）**：`mino.zsh` 输出的 OSC 133;A/B/C/D 与 OSC 7 在两条读取路径上嗅探——本地经 `sniffer::pty::SniffingPty`（dup 的 ptmx fd 包装读句柄，事件循环注册的仍是内层 fd），远程直接在 `remote_loop` 里喂 `OscSniffer`；标记转成 `SessionEvent::{Cwd, PromptReady, CommandStart, CommandFinished}`（`Cwd`/`PromptReady` 队列去重只留最后一条）。用途：权威工作目录（本地+远程）、AI 失败建议（退出码）、标签注意力点、关闭确认的 shell 空闲判定
- 写操作：本地走 `EventLoopSender`，远程走命令队列（UI 线程非阻塞）
- **`term_config()` 三处同源**（本地 spawn/远程连接/测试）：`kitty_keyboard = true` + `default_cursor_style = BlinkingBar`（Kaku 默认光标；程序 DECSCUSR 可覆盖，`CSI 0 q` 复位回默认）
- **本地会话后台创建（v1.1.2，`app.rs`）**：`new_local_tab_at` 只登记 `pending_local`（`mpsc`），后台线程 `Session::spawn_local` 完成后 `request_repaint`，UI 帧内 `poll_local_spawn` 就绪即挂标签——PTY fork + oh-my-zsh 初始化（可达数百毫秒）不再堵首帧与 ⌘T，等待期底栏状态指示显示"终端启动中…"；**测试构建仍走同步路径**（kittest `run_steps` 不等待后台线程），异步挂载由回归测试 `本地终端异步就绪后挂载标签` 直接驱动 `poll_local_spawn` 覆盖
- 渲染：UI 锁 `FairMutex<Term>` → `renderable_content()` → 逐行 hash 缓存 LayoutJob（增量重建）

### 终端渲染（v0.7 起为行级增量，`terminal_view.rs`）

- **行级增量核心**：每帧锁内先 `Term::damage()` 收集损坏行 + `reset_damage()`（必须同一持锁内），再 `renderable_content()` 拿 display_offset/cursor/colors。**damage 返回行号 = 网格行号 + display_offset = 显示行号**（`TermDamageIterator` 内部 `line.line + display_offset`），直接用 `damaged.contains(&v)` 判断显示行 v 是否需重建
- **显示行 ↔ 网格行映射**：显示行 v ↔ 网格行 `Line(v - display_offset)`（display_iter 同语义：每个网格行一个显示行，wrap 续行独立成行）。按此映射直接 `grid[Line(g)]` 读单行 cell（负行号 scrollback 行用 `Line::from(0) - n` 构造，`Line` tuple 构造器不公开）
- **行缓存 = `HashMap<i32 网格行号, RowCache>`**：`RowCache { hash, galley: Arc<Galley>, backgrounds }`——按网格行号索引，滚动后同一网格行直接命中（滚动只重建滚入的新行）。命中行绘制直接 `painter.galley(缓存 Arc clone)`，**完全跳过 layout_job**（曾每行每帧 `fonts_mut` 写锁 + 整行文本 hash）
- **hash 指纹**：fg+bg+样式+字符（`CellStyle::key` 含 bg 色），不含光标效果——**光标行不再反色 swap**（Block 光标最终被光标色实心矩形覆盖，反色不可见，视觉等价），光标移动/闪烁不触发行重建
- **CJK 宽字符逐 cell 成段（2026-09-11）**：宽字符（`Flags::WIDE_CHAR`）**每个 cell 单独成段**，绘制时按自身双列起点 `start_col * cell_width` 定位；同段多字符仍按字体实际 advance 排字，而 CJK 的 advance 是 1em（13px 字号下 13px）、双列宽是 2×cell_width（约 15.7-16.1px），差 3.1px/字累积——表现为「中文越打越多，光标离文字越来越远、中文与后面内容之间一片空白」（**曾把连续宽字符合并成段：段内漂移 5 个字就差 15px**）。宽字符段恒为单字符，Galley 按 `(字符, 样式)` 缓存（`wide_glyphs`，跨行跨帧复用，ppp 变化时随 `rows_cache` 清空）；带零宽组合符的宽字符整体 shaping、不入缓存。`English 标点/半角` 不受影响（advance 与 cell 等宽）。回归测试 `中文宽字符按终端列对齐`（像素级：5 个中文字各成墨迹簇、起点贴双列起点、中文末笔到后续半角间距 < 1 cell）、`中文与半角分段列定位`（分段 = 逐 cell）
- **内容未变行跳过 layout**：损坏行重建后若 `hash == 缓存 hash`（如光标行被标记但文本没变）跳过 layout 复用旧 Galley
- **pixels_per_point 失效**：Galley 与 ppp 绑定，每帧读 `ctx.pixels_per_point()`，变化时清空全部缓存（跨屏缩放）
- **cell 尺寸缓存**：`glyph_width/row_height` 只查一次（字体启动后不变），不每帧 fonts_mut
- **缓存上限**：`rows_cache.len() > rows*4` 时只保留当前可见网格行（滚动浏览大量历史防无限增长）
- **单锁化**：光标形状 `cursor_style()` 在第一次锁内读取（曾帧内二次上锁）
- **Wakeup 单次 repaint**：drain_events 不再对 Wakeup 二次 request（mino-core `Listener::send_event` 已直接调 on_event=request_repaint）
- 性能验证：idle 帧（无内容变化）构建耗时 0.03ms（此前全量扫描 0.5-2ms，降 15-60 倍）
- **v1.1.2 追加减负（egui 回退路径同样生效）**：一行 N 个 `CachedRun` 合成为**单个 `Mesh`** 提交（`row_meshes`，键为显示行号——mesh 顶点烘焙了绝对行位，滚动/尺寸变化整体重合成，`rows_cache` 保留）；按 **damage 的列区间**只重建受损列（`damaged_bits`：光标移动只损 1-2 列，曾整行 80-200 cell 重算）；Shape 一次 `Painter::extend` 批量提交（`shapes_scratch`，替代逐 Shape `add` 的上千次 Context 写锁）；输入法预编辑串 Galley 缓存（`ime_preedit_cache`）；底栏/标签目录读 `effective_local_directory`（300ms TTL，每帧 4 次系统调用 → 1 次；需要即时真值的入口走 `fresh_local_directory`）（装饰网格线已在 v2.0 随 Kaku 化移除）
- **一帧输入合并**：输入事件先攒进 `pending_writes` 再一次 `session.write`（曾每个按键/粘贴分片一次通道发送）
- **resize 节流**：拖拽窗口时 `resize_grid` 每帧更新本地网格（字不越界），后台通知（本地 SIGWINCH / 远程 SSH `window_change`）按 **50ms** 合并 + trailing 补发（`notify_backend_size`）
- **网格最小尺寸钳制 + 远端尺寸补发窗口（2026-09-28）**：窗口被缩到极小时（拖拽到底、最小化/分辨率变化瞬态），**不得把 1-2 行的尺寸通知远端**——vim/tmux 等全屏程序会据此把界面重绘成 1 行，而恢复尺寸的通知一旦未生效（发送失败/极小窗口下没有渲染帧），程序就永久停在该状态（用户现象：SSH + vim 编辑文件「内容只剩一行、滚动只更新那一行」；实证：跳过恢复通知即完整复现）。①尺寸计算处 `MIN_TERMINAL_COLS=20`/`MIN_TERMINAL_ROWS=4` 钳制，**网格与 `window_change` 同源**（超出窗口的部分由渲染自然裁剪）；②远端尺寸同步从「8 帧重试」改为**时间窗口**（`REMOTE_RESIZE_SYNC_DURATION=2s` 内每个渲染帧补发，`remote_resize_sync_until`），覆盖「窗口被遮挡/最小化时不出帧」导致一次都没送达的情况；回归测试 `极小窗口下终端网格尺寸被钳制`（120×60 窗口 → cols/rows ≥ 钳制值且网格同源，旧实现下只有 10 列）
- 远程逐 SSH 数据包唤醒合并为每帧一次（`notify_wakeup` 标志位，由 `drain_events` 复位）：`cat` 大文件时重绘请求不再与包数同阶

### 终端自管 GPU 渲染（v1.1.2 起，`views/terminal_gpu.rs` + `terminal_gpu.wgsl`）

- **动机**：egui 每帧把全部 Shape 重新 tessellate 并整块上传顶点（`Context::tessellate` 明确不做跨帧复用），终端这种「字形几乎不变、只有少数行在变」的场景等于每帧整屏重传。自管路径把每行顶点常驻显存：**行内容不变不重传、滚动只改 uniform 的 `row_origin`、空闲帧零上传**
- **启用条件**：`MinoApp::new` 用 `cc.wgpu_render_state` 建进程级 `TerminalGpu`（管线/布局/采样器/图集绑定），会话创建时 `TerminalView::set_gpu` 注入；kittest（`Harness::new_ui`）没有 wgpu 后端 → `gpu = None` → 自动走 egui Shape 回退路径（两条路径共存，回退路径由 kittest 全覆盖，**GPU 路径须实机运行验证**）
- **回调内绝不触碰 `egui_wgpu::Renderer`**：kittest 在 `update_buffers`/`render` 期间持写锁、eframe 渲染期持读锁，回调内再取锁必死锁。字体图集绑定只在 UI 线程帧内刷新（`gpu_atlas`），回调只读
- **管线逐项对齐 egui**（颜色格式/混合/采样数/顶点布局/抖动/gamma/预乘 alpha），否则自管绘制与相邻 egui 控件有可见色差；顶点 uv 是 epaint 的**纹素坐标**，归一化在顶点着色器里做（`a_tex_coord / r_locals.atlas_size`）
- **图集换代看门狗（双条件）**：行顶点 uv 与图集尺寸强绑定，按「尺寸变化 **或** 填充率骤降」检测——epaint 整份重建字体系统时尺寸可能一模一样但字形位置全变（只比尺寸会漏，表现为字符错位/串码）；图集还会在同帧更晚的位置（其它控件首次用到新字形）继续变大，因此必须在**下一帧开头**再比一次当前尺寸与 `mesh_atlas_size`，不一致立即整屏重建并请求重绘
- **逐行缓冲 + 区间分配器**：`RowBuffers` 每行独立槽位（容量翻倍冗余、`free_v`/`free_i` 空洞合并），容量不足时返回 false（该行本帧不可见）；uniform 步长按 `min_uniform_buffer_offset_alignment` 放大（`TerminalGpu::new` 读设备 limit），每行一个 `Locals`，**上限 `MAX_ROWS = 256`**
- **`screen_size` 必须是整个渲染目标的点尺寸**（窗口），不是终端内容区：着色器用「绝对坐标 ÷ 全屏尺寸」→ NDC，传内容区尺寸会把坐标放大（文字整体拉伸错位）

### 终端能力应答（v1.0.8 起，`terminal/mod.rs` + `terminal_view.rs` + `keys.rs`）
-
- **程序的终端查询必须应答**：mino 曾把 alacritty 的 `Event::ColorRequest` / `Event::TextAreaSizeRequest` 当无关事件丢掉，导致 `printf '\e]11;?\a'` **永远收不到答复**（实测对比 macOS Terminal.app：Terminal 回 `]11;rgb:1e1e/…`，minо 无任何输出）。查询终端配色的 TUI（omp 启动即查 OSC 11）只能按"未知终端"回退配色
- 事件链路：`SessionEvent::ColorRequest { index, formatter }` / `TextAreaSizeRequest(formatter)` / `ClipboardStore` / `ClipboardLoad` / `ResetTitle` 入队（`Listener::send_event`，formatter 是 alacritty 给的闭包），UI 侧 `TerminalView::drain_background_events` 应答后写回 PTY；`Bell` 转 0.6s 视觉脉冲（左上角圆点，无常驻重绘），`ResetTitle` 清标题缓存（曾在 `_ => false` 被丢弃，标题永久停旧值）
- **PtyWrite 队列 64KB 字节上限（v1.1.2）**：PtyWrite 是终端能力应答（DA/kitty/DECRQM/OSC 查询回执，程序可触发），不是用户数据——多标签高输出并发时后台标签长期不消费会无界追加；超限丢最旧的应答（程序按"未知终端"回退，不破坏终端状态），队列内存有界；回归测试 `写回队列超限丢最旧应答且有界`
- **颜色索引语义**（alacritty `term::color`）：0-255 调色板、256 前景、257 背景、258 光标、259+ Dim/Bright 变体；优先级与渲染 `resolve_color` 一致：**OSC 动态覆盖（`term.colors()`）> 主题调色板**。`Colors` 只实现越界即 panic 的 `Index`，查询索引由终端程序控制，**必须先做边界检查**（`COUNT` = 269）；**`COLORTERM=truecolor` 必须注入**（本地 `local_session_options_at` + 远程 `set_env`，sshd 未放行时静默忽略）：omp 的 `getColorMode` 只认该变量判 24-bit，`TERM=xterm-256color` 只判 256 色，缺了它 agent 全程降级（实证自 omp 二进制）
- **焦点事件上报（`DECSET 1004`）**：程序开启后，窗口获得/失去焦点必须发 `ESC [ I` / `ESC [ O`（`TerminalView::report_focus_change`，读 `ctx.input(|i| i.focused)`）；**首帧只记录基准状态**，不能补发历史变化（程序是在自己启用之后才开始期待事件）；回归测试 `终端查询与焦点上报有应答`（python3 脚本开 cbreak 读 PTY，断言收到 `]11;rgb:` 与 `\x1b[I`）、`颜色查询按索引返回主题颜色`、`颜色查询优先使用OSC覆盖`、mino-core `颜色与尺寸查询进入事件队列`
- **对照 Terminal.app 的方法**（本机排查用）：把 omp 包一层透明 PTY 代理分别跑在 mino 与 Terminal.app 里，比对**两个方向**的字节——`mino → 程序`方向能看出终端应答差异，`程序 → mino`方向能看出按键编码差异
- **按键编码（`keys.rs`）**：主键盘 Enter 永远 `\r`；**Shift+Enter=`ESC[13;2~`、Ctrl+Enter=`\n`（`Ctrl+J`）**——omp 把 submit/newLine 分绑裸 Enter 与 Shift+Enter/Ctrl+J，三者同发 `\r` 时输入框换不了行（changelog #8821 实证两种形态都兼容）；kitty 键盘协议已开启（`term::Config { kitty_keyboard: true }`，本地/远程/测试三处）并按规范分流：订阅 `DISAMBIGUATE`（或全量 `REPORT_ALL_KEYS_AS_ESC`）后，只有 Escape、**带修饰**的 Enter/Tab/Backspace 与 ctrl/alt 组合文本键走 CSI-u（`CSI 27u` / `CSI 13;2u` / `CSI 97;6u`，码点取未移位基准键）；**方向/Home/End/Insert/Delete/PageUp/PageDown/F1-F12 与无修饰的 Enter/Tab/Backspace/文本键保持 legacy 编码**（`ESC[A`、`ESC[3~`、`\r`、`\t`、`0x7f`）——kitty 规范 "Functional key definitions" 表里这些功能键没有 CSI-u 编号；Enter/Tab/Backspace 无修饰是规范明文保留的 legacy 例外（程序崩溃没复位模式时用户还能输 `reset`）。**曾把功能键编成自造私用区编号（`ESC[57358u` 实为 CAPS_LOCK、`ESC[57361u` 实为 PRINT_SCREEN 等）**，omp 订阅 kitty 后解不出"上下左右"（2026-09-16 修复，实测 omp：`ESC[57358u`/`ESC[57359u` 无反应、`ESC[A`/`ESC[B` 正常；`ESC[13u` 虽被 omp 容忍但对齐规范也改为发 `\r`）；回归测试 `kitty模式下按键编码符合协议` / `kitty订阅后按键按协议编码`
- **鼠标上报透传（1000/1002/1003 + SGR 1006）**：程序订阅 `MOUSE_MODE` 后点击/拖拽/释放直接发 xterm 序列（按下→拖拽→释放，按键全程快照一次，释放帧 input 已读不到）；拖拽需 1002+ 才发（只开 1000 不发 Drag）；未订阅走本地选区。滚轮既有分支不变。回归测试 `鼠标上报开启时点击透传程序`
- **OSC8 超链接**：同 URI 相邻 cell 合并进 `Segment::link`（进 hash 指纹），accent2 下划线 + 悬浮小手 + 单击 `open_url`（`same_tab`）；鼠标上报订阅时点击透传给程序、不抢链接。回归测试 `超链接分段与点击`
- **OSC52 剪贴板**：store 经 `ClipboardStore` 事件写系统剪贴板（`ClipboardReader::set_clipboard_text`，arboard；失败走 toast，不静默丢），load 用当前剪贴板文本回写（默认 OnlyCopy 下程序发不出 load）。回归测试 `程序复制写入系统剪贴板`
- **样式细节**：粗体=前景增亮 1/3（egui 无合成粗体，不做描边仿粗）；下划线变体全集（SGR 4/4:2 双线/4:3 波浪/4:4 点/4:5 虚，`UnderlineStyle` 进段合并条件+hash+宽字缓存键），Single 由 Galley 画、其余由 paint 侧矢量补（双线/波浪 8 段正弦/点虚 dash），颜色走 SGR58（无则跟前景）；DECRQM 查 2026 回 `0`（不支持）：alacritty 0.26 的 2026 set/unset 是空实现却回 `2`（已重置），程序据此误判终端支持同步更新并**全程用 BSU/ESU 包裹每次重绘**，而 mino 在 BSU..ESU 之间照常渲染（半成品画面直接上屏，用户现象：在 mino 里跑 omp 交互时"整个输出流都错乱了"）；改写点是 `Listener` 的 `PtyWrite` 应答出口（`rewrite_sync_update_reply`，覆盖本地/远程/测试三条路径），按**任意包内容**匹配——旧实现要求"整包恰好等于查询"，而 omp 实测把 2026 与 kitty/OSC 11/DA1 等混在同一次写入里（`\x1b[?u\x1b[c\x1b]11;?\x07\x1b[c\x1b[?2031h\x1b[?2026$p…`），旧改写从未命中（本地更是完全不经过改写层）。回归测试 `下划线变体与颜色进段缓存` / `同步更新查询回不支持` / `同步更新查询混包也回不支持`（后者在旧实现下失败）/ `铃声与标题重置有响应`

### 工作目录跟踪（`mino-app/src/workdir.rs`）

- 终端输入仍直接交给 shell；跟踪器只根据 PTY 中可观察到的可见字符、退格、回车和控制键，维护一个尽力而为的当前目录
- 本地 `cd` 使用 `canonicalize` 校验目录，远程 `cd` 只做 POSIX 路径词法归一化；`cdfoo` 不会被误判为 `cd foo`
- SFTP 面板读取 `TerminalView::current_directory()`，用于从终端当前目录快捷定位
- **egui 0.36 焦点坑：Text/Key 事件帧后终端焦点可能被清除（`Memory::end_pass` dead-man-switch，kittest 必现）**——`TerminalView` 维护 `had_focus`，曾聚焦且当前 `focused().is_none()` 时每帧 `request_focus` 恢复（对话框输入框后渲染覆盖，不冲突）

### SFTP

- `connect_sftp` 建立独立 SSH 连接（任务跑在共享 runtime 上，会话存活到 `Shutdown` 命令）
- UI 持 `SftpHandle`（克隆发送端）发命令；后台执行后经 `SftpEvent` 通道回报
- 事件流：`Listed / Progress / Done / Error / Closed`
- **传输失败清理半成品**：下载失败删本地半成品文件、上传失败删远程半成品（不残留截断文件误导用户）
- **文件时间列**：`format_time` 用 civil_from_days 公版算法精确换算 UTC 日期（曾用 `天/365 + 天%365/30` 近似，月长不均/闰年导致日期错位）；测试断言已知时间戳（0=1970-01-01、951782400=2000-02-29 闰日、1800000000=2027-01-15）
- **SFTP 主机名绑定在连接上**（`pending_sftp/ready_sftp` 元组携带 host label）：状态栏从 `tab.sftp.host_name()` 读取——曾用全局 `sftp_host` 字段，多远程标签共存时串台成"最后一次连接的主机名"
- **文件列表虚拟化（v0.7）**：`ScrollArea::vertical().show_rows(ui, row_h, total_rows, ...)` 只构建可见行（index 0 = ".." 上级行，其余 = entries）——千级目录不再每帧全量构建 String/RichText/Label/子 Ui（曾每帧遍历全部 entries）；表头固定在滚动区外不滚动；行渲染抽成 `render_list_row`（导航/选中动作写入 `open_dir`/`select` 闭包外统一应用，避免借用冲突）；`show_rows` 闭包内必须 `item_spacing.y = 0.0`（行高由 show_rows 精确分配）
- **事件驱动重绘（v0.7）**：`poll_events()` 返回是否收到新事件，show() 收到后 `request_repaint()`——传输进度/列表刷新不再依赖其它重绘源（曾面板自身从不 request_repaint，传输中进度条不刷新）
- **cell_width 缓存（v0.7）**：'0' 字符宽首次查询后缓存到字段（字体启动后不变），不再每帧 `fonts_mut`（Context 写锁）
- **加载指示静态化（v0.7）**：`ui.spinner()` 替换为静态圆点 + 文字（egui Spinner 内部每帧 `request_repaint` 强制 60fps 全帧重绘）

### 连接生命周期

- **主机密钥校验（TOFU，`ssh/known_hosts.rs`）**：终端与 SFTP 连接都经 `connect_verified` 校验服务器公钥——首次连接记录 `SHA256:base64` 指纹到 `~/.config/mino/known_hosts.toml`，后续必须一致，不一致（服务器换密钥/中间人攻击）则拒绝并在错误里给出修复指引（曾无条件接受所有服务器密钥）。进程级 `Mutex` 串行化读-改-写（终端+SFTP 双连接并发首连会丢更新）；known_hosts 与 hosts.toml 都强制 **0600 权限**（含明文密码/口令，默认 umask 022 会产生 0644）。**指纹拒绝后重连需删文件对应条目**；密钥文件/known_hosts 测试用 rand 0.10（与 russh 的 rand_core 版本一致）
- **SSH 配置**：`ssh_config()` 统一 30s keepalive（`keepalive_interval`）+ 3 次无响应断开（`keepalive_max`），空闲连接被 NAT/防火墙静默断开后能及时发现；私钥路径自动展开 `~`（配置里写 `~/.ssh/id_ed25519` 可直接加载）
- **进程级共享 tokio runtime（v1.1.2，`ssh::shared_runtime()`）**：远程终端与 SFTP 的全部任务共用一份 `LazyLock` runtime（上限 8 worker）——曾每个连接各建 2-worker runtime（N 个标签 = 2N 常驻线程 + N 份内存）；任务以 `ssh::wait_for_task`（`Notify`）等待结束。**runtime 不可用时也必须把失败事件回传**，否则 SFTP 面板永久停在"连接中…"；远程连接后台线程必须持有等到会话关闭，否则 tokio::spawn 的任务被取消
- `Session` 实现 `Drop` → 发 Shutdown 优雅关闭
- **本地会话关闭兜底**：alacritty 的 `Pty` 析构只发 SIGHUP 后 `wait()`，shell（zsh）偶发不响应 SIGHUP 会让 wait 永久阻塞（关闭标签页时 UI 线程卡死，kittest 回归测试约 50% 概率必现）——`spawn_local` 在创建 EventLoop 前保存 `pty.child().id()`（macOS 上 login exec 成 shell 后 PID 不变），`Session::drop` 先 SIGHUP 给 shell 优雅退出机会，再在守护线程延时 300ms 补 **SIGKILL** 保证 Pty 的 wait 必然返回（shell 已退时 kill 无害）；EventLoop::new 失败的错误路径同样先 SIGKILL 再析构 pty；**libc 依赖仅 `cfg(unix)`**（Windows 无 kill/SIGHUP，ConPTY 无此问题）

## 样式体系（Kaku 基准，v2.0）

- 设计 token 在 `theme.rs::tokens`（Kaku Dark 基准）：应用/标签栏/终端同底 `#15141b`；面板 `#1f1d28`（surface）、浮层/输入框与激活标签 `#29263c`（surface_active）；文字三阶 `#d5d4d6`/`#a8a6ae`/`#6d6d6d`；accent 紫 `#8e6ad9` + accent2 琥珀 `#daae76`；成功 `#58d8ad`/警示 `#daae76`/危险 `#d85d5d`/注意力点 `#daae76`（浅色主题分别取 Flexoki 的深色档）
- 浅色主题（Kaku Light）：bg `#FFFCF0`、panel `#F2F0EB`、elevated `#E8E6DB`、边框 `#E8E1D0`、文字 `#100F0F`/`#575653`/`#6F6E69`
- 圆角：`RADIUS_SM = 6`（按钮/输入框）、`RADIUS_ITEM = 4`（列表项/标签）；全局控件 hover = accent 低透明度叠加；自绘行用白 8% 叠加；边框统一半透明（深色 10% 白、浅色 `#E8E1D0`）
- 终端：背景铺满、文本在内边距内；无装饰网格线（Kaku 纯色底）；标签栏在底部；SFTP 悬浮按钮与终端内边距对齐（`padding_points`）
- 动效（`anim.rs`）保留：toast 降频、Bell 脉冲只在到期帧重绘、无持续 60fps 源
- 品牌 logo 保留（`draw_logo_mark`，黑底 `>` + 磷光绿光标块）

## 外观与窗口布局（Kaku 样板，v2.0）

- **主题**：`theme.rs` 只有 `THEMES = [Kaku Dark, Kaku Light]`（0 深 1 浅）；`resolve_appearance(auto|dark|light, system_dark)` 决定有效主题，`App::ui` 每帧检查系统外观变化并重套（`auto` 时）；配置字段 `appearance`（`#[serde(alias = "theme")]` 兼容旧主题名，未知值按 auto）。Kaku Dark 关键取值：bg/app/终端/标签栏同为 `#15141b`、surface `#1f1d28`、surface_active `#29263c`、白 `#d5d4d6`、灰 `#6d6d6d`、紫（光标/强调）`#8e6ad9`、琥珀 `#daae76`；终端调色板原样搬 Kaku（含 cyan=green、ANSI black 浅灰）；`color_overrides`/`foreground_color_overrides` 覆写表 + `text_min_contrast_ratio = 3.0`（`resolve_cell` 内 `enforce_min_contrast`）；选择色 55% 紫（浅色主题不透明米灰 `#E8E6DB`）
- **窗口布局**：`Panel::top("titlebar")`（28pt 拖拽条，红绿灯悬浮其上，双击 zoom）+ `Panel::bottom("tab_bar")`（底栏，标签 + ＋ + 右端工作区按钮/状态指示/齿轮）+ 右侧 SFTP 面板 + CentralPanel；**没有独立状态栏**（会话状态/SFTP 错误折入底栏 `session_status` 与 toast/SFTP 面板）
- **底栏**：Kaku `tab_bar_at_bottom` 同构——方角标签、激活底 `bg_elevated`、非激活灰字、hover `bg_panel`、`＋` 新标签（`show_new_tab_button_in_tab_bar`）；「×」hover/选中可见（隐藏时 alpha=0 仍进辅助功能树，测试以 × 数量判定标签数）；与 Kaku 的差异：单标签也显示（Mino 没有 macOS 菜单栏，入口按钮都在这里）
- **字体**（`main.rs::setup_fonts`）：JetBrains Mono **Regular/Medium/SemiBold** 三字重 + Symbols Nerd Font Mono（私有区图标）+ Menlo（macOS 生僻符号兜底）+ egui 默认链尾（Hack/NotoEmoji 等，彩色 emoji 靠它）。字重按**命名族**表达：`mino-mono-dark`（Regular）/`-dark-bold`（Medium）/`-light`（Medium）/`-light-bold`（SemiBold），终端渲染按「主题深浅 × 粗体」选 `mono_font_id(..., families_bound)`（**裸 Context 测试无命名族时回退 Monospace**，否则 epaint panic "not bound to any fonts"）；CJK fallback 后台加载后并入全部 5 个等宽族 + 比例族（命名族不回落到 Monospace 族）
- **内边距/字号**：终端 padding = 40 设备像素（ppp ≤ 1 用 26）换算成点（Kaku `get_default_padding` 语义，底边 0）；默认字号 15/17（低分屏 15、高分屏 17），老默认 13 一次性迁移；行高沿用字体自然行高（egui 下 1.32em，与 Kaku 的 1.28 等价视觉）；标签栏尺寸常量在 `app.rs` 顶部（`TAB_BAR_HEIGHT`/`TAB_MAX_WIDTH` 等）
- **粗体不增亮**（Kaku `bold_brightens_ansi_colors = false`）：`resolve_color` 不再做 8-15 映射，粗体只换字重族；斜体保持正体（Kaku "keep upright"）

## 分屏（Panes，v2.0）

- 数据结构：`TerminalTab { panes: Vec<Pane>, layout: PaneNode, active_pane, zoomed, origin, .. }`；`Pane { id, view: TerminalView }`；布局树（`views/pane_tree.rs`）叶子 id 与 panes 一一对应；`SessionOrigin::{Local, Remote(HostProfile)}` 记录来源（分屏继承：本地按活动窗格 cwd 新起本地 shell、SSH 重连同主机；快照恢复也用它）
- 几何：`pane_tree::layout(tree, rect, gap, &mut out)` 相邻窗格间隙恰为 `gap`，外缘贴齐；比例调整取「离目标窗格最近的同轴 Split」钳 0.1..0.9。**间隙 = 5 个字宽**（Kaku `split_pane_gap = 2` 的渲染语义 `gutter = 1 + 2 × gap`，其默认配置注释与 README 截图实测同量级）：`pane_gap()` 用活动窗格实测字宽换算（`PANE_GAP_CELLS`，字宽未测量时退回 `PANE_GAP_FALLBACK`），随字号变化；曾固定 10px（比 Kaku 明显窄）
- 渲染：`MinoApp::render_tab_panes` 按 rect 分配子 Ui（`UiBuilder::max_rect` + `set_clip_rect`），各窗格独立 `show_with_input`；渲染时写回 `tab.pane_rects`（邻居查找用真实几何）
- **焦点铁律**：egui 键盘焦点决定活动窗格（渲染后焦点跟随），**任何程序化切换活动窗格必须同时 `request_pane_focus`**（⌘⌥方向键/关闭窗格/新分屏挂载），否则下一帧焦点跟随把活动窗格拉回旧窗格（"⌘⌥← 没反应"的根因）；`TerminalView::focus_id` 每实例唯一（`NEXT_VIEW_ID` 自增；曾是常量 Id，分屏后互相顶焦点）
- 快捷键：⌘D 左右 / ⌘⇧D 上下 / ⌘⇧S 翻转最近一层方向 / ⌘⇧Enter 放大还原 / ⌘⌥←→↑↓ 移焦 / ⌘⌃←→↑↓ 调整比例（每次 5 列，按窗格尺寸换算比例增量）；⌘W 智能关闭（多窗格→关窗格；多标签→关标签；最后一个→最小化窗口——Kaku 是 hide，Mino 用最小化保证一定找得回）
- 右键菜单（`MenuChoice`）：复制/粘贴/全选/搜索/打开链接 + 分屏/关窗格/新建标签/关闭标签/清屏/重置字号；本地动作在 `TerminalView::apply_menu_choice` 内执行，跨窗格动作经 `take_menu_choices()` 交应用层

## Kaku 行为与渲染对齐（B 组，v2.0）

按「以 Kaku 自带默认配置（`/Applications/Kaku.app/Contents/Resources/kaku.lua`）+ 官方 docs 为基准」逐项对齐的终端行为；每条都带回归测试。

- **标签/窗口标题**：标签标题默认「父目录/当前目录」（`tab_dir_label`，Kaku `format-tab-title` 默认语义，`/Users/me/www/Kaku` → `www/Kaku`），放大态追加 ` [Z]`；多窗格按窗格目录标签以 `∙` 去重连接；**窗口标题**每帧同步（`sync_window_title`，多标签前缀 `[i/n]`，只在变化时下发 `ViewportCommand::Title`）——标题条文字本身隐藏，但 Mission Control/窗口菜单看到的是它
- **`COLORFGBG` 注入**（`colorfgbg()`）：仅本地会话（Kaku 的 `set_environment_variables` 同样只作用于本地 pty），浅色 `0;15`、深色 `15;0`，会话创建时定值
- **`OSC 9;4` 进度上报**：`sniffer::Marker::Progress` → `SessionEvent::Progress`，出错(2)/暂停(4) 点亮标签注意力点（Kaku 的琥珀点语义）；进行中(1)/不确定(3) 不上点（Kaku 明确不上"运行中"点），`OSC 9;<message>`（桌面通知）不误判成进度；测试注入走 `Session::inject_sniffed_bytes_for_test`（`inject_program_output_for_test` 绕过嗅探器，收不到这类事件）
- **拖选时滚轮**（Kaku `selection_wheel_scroll_behavior = "Extend"` 默认）：滚轮滚动视口后，用**新** `display_offset` 按当前指针位置重算选区端点（拖动期间 egui 不发 PointerMoved），跨屏选择不用松手重选；实现在 `handle_input` 的 pending_scroll 之后
- **替代屏滚轮**：维持「映射方向键」（`WheelTarget::AlternateScroll`）。Kaku 的 `alternate_screen_wheel_scrolls_terminal` 默认 false（窥视回滚缓冲）在 Mino 引擎下不可实现——alacritty 的替代屏 `display_offset` 恒为 0（`替代屏没有可滚动历史` 哨兵测试），照搬只会让滚轮变空操作
- **文件链接编辑器**（Kaku `file_link_editor`）：`open_local_file_link` 命中条件＝本地会话 + 文件路径（含 `file://`、`:行[:列]`）+ 路径存在 + 能解析出编辑器；编辑器解析顺序＝配置 → `code`/`cursor`（PATH 探测）→ `$VISUAL`/`$EDITOR` → 回退 `open_url`（系统默认程序）；命令串支持引号（`parse_command_line`），`路径:行:列` 作为**单个参数**追加；远程会话不打开本地编辑器
- **右 Option 组合字符**（Kaku：左 Meta、右 Compose）：`option_side` 字段跟踪 `Key::AltLeft`/`AltRight` 的按下抬起；右 Option 的组合字符经 Text 事件上屏，左 Option 保持 ESC 前缀（egui 的 `Modifiers::alt` 不带左右信息，只能自己跟踪）
- **窗口不透明度 / 毛玻璃**（Kaku `window_background_opacity` / `macos_window_background_blur`）：`theme::window_opacity()` 进程级静态 + `apply_window_alpha()`，终端背景与标题条/标签栏/SFTP 面板取色统一叠加（只作用于终端**默认背景**，显式着色的 cell 背景保持不透明——对应 Kaku 的 `window_background_opacity` 与 `text_background_opacity = 1.0` 分工）；< 1.0 或模糊 > 0 时 `main` 以 `with_transparent(true)` 建窗口（**窗口是否透明在创建时定死**，运行中改配置只改配色，提示重启）；macOS 毛玻璃用 `NSVisualEffectView`（`native::apply_window_blur`，插在 contentView 最底层）；AppKit 不暴露模糊半径，数值只区分开/关
- **窗口尺寸/位置记忆**（Kaku `state.json` 的 `window_geometry`/`window_position`）：退出时写入 `snapshot.toml`（`window_size`/`window_position`，外框位置 + 内框尺寸），启动 `restored_window_geometry()` 读取并 `with_inner_size`/`with_position`（有位置记录时不再 `centered`）；越界坐标（\|x\|,\|y\| > 32000）与不合理尺寸（< 200×150）按无记录处理
- **块字形无缝**：Kaku 用 `custom_block_glyphs` 自绘块/制表字形；Mino 依赖打包的 JetBrains Mono（块字形覆盖整行高、相邻行自然重叠），像素级回归测试 `块字形无缝拼接` 守卫（满块区域内不许出现整行/整列的背景色）。注意该测试按"与背景亮度差"判定前景（主题可能是浅底深字）

## 覆盖层（v2.0）

- **搜索（⌘F，`terminal_view.rs`）**：浮层（`Area` 右上角）输入 + `Aa` + ↑↓ + ×；命中在锁内全量扫描（含历史行，`grid_line_text` 保留「字节→列」映射；扫描用**网格自身**的 history/screen_lines/columns，不依赖自渲染字段）；内容变化按 150ms 节流重扫；命中高亮（当前命中深色 + 其余浅色）；Enter 下一个 / ⇧Enter 上一个 / Esc 关闭；**焦点回补**（egui 0.36 事件帧清焦点的坑，kittest 必现）+ Esc 无条件关闭（搜索打开时 Esc 第一语义是关搜索）
- **Launcher（`views/launcher.rs`）**：标签导航（⌘⇧O，每帧 `set_items` 同步标签列表，选中按 key 跟随）与命令面板（⌘⇧P，静态条目：标签/分屏/搜索/字号/工作区/AI/外观…）共用；fuzzy 子序列过滤 + 打分；↑↓/Ctrl+N/P/PageUp/Down/Enter/Esc；行点击显式 `ui.interact` 注册在内容之后
- **AI 聊天面板（⌘L，`views/ai_panel.rs`）**：流式（后台线程 + mpsc，取消用 `Arc<AtomicBool>`）、极简 Markdown（围栏块 + 行内 code，按围栏拆段是纯函数）、Shift+Tab 切换 Simple/Deep 模型、未配置态引导「去配置 AI」；`AiPanelEvent::{InsertCommand, OpenSettings}` 由 app 层消费（插入不回车）；打开时带终端上下文（目录 + 可见行快照）

## 统一工作区（SSH 与项目收藏融合，v2.0）

- 模型（`mino-core/src/config/mod.rs`）：`Workspace::{Local { name, path, command }, Ssh { name, host, port, user, auth, path, command }}`，`#[serde(tag = "kind")]` → `[[workspaces]]`；`HostConfig.workspaces` 为唯一写入源，旧 `hosts`/`projects` 是 `skip_serializing` 只读兼容字段，`load()` 时 `workspaces` 为空才迁移（先 hosts 后 projects，顺序保持）
- UI（`app.rs`）：标签栏单一「工作区」按钮（`workspace_quick_button` 矢量图标）+ 快捷菜单（`workspace_quick_menu`，单击即开）；⌘O 面板（`workspaces_panel`：搜索/↑↓/回车，空态给两个新增入口）；设置单卡片 `workspace_manager`（行模板与菜单/面板共用自由函数 `paint_workspace_row`：头像 + 本地/SSH 徽标 + 名称/详情）；编辑对话框 `workspace_edit_dialog`（类型分段：本地目录 [路径 + 浏览/使用当前终端目录 + 启动命令] / SSH [用户名、主机端口、认证分段、远端目录与命令]）；`WorkspaceEdit::{from_workspace, to_workspace}` 负责表单互转与必填校验（校验失败保留用户输入）
- 打开动作：`open_workspace`（本地需 `is_dir()` 才建标签；SSH 走 `start_connect_into`）；⌘B `bookmark_current_workspace` 按 **origin** 判定（Remote → 存主机档案按 user@host:port 去重；Local → 存 canonical 目录按路径去重）
- 关闭编辑器统一走 `close_workspace_editor()`（恢复 `settings_before_workspace_edit` 的设置窗口；Esc/取消/保存成功三路一致）

## AI 助手（v2.0）

- 配置：`~/.config/mino/assistant.toml`（0600 原子写），字段 enabled/api_key/model/chat_model/base_url/api_mode/custom_headers/auth_type（api_key 或 codex：读 `~/.codex/config.toml` + `auth.json`，ChatGPT 登录态明确不支持）；设置弹窗「AI 助手」卡片可编辑，保存后 `ai_panel.reload_config()`
- 触发（`poll_ai_tasks`）：shell 集成 `133;D` 非 0（跳过 130 Ctrl+C、无命令行的裸回车）→ 后台 `suggest_fix` → toast + `ai_fix_suggestion`（⌘⇧E 经 `TerminalView::inject_command` 填入提示符，不执行）；`# 描述` 回车被 `TerminalView` 在 Key 分支拦截（Ctrl+U 撤回整行、不发回车）→ `suggest_command` → 回填；未配置助手时静默跳过
- 高亮限制：`suggest_command` 返回前会清洗（去围栏/前缀），错误均为中文；api_key 绝不进日志/错误文案（客户端层承诺）

## Shell 集成（v2.0）

- 脚本：`mino-core/assets/shell-integration/mino.zsh`（`include_str!` 内嵌）；守卫三重（`MINO_SHELL_INTEGRATION=1` + 交互式 + 未加载标记），非 mino 会话/非交互/重复 source 零副作用；输出 OSC 133 A/B（PS1 内嵌）+ C（preexec）+ D;<exit>（precmd）+ OSC 7 cwd；插件顺序 autosuggestions → z → completions → syntax-highlighting；Smart Tab 三模式（suggestion_first 默认 / completion_first / off，`MINO_SMART_TAB_MODE` 优先）；starship 仅二进制与预设同时存在时启用
- 安装器（`shell_integration.rs`）：受管块（`# >>> mino shell integration >>>`）+ 首次备份 `.zshrc.mino.bak`（不覆盖旧备份）+ 卸载逐字节还原 + 符号链接按真实目标原子替换；脚本/预设 0600、vendor 目录 0700；`bootstrap_vendor` 串行 `git clone --depth 1`
- 应用接线：会话 env 注入 `TERM_PROGRAM=Mino` + `MINO_SHELL_INTEGRATION=1`（本地 `local_session_options_at`，远程 `set_env`，sshd 不放行则静默失败）；首次运行自动安装（非测试构建；测试用隔离 zshrc 路径 `config_path.with_extension("zshrc")`）；设置「Shell 集成」卡片（安装/修复/卸载/下载插件）

## 窗口快照（v2.0）

- `App::on_exit` 保存 `snapshot.toml`（与 hosts.toml 同目录，0600 原子写）：每个标签记 `SessionOrigin`（本地含目录；SSH 含完整档案）——**含明文认证信息**，权限与 hosts.toml 同级；另记 `window_size`/`window_position`（窗口几何记忆，启动恢复）；⌘⌥⇧T `restore_window_snapshot` 追加式恢复（不销毁当前会话）；损坏/缺失按「没有快照」处理

## 关键约定

- 集成测试依赖本地测试 sshd：`/usr/sbin/sshd -f /tmp/mino-test-sshd/sshd_config`（端口 2222、公钥认证、含 sftp subsystem）；启动方式 `bash scripts/test-sshd.sh start`
- 配置文件：`~/.config/mino/hosts.toml`（统一 `[[workspaces]]`，`kind = "local" | "ssh"`；SSH 认证枚举用内部标记：`[workspaces.auth.Key]`；旧 `hosts`/`projects` 只读迁移、保存不再写回）。**偏好字段**与工作区同文件：`appearance`、`font_size`、`file_link_editor`、`window_background_opacity`、`macos_window_background_blur`（都在设置弹窗「外观」卡片里可改，滑杆/输入框即时落盘）；**保存走原子写**（临时文件带 pid + fsync + rename 覆盖 + 父目录条目同步，进程被杀/磁盘满不会截断或清空配置）；**加载失败绝不静默清空**——文件存在但解析失败时先备份为 `hosts.toml.bak` 再按空配置启动并 toast 提示（`MinoApp::new_with_config`）
- **崩溃日志 `~/.config/mino/crash.log`**（`main.rs::crash_log_path`，**追加**写入，保留多次崩溃历史；写失败回落 `/tmp/mino-panic.log`）——Finder/Dock 启动时 stderr 无人接收，闪退现场只能靠文件留存。下次启动 `take_crash_report`（与配置同目录，测试传隔离路径）**归档为 `crash.log.1` 并 toast 提示路径**（归档而非删除：日志是用户报告闪退的唯一证据；归档保证同一个崩溃不反复提示）；测试构建跳过该检查（`#[cfg(not(test))]`），行为由 `崩溃日志取走后归档` 单测覆盖
- **测试绝不能读写用户真实配置**：`MinoApp::new` 使用 `default_config_path()`（用户真实 `~/.config/mino/hosts.toml`），涉及配置读写的测试必须走 `MinoApp::new_with_config(cc, test_config_path("标签"))`（`/tmp/mino-test-config-{tag}-{pid}.toml` 隔离路径）——**曾发生测试直接 save + remove_file 用户真实 hosts.toml：跑一次 `cargo test` 就覆盖并删除用户主机列表一次（表现为"每次更新新版本后主机全部消失"）**
- 应用图标：`assets/icon.png`（**黑底圆角 + 白色 `>` + 磷光绿光标块，四周留透明边距**，`scripts/make-icon.swift` 绘制，与应用内品牌 logo 同构图）→ `load_icon()` 解码为 IconData → `ViewportBuilder::with_icon`——**eframe 在 macOS 上通过 NSApp 运行时设置 Dock 图标**，无 .app bundle 的 debug 构建也能生效；`.app` 安装版的 Dock 图标由 package-macos.sh 的 mino.icns 提供（同一绘制源）。**图标必须留透明边距：占满画布的无边距图标会被 macOS Dock 放大显示（比邻图标大一圈）**。**make-icon.swift 必须用 `NSBitmapImageRep` 位图上下文渲染（`NSImage.lockFocus` 在 Retina 屏按 2x 渲染导致输出尺寸翻倍）**
- 字体：Monospace 族 = **JetBrains Mono（打包随附 Regular/Italic/Bold/BoldItalic 四字重）** + Menlo（符号兜底，仅补生僻符号）+ STHeiti（CJK）+ egui 默认；Proportional 族 = SF 主 + STHeiti。JetBrains Mono 自带 `➜`(U+279C)/`❯`(U+276F)/`⚡`/powerline/制表符字形（fontTools 实测 1363 字形），提示符不再依赖 Menlo；四字重族内字形互补（egui 0.36 的 FontId 无字重字段，shaping 按字形覆盖选字体，TextFormat 只管颜色/下划线，italics 只做顶点剪切——见 main.rs 注释）。符号兜底与 CJK fallback 都必须排在打包字重之后。**禁止加载 Apple Color Emoji.ttc**（192MB 彩色位图字体，ab_glyph 无法解析 → egui panic）
- **CJK fallback 后台加载（v1.1.2，`main.rs::CjkFontLoader`）**：中文 `.ttc` 是启动期最大一笔字体 IO（PingFang 78MB / STHeiti Light 55.8MB 全量读入），同步读完明显推迟首帧——改为后台线程读取，就绪后 `ctx.add_font`（`FontPriority::Lowest`）**增量追加**；**不能用「克隆定义 + `set_fonts`」**：`set_fonts` 跨帧比较整份定义（含 TTF 字节）易误判 `==` 吞掉并入（中文"读到了却没装上"的根因）。并入**下一帧** `begin_pass` 才真正重建字体系统，渲染侧因此按**字体定义指纹**在帧开头比对、命中才清行缓存（`terminal_view.rs::font_fingerprint`）——在并入当帧清缓存仍会用旧字体重建，乱码 Galley 被再次缓存且 hash 未变，此后永不重建（启动后登录横幅中文持续乱码的真根因）；平台候选路径见 `cjk_candidates()`
- 主字体装配（打包 JetBrains Mono 约 1.1MB + 符号兜底 + 界面字体）仍同步执行：首帧必须有正确字形
- 提示符 `?➜` 中的 `?` 是 oh-my-zsh robbyrussell 主题 `%1{➜%}` 语法在 zsh 5.9 的真实输出（script 捕获字节流验证：`0x3F E2 9E 9C`），Terminal.app 同样显示，**非 mino 渲染问题，勿尝试"修复"**
- egui 0.36 API 注意：`App::ui` 替代 `update`、`Panel::top/left` 替代 `TopBottomPanel`、`Fonts` 需要 `fonts_mut`、`Event::Key` 无 `text` 字段（Text 独立事件）
- 终端视图的 `focus_id` 每实例唯一（分屏安全，见「分屏」节）；对话框打开时自动聚焦首个输入框
- **测试支持模块**：跨测试模块的共用工具放 `app.rs` 底部的 `#[cfg(test)] mod workspace_test_support`（如 `open_workspace_menu`：按底部区域 + 右缘排序定位空文字矢量按钮）
- **主题测试串行**：`CURRENT_THEME` 是进程级静态，主题/外观测试必须持 `THEME_TEST_LOCK`

## 验证

```bash
 cargo test --workspace -- --test-threads=1   # 单元 + ssh/sftp 集成（需 scripts/test-sshd.sh start）+ UI 渲染
#   B 组对齐（v2.0）新增/守卫：分屏间隙 5 字宽、标签「父/子」与 [Z]、窗口标题 [i/n]、
#   COLORFGBG、OSC 9;4 注意力点、拖选滚轮扩展、替代屏无可滚历史（哨兵）、文件链接编辑器
#   （真实启动假编辑器收参数）、右 Option 组合字符、窗口不透明度叠加、窗口特效配置、
#   窗口几何记忆与保存、外观卡片控件、块字形无缝（像素级，真实字体链） + 字体链 + 标签页（标题条双击 zoom/拖拽）+ 分屏拆分与关闭窗格 + Kaku 快捷键族（⌘F 搜索/⌘K/⌘L/⌘O）+ 标签导航与命令面板 + 窗口快照保存恢复 + 工作区（菜单/面板/管理器/收藏/迁移）+ AI 面板与建议路径 + 表单默认值 + 设置弹窗 + scrollback + 完整应用回车 + TOFU 主机密钥校验 + F 键修饰编码 + SFTP 时间换算/列对齐 + 单击选中双击目录进入 + 中文宽字符列对齐（像素级）+ 字号滑块轨道可辨（像素级）+ 标签标题跟随当前目录 + IME 候选窗跟随光标 + 终端能力应答（OSC 颜色查询/焦点上报/同步更新 2026 混包改写，需 python3）+ 全屏程序字节流下行缓存一致（5 字节分片注入 vim/omp 类序列）+ 崩溃日志归档 + 后台会话异步挂载 + 写回队列有界 + 死锁回归；注意 sftp/ssh 集成测试需先 `bash scripts/test-sshd.sh start`
cargo clippy --workspace --all-targets   # 零警告
cargo fmt --all
```

**自管 GPU 路径（v2.0 起）**：kittest 默认 harness 没有 wgpu 后端（`terminal_gpu = None`，
走 egui 回退），实机验证用其一：
- 真实应用：`cargo run -p mino-app` 目视（窗口截图需屏幕录制权限，
  `screencapture -l <CGWindowID>` 同样受该权限约束）；
- 临时 wgpu 测试：`egui_kittest::Harness::builder().wgpu().build_eframe(|cc| MinoApp::new_with_config(...))`
  —— `.wgpu()` 注入 `cc.wgpu_render_state`，应用走自管 GPU 回调路径，
  `harness.render()` 返回 Metal 渲染的真实像素（可断言行内容/叠加层；已实测
  命令输出/分屏/搜索浮层/右键菜单/标签导航均正常）。该测试依赖本机 GPU 适配器，
  **不要提交进常驻套件**（无适配器环境会挂）。

## 发布流程

- 版本发布必须先把代码合并到 `main`，再在 `main` 上打 `v<主>.<次>.<补丁>` 标签触发 Release 工作流；禁止在功能/诊断分支上直接打 tag 发布
- 发布门禁三件套必须一致：`Cargo.toml [workspace.package] version` == tag 去掉 `v` == `CHANGELOG.md` 顶部的 `## [版本]` 段落（Release 工作流 `Validate release tag and version` 会强制校验）

**UI 测试必须串行**（`-- --test-threads=1`，CI 与 Release 工作流都这样跑）：并行时 kittest 争抢全局状态（主题静态、图形后端）会出现假失败——曾实测 `拖入文件目录应用路径写入终端` 失败、`超链接分段与点击` 挂起 60s+，串行后全部通过。

窗口圆角为运行时原生效果（kittest 无真实窗口句柄，无法单测断言）；验证方式：`cargo run -p mino-app` 后 `screencapture -l <CGWindowID>` 截窗，四角像素应全透明（RGBA alpha=0）。

**自管 GPU 渲染路径 kittest 覆盖不到**（测试环境无 wgpu 后端 → 走 egui 回退）：须实机验证——`cargo run -p mino-app`，实测输入/中文/滚动/大输出渲染正常。
