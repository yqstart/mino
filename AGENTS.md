# Mino 项目架构

轻量终端 + SFTP 可视化工具。Rust workspace，两个 crate。

## 技术选型

| 组件 | 选型 | 理由 |
|---|---|---|
| GUI | eframe/egui 0.36 + wgpu | 原生轻量、macOS 走 Metal |
| 终端内核 | alacritty_terminal 0.26 | 生产级 VT 仿真 + PTY（Alacritty 自用） |
| SSH/SFTP | russh 0.62 + russh-sftp 2.4 | 纯 Rust 无 C 依赖 |
| 配置 | serde + toml | hosts.toml 持久化 |
| UI 测试 | egui_kittest | 渲染级断言 |

## 目录结构

```
crates/
├── mino-core/                 # 纯引擎，禁止依赖 egui
│   ├── src/lib.rs
│   ├── src/terminal/mod.rs   # Session 统一封装（本地/远程）、TermSize、Listener
│   ├── src/terminal/keys.rs  # 键盘 → 字节序列编码（XTerm 修饰键）
│   ├── src/ssh/mod.rs        # connect_remote（后台线程 + tokio）、authenticate、connect_verified（TOFU + keepalive 配置）
│   ├── src/ssh/known_hosts.rs # HostKeyVerifier（TOFU 主机密钥校验）+ known_hosts.toml 持久化（0600）
│   ├── src/ssh/sftp.rs       # connect_sftp、SftpHandle（命令队列）、SftpEvent（事件流）
│   ├── src/config/mod.rs     # HostProfile/HostConfig + hosts.toml
│   └── tests/                # 集成测试（需本地测试 sshd: 127.0.0.1:2222）
└── mino-app/                  # egui 应用
    ├── src/main.rs           # 入口 + 字体装配（同步主字体，CJK fallback 走后台 CjkFontLoader）+ 圆角注入
    ├── src/native.rs         # macOS 原生窗口定制（无边框窗口整体圆角，AppKit layer）
    ├── src/app.rs            # MinoApp：tabs 列表 + 设置弹窗（show_settings）+ 布局/连接管理/对话框/快捷键；本地会话后台创建（poll_local_spawn 帧内挂载）
    ├── src/perf.rs           # 性能 HUD（帧耗时/FPS/Shape 数/重建行/上传字节，⌥P 切换）
    ├── src/theme.rs          # 三套深色主题
    ├── src/workdir.rs        # 根据 PTY 输入跟踪本地/远程 cd 工作目录
    └── src/views/
        ├── terminal_view.rs  # cell 渲染（行级增量 + Galley 缓存 + 自管 GPU 提交）、输入转发、滚动
        ├── terminal_gpu.rs   # 自管 GPU 渲染：逐行顶点常驻显存 + 动态 uniform + 图集换代看门狗
        ├── terminal_gpu.wgsl # 行网格着色器（与 egui.wgsl 同源的颜色/抖动/gamma 语义）
        └── sftp_view.rs      # 文件面板：列表（show_rows 虚拟化）/导航/传输进度/确认对话框
```

## 核心数据流

### 终端会话（本地与远程统一）

- 本地：`tty::new` 建 PTY → alacritty `EventLoop` 线程读 → vte 解析 → `Term` 网格
- 远程：后台线程 tokio runtime → SSH channel 读 → `Processor::advance` → 同一 `Term`
- 统一接口：`Session { term, writer, resizer, shuttor, is_remote }`（Writer/Resizer/Shuttor 闭包抽象）；本地与远程会话共用同一终端输入与渲染链路
- 写操作：本地走 `EventLoopSender`，远程走命令队列（UI 线程非阻塞）
- **本地会话后台创建（v1.1.2，`app.rs`）**：`new_local_tab_at` 只登记 `pending_local`（`mpsc`），后台线程 `Session::spawn_local` 完成后 `request_repaint`，UI 帧内 `poll_local_spawn` 就绪即挂标签——PTY fork + oh-my-zsh 初始化（可达数百毫秒）不再堵首帧与 ⌘T，等待期状态栏显示"终端启动中…"；**测试构建仍走同步路径**（kittest `run_steps` 不等待后台线程），异步挂载由回归测试 `本地终端异步就绪后挂载标签` 直接驱动 `poll_local_spawn` 覆盖
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
- **v1.1.2 追加减负（egui 回退路径同样生效）**：一行 N 个 `CachedRun` 合成为**单个 `Mesh`** 提交（`row_meshes`，键为显示行号——mesh 顶点烘焙了绝对行位，滚动/尺寸变化整体重合成，`rows_cache` 保留）；按 **damage 的列区间**只重建受损列（`damaged_bits`：光标移动只损 1-2 列，曾整行 80-200 cell 重算）；Shape 一次 `Painter::extend` 批量提交（`shapes_scratch`，替代逐 Shape `add` 的上千次 Context 写锁）；装饰网格线 tessellate 结果缓存（`grid_lines_cache`）；输入法预编辑串 Galley 缓存（`ime_preedit_cache`）；标签/状态栏目录读 `effective_local_directory`（300ms TTL，每帧 4 次系统调用 → 1 次；需要即时真值的入口走 `fresh_local_directory`）
- **一帧输入合并**：输入事件先攒进 `pending_writes` 再一次 `session.write`（曾每个按键/粘贴分片一次通道发送）
- **resize 节流**：拖拽窗口时 `resize_grid` 每帧更新本地网格（字不越界），后台通知（本地 SIGWINCH / 远程 SSH `window_change`）按 **50ms** 合并 + trailing 补发（`notify_backend_size`）
- 远程逐 SSH 数据包唤醒合并为每帧一次（`notify_wakeup` 标志位，由 `drain_events` 复位）：`cat` 大文件时重绘请求不再与包数同阶

### 终端自管 GPU 渲染（v1.1.2 起，`views/terminal_gpu.rs` + `terminal_gpu.wgsl`）

- **动机**：egui 每帧把全部 Shape 重新 tessellate 并整块上传顶点（`Context::tessellate` 明确不做跨帧复用），终端这种「字形几乎不变、只有少数行在变」的场景等于每帧整屏重传。自管路径把每行顶点常驻显存：**行内容不变不重传、滚动只改 uniform 的 `row_origin`、空闲帧零上传**
- **启用条件**：`MinoApp::new` 用 `cc.wgpu_render_state` 建进程级 `TerminalGpu`（管线/布局/采样器/图集绑定），会话创建时 `TerminalView::set_gpu` 注入；kittest（`Harness::new_ui`）没有 wgpu 后端 → `gpu = None` → 自动走 egui Shape 回退路径（两条路径共存，回退路径由 kittest 全覆盖，**GPU 路径须实机运行验证**）
- **回调内绝不触碰 `egui_wgpu::Renderer`**：kittest 在 `update_buffers`/`render` 期间持写锁、eframe 渲染期持读锁，回调内再取锁必死锁。字体图集绑定只在 UI 线程帧内刷新（`gpu_atlas`），回调只读
- **管线逐项对齐 egui**（颜色格式/混合/采样数/顶点布局/抖动/gamma/预乘 alpha），否则自管绘制与相邻 egui 控件有可见色差；顶点 uv 是 epaint 的**纹素坐标**，归一化在顶点着色器里做（`a_tex_coord / r_locals.atlas_size`）
- **图集换代看门狗（双条件）**：行顶点 uv 与图集尺寸强绑定，按「尺寸变化 **或** 填充率骤降」检测——epaint 整份重建字体系统时尺寸可能一模一样但字形位置全变（只比尺寸会漏，表现为字符错位/串码）；图集还会在同帧更晚的位置（其它控件首次用到新字形）继续变大，因此必须在**下一帧开头**再比一次当前尺寸与 `mesh_atlas_size`，不一致立即整屏重建并请求重绘
- **逐行缓冲 + 区间分配器**：`RowBuffers` 每行独立槽位（容量翻倍冗余、`free_v`/`free_i` 空洞合并），容量不足时返回 false（该行本帧不可见）；uniform 步长按 `min_uniform_buffer_offset_alignment` 放大（`TerminalGpu::new` 读设备 limit），每行一个 `Locals`，**上限 `MAX_ROWS = 256`**
- **`screen_size` 必须是整个渲染目标的点尺寸**（窗口），不是终端内容区：着色器用「绝对坐标 ÷ 全屏尺寸」→ NDC，传内容区尺寸会把坐标放大（文字整体拉伸错位）
- 性能 HUD（⌥P）显示每帧 Shape 数 / 重建行 / 复用行 / 上传字节——渲染优化的唯一可观察证据（帧耗时区分不了「CPU 侧重建」与「GPU 侧上传」）

### 终端能力应答（v1.0.8 起，`terminal/mod.rs` + `terminal_view.rs` + `keys.rs`）
-
- **程序的终端查询必须应答**：mino 曾把 alacritty 的 `Event::ColorRequest` / `Event::TextAreaSizeRequest` 当无关事件丢掉，导致 `printf '\e]11;?\a'` **永远收不到答复**（实测对比 macOS Terminal.app：Terminal 回 `]11;rgb:1e1e/…`，minо 无任何输出）。查询终端配色的 TUI（omp 启动即查 OSC 11）只能按"未知终端"回退配色
- 事件链路：`SessionEvent::ColorRequest { index, formatter }` / `TextAreaSizeRequest(formatter)` / `ClipboardStore` / `ClipboardLoad` / `ResetTitle` 入队（`Listener::send_event`，formatter 是 alacritty 给的闭包），UI 侧 `TerminalView::drain_background_events` 应答后写回 PTY；`Bell` 转 0.6s 视觉脉冲（左上角圆点，无常驻重绘），`ResetTitle` 清标题缓存（曾在 `_ => false` 被丢弃，标题永久停旧值）
- **PtyWrite 队列 64KB 字节上限（v1.1.2）**：PtyWrite 是终端能力应答（DA/kitty/DECRQM/OSC 查询回执，程序可触发），不是用户数据——多标签高输出并发时后台标签长期不消费会无界追加；超限丢最旧的应答（程序按"未知终端"回退，不破坏终端状态），队列内存有界；回归测试 `写回队列超限丢最旧应答且有界`
- **颜色索引语义**（alacritty `term::color`）：0-255 调色板、256 前景、257 背景、258 光标、259+ Dim/Bright 变体；优先级与渲染 `resolve_color` 一致：**OSC 动态覆盖（`term.colors()`）> 主题调色板**。`Colors` 只实现越界即 panic 的 `Index`，查询索引由终端程序控制，**必须先做边界检查**（`COUNT` = 269）；**`COLORTERM=truecolor` 必须注入**（本地 `local_session_options_at` + 远程 `set_env`，sshd 未放行时静默忽略）：omp 的 `getColorMode` 只认该变量判 24-bit，`TERM=xterm-256color` 只判 256 色，缺了它 agent 全程降级（实证自 omp 二进制）
- **焦点事件上报（`DECSET 1004`）**：程序开启后，窗口获得/失去焦点必须发 `ESC [ I` / `ESC [ O`（`TerminalView::report_focus_change`，读 `ctx.input(|i| i.focused)`）；**首帧只记录基准状态**，不能补发历史变化（程序是在自己启用之后才开始期待事件）；回归测试 `终端查询与焦点上报有应答`（python3 脚本开 cbreak 读 PTY，断言收到 `]11;rgb:` 与 `\x1b[I`）、`颜色查询按索引返回主题颜色`、`颜色查询优先使用OSC覆盖`、mino-core `颜色与尺寸查询进入事件队列`
- **对照 Terminal.app 的方法**（本机排查用）：把 omp 包一层透明 PTY 代理分别跑在 mino 与 Terminal.app 里，比对**两个方向**的字节——`mino → 程序`方向能看出终端应答差异，`程序 → mino`方向能看出按键编码差异
- **按键编码（`keys.rs`）**：主键盘 Enter 永远 `\r`；**Shift+Enter=`ESC[13;2~`、Ctrl+Enter=`\n`（`Ctrl+J`）**——omp 把 submit/newLine 分绑裸 Enter 与 Shift+Enter/Ctrl+J，三者同发 `\r` 时输入框换不了行（changelog #8821 实证两种形态都兼容）；kitty 键盘协议已开启（`term::Config { kitty_keyboard: true }`，本地/远程/测试三处）：订阅 `DISAMBIGUATE` 后非可打印键走 CSI-u（Enter=13/Tab=9/Esc=27/F1-F12=57345-57356/方向 57358-57361 等，修饰位同 xterm），可打印字符永远 legacy；释放事件 `encode_kitty_release`（`:3` 后缀）暂备而不用（egui 只给 press）
- **鼠标上报透传（1000/1002/1003 + SGR 1006）**：程序订阅 `MOUSE_MODE` 后点击/拖拽/释放直接发 xterm 序列（按下→拖拽→释放，按键全程快照一次，释放帧 input 已读不到）；拖拽需 1002+ 才发（只开 1000 不发 Drag）；未订阅走本地选区。滚轮既有分支不变。回归测试 `鼠标上报开启时点击透传程序`
- **OSC8 超链接**：同 URI 相邻 cell 合并进 `Segment::link`（进 hash 指纹），accent2 下划线 + 悬浮小手 + 单击 `open_url`（`same_tab`）；鼠标上报订阅时点击透传给程序、不抢链接。回归测试 `超链接分段与点击`
- **OSC52 剪贴板**：store 经 `ClipboardStore` 事件写系统剪贴板（`ClipboardReader::set_clipboard_text`，arboard；失败走 toast，不静默丢），load 用当前剪贴板文本回写（默认 OnlyCopy 下程序发不出 load）。回归测试 `程序复制写入系统剪贴板`
- **样式细节**：粗体=前景增亮 1/3（egui 无合成粗体，不做描边仿粗）；下划线变体全集（SGR 4/4:2 双线/4:3 波浪/4:4 点/4:5 虚，`UnderlineStyle` 进段合并条件+hash+宽字缓存键），Single 由 Galley 画、其余由 paint 侧矢量补（双线/波浪 8 段正弦/点虚 dash），颜色走 SGR58（无则跟前景）；DECRQM 查 2026 回 `0`（不支持）：alacritty 0.26 的 2026 set/unset 是空实现却回 `2`（已重置），程序会误判支持同步更新——`feed_program_output`（本地注入与远程读循环共用）在解析前改写，混合包暂不拆（omp 查询独包）。回归测试 `下划线变体与颜色进段缓存` / `同步更新查询回不支持` / `铃声与标题重置有响应`

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
- **SFTP 主机名绑定在连接上**（`pending_sftp/ready_sftp` 元组携带 host label）：状态栏从 `tab.sftp.host_name()` 读取——曾用全局 `sftp_host` 字段，多远程标签共存时状态栏串台成"最后一次连接的主机名"
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

## 样式体系（Tabby 风格）

- 视觉参照 Tabby 终端（tabby-core theme.vars.scss）：深蓝灰分层背景、扁平控件、低对比 hover、小圆角紧凑排版、蓝色强调
- 设计 token 在 `theme.rs::tokens`：分层背景 `#0e151d`（应用）/`#0a1017`（标签栏，最深）/`#16202b`（面板，激活 tab 凸起层）/`#1d272d`（浮层）/`#0b1016`（终端，最深）
- 文字三阶 `#e2e9f0/#b3c0cc/#6e7d8c`；accent `#4f9df5`（Tabby 激活蓝）+ `accent2 #5bc0de`（青蓝辅助）；语义色 `#5cb85c/#f0ad4e/#d9534f`
- **hover 高亮 = 白色低透明度叠加（`rgba(255,255,255,12~20)`），不再用 accent 底**；选中/激活态才用 `ACCENT_SOFT`（全局 widgets.hovered/active 在 `apply_theme` 统一设置，主机行/标签页自绘处同步遵循）
- 边框统一半透明白（`BORDER_SUBTLE`，白 9%）；圆角 6px（按钮/输入）/5px（列表项/标签页）；按钮 padding 紧凑 `(9,4)`
- 面板用 `Panel::frame(Frame)` 指定背景与边框；主按钮用 accent 填充
- 终端背景在 TerminalView 里绘制 `BG_TERMINAL`；终端光标白色（Tabby 风）
- **动效**（`anim.rs`，无第三方依赖）：`smooth/smooth_bool` 指数平滑（状态存 ctx 临时数据，收敛中自动 request_repaint）、`paint_shimmer_line` 扫光 Mesh、`paint_rounded_gradient` 圆角渐变三角扇、`paint_glow` 辉光（仅品牌 logo hover 使用）
- **品牌 logo**（`draw_logo_mark`）：紫金渐变圆底 + **白色粗体 "K" 字**（居中，与应用图标同构图，品牌识别保留渐变）；hover 时 accent2 辉光；无动画、无持续重绘（省电）。**kittest 注意：持续重绘会让 `Harness::run()` 超 max_steps panic——测试统一用 `harness.run_steps(6)` 显式步进**
- **hover 光标**：`apply_theme` 设 `visuals.interact_cursor = Some(CursorIcon::PointingHand)`，所有标准控件（Button/SelectableLabel/ComboBox）hover 自动变小手（TextEdit 内部显式设 I-beam 覆盖）；自定义 `ui.interact` 区（主机行/删除/logo/toast）需手动 `.on_hover_cursor`
- 终端内容内边距 `PADDING=10`（`terminal_view.rs`）：背景铺满 `ui.max_rect()`，文本/光标在内边距内绘制。**注意 egui 陷阱：`ui.min_rect()` 是"已用内容"包围盒，无子项时为 0x0**——背景/点击区域必须用 `ui.max_rect()`（布局分配区域），否则背景画不出来、点击无法重新聚焦
- **display_iter 行号是网格坐标**（alacritty `grid/mod.rs::display_iter`）：viewport 顶行为 0，向上滚动后可见的 scrollback 行是**负行号**（`Line(-(display_offset)-1)`）。渲染循环必须用相对视口顶行的显示行号（换行时计数），**严禁 `point.line.0 as usize`**——负行号 cast 成 usize::MAX 级巨值，`rows_cache.resize` 直接 `capacity overflow` 闪退（offset=1 时则是 index 越界）；光标定位同样需换算：显示行 = 光标网格行 + `display_offset`。回归测试：`滚动scrollback后渲染不崩溃`
- 主机条目（设置弹窗 → 主机管理卡片内）：单击选中、双击连接；样式：**accent 圆形头像（主机名首字符）+ 名称（超长截断不换行）+ 🗑 删除图标**（行右缘，hover 红底）；**行点击区横向扩展到面板可用宽度**（`row_rect`），短名称主机也能整行点击
- **egui 0.36 交互坑：`Response::interact()`（scope_builder(...).response.interact(...)）的点击无法命中**（响应链问题，kittest 实测 clicked/hovered 恒 false）——必须用 `ui.interact(rect, id, sense)` 显式注册交互区，删除按钮等行内控件最后注册以覆盖行点击区
- **egui 0.36 锁序铁律：唯一合法顺序是「先终端锁、后 Context」——任何地方都禁止反向（Context → 终端 `FairMutex`）**。Context 的 `input`/`memory`/`output_mut`/`fonts_mut`/`request_repaint` 都是 **写锁**，而 PTY 读线程按 `pty_read`（持 lease）→ `send_event` → `on_event` → `Context::request_repaint` 的固定顺序取锁；一旦 UI 侧出现在持 Context 写锁时等终端锁，两线程各持一把等对方 = AB-BA，双方都是阻塞等待、永不恢复（用户现象：SSH + omp 双窗口整窗无响应；`sample` 实证主线程停在 `Context::input` 闭包内等终端锁，PTY 读线程停在 `request_repaint` 等 Context）。两处历史违规：①`ui.input` 闭包内 `scroll_display`（滚轮/Shift+PageUp）②渲染持锁期间 `ctx.input(|i| i.time)` 与闪烁 `request_repaint_after`。修法：滚动/重绘/时间读取等一律记意图，锁外或闭包外统一执行。回归测试 `后台持锁请求重绘时终端仍能推帧`（后台线程持终端锁并回调 `request_repaint`，主循环推帧 + 滚轮——旧实现 10 秒内触发 epaint 死锁检测，测试有界失败）
- 终端调色板（Catppuccin Mocha 16 色 + xterm 256 色表）在渲染层解析（`theme.rs::TERM_PALETTE_16`/`xterm256`），优先级：Spec > OSC 覆盖（term.colors）> 内置调色板；`Term.colors` 默认全 None，不设调色板则全部渲染为白色；**v0.7：xterm256 固定部分（index ≥ 16）用 `XTERM_FIXED` OnceLock 查表**（曾逐 cell 现算乘除，全彩色屏每帧上万次算术）
- 选中主机条目左侧 2px accent 竖条；标签页底部有选中指示条（白色细线，宽度动画，Tabby current-tab-indicator）
- **标签页（tab）无双边框**：tab 内容用无边框透明 `Button`（`selectable_label` 选中自带边框，与手绘高亮叠加会成"双重框"），高亮只有一层手绘——**激活 tab 用面板色（`bg_panel`）凸起底 + 底部白色指示条，hover 用白 8% 叠加**（Tabby 风：标签栏比激活 tab 深一级）。**选中底必须画在内容之前**：用 `egui::Frame`（fill=bg_panel 随 sel_alpha 动画、inner_margin(2,3)）+ `.show()` 包住 tab 内容（Frame 先铺底再放内容）——曾把不透明面板色 `painter.rect_filled` 画在内容之后（同一 layer 后画在上），激活 tab 文字被整块盖住完全不可见（像素扫描验证：除底部指示条外无任何文字像素）；hover 白 8% 叠加是极淡提亮，画在内容之后无害
- **三套深色主题**（`theme.rs::THEMES`）：深色（Tabby 蓝灰）/ 深蓝 / 霓虹；每套含 UI token + 终端调色板（16 色 + fg/bg/cursor）；`current_theme()` 静态读取，**设置弹窗 → 外观卡片**切换（`set_theme`）——ComboBox 选当前主题名（**勿用 ◐ 等 unicode 符号作为图标，SF 字体缺字形渲染为方块**）；**主题选择持久化在 `hosts.toml` 顶层 `theme` 字段（存名称非下标，顺序调整不影响）**：切换走 `apply_theme_and_persist`（设置弹窗 + ⌥1-3 统一入口，落盘失败回滚内存态并 toast），启动按配置名恢复（未知/空回默认第一套，老配置缺字段 `#[serde(default)]` 兼容）；**勿直接调裸 `set_theme` 做用户切换**（只改内存，退出重进回到原来的——本 bug 根因：启动还曾硬编码 `set_theme(0)`）；回归测试 `三套主题渲染截图`（断言切换即落盘）/ `主题切换重启后保持`；**`CURRENT_THEME` 是进程级全局静态，主题测试必须 `THEME_TEST_LOCK` 串行**（并跑会互相污染 current_theme 断言）
- 主题切换后需重新渲染终端（terminal_view 每帧读 `current_theme()`）
- 终端输入/回车功能验证正常；**"回车不执行"两大真根因**：①TERM=dumb（GUI 启动继承，见下条 env 注入）②zsh 启用应用键盘模式（APP_KEYPAD）后主键盘 Enter 曾被误编码成 `ESC O M`——已修复为永远发 `\r`（见 keys.rs）；此外中文输入法（IME）组字中回车被输入法消费（确认拼音候选）是所有终端应用共性，先选词上屏或切英文输入法即可
- **中文输入法（IME，`terminal_view.rs`）**：egui 原生后端默认不启用输入法——终端聚焦时必须每帧写 `PlatformOutput::ime`（`IMEPurpose::Terminal` + 光标矩形跟随），egui-winit 才会调 `Window::set_ime_allowed(true)`，否则 OS 根本不发组字事件（用户现象"中文状态下输不进中文、进去的都是英文"）；**`IMEOutput.rect` 必须是光标 cell 级小矩形**——egui-winit 0.36 的 winit 后端调 `set_ime_cursor_area` 只用 `rect`（忽略 `cursor_rect`，`handle_platform_output_inner`），macOS 经 `firstRectForCharacterRange` 拿到大矩形就会把候选窗放在终端左下角（曾传整个终端 `inner`，用户报告"输入法不在输入位置附近而在左下方"，2026-09-11 修复）；光标滚出视口退首行行首单 cell；**`Event::Ime` 三分支**：`Preedit` 只存不写（光标处内联下划线渲染，不进 PTY/不进回显/不污染行 hash，组字期零散 Text 同步忽略防拼音污染命令行）、`Commit` 才 `session.write` 上屏中文（+ workdir.push_text）、`DeleteSurrounding` 转退格/删除序列（Gboard 类）、空 Preedit=取消组字；前台弹窗打开（`input_enabled=false`）不声明 IME；回归测试 `中文输入法组字与上屏`/`聚焦终端声明IME意图`/`弹窗打开时不声明IME意图`/`IME候选窗跟随终端光标`
- **"删除键插入空格"**：有两类独立根因——①**微信输入法 wetype**：退格/删除键按下时输入法伴随发送"空格类" Text 事件（ASCII 空格/零宽）→ 已修复：`suppress_next_text` 跨帧标记 + 退格后一帧内的空白类 Text 丢弃（只影响空白字符，不误伤正常输入）；回归测试 `退格伴随空格文本不插入` ②**TERM=dumb**：zle 判定非交互终端，删除回显走「原地空格覆盖」（只发空格缺 `\b`）→ 已修复：env 注入 TERM（见下条）
- **布局重构（v0.5/v0.6）**：移除了左侧固定主机栏（`Panel::left("hosts")`）与顶部工具栏（`Panel::top("toolbar")`），所有原工具栏入口（主题/更新/品牌 logo）搬入**设置弹窗**（不再用 tab）；标签栏最右侧 `settings_gear_button` **22×22 纯图标齿轮**（无文字 label，**朴素风：⚙ 符号次要色、hover 变主色 + 白 8% 圆角底**（曾为渐变圆底 + accent2 辉光，用户要求朴素化），包装为 `egui::Button` 以便被 kittest `Role::Button` 找到），click → `show_settings = true`；`⌘,` toggle 设置弹窗（macOS 标准"应用偏好设置"快捷键，取代原 `⌘B` 折叠侧栏）。**`MinoApp::new` 启动仅创建本地终端 tab**（`Vec<Box<TerminalTab>>`），`active_tab = 0`（不打扰用户进入终端）；设置弹窗默认关闭。标签栏图标按钮：**＋（新建本地终端）→ `>_`（快速 SSH 连接）→ ⚙ 齿轮**（复制 tab「⎘」按钮已移除，相关 `duplicate_active_tab`/`new_local_tab_with_label` 一并删除）
- **快速 SSH 连接按钮**（`ssh_quick_button` + `host_quick_menu`）：标签栏 `>_` 图标（monospace ">_" 终端符号，风格同齿轮），点击弹出 `egui::Popup::menu` 主机菜单——**单击主机行直接发起连接**（与设置弹窗主机行双击不同，快捷入口单击即连，点击后 `ui.close()` 关闭菜单）；行样式：**固定宽 256、行高 40、先 allocate 满宽再绘内容**（hover 白 8% 圆角底贴齐左右内边距 4px，不按内容包围盒 expand——短名称曾只亮一小块）；**渐变圆头像 22px 垂直居中 + 名称/user@host 左对齐截断**（名称 `text_primary` / 地址 `text_secondary`）——**禁止 `add_sized` 放 Label**（其内部 `centered_and_justified` 把短名称水平居中，和长地址错位）；无已保存主机时提示"暂无已保存主机"+ 新建连接按钮（打开 `show_new_conn`）。**`Popup::menu` 的开关状态按按钮 Id 记忆，按钮必须 `ui.push_id("ssh_quick")` 固定 Id**（自动 Id 帧间漂移会让菜单闪断）；Popup::show 只接受 content 闭包（ctx 在构造时绑定，无 ctx 参数）；回归测试 `ssh快捷按钮连接主机`（隔离配置 + 端口 9 立即拒绝，断言 pending_label）/ `ssh快捷按钮无主机提示` / `ssh快捷菜单行左对齐`（短名与长名左缘对齐、名称与地址同一列）
- **窗口实现现状（v0.7.1）**：macOS 保留透明的全尺寸标题栏并隐藏标题文字，保留系统原生交通灯按钮，避免无边框窗口退出时的 AppKit 崩溃；应用自绘标签栏拖拽区，`native.rs` 负责圆角与透明背景。
- **窗口整体圆角**（`native.rs`，仅 macOS）：`with_decorations(false)` 无边框窗口是方角，通过 AppKit 给 contentView 的 backing layer 设 `cornerRadius=12` + `masksToBounds`，并 `setOpaque(false)` + `backgroundColor=clearColor` 让圆角外侧透明露出桌面（像素验证：四角 RGBA=(0,0,0,0) 全透明）。入口在 `main.rs` 的 app_creator 闭包 `native::apply_rounded_window(cc)`——用 `cc.window_handle()` 拿 `RawWindowHandle::AppKit` → `ns_view` 指针转 `&NSView`（同 eframe 内部写法，指针窗口生命周期内有效）；kittest 测试环境无真实 AppKit 句柄（`window_handle()` 失败/非 AppKit 变体）时静默返回，不 panic。**依赖**：mino-app 直接声明 `objc2-app-kit`（补 `NSColor`/`NSWindow`/`NSView` + `objc2-quartz-core` feature 使 CALayer 可用，features 全局合并不影响 eframe/winit）+ `objc2` + `raw-window-handle`（版本与 eframe 一致 0.6.2）；NSWindow 为 `MainThreadOnly`，必须在 eframe 主线程创建期调用
- **标签栏拖拽与双击 zoom**（`tab_bar`）：无系统标题栏，`tab_bar` 开头对**整行**（`ui.max_rect()`，Panel 分配区域非 0x0）注册底层 `Sense::click_and_drag()` 背景（id `tab_bar_drag`）——拖动（`drag_started_by(Primary)`）发 `ViewportCommand::StartDrag`（egui-winit 转 winit `drag_window()`，需窗口有焦点），双击（`double_clicked()`）读 `viewport().maximized` 取反发 `ViewportCommand::Maximized`（macOS 标题栏双击标准行为，winit 转 `zoom:`，zoom/恢复由 AppKit 记住的标准 frame 完成）。**Sense 必须带 CLICK**：纯 `Sense::drag` 按下瞬间即判 drag，第二次按下时指针几乎不动、仍是 drag 状态，`double_clicked` 永假（官方 custom_window_frame 示例同样用 click_and_drag；`drag_started_by(Primary)` 同示例，避免右键拖拽误移窗）。**关键交互顺序**：egui 命中规则是**后注册 widget 在顶层**（`WidgetRects::get_layer` back-to-front，`hit_test` 中 `drag_idx < click_idx` 判定 click 在 drag 之上）——拖拽背景**先注册**（底层），tabs/红绿灯/齿轮**后注册**（顶层），故控件点击/拖拽优先，仅标签栏空白处按下拖动才移窗；`Sense::drag` 的大背景 + 附近小控件会被 hit_test 的 `contains_rect` 帮助逻辑优先给小控件。**实测验证**：CGEvent 模拟标签栏空白处拖拽 → 窗口位置精确移动对应距离；点击齿轮 → 设置弹窗正常打开（kittest 齿轮测试亦通过）；`ui.interact` 不占布局空间不移动 cursor，horizontal 布局不受影响
- **设置弹窗**（`fn settings_panel(ctx)`，`egui::Window` 居中，标题"设置"）：卡片化三组（`Self::settings_card` helper 渲染 `bg_elevated` 底 + 边框 + 圆角 + 紧凑内边距）：**主机管理**（复用 `host_sidebar`）/ **外观**（主题 ComboBox）/ **关于**（版本号 + 检查更新按钮）。`ScrollArea` 垂直滚动，max_height 420 防止超出视口。**主机管理卡片内不要画细分隔线**——曾用 `hairline()` 画「新建连接」与主机列表的分隔线，但 hairline 取 `ui.max_rect().top()`（卡片内容区**顶部**）而非当前光标位置，线实际出现在"主机管理"标题下方、压在新建连接按钮上方（像素验证 y=266），用户要求去掉；`hairline` 函数已连同删除（`ui.max_rect().top()` 不等于 cursor.y，任何分隔线都应用 `ui.cursor().top()`）
- **应用内更新**（`mino-core::updater` + `app.rs` 状态机）：版本检查走 `releases.atom`（不受 API 限流），资产直链按 `mino-{版本}-macos-{arm64|x64}.dmg` 构造并流式下载（进度回调）；**下载失败自动删除半成品 dmg**（不残留损坏镜像误导安装）；安装由后台 shell 脚本完成——`hdiutil attach` 挂载 → `pgrep -x mino-app` 等待主进程退出 → `ditto` 替换 `/Applications/Mino.app`（失败回退 `~/Applications`）→ `open` 重启；`installed` 状态延时 0.9s 后 `ViewportCommand::Close`；入口在设置弹窗 → 关于卡片
- 主机行双击连接为**自实现检测**（`last_row_click`：0.3s 内同行的第二次点击）：egui 多击计数会把无关点击（如工具栏其他按钮）计入序列导致 count=3 而 `double_clicked`（count==2）失效；**双击/新建连接成功后自动关闭设置弹窗**——settings_panel 结尾同步必须用 `self.show_settings = open && self.show_settings`（egui 只把用户关闭动作回写局部 `open`，闭包内主动关闭会被 `= open` 覆盖失效）
- **多标签页**（warp 风格）：`MinoApp` 持 `tabs: Vec<Box<TerminalTab>>`（`pub type Tab = Box<TerminalTab>`），SFTP 按 TerminalTab 挂载（远程 tab 独享）；**`Box` 包裹 TerminalTab**——避免 `Vec` 各槽位按最大元素对齐造成内存浪费（原 enum 形式触发的 `large_enum_variant` 警告在去 enum 化后自动消失）；标签栏在顶栏（`Panel::top("tabs")`，合并了原工具栏区域），当前标签 accent-soft 高亮，支持点击切换/×关闭/＋新建；最右侧为设置齿轮；快捷键 ⌘T 新建本地、⌘W 关闭当前、⌘1-9 切换、⌘N 新建连接、⌘, 切换设置弹窗、⌥1-3 主题
- **标签/状态栏标题跟随当前目录（2026-09-11）**：`TerminalTab::title()` 返回 `(显示文本, 全路径悬浮提示)`——**本地标签显示当前目录的末级文件夹名**（`dir_display_name`，zsh `%c` 语义：home → `~`、根 → `/`、尾随 `/` 剥掉），**悬浮标签标题/状态栏标题显示全路径**；远程标签仍用主机名（远端目录由 sshd 决定，本地跟踪器不适用）。目录来源是 `TerminalView::effective_local_directory()`（shell 子进程内核 cwd 优先、`child_current_dir` 偶发失败时回退跟踪值；纯跟踪 `current_directory()` 在粘贴/补全/别名/函数/`cd -` 下会停在旧目录，标题/收藏都不能依赖它），**不采用 shell 上报的窗口标题**：oh-my-zsh 的标题是截断过的 `%15<..<%~%<<`，既非末级目录名也拿不到完整路径（曾固定显示 `label = "本地终端"`）。⌘D 收藏与“使用当前终端目录”与标题同源（曾用纯跟踪值，粘贴 cd 后收藏的仍是启动目录）。回归测试 `目录展示名只保留末级` / `本地标签标题跟随当前目录`（cd 进嵌套目录后标题 = 末级名、提示 = canonical 全路径）/ `标签栏与状态栏显示当前目录名`（完整应用渲染：标签栏 + 状态栏各一处，hover 标题出现全路径提示）/ `粘贴cd后收藏真实目录`（粘贴 cd 致跟踪失效后收藏仍为内核真实目录，仅 macOS）
- 本地终端默认工作目录为 **home**（`local_session_options`：`working_directory = $HOME`）——Finder/Dock 启动时进程 cwd 为 `/`，不指定会导致终端落在根目录；远程会话不受影响（由 sshd 决定）
- **终端 `ls` 颜色与 TERM**：渲染层已支持 ANSI 色，但 macOS `ls` 默认无颜色输出（无 `CLICOLOR`）→ `local_session_options` 注入 `CLICOLOR=1` + 深色优化 `LSCOLORS=Gxfxcxdxbxegedabagacad`（目录亮青/链接紫红/可执行红/socket 绿/管道黄）+ **`TERM=xterm-256color`**（必须注入：GUI/Finder/Dock 启动继承 `TERM=dumb`，alacritty 的 `setup_env()` 只在 alacritty 主应用入口调用、mino 未调用，`TERM=dumb` 会致 zsh zle 删除回显异常/回车异常——同 Miro Code 根因；**勿注入 locale**，会引发回车不执行）；经 `SessionOptions.env`（alacritty 为覆盖语义，`builder.env` 追加继承）传给 PTY；与 Terminal.app/iTerm2 行为一致，不篡改 shell
- **键盘编码**（`mino-core/src/terminal/keys.rs`）：主键盘 **Enter 永远发 `\r`**（应用键盘模式 APP_KEYPAD 只影响数字小键盘 Enter = `ESC O M`，主键盘 Enter 不受影响）——曾误按 APP_KEYPAD 把主键盘 Enter 编码成 `\x1bOM`，而 zsh 在 TERM=xterm-256color 下 zle 自动开启应用键盘模式 → 回车不执行（命令停在命令行）；回归测试 `enter应用键盘模式` 断言 Enter 在 APP_KEYPAD 下仍发 `\r`。方向键按 APP_CURSOR 用 SS3（`ESC O A` 等），Backspace=`\x7f`，Delete=`\x1b[3~`，Shift+Tab=`\x1b[Z`。**F1-F4 带修饰键的 CSI 修饰形式末尾字母随键递增**（P/Q/R/S = F1-F4，xterm 规范）——曾固定发 `P`，F2-F4 带修饰键时被终端识别为 F1（回归测试 `功能键带修饰键按xterm规范编码`）
- 终端内容颜色：prompt 彩色来自 shell 主题（如 oh-my-zsh robbyrussell 的绿/青/蓝/黄）
- 主题背景跟随当前深色主题；`apply_theme` 同步系统深色主题（`ctx.set_theme`）
- 主题快捷键：⌥1-⌥3 快速切换三套主题
- **新建连接表单**（`connect_dialog`）：默认用户名 `root`、端口 `22`（`ConnectForm` 手写 `Default`，可修改）；输入框统一走 `form_input` helper——圆角深色底（`bg_elevated`）+ 焦点 accent 边框 + `vertical_align(Center)`（**TextEdit 默认 `Align2::LEFT_TOP` 文字偏上，单行输入框必须居中**）
- **状态栏**（底部）：左侧会话状态（状态点 + 会话标题 + SFTP 状态/错误）——会话标题与标签栏同源（本地 = 当前目录末级名，hover 看全路径，见上条 `TerminalTab::title()`），**右下角快捷键提示块已移除**（曾显示 ⌘B/⌘T/⌘W/⌘N/⌥1-3 五个圆角块，用户要求删除）；主题快捷键 ⌥1-3 仍在（勿写 ⌥1-4）
- **持续重绘治理（v0.7）**：egui `Spinner` 内部每帧 `request_repaint` 强制 60fps 全帧重绘，全部替换为 `loading_hint`（app.rs 静态圆点 + 文字，零重绘）：连接等待页/状态栏 SFTP 连接中/更新安装中/SFTP 面板加载中；**toast 降频**：滑入动画期（~0.32s）16ms 重绘，动画结束后仅 `request_repaint_after(剩余时长)` 安排到期关闭帧（曾 4 秒全程 60fps）；**Bell 脉冲降频（v1.1.2）**：0.6s 脉冲只在到期时刻 `request_repaint_after(剩余时长)` 安排一帧，不再每帧 `request_repaint`——omp 任务通知走 BEL，频繁 BEL 会把窗口续成永久 60fps 冻窗
- **性能 HUD（`perf.rs`，v0.7）**：`⌥P` 切换（设置弹窗「关于」卡片也有开关），右上角半透明面板显示帧耗时/FPS/终端构建/布局/绘制分段耗时（滑动平均）；`MinoApp::ui` 开头 `perf.begin_frame()`、末尾 `end_frame()`，活动标签页 `terminal.last_timing()` 喂分段统计；terminal_view 内 `build_start/layout_start/paint_start` 三段打点；**v1.1.2 新增** Shape 数 / 重建行 / 复用行 / GPU 上传字节（`last_stats()`）与「首帧」「终端就绪」启动耗时——帧耗时区分不了「CPU 侧重建」与「GPU 侧上传」，规模计数是渲染优化的唯一可观察证据；**默认开启**（测试 `性能 HUD 启动时应默认展示`），不影响 kittest 截图断言
- **SFTP 面板**（tabby 形式）：`Panel::right("sftp_panel")` **必须是顶层面板（先于 CentralPanel 注册）**——嵌套在 CentralPanel 内时面板状态会被顶层布局污染，面板错位覆盖终端（表现为"面板打不开"）；**默认收起只显示终端，终端右上角悬浮 `sftp_floating_button`（app.rs 顶层函数，"SFTP" 圆角按钮，点击切换 `TerminalTab.sftp_open`）**；展开用官方 `show_collapsible`（滑动动画，收起后右缘保留细拖拽把手可拖开）；尺寸 `default_size(窗口 40%)/min_size(260)/max_size(窗口 50%)`——曾固定 340px（大窗口下偏窄）与 45% 上限（用户要求 40% 默认、50% 上限）；曾 resizable 无上限拖到 ~70% 窗口宽把终端压成窄条，max_size 每帧按 `viewport_rect().width()*0.50` 钳制；面板 frame 内边距 `Margin::symmetric(12, 10)`；**工具栏三行布局**（`sftp_view.rs`）：①标题行=左 `SFTP · 主机名`（truncate 截断占剩余宽）+ 右缘「刷新」按钮（right_to_left 分居两端，曾与「删除」重叠）②操作行=`horizontal_wrapped` 五按钮（上传/下载/新建目录/重命名/删除，空间不足自动换行——曾单行 horizontal 塞全部按钮，340 宽面板溢出右缘被裁剪）③路径行=「上级」按钮 + 路径 truncate 截断（曾长路径溢出被裁）；按钮统一 `sftp_tool_button` 紧凑样式（11.5px 文字 + bg_elevated 底 + 细边框 + RADIUS_ITEM 圆角）；回归测试 `工具栏按钮不截断不重叠`（340 宽内各按钮 rect 不越界且两两不相交）；`poll_sftp` 处理 `Closed` 事件（连接中断时不能停在"连接中…"），失败写入 `sftp_error` 字段由状态栏**持久显示**（toast 一闪而过易忽略），下次 `start_connect` 清空；**文件列表列布局**（`sftp_view.rs`）：名称列左对齐、大小/时间列右对齐（均 `.halign(egui::Align::LEFT/RIGHT)` 显式声明，避免依赖默认对齐），`new_child` 子 Ui 需 `spacing_mut().item_spacing.x = 0.0` 归零自动间距（否则子项间默认 8px 叠加导致列位错乱），列间距用 add_space 精确控制，名称 `Label::truncate()` 截断；**`cell_width` 必须用 `'0'` 字符宽（数字等宽字体的真实字宽）而非空格宽**——空格 ≈ 0.25em、数字 ≈ 0.6em，用空格宽估算列宽会导致时间列 "2026-08-14" 被截到 "2026-01"（曾用空格宽 + 11 字符列宽只能容 7 字符数字）；大小/时间列宽 12 字符等宽（足够显示 10 字符日期 + 缓冲），名称列 = 总宽 - 12×2 - 12；回归测试 `文件列表列对齐`、`sftp面板默认收起悬浮按钮切换`；kittest 截图布局断言：上传按钮 left > 400（面板在右半侧）、齿轮按钮 right > 400（标签栏最右侧）；**2026-08 面板升级与目录导航**：①标题行加 accent2 状态点（与状态栏同款）②路径改为**圆角"地址条"**（bg_elevated 底 + 细边框 + 内部 truncate）③文件列表**表头行首留 22px 图标位**（`icon_pad`，表头与行共用基准）、行高 22、表头 11px muted ④**".." 行**置顶（点击返回上级目录，文件管理器通用习惯）⑤**行首矢量图标**（`paint_entry_icon`：文件夹 accent2 填充提手+圆角主体、文件细描边轮廓——矢量绘制避免 emoji 字形随字体变化）⑥目录名 text_primary/文件名 text_secondary，选中 accent_soft 底 + 左侧 2px accent 竖条（与主机行一致）、hover 白 8% 叠加 ⑦传输记录用主题语义色（success/danger/accent2）⑧**目录导航 = 单击选中、再次单击已选中的目录进入**（不用 `double_clicked`——egui 多击计数被无关点击污染，双击时灵时不灵）；**行点击必须显式 `ui.interact(row_rect, 稳定 Id, Sense::click())` 且注册在列内容之后**——`allocate_exact_size` 的自动 Id 帧间漂移 + `new_child` 子 Ui 叠加，行点击从未生效（"点击文件夹进不去"的根因，kittest 探针逐步定位：snap_clicked 指向行内容区而非行交互区）；hover 用 `pointer.hover_pos()` 判定（子 Ui 会抢走 `response.hovered()`）；目录进入后 `loading=true` 显示静态加载指示（v0.7 起非 spinner，零持续重绘），相关测试用 `run_steps(6)` 显式步进（`Harness::run()` 会超 max_steps）；回归测试 `单击选中再次单击进入目录`（断言第一次单击只选中无 List 命令、再次单击发 `List("/workspace")`、文件两次单击不导航）、`文件列表列对齐` 已适配 ".." 行（"—" 共 3 个：.. 时间/.. 大小/目录大小）

> 当前 SFTP 交互约定（2026-09-10）：目录和 `..` 行均为单击选中、普通双击导航；传输进度平时只显示标题行右缘的**状态徽标**（点击展开/收起详情卡片）：进行中=accent2 圆底+白色上箭头+总进度弧环、全部完成=success 圆底+白对勾、有失败=danger 圆底+白感叹号；新传输开始自动展开详情，上传全部结束后详情卡片显示「关闭」按钮（只清理已结束的上传）。

## 关键约定

- 集成测试依赖本地测试 sshd：`/usr/sbin/sshd -f /tmp/mino-test-sshd/sshd_config`（端口 2222、公钥认证、含 sftp subsystem）；启动方式 `bash scripts/test-sshd.sh start`
- 配置文件：`~/.config/mino/hosts.toml`（toml 中 enum 用内部标记：`[hosts.auth.Key]`）；**保存走原子写**（先写 `hosts.toml.tmp` 再 rename 覆盖，进程被杀/磁盘满不会截断或清空配置）；**加载失败绝不静默清空**——文件存在但解析失败时先备份为 `hosts.toml.bak` 再按空配置启动并 toast 提示（`MinoApp::new_with_config`）
- **崩溃日志 `~/.config/mino/crash.log`**（`main.rs::crash_log_path`，**追加**写入，保留多次崩溃历史；写失败回落 `/tmp/mino-panic.log`）——Finder/Dock 启动时 stderr 无人接收，闪退现场只能靠文件留存。下次启动 `take_crash_report`（与配置同目录，测试传隔离路径）**归档为 `crash.log.1` 并 toast 提示路径**（归档而非删除：日志是用户报告闪退的唯一证据；归档保证同一个崩溃不反复提示）；测试构建跳过该检查（`#[cfg(not(test))]`），行为由 `崩溃日志取走后归档` 单测覆盖
- **测试绝不能读写用户真实配置**：`MinoApp::new` 使用 `default_config_path()`（用户真实 `~/.config/mino/hosts.toml`），涉及配置读写的测试必须走 `MinoApp::new_with_config(cc, test_config_path("标签"))`（`/tmp/mino-test-config-{tag}-{pid}.toml` 隔离路径）——**曾发生测试直接 save + remove_file 用户真实 hosts.toml：跑一次 `cargo test` 就覆盖并删除用户主机列表一次（表现为"每次更新新版本后主机全部消失"）**
- 应用图标：`assets/icon.png`（**紫青渐变圆角底 + 白色 `>_` 终端提示符，四周留 10% 透明边距**，make-icon.swift 绘制，与应用内动态 logo 同构图）→ `load_icon()` 解码为 IconData → `ViewportBuilder::with_icon`——**eframe 在 macOS 上通过 NSApp 运行时设置 Dock 图标**，无 .app bundle 的 debug 构建也能生效；`.app` 安装版的 Dock 图标由 package-macos.sh 的 mino.icns 提供（同一设计）。**图标必须留透明边距：占满画布的无边距图标会被 macOS Dock 放大显示（比邻图标大一圈）**。**make-icon.swift 必须用 `NSBitmapImageRep` 位图上下文渲染（`NSImage.lockFocus` 在 Retina 屏按 2x 渲染导致输出尺寸翻倍）**
- 字体：Monospace 族 = SF Mono（主）+ **Menlo（符号 fallback）** + STHeiti（CJK）+ egui 默认；Proportional 族 = SF 主 + STHeiti。**SF Mono 缺 `➜`(U+279C)/`❯`(U+276F)/`⚡` 等常用 zsh 提示符符号**，缺字形会被 egui 渲染为 `?` 替换符；Menlo 同为等宽且完整覆盖（宽度一致不漂移），必须排在 CJK fallback 之前。**禁止加载 Apple Color Emoji.ttc**（192MB 彩色位图字体，ab_glyph 无法解析 → egui panic）
- **CJK fallback 后台加载（v1.1.2，`main.rs::CjkFontLoader`）**：中文 `.ttc` 是启动期最大一笔字体 IO（PingFang 78MB / STHeiti Light 55.8MB 全量读入），同步读完明显推迟首帧——改为后台线程读取，就绪后 `ctx.add_font`（`FontPriority::Lowest`）**增量追加**；**不能用「克隆定义 + `set_fonts`」**：`set_fonts` 跨帧比较整份定义（含 TTF 字节）易误判 `==` 吞掉并入（中文"读到了却没装上"的根因）。并入**下一帧** `begin_pass` 才真正重建字体系统，渲染侧因此按**字体定义指纹**在帧开头比对、命中才清行缓存（`terminal_view.rs::font_fingerprint`）——在并入当帧清缓存仍会用旧字体重建，乱码 Galley 被再次缓存且 hash 未变，此后永不重建（启动后登录横幅中文持续乱码的真根因）；平台候选路径见 `cjk_candidates()`
- 主字体装配（SF Mono + Menlo + 界面字体约 10MB）仍同步执行：首帧必须有正确字形
- 提示符 `?➜` 中的 `?` 是 oh-my-zsh robbyrussell 主题 `%1{➜%}` 语法在 zsh 5.9 的真实输出（script 捕获字节流验证：`0x3F E2 9E 9C`），Terminal.app 同样显示，**非 mino 渲染问题，勿尝试"修复"**
- egui 0.36 API 注意：`App::ui` 替代 `update`、`Panel::top/left` 替代 `TopBottomPanel`、`Fonts` 需要 `fonts_mut`、`Event::Key` 无 `text` 字段（Text 独立事件）
- 终端视图使用固定 `focus_id` 管理键盘焦点；对话框打开时自动聚焦首个输入框

## 验证

```bash
 cargo test --workspace -- --test-threads=1   # 单元 + ssh 集成 + sftp 集成 + UI 渲染 + 字体链 + 标签页（含标题栏双击 zoom/拖拽） + 双击交互 + 表单默认值 + 设置弹窗 + scrollback + 完整应用回车 + TOFU 主机密钥校验 + F 键修饰编码 + SFTP 时间换算 + 目录单击选中再击进入 + ssh 快捷菜单 + 中文宽字符列对齐（像素级）+ 标签标题跟随当前目录 + IME候选窗跟随光标 + 终端能力应答（OSC 颜色查询/焦点上报，需 python3）+ 崩溃日志归档 + 后台会话异步挂载 + 写回队列有界 + 死锁回归；注意 sftp/ssh 集成测试需先 `bash scripts/test-sshd.sh start`
cargo clippy --workspace --all-targets   # 零警告
cargo fmt --all
```

## 发布流程

- 版本发布必须先把代码合并到 `main`，再在 `main` 上打 `v<主>.<次>.<补丁>` 标签触发 Release 工作流；禁止在功能/诊断分支上直接打 tag 发布
- 发布门禁三件套必须一致：`Cargo.toml [workspace.package] version` == tag 去掉 `v` == `CHANGELOG.md` 顶部的 `## [版本]` 段落（Release 工作流 `Validate release tag and version` 会强制校验）

**UI 测试必须串行**（`-- --test-threads=1`，CI 与 Release 工作流都这样跑）：并行时 kittest 争抢全局状态（主题静态、图形后端）会出现假失败——曾实测 `拖入文件目录应用路径写入终端` 失败、`超链接分段与点击` 挂起 60s+，串行后全部通过。

窗口圆角为运行时原生效果（kittest 无真实窗口句柄，无法单测断言）；验证方式：`cargo run -p mino-app` 后 `screencapture -l <CGWindowID>` 截窗，四角像素应全透明（RGBA alpha=0）。

**自管 GPU 渲染路径 kittest 覆盖不到**（测试环境无 wgpu 后端 → 走 egui 回退）：须实机验证——`cargo run -p mino-app`，HUD（⌥P）空闲帧 `shapes` 应为个位数、`上传` 接近 0，且实测输入/中文/滚动/大输出渲染正常。
