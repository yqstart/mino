//! 终端视图：cell 渲染、键盘输入转发、滚动。
//!
//! 渲染为**行级增量**：每帧用 `Term::damage()` 拿到终端损坏行集合（行号即显示行号），
//! 只对损坏/新出现/滚入的行重建文本段与 Galley（已布局文本），其余行直接复用缓存
//! Galley 绘制（零扫描、零 layout）。内容未变的帧（PTY 空转、光标闪烁、无输入）仅
//! 绘制已有 Galley。

use std::cmp::Ordering;
use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::term::cell::{Flags, LineLength};
use alacritty_terminal::term::color::Colors;
use alacritty_terminal::term::TermDamage;
use alacritty_terminal::vte::ansi::{Color as AColor, CursorShape, NamedColor, Rgb};
use egui::text::LayoutJob;
use egui::{Color32, FontId, Rect, Stroke, TextFormat, Ui, Vec2};
use mino_core::terminal::keys::{
    self, Key, Mods, MouseButton, MouseEventKind, MouseWheelDirection,
};
use mino_core::terminal::{Session, SessionEvent, TermMode};

/// 行缓存：内容 hash 未变时复用已布局文本（Galley），避免每帧重建。
/// `runs` 与 pixels_per_point 绑定，窗口缩放后需全量失效（见 `show`）。
#[derive(Clone)]
struct RowCache {
    /// 内容指纹（fg+bg+样式+字符；不含光标效果，光标移动不触发重建）。
    hash: u64,
    /// 已布局文本分段（每段按终端列定位绘制，无需整行 layout_job）。
    runs: Vec<CachedRun>,
    /// 背景段（合并相邻相同背景色，含起止列）。
    backgrounds: Vec<BgRect>,
}

/// 缓存的文本分段（列定位 + 已布局 Galley）。
///
/// 每个分段单独 layout 后按终端列 `x = start_col * cell_width` 绘制——
/// 整行一个 LayoutJob 会让 egui 按字体实际 advance 排字：
/// CJK 字形经 fallback 字体实际宽度 ≠ 2× 等宽 cell，
/// 后续字符整体左移，输入越多光标漂移越远。
/// 分段绘制恢复「终端列 = 屏幕列」的不变量。
///
/// 宽字符段严格只含**一个**宽字符（见 `push_or_merge`）：段内多字符仍按字体
/// 实际 advance 排字，CJK 的 1em advance（PingFang/Heiti 13px）比双列
/// （2×cell_width ≈ 16.1px）窄，同段连续排字会重新累积漂移。
#[derive(Clone)]
struct CachedRun {
    /// 起始终端列（含宽字符占用的双列）。
    start_col: usize,
    /// 已布局文本（绘制直接使用，无需 layout_job）。
    galley: std::sync::Arc<egui::Galley>,
    /// OSC8 超链接目标（悬浮手指 + 点击打开；无链接为 None）。
    link: Option<String>,
    /// 下划线变体（SGR 4 系列；Galley 只画单线，变体由 paint 侧矢量补）。
    underline: UnderlineStyle,
    /// 下划线颜色（SGR 58；无则跟前景，需段 fg）。
    underline_color: Option<Color32>,
    /// 段前景（变体线默认色）。
    fg: Color32,
}

/// 宽字符 Galley 缓存键。
///
/// 宽字符段恒为单字符，同一（字符, 影响布局的样式）的 Galley 可跨行跨帧复用，
/// 中文文本字符高度重复，layout 实际只发生一次。粗体不参与——它只经 `fg`
/// 映射到亮色（见 `singleline_job`）；背景不参与——背景由背景段单独绘制。
type WideGlyphKey = (char, Color32, bool, UnderlineStyle, Option<Color32>, bool);

/// 下划线变体（SGR 4 系列；VT 层 `Flags` 的渲染侧映射）。
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum UnderlineStyle {
    /// 无下划线。
    None,
    /// 单下划线（SGR 4）。
    Single,
    /// 双下划线（SGR 4:2，omp 的错误/拼写标记用它）。
    Double,
    /// 波浪线（SGR 4:3，LSP 诊断/拼写错误主流形态）。
    Curly,
    /// 点线（SGR 4:4）与虚线（SGR 4:5）。
    Dotted,
    Dashed,
}

/// 文本段（合并相邻相同前景样式的 cell；`start_col` 为终端列定位用）。
struct Segment {
    start_col: usize,
    text: String,
    fg: Color32,
    bold: bool,
    italic: bool,
    underline: UnderlineStyle,
    strikeout: bool,
    /// SGR 58 下划线颜色（`None` = 跟随前景色）。
    underline_color: Option<Color32>,
    /// 本段是否为宽字符段（CJK/emoji，占双列；恒为单字符，与半角不混排）。
    is_wide: bool,
    /// OSC8 超链接目标（同 URI 的相邻 cell 才合并；点击经 `open_url` 打开）。
    link: Option<String>,
}

/// 背景矩形（合并相邻相同背景色的 cell，含起止列）。
#[derive(Clone)]
struct BgRect {
    start: usize,
    end: usize,
    color: Color32,
}

/// 单行渲染数据（锁内构建，锁外 layout）。
struct LineData {
    hash: u64,
    segments: Vec<Segment>,
    backgrounds: Vec<BgRect>,
}

/// 终端选区中的一个 cell 坐标。
///
/// 行使用 alacritty 的网格坐标而不是当前视口行号，因此用户滚动 scrollback
/// 时，选区仍然绑定在原来的输出内容上。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SelectionPoint {
    grid_line: i32,
    col: usize,
}

impl Ord for SelectionPoint {
    fn cmp(&self, other: &Self) -> Ordering {
        self.grid_line
            .cmp(&other.grid_line)
            .then_with(|| self.col.cmp(&other.col))
    }
}

impl PartialOrd for SelectionPoint {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// 终端鼠标选区。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct TerminalSelection {
    anchor: SelectionPoint,
    focus: SelectionPoint,
}

impl TerminalSelection {
    /// 返回当前行应绘制的选区列范围（右端为 exclusive）。
    fn columns_for_line(self, grid_line: i32, cols: usize) -> Option<(usize, usize)> {
        let (start, end) = if self.anchor <= self.focus {
            (self.anchor, self.focus)
        } else {
            (self.focus, self.anchor)
        };
        if grid_line < start.grid_line || grid_line > end.grid_line {
            return None;
        }
        let (from, to) = if start.grid_line == end.grid_line {
            (start.col, end.col.saturating_add(1))
        } else if grid_line == start.grid_line {
            (start.col, cols)
        } else if grid_line == end.grid_line {
            (0, end.col.saturating_add(1))
        } else {
            (0, cols)
        };
        let from = from.min(cols);
        let to = to.min(cols);
        (from < to).then_some((from, to))
    }
}

/// 终端内容内边距（文本与面板边缘的间距，参照 Terminal.app 观感）。
const PADDING: f32 = 10.0;

/// SSH 的 `window_change` 是异步发送的。连接刚建立或终端刚改变布局时，
/// 远端 shell 可能在收到第一次尺寸通知前就开始输出动态内容（例如 npm 的
/// 进度条）。在尺寸稳定后的几帧内重复发送，避免远端仍按旧宽度换行。
const REMOTE_RESIZE_SYNC_FRAMES: u8 = 8;

/// 终端视图。
pub struct TerminalView {
    session: Session,
    /// 行缓存：网格行号 → 渲染数据（Galley + 背景段 + hash）。
    /// 按网格行号索引：滚动后同一网格行直接命中，无需重建。
    rows_cache: HashMap<i32, RowCache>,
    font_size: f32,
    cell_width: f32,
    cell_height: f32,
    cols: u16,
    rows: u16,
    /// 上次渲染时的 pixels_per_point（Galley 与其绑定，变化需全量失效）。
    last_ppp: f32,
    /// 上次渲染时的主题修订号（主题切换后 Galley/背景均需失效）。
    last_theme_revision: u64,
    /// 宽字符（CJK/emoji）Galley 缓存：同一字符的布局跨行跨帧复用
    /// （宽字符段恒为单字符，定位由终端列决定、与内容无关）。
    wide_glyphs: HashMap<WideGlyphKey, std::sync::Arc<egui::Galley>>,
    focus_id: egui::Id,
    initialized: bool,
    last_mode: TermMode,
    /// 上次上报给 PTY 的窗口焦点状态（`None` = 尚未上报过）。
    ///
    /// 程序用 `DECSET 1004` 打开焦点上报后，终端必须在窗口获得/失去焦点时
    /// 发 `ESC [ I` / `ESC [ O`；缺了它，依赖焦点事件的程序（vim、tmux、
    /// omp 等）会一直以为窗口仍处于上一次的状态。
    last_reported_focus: Option<bool>,
    /// 退格/删除键按下后，下一帧的"空白类" Text 事件应丢弃。
    /// （某些输入法（如微信输入法）退格时会伴随发送空格类文本，
    /// 写入终端表现为"删除键插入空格"；正常字符不受影响）
    suppress_blank_frames: u8,
    /// 当前工作目录跟踪器（供 SFTP 面板快捷定位使用）。
    workdir: crate::workdir::WorkdirTracker,
    /// 执行 `pwd` 前的终端可见行，用于从后续屏幕变化中提取实际目录。
    pwd_output_rows: Option<Vec<String>>,
    /// 远程会话的初始目录（由 SFTP realpath(".") 提供）。
    remote_home: Option<std::path::PathBuf>,
    /// 上一帧终端是否持有焦点（焦点自动恢复用）。
    had_focus: bool,
    /// 分段耗时打点（性能 HUD 读数；默认不共享，仅本视图内部使用）。
    last_build_ms: f32,
    last_layout_ms: f32,
    last_paint_ms: f32,
    /// 会话标题缓存（`SessionEvent::Title` 时更新，避免每帧 Mutex + String clone）。
    cached_title: String,
    /// Bell 脉冲到期时间（`SessionEvent::Bell` 时记录；paint 时若未到期则在
    /// 终端左上角画一个短暂圆点，omp 的任务完成/错误提示音不再静默丢失）。
    bell_until: Option<std::time::Instant>,
    /// 远程 PTY 尺寸同步重试次数（`window_change` 由 SSH 后台异步发送）。
    remote_resize_sync_frames: u8,
    /// 当前终端选区（⌘C / Ctrl+Shift+C 复制）。
    selection: Option<TerminalSelection>,
    /// 是否正在进行鼠标拖选。
    selecting: bool,
    /// 复制后的短暂反馈 chip 到期时间。
    copy_flash_until: Option<f64>,
    /// 图片粘贴失败信息（下一帧 `toast` 显示一次；`TerminalView` 无 toast 通道）。
    image_paste_error: Option<String>,
    /// OSC52 剪贴板写入失败信息（下一帧 `toast` 显示一次，同上通道）。
    /// 远程图片粘贴待上传（本地中转路径；`MinoApp` 经 SFTP 上传后写远端 token）。
    pending_image_upload: Option<std::path::PathBuf>,
    clipboard_write_error: Option<String>,
    /// 剪贴板图片读取器（正式为系统剪贴板；测试注入 `Fake`）。
    clipboard: Box<dyn crate::clip_image::ClipboardReader>,
    /// 当前 IME 预编辑文本（拼音/注音组字中、尚未上屏的组合串）。
    ///
    /// 真机链路：egui 每帧把本视图输出的 `PlatformOutput::ime` 转成
    /// `winit::Window::set_ime_allowed(true)` 后，输入法才会激活；
    /// 用户组字（拼音→候选）期间 OS 只发 `ImeEvent::Preedit`（不产生
    /// `Key`/`Text`），选词上屏时才发一次 `ImeEvent::Commit`。
    /// 该串不写入 PTY、只做内联渲染（光标处下划线）；`Commit` 到达后
    /// 才真正写入终端。无预编辑时为 `None`。
    ime_preedit: Option<ImePreedit>,
}

/// 内联 IME 预编辑状态（终端光标处的组字串）。
#[derive(Clone, Debug, Default)]
struct ImePreedit {
    /// 组字串全文（`Preedit(text)` 最新一次的值）。
    text: String,
    /// 输入法给出的活跃区间（字符下标；绘制时加粗/高亮该区间）。
    active_range: Option<std::ops::Range<usize>>,
}

impl TerminalView {
    /// 创建终端视图并启动本地会话。
    pub fn new(session: Session) -> Self {
        let is_remote = session.is_remote();
        // 本地会话初始工作目录：会话启动目录（HOME）。
        let cwd = if is_remote {
            std::path::PathBuf::from("/")
        } else {
            std::env::var("HOME")
                .map(std::path::PathBuf::from)
                .unwrap_or_default()
        };
        // 会话标题初值（一次 Mutex；后续由 Title 事件增量更新）。
        let cached_title = session.title();
        Self {
            session,
            rows_cache: HashMap::new(),
            font_size: 13.0,
            cell_width: 8.0,
            cell_height: 16.0,
            // 真实尺寸要等到首帧布局后才能从 egui 区域计算出来；不要把
            // SSH 建连时的 80x24 初始值误当成已经同步的窗口尺寸。
            cols: 0,
            rows: 0,
            last_ppp: 0.0,
            last_theme_revision: crate::theme::theme_revision(),
            wide_glyphs: HashMap::new(),
            focus_id: egui::Id::new("terminal_view"),
            initialized: false,
            last_mode: TermMode::NONE,
            last_reported_focus: None,
            suppress_blank_frames: 0,
            workdir: crate::workdir::WorkdirTracker::new(cwd),
            pwd_output_rows: None,
            remote_home: None,
            had_focus: false,
            last_build_ms: 0.0,
            last_layout_ms: 0.0,
            last_paint_ms: 0.0,
            cached_title,
            bell_until: None,
            remote_resize_sync_frames: if is_remote {
                REMOTE_RESIZE_SYNC_FRAMES
            } else {
                0
            },
            selection: None,
            selecting: false,
            copy_flash_until: None,
            image_paste_error: None,
            pending_image_upload: None,
            clipboard_write_error: None,
            clipboard: Box::new(crate::clip_image::SystemClipboard::new()),
            ime_preedit: None,
        }
    }

    /// 测试用：注入剪贴板读取器（模拟截图/文件/空剪贴板）。
    #[cfg(test)]
    pub fn set_clipboard_for_test(
        &mut self,
        clipboard: Box<dyn crate::clip_image::ClipboardReader>,
    ) {
        self.clipboard = clipboard;
    }

    /// 取出远程图片粘贴待上传（本地中转路径；`MinoApp` 经 SFTP 上传后写远端 token）。
    pub fn take_pending_image(&mut self) -> Option<std::path::PathBuf> {
        self.pending_image_upload.take()
    }

    /// 取出图片粘贴失败信息（`MinoApp` 转 `toast` 显示一次）。
    pub fn take_image_paste_error(&mut self) -> Option<String> {
        self.image_paste_error.take()
    }

    /// 取出 OSC52 剪贴板写入失败信息（`MinoApp` 转 `toast` 显示一次）。
    pub fn take_clipboard_write_error(&mut self) -> Option<String> {
        self.clipboard_write_error.take()
    }

    /// 本帧终端渲染分段耗时（性能 HUD 读取；未渲染时均为 0）。
    pub fn last_timing(&self) -> (f32, f32, f32) {
        (self.last_build_ms, self.last_layout_ms, self.last_paint_ms)
    }

    /// 会话引用（供状态栏等读取标题）。
    pub fn session(&self) -> &Session {
        &self.session
    }

    /// 会话标题（缓存，`Title` 事件时更新；避免每帧 Mutex + String clone）。
    pub fn session_title(&self) -> &str {
        &self.cached_title
    }

    /// 当前终端已知的工作目录（供 SFTP 快捷定位使用）。
    pub fn current_directory(&self) -> Option<String> {
        if self.session.is_remote() && self.remote_home.is_none() {
            return None;
        }
        Some(self.workdir.cwd().to_string_lossy().into_owned())
    }

    /// 本地会话的真实工作目录（收藏/标题/对话框用）。
    ///
    /// 以 shell 子进程的内核 cwd 为准（`Session::child_current_dir`）：
    /// 输入跟踪（`WorkdirTracker`）在粘贴/补全/别名/函数/`cd -` 等场景下
    /// 会 `invalidate` 并永久停在旧值，不能作为"当前目录"的数据源；
    /// 内核查询偶发失败（子进程刚 fork 间隙）时才回退到跟踪值。
    /// 远程会话返回 `None`。
    pub fn effective_local_directory(&self) -> Option<String> {
        if self.session.is_remote() {
            return None;
        }
        self.session
            .child_current_dir()
            .map(|path| path.to_string_lossy().into_owned())
            .or_else(|| self.tracked_directory())
    }

    /// 输入跟踪器维护的目录（仅本地标题的内核查询失败时回退用）。
    pub fn tracked_directory(&self) -> Option<String> {
        Some(self.workdir.cwd().to_string_lossy().into_owned())
    }
    /// 测试用：直接访问工作目录跟踪器（模拟 Tab/粘贴后的失效态）。
    #[cfg(test)]
    pub fn workdir_for_test(&mut self) -> &mut crate::workdir::WorkdirTracker {
        &mut self.workdir
    }
    /// SFTP 定位前调用：当前输入行为空时向 shell 注入一条 `pwd` 并等待输出。
    ///
    /// 返回 true 表示已注入 `pwd`（调用方应等待若干帧后的定位结果，不要
    /// 立即用旧的推测目录导航）；false 表示此刻不适合自动探测（已有未
    /// 完成的探测/手输 pwd、全屏应用占用终端、当前有未执行的输入），
    /// 调用方应直接回退到已知目录。
    pub fn request_fresh_pwd(&mut self) -> bool {
        if self.workdir.awaiting_pwd_output() || !self.workdir.begin_auto_pwd() {
            return false;
        }
        if !self.workdir_input_is_idle() || self.is_fullscreen_app() {
            self.workdir.cancel_auto_pwd();
            return false;
        }
        // 与用户在空提示符下手输 `pwd\r` 的字节流一致；终端随后走已有的
        // `awaiting_*_pwd_output` 输出校正管线把目录更新到真实值。
        self.session.write(b"pwd\n");
        self.workdir.reset();
        self.workdir.begin_auto_pwd();
        true
    }

    /// 自动 `pwd` 探测是否已拿到终端输出（SFTP 定位轮询用）。
    pub fn auto_pwd_ready(&self) -> bool {
        !self.workdir.awaiting_auto_pwd_output()
    }

    /// 取消未完成的自动 `pwd` 探测（定位超时 / 面板切到无终端上下文时）。
    pub fn cancel_fresh_pwd(&mut self) {
        self.workdir.cancel_auto_pwd();
    }

    /// 测试用：向工作目录跟踪器注入可见文本（模拟用户正在编辑命令行）。
    #[cfg(test)]
    pub fn push_workdir_text_for_test(&mut self, text: &str) {
        self.workdir.push_text(text);
    }

    /// 当前输入行是否为空（没有任何等待执行的字符）。
    ///
    /// 定位注入 `pwd` 必须在空提示符下进行，否则会污染用户正在编辑的
    /// 命令行。跟踪器只记录“可观察到的输入”，Tab/粘贴/方向键等已让它
    /// 失效——失效本身不代表输入行为空，这里只能做保守判断：
    /// 跟踪器有效且文本为空时才认为空闲。
    fn workdir_input_is_idle(&self) -> bool {
        self.workdir.input_is_idle()
    }

    /// 终端是否被全屏应用占用（vim/less/top 等）。
    ///
    /// 此时注入 `pwd` 会变成应用的按键而不是 shell 命令；SFTP 定位应
    /// 直接回退到已知目录，等用户退出全屏应用后再定位。
    fn is_fullscreen_app(&self) -> bool {
        let term_arc = self.session.term();
        let guard = term_arc.lock();
        guard.mode().contains(TermMode::ALT_SCREEN)
    }

    /// 设置远程会话的初始工作目录，不覆盖已经由终端输入跟踪到的目录。
    pub fn set_remote_current_directory(&mut self, path: &str) {
        if path.is_empty() {
            return;
        }
        let cwd = std::path::PathBuf::from(path);
        self.remote_home = Some(cwd.clone());
        self.workdir.set_cwd_if_unmodified(cwd);
    }

    /// 轮询后台事件但不渲染终端。
    ///
    /// 应用层会对所有存活标签调用它，避免非活动标签长期不消费事件；
    /// 当前活动标签随后进入 `show` 时会再次轮询但不会重复处理。
    pub fn drain_background_events(&mut self) {
        for event in self.session.drain_events() {
            match event {
                SessionEvent::PtyWrite(text) => self.session.write(text.as_bytes()),
                SessionEvent::Title(title) => self.cached_title = title,
                SessionEvent::ResetTitle => self.cached_title.clear(),
                // Bell：程序完成/错误提示音。egui 没有声音通道，转为 0.6s
                // 的视觉脉冲（左上角圆点）+ 立即重绘；无脉冲时不常驻重绘。
                SessionEvent::Bell => {
                    let until = std::time::Instant::now() + std::time::Duration::from_millis(600);
                    self.bell_until =
                        Some(self.bell_until.map(|prev| prev.max(until)).unwrap_or(until));
                }
                // 程序查询终端配色（OSC 4/10/11/12）：VT 仿真层不知道主题，
                // 必须由这里给出真实颜色，否则查询永无应答，TUI 只能按
                // “未知终端”回退（omp 启动时就会查 OSC 11）。
                SessionEvent::ColorRequest { index, formatter } => {
                    let color = self.query_color(index);
                    let reply = formatter(color);
                    self.session.write(reply.as_bytes());
                }
                // OSC52 程序复制：写系统剪贴板（omp 的 yank/复制代码块都走这里）。
                // 失败不静默：记入 `clipboard_write_error`，由 `MinoApp` 转 toast。
                SessionEvent::ClipboardStore { text, .. } => {
                    if let Err(message) = self.clipboard.set_clipboard_text(&text) {
                        self.clipboard_write_error = Some(message);
                    }
                }
                // OSC52 程序读剪贴板：默认配置拒绝，几乎不到达；到达则把当前
                // 剪贴板文本按程序要的格式回写（formatter 已含终止符）。
                SessionEvent::ClipboardLoad { formatter, .. } => {
                    let text = self.clipboard.clipboard_text().unwrap_or_default();
                    let reply = formatter(&text);
                    self.session.write(reply.as_bytes());
                }
                // 文本区像素尺寸查询（CSI 14 t）：用实际 cell 尺寸换算。
                SessionEvent::TextAreaSizeRequest(formatter) => {
                    let size = alacritty_terminal::event::WindowSize {
                        num_lines: self.rows,
                        num_cols: self.cols,
                        cell_width: self.cell_width.max(1.0).round() as u16,
                        cell_height: self.cell_height.max(1.0).round() as u16,
                    };
                    let reply = formatter(size);
                    self.session.write(reply.as_bytes());
                }
                _ => {}
            }
        }
    }

    /// 解析终端查询的颜色索引为真实 RGB（与渲染层同一套优先级）。
    ///
    /// 优先级与 `resolve_color` 一致：OSC 动态覆盖 > 内置主题调色板。
    /// 索引语义见 alacritty `term::color`：0-255 调色板、256 前景、
    /// 257 背景、258 光标；更远的 Dim/Bright 变体用 256 色表兜底。
    fn query_color(&self, index: usize) -> Rgb {
        // `Colors` 只实现越界即 panic 的 `Index`，查询索引由终端程序控制，
        // 必须先做边界检查（269 = alacritty 的 Colors::COUNT）。
        let stored = {
            let term = self.session.term();
            let guard = term.lock();
            let colors = guard.colors();
            (index < alacritty_terminal::term::color::COUNT).then(|| colors[index])
        };
        if let Some(Some(rgb)) = stored {
            return rgb;
        }
        let theme = crate::theme::current_theme();
        match index {
            256 => theme.term_fg,
            257 => theme.term_bg,
            258 => theme.term_cursor,
            0..=255 => crate::theme::xterm256(index as u8, theme.term_palette),
            // 越界（Dim/Bright 变体）：回落到默认前景，避免给出伪造颜色。
            _ => theme.term_fg,
        }
    }

    /// 每帧渲染入口。
    pub fn show(&mut self, ui: &mut Ui) {
        self.show_with_input(ui, true);
    }

    /// 渲染终端，并按需禁用键盘、鼠标与滚轮输入。
    ///
    /// 前台弹窗打开时终端仍需持续渲染后台输出，但不能直接读取
    /// egui 全局输入事件，否则位于终端坐标范围内的弹窗滚轮会穿透。
    pub fn show_with_input(&mut self, ui: &mut Ui, input_enabled: bool) {
        let ctx = ui.ctx().clone();
        let term_arc = self.session.term();

        // 主题会改变默认前景、基本色和终端背景；旧 Galley 与背景段不能跨主题复用。
        let theme_revision = crate::theme::theme_revision();
        if self.last_theme_revision != theme_revision {
            self.rows_cache.clear();
            self.last_theme_revision = theme_revision;
        }

        // 终端区域背景（当前主题的终端色）。
        // 注意：用 max_rect（布局分配区域）而非 min_rect（已用内容包围盒，
        // 无子项时为 0x0，会导致背景画不出来）。
        let theme = crate::theme::current_theme();
        let term_bg = theme.term_bg;
        let outer = ui.max_rect();
        ui.painter().rect_filled(
            outer,
            0.0,
            Color32::from_rgb(term_bg.r, term_bg.g, term_bg.b),
        );
        // 低对比网格：提供科技感的空间层次，但不干扰终端文本。
        let grid_step = 32.0;
        let grid_color = crate::theme::tokens::GRID_LINE;
        let first_x = outer.left() - outer.left().rem_euclid(grid_step);
        let first_y = outer.top() - outer.top().rem_euclid(grid_step);
        for x in (0..=((outer.width() / grid_step).ceil() as usize + 1))
            .map(|i| first_x + i as f32 * grid_step)
        {
            ui.painter().line_segment(
                [egui::pos2(x, outer.top()), egui::pos2(x, outer.bottom())],
                egui::Stroke::new(1.0, grid_color),
            );
        }
        for y in (0..=((outer.height() / grid_step).ceil() as usize + 1))
            .map(|i| first_y + i as f32 * grid_step)
        {
            ui.painter().line_segment(
                [egui::pos2(outer.left(), y), egui::pos2(outer.right(), y)],
                egui::Stroke::new(1.0, grid_color),
            );
        }
        // 终端内容区域：背景铺满面板，文本/光标在内边距内绘制。
        let inner = outer.shrink(PADDING);

        // ==================== 事件泵 ====================
        // 诊断：PTY 读取线程退出会导致输入写入失效。
        if self.session.pty_thread_finished() {
            log::warn!("PTY 读取线程已退出！输入将无法写入终端。");
        }
        // 注意：Wakeup 不再在此处二次 request_repaint——mino-core 的
        // `Listener::send_event` 已在事件到达时直接调过 on_event
        // （app.rs 的 `ctx.request_repaint()`），此处仅处理 PtyWrite 回写
        // 与标题缓存更新。
        self.drain_background_events();

        // ==================== 工作目录校正 ====================
        // 目录跟踪通常只需处理键盘输入；只有执行 pwd、等待其输出时才读取
        // 可见网格，避免为了一个低频兜底路径让每个空闲帧都扫描整个终端。
        let has_enter = input_enabled
            && ui.input(|input| {
                input.events.iter().any(|event| {
                    matches!(
                        event,
                        egui::Event::Key {
                            key: egui::Key::Enter,
                            pressed: true,
                            ..
                        }
                    )
                })
            });
        let should_observe_output = self.workdir.awaiting_any_pwd_output() || has_enter;
        let output_rows = should_observe_output.then(|| visible_terminal_rows(&self.session));
        if self.workdir.awaiting_any_pwd_output() {
            if let Some(current) = output_rows.as_deref() {
                if let Some(previous) = self.pwd_output_rows.take() {
                    let corrected = if self.session.is_remote() {
                        self.workdir.observe_remote_output(&previous, current)
                    } else {
                        self.workdir.observe_local_output(&previous, current)
                    };
                    if !corrected && self.workdir.awaiting_any_pwd_output() {
                        // 命令回显和命令输出可能跨多个帧到达；每次继续
                        // 以前一帧作为基线，避免漏掉后续被改写的行。
                        self.pwd_output_rows = Some(current.to_vec());
                    }
                } else {
                    self.pwd_output_rows = Some(current.to_vec());
                }
            }
        }

        // ==================== 尺寸计算与 resize ====================
        // cell 尺寸只依赖字体（启动时加载），缓存到字段避免每帧 fonts_mut。
        let ppp = ui.ctx().pixels_per_point();
        if self.cell_width == 0.0 || ppp != self.last_ppp {
            self.last_ppp = ppp;
            let (cell_width, cell_height) = ui.fonts_mut(|f| {
                let font = FontId::monospace(self.font_size);
                // 空格在部分等宽字体中比数字窄；终端列宽按真实等宽
                // 字符测量，否则本地列数会偏大，远端动态输出会错位换行。
                (f.glyph_width(&font, '0'), f.row_height(&font))
            });
            self.cell_width = cell_width;
            self.cell_height = cell_height;
            // Galley 与 pixels_per_point 绑定：缩放变化后旧布局失效，全量重建。
            self.rows_cache.clear();
            self.wide_glyphs.clear();
        }
        let cell_width = self.cell_width;
        let cell_height = self.cell_height;

        let avail = inner.size();
        let cols = ((avail.x / cell_width).floor() as usize).max(2);
        let rows = ((avail.y / cell_height).floor() as usize).max(1);
        let size_changed = cols as u16 != self.cols || rows as u16 != self.rows;
        if size_changed {
            self.cols = cols as u16;
            self.rows = rows as u16;
            // 通知 PTY 并同步终端状态机网格（Session::resize 内部完成锁内 resize）。
            self.session.resize(self.cols, self.rows);
            if self.session.is_remote() {
                // 布局变化后重新开始短暂重试窗口，确保 SSH 的异步
                // window_change 在远端下一次动态渲染前到达。
                self.remote_resize_sync_frames = REMOTE_RESIZE_SYNC_FRAMES;
            }
            self.rows_cache.clear();
            // 选区的 grid_line 是建立时的快照，resize 重排网格后可能悬空
            // （复制时越界索引在 release 下会 panic），尺寸变化即放弃选区。
            self.selection = None;
        }
        if !size_changed && self.remote_resize_sync_frames > 0 {
            // 连接初始布局可能与 SSH request_pty 的 80x24 不同；即使本帧
            // 本地尺寸未变，也要把当前尺寸再送几次给远端，覆盖建连/输出
            // 并发时第一次 window_change 被延后的情况。
            self.session.resize(self.cols, self.rows);
            self.remote_resize_sync_frames -= 1;
        }

        // ==================== 构建渲染数据（锁内，行级增量） ====================
        // 只处理损坏行（`Term::damage`，行号 = 显示行号）与尚未缓存的行：
        // 内容未变的帧零遍历、零 layout；滚动只重建滚入的新行。
        let mut lines_data: Vec<(i32, LineData)> = Vec::new();
        let mut cursor_rect: Option<Rect> = None;
        let mut cursor_color: Option<Color32> = None;
        let cursor_shape: CursorShape;
        let display_offset: usize;
        let build_start = std::time::Instant::now();

        {
            let mut guard = term_arc.lock();
            // damage 收集（行号 = 网格行号 + display_offset = 显示行号），
            // 必须在同一持锁内 reset，否则下帧重复返回旧损伤。
            let (full_damage, damaged) = match guard.damage() {
                TermDamage::Full => (true, Vec::new()),
                TermDamage::Partial(iter) => (false, iter.map(|b| b.line).collect::<Vec<_>>()),
            };
            guard.reset_damage();

            let content = guard.renderable_content();
            let colors = content.colors;
            display_offset = content.display_offset;
            let default_fg =
                colors[NamedColor::Foreground].unwrap_or(crate::theme::current_theme().term_fg);
            // 背景色强制跟随主题（忽略 OSC 背景覆盖——zsh 主题常设置深色背景，
            // 会导致浅色主题下终端仍为深色）。
            let default_bg = crate::theme::current_theme().term_bg;
            self.last_mode = content.mode;
            let mode = content.mode;
            let cursor = content.cursor;
            let cursor_style = guard.cursor_style();
            cursor_shape = cursor_style.shape;

            // 光标可见性（含闪烁）。
            let time = ctx.input(|i| i.time);
            let blinking = cursor_style.blinking;
            let cursor_visible = mode.contains(TermMode::SHOW_CURSOR)
                && (!blinking || ((time * 2.0) as u64).is_multiple_of(2));
            if blinking {
                ctx.request_repaint_after(Duration::from_millis(500));
            }

            // 逐显示行：缓存命中（未损坏且 hash 一致）则跳过，否则锁内构建段。
            // 显示行 v ↔ 网格行 Line(v - display_offset)（display_iter 同语义：
            // 每个网格行一个显示行，wrap 续行独立成行）。
            let default_bg_egui = to_egui(default_bg);
            let grid = guard.grid();
            for v in 0..self.rows as usize {
                let grid_line = v as i32 - display_offset as i32;
                let cached = self.rows_cache.get(&grid_line);
                // 未损坏且已缓存：直接复用（不再遍历该行 cell）。
                if !full_damage && !damaged.contains(&v) && cached.is_some() {
                    continue;
                }
                // 锁内读取该网格行构建段与 hash。
                let data = build_line_data(
                    grid,
                    grid_line,
                    self.cols as usize,
                    colors,
                    default_fg,
                    default_bg,
                    default_bg_egui,
                );
                // 内容未变（如光标行被标记损伤但文本没变）：跳过 layout 复用旧 Galley。
                if let Some(c) = cached {
                    if c.hash == data.hash {
                        continue;
                    }
                }
                lines_data.push((grid_line, data));
            }

            // 光标矩形（Block 之外的光标形状）。
            if cursor_visible && cursor.shape != CursorShape::Hidden {
                let (line, col) = (cursor.point.line.0 as usize, cursor.point.column.0);
                // 滚动（查看 scrollback）时视口向上偏移 display_offset 行，
                // 光标网格行号需换算为显示行号；滚出视口则不绘制。
                let disp_line = line.saturating_add(display_offset);
                if disp_line < self.rows as usize && col < self.cols as usize {
                    let color = colors[NamedColor::Cursor]
                        .unwrap_or(crate::theme::current_theme().term_cursor);
                    cursor_color = Some(to_egui(color));
                    cursor_rect = Some(Rect::from_min_size(
                        inner.min
                            + Vec2::new(col as f32 * cell_width, disp_line as f32 * cell_height),
                        Vec2::new(cell_width, cell_height),
                    ));
                }
            }
        }
        self.last_build_ms = build_start.elapsed().as_secs_f32() * 1000.0;

        // ==================== 绘制（锁外） ====================
        let layout_start = std::time::Instant::now();
        // 先为新构建的行做文本布局并写缓存（命中行不进入此循环）。
        // 每个分段独立 layout（单行不换行），绘制时按终端列定位——
        // 避免整行 LayoutJob 的字体实际 advance 累积漂移（CJK 宽字符）。
        // 宽字符段恒为单字符，Galley 按 (字符, 样式) 跨行跨帧复用。
        for (grid_line, data) in &lines_data {
            let mut runs = Vec::with_capacity(data.segments.len());
            for seg in &data.segments {
                let galley = match wide_glyph_key(seg) {
                    Some(key) => {
                        if let Some(cached) = self.wide_glyphs.get(&key) {
                            cached.clone()
                        } else {
                            let galley = layout_segment(ui, seg, self.font_size);
                            self.wide_glyphs.insert(key, galley.clone());
                            galley
                        }
                    }
                    None => layout_segment(ui, seg, self.font_size),
                };
                runs.push(CachedRun {
                    start_col: seg.start_col,
                    galley,
                    link: seg.link.clone(),
                    underline: seg.underline,
                    underline_color: seg.underline_color,
                    fg: seg.fg,
                });
            }
            self.rows_cache.insert(
                *grid_line,
                RowCache {
                    hash: data.hash,
                    runs,
                    backgrounds: data.backgrounds.clone(),
                },
            );
        }
        self.last_layout_ms = layout_start.elapsed().as_secs_f32() * 1000.0;

        let paint_start = std::time::Instant::now();
        let painter = ui.painter();
        let origin = inner.min;
        let selection_bg = Color32::from_rgba_unmultiplied(
            theme.accent.r(),
            theme.accent.g(),
            theme.accent.b(),
            92,
        );
        for v in 0..self.rows as usize {
            let grid_line = v as i32 - display_offset as i32;
            let Some(cache) = self.rows_cache.get(&grid_line) else {
                continue;
            };
            // 背景矩形（行内连续背景段）。
            for bg in &cache.backgrounds {
                let rect = Rect::from_min_size(
                    origin + Vec2::new(bg.start as f32 * cell_width, v as f32 * cell_height),
                    Vec2::new((bg.end - bg.start) as f32 * cell_width, cell_height),
                );
                painter.rect_filled(rect, 0.0, bg.color);
            }
            if let Some(selection) = self.selection {
                if let Some((start, end)) =
                    selection.columns_for_line(grid_line, self.cols as usize)
                {
                    let rect = Rect::from_min_size(
                        origin + Vec2::new(start as f32 * cell_width, v as f32 * cell_height),
                        Vec2::new((end - start) as f32 * cell_width, cell_height),
                    );
                    painter.rect_filled(rect, 2.0, selection_bg);
                }
            }
            // 文本分段：每段按终端列定位绘制（Arc clone 零成本；不再 layout_job）。
            // 分段起点 x = start_col * cell_width——终端列与屏幕列严格对齐，
            // 同段内连续同字宽字符（半角 run / CJK run 不混排）无累积漂移。
            let row_top = origin.y + v as f32 * cell_height;
            for run in &cache.runs {
                let pos = egui::pos2(origin.x + run.start_col as f32 * cell_width, row_top);
                painter.galley(pos, run.galley.clone(), Color32::WHITE);
                // 下划线变体矢量线（SGR 4 系列；Galley 只画 Single，其余在这里补）。
                if run.underline != UnderlineStyle::None && run.underline != UnderlineStyle::Single
                {
                    let width = run.galley.size().x;
                    if width > 0.0 {
                        let color = run.underline_color.unwrap_or(run.fg);
                        let base_y = row_top + cell_height - 2.0;
                        paint_underline_variant(
                            painter,
                            pos.x,
                            base_y,
                            width,
                            run.underline,
                            color,
                        );
                    }
                }
                // OSC8 超链接下划线（accent2 色，1px，基线处；只画有链接的段）。
                if run.link.is_some() {
                    let width = run.galley.size().x;
                    if width > 0.0 {
                        painter.line_segment(
                            [
                                egui::pos2(pos.x, row_top + cell_height - 2.0),
                                egui::pos2(pos.x + width, row_top + cell_height - 2.0),
                            ],
                            Stroke::new(1.0, theme.accent2),
                        );
                    }
                }
            }
        }
        // OSC8 超链接点击：命中链接段则经 `open_url` 打开（浏览器/文件）。
        // 只在未订阅鼠标上报时处理——订阅时点击已透传给程序，链接由程序自己管。
        if input_enabled && !self.last_mode.intersects(TermMode::MOUSE_MODE) {
            self.open_hovered_hyperlink(ui, inner, display_offset);
        }

        // 光标形状绘制（shape 已在锁内读取，无需二次上锁）。
        if let (Some(rect), Some(color)) = (cursor_rect, cursor_color) {
            match cursor_shape {
                CursorShape::Block => {
                    painter.rect_filled(rect, 0.0, color);
                }
                CursorShape::Underline => {
                    painter.line_segment(
                        [
                            rect.left_bottom() + Vec2::new(0.0, -1.0),
                            rect.right_bottom() + Vec2::new(0.0, -1.0),
                        ],
                        Stroke::new(1.5, color),
                    );
                }
                CursorShape::Beam => {
                    painter.line_segment(
                        [rect.left_top(), rect.left_bottom()],
                        Stroke::new(1.5, color),
                    );
                }
                CursorShape::HollowBlock => {
                    painter.rect_stroke(
                        rect,
                        0.0,
                        Stroke::new(1.0, color),
                        egui::StrokeKind::Middle,
                    );
                }
                CursorShape::Hidden => {}
            }
        }
        // Bell 视觉脉冲：到期前在终端左上角画 accent 色圆点（0.6s 自消失）。
        // 脉冲期间每帧安排重绘以保证到期即消失；无 Bell 时零额外重绘。
        if let Some(until) = self.bell_until {
            if std::time::Instant::now() < until {
                painter.circle_filled(
                    origin + Vec2::new(8.0, 8.0),
                    3.0,
                    Color32::from_rgb(theme.accent.r(), theme.accent.g(), theme.accent.b()),
                );
                ui.ctx().request_repaint();
            } else {
                self.bell_until = None;
            }
        }
        // IME 预编辑串内联渲染（光标处、下划线标出组字中文本）。
        self.paint_ime_preedit(ui, inner, cursor_rect);
        // 绘制耗时（背景 rect + 文本 galley + 光标形状）。
        self.last_paint_ms = paint_start.elapsed().as_secs_f32() * 1000.0;

        // 缓存上限：滚动浏览大量历史时防止无限增长，超限只保留当前可见行。
        if self.rows_cache.len() > (self.rows as usize).saturating_mul(4).max(64) {
            let visible: std::collections::HashSet<i32> = (0..self.rows as usize)
                .map(|v| v as i32 - display_offset as i32)
                .collect();
            self.rows_cache.retain(|g, _| visible.contains(g));
        }

        // ==================== 焦点与输入 ====================
        if !self.initialized {
            ui.memory_mut(|m| m.request_focus(self.focus_id));
            self.initialized = true;
        }
        // 焦点自动恢复：egui 0.36 在 Text/Key 事件帧后可能清除焦点
        // （kittest 与部分平台；无事件时保持）。终端曾聚焦且当前无其他
        // 焦点（对话框/输入框等）时恢复，保证输入连续性。
        let has_focus_now = ui.memory(|m| m.has_focus(self.focus_id));
        if !has_focus_now && self.had_focus && ui.memory(|m| m.focused().is_none()) {
            ui.memory_mut(|m| m.request_focus(self.focus_id));
        }
        // 终端是一个整体的键盘控件，Tab/方向键/Esc 都应交给 shell 处理，
        // 不能触发 egui 的控件焦点导航。否则 shell 执行 Tab 后，终端会失去焦点，
        // 紧接的 Ctrl+C 可能被 UI 吞掉。
        let has_terminal_focus = ui.memory(|m| m.has_focus(self.focus_id));
        if has_terminal_focus {
            ui.memory_mut(|m| {
                m.set_focus_lock_filter(
                    self.focus_id,
                    egui::EventFilter {
                        tab: true,
                        horizontal_arrows: true,
                        vertical_arrows: true,
                        escape: true,
                    },
                );
            });
        }
        self.had_focus = has_terminal_focus;
        // 窗口焦点变化上报（DECSET 1004）：程序（vim/tmux/omp 等）开启后
        // 终端必须在获得/失去焦点时发 `ESC [ I` / `ESC [ O`。
        let window_focused = ui.ctx().input(|i| i.focused);
        self.report_focus_change(window_focused);
        // 点击/拖拽区域覆盖整个面板：终端文字不是 egui Label，必须自己维护
        // cell 选区，才能实现 Warp/Terminal.app 习惯的拖选后 ⌘C。
        let surface_rect = ui.max_rect();
        self.handle_dropped_files(ui, surface_rect, input_enabled);
        let response = ui.interact(surface_rect, self.focus_id, egui::Sense::click_and_drag());
        // 程序订阅鼠标上报（1000/1002/1003）时，点击/拖拽/释放直接透传为
        // xterm 序列（omp 的 `/tree`、选择框、滚动都依赖它）；此前全部被
        // 本地选区逻辑吞掉，替代屏里的程序永远收不到点击。SGR（1006）优先。
        // 未订阅时走本地选区（拖选后 ⌘C），行为不变。
        let mouse_mode = self.last_mode;
        let mouse_reporting = mouse_mode.intersects(TermMode::MOUSE_MODE);
        if input_enabled && mouse_reporting {
            self.forward_mouse_events(ui, &response, inner);
        }
        if response.clicked() {
            ui.memory_mut(|m| m.request_focus(self.focus_id));
            // 单击空白处清除旧选区；拖选会在 drag_started 时重新建立选区。
            // 鼠标上报开启时不碰本地选区——点击已透传给程序，本地留选区
            // 只会画出一块程序不知道的高亮。
            if !mouse_reporting {
                self.selection = None;
            }
        }
        if response.drag_started() {
            let start_pos = ui
                .input(|i| i.pointer.press_origin())
                .or_else(|| response.interact_pointer_pos());
            if let Some(pos) = start_pos {
                let point = selection_point_from_screen(
                    pos,
                    inner,
                    cell_width,
                    cell_height,
                    display_offset,
                    self.cols as usize,
                    self.rows as usize,
                );
                self.selection = Some(TerminalSelection {
                    anchor: point,
                    focus: point,
                });
                self.selecting = true;
                ui.memory_mut(|m| m.request_focus(self.focus_id));
            }
        }
        if self.selecting && response.dragged() {
            if let Some(pos) = response.interact_pointer_pos() {
                let point = selection_point_from_screen(
                    pos,
                    inner,
                    cell_width,
                    cell_height,
                    display_offset,
                    self.cols as usize,
                    self.rows as usize,
                );
                if let Some(selection) = &mut self.selection {
                    selection.focus = point;
                }
                ui.ctx().request_repaint();
            }
        }
        if response.drag_stopped() {
            self.selecting = false;
        }
        // 向后端声明 IME 意图：终端聚焦（且前台无弹窗抢夺输入）时允许输入法；
        // 否则禁用，避免中文输入法候选窗跟随一个不接受中文的视图。
        // 声明必须每帧执行——egui-winit 只在 `PlatformOutput::ime` 为 Some
        // 时才调 `Window::set_ime_allowed(true)`，缺了这一帧输入法就起不来。
        self.update_ime_output_with_input(ui, inner, cursor_rect, input_enabled);
        if input_enabled && ui.memory(|m| m.has_focus(self.focus_id)) {
            self.handle_input(ui, inner, output_rows);
        }
    }

    /// 鼠标点击/拖拽/释放上报（xterm 鼠标协议，按下→拖拽→释放）。
    ///
    /// 只有程序 DECSET 订阅后才调用（调用方已判定 `MOUSE_MODE`）：未订阅时
    /// 本地选区逻辑不受影响。坐标用按下/当前位置换算为视口 cell（1-based 由
    /// 编码层处理）；滚轮走既有的 `MouseWheel` 分支，不在这里处理。
    /// 按键在进入时快照一次（`pressed_mouse_button`）：释放帧按键已弹起，
    /// 再读 input 只能拿到“无按下”而误报左键。
    fn forward_mouse_events(&mut self, ui: &Ui, response: &egui::Response, inner: Rect) {
        let mode = self.last_mode;
        if !mode.intersects(TermMode::MOUSE_MODE) {
            return;
        }
        let mods = ui.input(|i| {
            let m = i.modifiers;
            Mods {
                shift: m.shift,
                alt: m.alt,
                ctrl: m.ctrl,
                super_: false,
            }
        });
        // 按下的键在释放帧已读不到：全程用同一快照，释放也用它。
        let pressed = pressed_mouse_button(ui);
        let mut events: Vec<(MouseButton, MouseEventKind, Option<egui::Pos2>)> = Vec::new();
        if response.drag_started() {
            // drag_started 与 clicked 同帧到达（click_and_drag）：按下事件优先，
            // 否则程序只看到拖拽看不到按下，选区类交互起不来。
            let pos = ui
                .input(|i| i.pointer.press_origin())
                .or(response.interact_pointer_pos());
            events.push((pressed, MouseEventKind::Press, pos));
        }
        if response.dragged() {
            events.push((
                pressed,
                MouseEventKind::Drag,
                response.interact_pointer_pos(),
            ));
        }
        if response.drag_stopped() {
            events.push((
                pressed,
                MouseEventKind::Release,
                ui.input(|i| i.pointer.interact_pos()),
            ));
        } else if response.clicked() {
            // 纯点击（无拖拽）：补一次按下+释放；drag_started 分支已处理过
            // 按下的情况不再重复（clicked 会在 drag 结束后也触发一次，
            // 那一次由 drag_stopped 的 Release 覆盖）。
            if !response.drag_started() {
                let pos = response.interact_pointer_pos();
                events.push((pressed, MouseEventKind::Press, pos));
                events.push((pressed, MouseEventKind::Release, pos));
            }
        }
        for (button, kind, pos) in events {
            let Some(pos) = pos.filter(|pos| inner.contains(*pos)) else {
                continue;
            };
            let (column, row) = terminal_cell_from_screen(
                pos,
                inner,
                self.cell_width,
                self.cell_height,
                self.cols as usize,
                self.rows as usize,
            );
            if let Some(bytes) = keys::encode_mouse_click(button, kind, mods, mode, column, row) {
                self.session.write(&bytes);
                // 上报字节同样是“已写入 PTY 的输入”：控制序列进跟踪器只会
                // invalidate 当前行；鼠标事件不应污染 shell 命令行跟踪。
                let _ = bytes;
            }
        }
    }

    /// 按窗口焦点变化向 PTY 上报（`DECSET 1004`，xterm 焦点事件）。
    ///
    /// 只有状态真的变化、且程序已启用焦点上报时才发；首帧只记录基准
    /// 状态——程序是在自己启用之后才开始期待事件，补发历史变化会让它
    /// 收到一个从未发生的「焦点切换」。
    fn report_focus_change(&mut self, focused: bool) {
        match self.last_reported_focus {
            Some(previous) if previous != focused => {
                self.last_reported_focus = Some(focused);
                if self.last_mode.contains(TermMode::FOCUS_IN_OUT) {
                    self.session
                        .write(if focused { b"\x1b[I" } else { b"\x1b[O" });
                }
            }
            None => self.last_reported_focus = Some(focused),
            Some(_) => {}
        }
    }

    /// 每帧向 egui 后端声明本终端的 IME 意图（允许/跟随光标/中断组合）。
    ///
    /// 声明条件：前台输入可用（无设置弹窗等）且终端持有键盘焦点。
    /// 条件不满足时不写 `output.ime`——egui-winit 收到 `None` 即
    /// `set_ime_allowed(false)`，候选窗自动收起，无需手动中断。
    /// 仅测试走 `update_ime_output_with_input` 传参；正式渲染恒为前台可用。
    #[allow(dead_code)]
    fn update_ime_output(&self, ui: &Ui, inner: Rect, cursor_rect: Option<Rect>) {
        self.update_ime_output_with_input(ui, inner, cursor_rect, true);
    }

    /// `update_ime_output` 的可测试内核：`input_enabled` 为 false 时
    /// （设置弹窗等前台模态打开）不声明 IME，避免输入法跟随后台终端。
    fn update_ime_output_with_input(
        &self,
        ui: &Ui,
        inner: Rect,
        cursor_rect: Option<Rect>,
        input_enabled: bool,
    ) {
        if !input_enabled || !ui.memory(|m| m.has_focus(self.focus_id)) {
            return;
        }
        // 候选窗跟随：egui-winit 0.36 的 winit 后端调 `set_ime_cursor_area`
        // 时只用 `IMEOutput.rect`（忽略 `cursor_rect`，见 egui-winit
        // `handle_platform_output_inner`），macOS 经
        // `firstRectForCharacterRange` 拿到的就是这个矩形——`rect` 必须是
        // 光标 cell 级小矩形。曾传整个终端 `inner`，候选窗落在终端左下角一带、
        // 远离实际输入位置。滚出视口（None）时退到首行行首单 cell，
        // 保证输入法窗口仍落在终端区域内、不飘到屏幕角落。
        let cursor_rect = cursor_rect.unwrap_or_else(|| {
            Rect::from_min_size(inner.min, Vec2::new(self.cell_width, self.cell_height))
        });
        // 与 TextEdit 同口径转全局坐标（popup/layer 变换下才与屏幕对齐）。
        let to_global = ui
            .ctx()
            .layer_transform_to_global(ui.layer_id())
            .unwrap_or_default();
        let cursor_global = to_global * cursor_rect;
        ui.ctx().output_mut(|o| {
            o.ime = Some(egui::output::IMEOutput {
                // Terminal 语义：macOS 下输入法候选窗跟随光标、不接管回车
                // 确认行为（Normal 会让部分输入法把回车当确认键吃掉）。
                purpose: egui::IMEPurpose::Terminal,
                rect: cursor_global,
                cursor_rect: cursor_global,
                should_interrupt_composition: false,
            });
        });
    }

    /// IME 预编辑串内联渲染：光标所在 cell 起、下划线标出组字中文本。
    ///
    /// 只读 `self.ime_preedit`（已在 `handle_input` 里由 Preedit 事件更新），
    /// 不触终端锁、不写缓存——组字串不进 PTY、不进回显、不污染行 hash。
    fn paint_ime_preedit(&self, ui: &Ui, inner: Rect, cursor_rect: Option<Rect>) {
        let Some(preedit) = self.ime_preedit.as_ref() else {
            return;
        };
        if preedit.text.is_empty() {
            return;
        }
        let Some(cursor) = cursor_rect else {
            return;
        };
        let _ = inner;
        let theme = crate::theme::current_theme();
        let painter = ui.painter();
        // 组字串底：与终端底色区分、与选中态区分的半透明 accent 底。
        let bg = Color32::from_rgba_unmultiplied(
            theme.accent.r(),
            theme.accent.g(),
            theme.accent.b(),
            56,
        );
        let galley = painter.layout_no_wrap(
            preedit.text.clone(),
            FontId::monospace(self.font_size),
            Color32::from_rgb(theme.term_fg.r, theme.term_fg.g, theme.term_fg.b),
        );
        let rect = Rect::from_min_size(cursor.min, galley.size());
        painter.rect_filled(rect, 2.0, bg);
        painter.galley(rect.min, galley, Color32::WHITE);
        // 活跃区间加粗下划线（输入法标出的当前转换节）；无区间时整串下划线。
        let underline_y = rect.bottom() - 1.0;
        let active = preedit
            .active_range
            .clone()
            .unwrap_or(0..preedit.text.chars().count());
        let chars: Vec<char> = preedit.text.chars().collect();
        let char_w = (rect.width() / chars.len().max(1) as f32).max(1.0);
        let (range, stroke_w) =
            if !preedit.text.is_empty() && (active.start != 0 || active.end != chars.len()) {
                (active, 2.0)
            } else {
                (0..chars.len(), 1.0)
            };
        let from = range.start.min(chars.len());
        let to = range.end.min(chars.len()).max(from);
        if from < to {
            painter.line_segment(
                [
                    egui::pos2(rect.left() + from as f32 * char_w, underline_y),
                    egui::pos2(rect.left() + to as f32 * char_w, underline_y),
                ],
                Stroke::new(stroke_w, theme.accent2),
            );
        }
        // 组字期间持续重绘，保证候选变化/光标闪烁即时反映。
        ui.ctx().request_repaint();
    }

    /// 将拖入终端区域的本地文件、目录或应用路径写入当前会话。
    ///
    /// egui 的原生后端把这三类对象统一表示为 `dropped_files`；有坐标时
    /// 沿用本帧的鼠标位置判断是否落在终端区域。部分 macOS 跨窗口拖放
    /// 不会提供坐标，此时当前窗口的终端是唯一的文本输入区，应接收该路径。
    fn handle_dropped_files(&mut self, ui: &Ui, surface_rect: Rect, input_enabled: bool) {
        if !input_enabled {
            return;
        }

        let paths = ui.input(|input| {
            if let Some(pointer) = input.pointer.hover_pos() {
                if !surface_rect.contains(pointer) {
                    return Vec::new();
                }
            }
            input
                .raw
                .dropped_files
                .iter()
                .map(|file| file.path().to_path_buf())
                .collect::<Vec<_>>()
        });
        let Some(text) = dropped_paths_text(&paths) else {
            return;
        };

        // 路径已经按 shell 语法转义，不需要执行回车；用户可以继续编辑
        // 命令，或在需要时手动按 Enter 执行。
        self.session.write(text.as_bytes());
        self.workdir.push_text(&text);
        self.selection = None;
        ui.memory_mut(|memory| memory.request_focus(self.focus_id));
        // 某些后端的 PTY 回显不会同步触发下一帧，主动安排一次重绘以便
        // 拖放后的路径尽快显示出来。
        ui.ctx().request_repaint();
    }

    /// 处理键盘与鼠标输入（转发到 PTY / 网格滚动）。
    fn handle_input(&mut self, ui: &Ui, inner: Rect, output_rows: Option<Vec<String>>) {
        let session = &self.session;
        let mode = self.last_mode;
        let cell_height = self.cell_height;
        let ctx = ui.ctx().clone();
        // 滚动后需要重绘；不能在 ui.input 闭包内调用 request_repaint
        // （Context 锁已被 input 持有，会自死锁 10 秒后 panic），用 flag 延后。
        let mut need_repaint = false;
        // 本帧输入动作（闭包内只读 self 写入 PTY，闭包外统一更新工作目录跟踪器）。
        let mut actions: Vec<InputAction> = Vec::new();
        // 本帧是否有文本粘贴事件：有则走文本老路，释放键分支不再读剪贴板图片。
        let has_text_paste =
            ui.input(|i| i.events.iter().any(|e| matches!(e, egui::Event::Paste(_))));

        // 检测本帧是否有退格/删除键按下（含上一帧的抑制状态）。
        // 某些输入法（如微信输入法）在退格时会伴随发送"空格类" Text 事件，
        // 写入终端会表现为"删除键插入空格"。
        let backspace_this_frame = ui.input(|i| {
            i.events.iter().any(|e| {
                matches!(
                    e,
                    egui::Event::Key {
                        key: egui::Key::Backspace | egui::Key::Delete,
                        pressed: true,
                        ..
                    }
                )
            })
        });
        // 正常的空格也会同时产生 Key::Space + Text(" ")。退格后若用户立刻
        // 输入空格，不能因为抑制输入法伪事件而把这个真实空格吞掉。
        let explicit_space_this_frame = ui.input(|i| {
            i.events.iter().any(|e| {
                matches!(
                    e,
                    egui::Event::Key {
                        key: egui::Key::Space,
                        pressed: true,
                        ..
                    }
                )
            })
        });
        let suppress_blank_text =
            (self.suppress_blank_frames > 0 || backspace_this_frame) && !explicit_space_this_frame;
        self.suppress_blank_frames = if backspace_this_frame {
            2
        } else {
            self.suppress_blank_frames.saturating_sub(1)
        };

        ui.input(|i| {
            for event in &i.events {
                match event {
                    egui::Event::Key {
                        key,
                        modifiers,
                        pressed,
                        ..
                    } => {
                        // 释放粘贴键且本帧无文本粘贴事件 = 剪贴板是图片（或空）：
                        // egui-winit 只在 `clipboard.get()` 有文本时才发 `Paste`，
                        // 有图无文时只剩释放键。必须在 `!pressed → continue`
                        // 之前特化处理（释放帧 `pressed=false`）。
                        if !*pressed
                            && *key == egui::Key::V
                            && (modifiers.command || modifiers.ctrl)
                            && !has_text_paste
                        {
                            actions.push(InputAction::ImagePasteRequest);
                            continue;
                        }
                        if !*pressed {
                            continue;
                        }
                        // Linux/Windows 终端惯用 Ctrl+Shift+C 复制选区；保留
                        // macOS 的 ⌘C，同时避免把组合键继续送进 shell。
                        if modifiers.ctrl && modifiers.shift && *key == egui::Key::C {
                            actions.push(InputAction::CopySelection);
                            continue;
                        }
                        // ⌘C 是终端复制；没有选区时不向 shell 发送任何字符。
                        if modifiers.command {
                            if *key == egui::Key::C {
                                actions.push(InputAction::CopySelection);
                            }
                            continue;
                        }
                        let mods = Mods {
                            shift: modifiers.shift,
                            alt: modifiers.alt,
                            ctrl: modifiers.ctrl,
                            super_: false,
                        };
                        if let Some(scroll) = scrollback_key(key, *modifiers) {
                            // Shift+PageUp/PageDown 不应发送给 shell，而是作为
                            // 终端窗口的本地 scrollback 翻页。编码层为这两个
                            // 组合返回 None；这里必须真正执行滚动，否则按键
                            // 会变成“既不发数据也不滚动”的无操作。
                            let term_arc = session.term();
                            let mut guard = term_arc.lock();
                            guard.scroll_display(scroll);
                            need_repaint = true;
                            continue;
                        }
                        // Ctrl/Alt 修饰的字母与符号键：直接编码为控制字符/转义前缀
                        // （egui 0.36 的 Text 事件与 Key 事件独立，这里处理并让 Text 事件跳过）。
                        if mods.ctrl || mods.alt {
                            if let Some(k) = map_char_key(key, modifiers.shift) {
                                if let Some(bytes) = keys::encode_key(k, mods, mode) {
                                    session.write(&bytes);
                                    actions.push(InputAction::Bytes(bytes));
                                }
                                continue;
                            }
                        }
                        if let Some(k) = map_special_key(key) {
                            if let Some(bytes) = keys::encode_key(k, mods, mode) {
                                session.write(&bytes);
                                actions.push(InputAction::Bytes(bytes));
                            }
                            continue;
                        }
                    }
                    egui::Event::Text(text) => {
                        // 组字中（有预编辑串）时忽略零散 Text：组字期的拼音字母
                        // 已由 Preedit 接管显示，不应提前写入 PTY 污染命令行；
                        // 选词上屏走 Commit 分支（见下）。无预编辑时走正常路径。
                        if self
                            .ime_preedit
                            .as_ref()
                            .is_some_and(|p| !p.text.is_empty())
                        {
                            continue;
                        }
                        // 退格/删除键伴随的"空白类"文本（输入法产物）丢弃，
                        // 只影响空格/零宽等空白字符，正常输入不受影响。
                        if suppress_blank_text
                            && text.chars().all(|c| c == ' ' || !is_printable_text_char(c))
                        {
                            continue;
                        }
                        // Ctrl/Alt 组合已在 Key 事件处理，跳过避免重复写入。
                        let mods = i.modifiers;
                        if mods.ctrl || mods.alt {
                            continue;
                        }
                        // 过滤不可打印字符（控制符/私有区/零宽字符等）。
                        // 某些输入法或平台在退格等按键时会产生零宽空格（\u{200b}），
                        // 直接写入会在终端插入空格。
                        if !text.chars().all(is_printable_text_char) {
                            continue;
                        }
                        session.write(text.as_bytes());
                        actions.push(InputAction::Text(text.clone()));
                    }
                    egui::Event::Ime(ime) => {
                        // 中文输入法组字管线（见 `ime_preedit` 字段注释）：
                        // Preedit 只存不写（等选词）；Commit 才写入 PTY。
                        match ime {
                            egui::ImeEvent::Preedit {
                                text: preedit,
                                active_range_chars,
                            } => {
                                // 空串 = 取消组字（Esc/切输入法/删光拼音）：
                                // 丢弃未上屏串、不写 PTY、不留痕。
                                if preedit.is_empty() {
                                    self.ime_preedit = None;
                                } else {
                                    self.ime_preedit = Some(ImePreedit {
                                        text: preedit.clone(),
                                        active_range: active_range_chars.clone(),
                                    });
                                }
                                need_repaint = true;
                            }
                            egui::ImeEvent::Commit(commit) => {
                                // 组字结束：先清预编辑显示，再把上屏词写入终端。
                                self.ime_preedit = None;
                                if commit.is_empty() {
                                    need_repaint = true;
                                    continue;
                                }
                                session.write(commit.as_bytes());
                                actions.push(InputAction::Text(commit.clone()));
                                need_repaint = true;
                            }
                            egui::ImeEvent::DeleteSurrounding {
                                before_chars,
                                after_chars,
                            } => {
                                // 终端是 PTY 字节流、无 egui 文本缓冲可删；
                                // 输入法要删的是它自己刚提交的环绕文本，
                                // 转成退格/删除序列发给 shell 才是正确语义。
                                // （不处理会让移动端 Gboard 等删不掉刚上屏的字。）
                                let before = (*before_chars).min(64);
                                let after = (*after_chars).min(64);
                                for _ in 0..before {
                                    session.write(b"\x7f");
                                    actions.push(InputAction::Bytes(vec![0x7f]));
                                }
                                for _ in 0..after {
                                    session.write(b"\x1b[3~");
                                    actions.push(InputAction::Bytes(b"\x1b[3~".to_vec()));
                                }
                                need_repaint = true;
                            }
                            // Enabled/Disabled 在 egui 0.36 已废弃（winit 层忽略），
                            // 无需处理。
                            _ => {}
                        }
                    }
                    egui::Event::Paste(text) => {
                        // 括号粘贴模式（bracketed paste）下包装转义序列。
                        let payload = if mode.contains(TermMode::BRACKETED_PASTE) {
                            bracketed_paste_payload(text)
                        } else {
                            text.clone()
                        };
                        session.write(payload.as_bytes());
                        // 粘贴内容不可逐字节信任（可能含控制序列），模型失效。
                        actions.push(InputAction::Paste);
                    }
                    egui::Event::Copy => {
                        actions.push(InputAction::CopySelection);
                    }
                    egui::Event::MouseWheel {
                        unit,
                        delta,
                        modifiers,
                        ..
                    } => {
                        let Some(pointer) =
                            i.pointer.hover_pos().filter(|pos| inner.contains(*pos))
                        else {
                            continue;
                        };
                        let Some(direction) = mouse_wheel_direction(delta.y) else {
                            continue;
                        };
                        let steps = mouse_wheel_steps(*unit, delta.y);
                        if steps == 0 {
                            continue;
                        }

                        // 终端应用（如 Vim）先于本地 scrollback 取得滚轮：应用打开
                        // DECSET 鼠标上报后，必须收到 xterm 鼠标按键序列才能处理滚动。
                        match wheel_target(mode) {
                            WheelTarget::ApplicationMouse => {
                                let (column, row) = terminal_cell_from_screen(
                                    pointer,
                                    inner,
                                    self.cell_width,
                                    cell_height,
                                    self.cols as usize,
                                    self.rows as usize,
                                );
                                let mods = Mods {
                                    shift: modifiers.shift,
                                    alt: modifiers.alt,
                                    ctrl: modifiers.ctrl,
                                    super_: false,
                                };
                                let Some(bytes) =
                                    keys::encode_mouse_wheel(direction, mods, mode, column, row)
                                else {
                                    continue;
                                };
                                for _ in 0..steps {
                                    session.write(&bytes);
                                    actions.push(InputAction::Bytes(bytes.clone()));
                                }
                            }
                            // 未启用鼠标上报的替代屏应用仍应遵循终端惯例，将滚轮
                            // 映射为方向键（例如未设 mouse=a 的 Vim 或 less）。
                            WheelTarget::AlternateScroll => {
                                let key = match direction {
                                    MouseWheelDirection::Up => Key::Up,
                                    MouseWheelDirection::Down => Key::Down,
                                };
                                let Some(bytes) = keys::encode_key(key, Mods::default(), mode)
                                else {
                                    continue;
                                };
                                for _ in 0..steps {
                                    session.write(&bytes);
                                    actions.push(InputAction::Bytes(bytes.clone()));
                                }
                            }
                            WheelTarget::Scrollback => {
                                let lines = match unit {
                                    egui::MouseWheelUnit::Point | egui::MouseWheelUnit::Line => {
                                        let steps = steps as i32;
                                        if delta.y > 0.0 {
                                            steps
                                        } else {
                                            -steps
                                        }
                                    }
                                    egui::MouseWheelUnit::Page => {
                                        let term_arc = session.term();
                                        let mut guard = term_arc.lock();
                                        if delta.y > 0.0 {
                                            guard.scroll_display(Scroll::PageUp);
                                        } else {
                                            guard.scroll_display(Scroll::PageDown);
                                        }
                                        need_repaint = true;
                                        0
                                    }
                                };
                                if lines != 0 {
                                    let term_arc = session.term();
                                    let mut guard = term_arc.lock();
                                    if modifiers.alt {
                                        if lines > 0 {
                                            guard.scroll_display(Scroll::PageUp);
                                        } else {
                                            guard.scroll_display(Scroll::PageDown);
                                        }
                                    } else {
                                        guard.scroll_display(Scroll::Delta(lines));
                                    }
                                    need_repaint = true;
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
        });

        if need_repaint {
            ctx.request_repaint();
        }

        // 闭包外统一应用输入动作，更新工作目录跟踪器。
        for action in actions {
            self.apply_input_action(action, &ctx);
        }
        if self.workdir.awaiting_any_pwd_output() {
            // `output_rows` 是处理本帧 Enter 之前的屏幕快照，正好作为
            // pwd 输出的基线；这样即使 shell 很快返回，也不会把新结果
            // 误当成旧输出。
            self.pwd_output_rows =
                Some(output_rows.unwrap_or_else(|| visible_terminal_rows(&self.session)));
        } else {
            self.pwd_output_rows = None;
        }
        self.render_copy_feedback(ui);
    }

    /// 应用一帧内的输入动作（写入 PTY 的字节与拦截的按键）。
    fn apply_input_action(&mut self, action: InputAction, ctx: &egui::Context) {
        match action {
            InputAction::Bytes(bytes) => self.track_input_bytes(&bytes),
            InputAction::Text(text) => {
                self.workdir.push_text(&text);
            }
            InputAction::Paste => {
                // 粘贴内容不可逐字节信任（可能包含控制序列），暂停目录跟踪。
                self.workdir.invalidate();
            }
            InputAction::ImagePasteRequest => self.handle_image_paste(),
            InputAction::CopySelection => self.copy_selection(ctx),
        }
    }

    /// 处理图片粘贴请求（释放 `Cmd/Ctrl+V` 且本帧无文本粘贴事件时）。
    ///
    /// 优先级见 `clip_image::paste_image_token`：文本优先（此处已无文本）、
    /// macOS 文件直接用原路径、截图编码 PNG 落盘。本地会话直接写 `@token`；
    /// 远程会话暂存中转路径，由 `MinoApp` 经 SFTP 上传后写远端 token。
    /// 空剪贴板静默无操作；失败记入 `image_paste_error` 由上层 `toast`。
    fn handle_image_paste(&mut self) {
        let result = crate::clip_image::paste_image_token(&mut *self.clipboard, shell_escape_path);
        match result {
            Ok(None) => {}
            Ok(Some(pasted)) => {
                if self.session.is_remote() {
                    // token 暂不写 PTY，等 SFTP `Done` 后写远端路径。
                    // Finder 文件（`staged=None`）同样需上传：token 去掉 `@`
                    // 即本地原路径，走同一套 SFTP 中转链路。
                    let local = pasted.staged.unwrap_or_else(|| {
                        std::path::PathBuf::from(pasted.token.trim_start_matches('@'))
                    });
                    self.pending_image_upload = Some(local);
                } else {
                    self.session.write(pasted.token.as_bytes());
                    self.workdir.push_text(&pasted.token);
                    self.selection = None;
                }
            }
            Err(message) => {
                self.image_paste_error = Some(message);
            }
        }
    }

    /// 写入已生成的粘贴 token（远程图片上传完成、本地文件路径等上层装配后调用）。
    pub fn push_pasted_text(&mut self, token: &str) {
        self.session.write(token.as_bytes());
        self.workdir.push_text(token);
        self.selection = None;
    }

    /// 将当前终端选区交给 egui 平台层写入系统剪贴板。
    fn copy_selection(&mut self, ctx: &egui::Context) {
        let Some(selection) = self.selection else {
            return;
        };
        let term_arc = self.session.term();
        let text = {
            let guard = term_arc.lock();
            selection_to_text(guard.grid(), selection, self.cols as usize)
        };
        if text.is_empty() {
            return;
        }
        ctx.copy_text(text);
        let now = ctx.input(|i| i.time);
        self.copy_flash_until = Some(now + 1.2);
        ctx.request_repaint_after(Duration::from_millis(1200));
    }

    /// 分析写入 PTY 的字节并同步工作目录跟踪器（本地与远程会话）。
    fn track_input_bytes(&mut self, bytes: &[u8]) {
        match bytes {
            // 回车：执行命令并尝试解析 cd，清空当前输入跟踪。
            b"\r" | b"\n" => {
                if self.session.is_remote() {
                    self.workdir.execute_remote(self.remote_home.as_deref());
                } else {
                    self.workdir.execute();
                }
            }
            // Ctrl+C：重置当前行。
            b"\x03" => {
                self.workdir.reset();
            }
            // 退格/删除。
            b"\x7f" | b"\x08" => {
                self.workdir.backspace();
            }
            // Tab（shell 自身补全/移动光标）：输入行可能被 shell 改写，暂停目录跟踪。
            b"\t" => {
                self.workdir.invalidate();
            }
            _ => {
                // 可见文本（ASCII 可打印 / 空格 / 非 ASCII，含 IME Commit 的中文）。
                if let Ok(s) = std::str::from_utf8(bytes) {
                    if s.chars()
                        .all(|c| c.is_ascii_graphic() || c == ' ' || !c.is_ascii())
                    {
                        self.workdir.push_text(s);
                        return;
                    }
                }
                // 控制序列/编辑键（箭头、Ctrl+U/W 等）：光标位置不可追踪，模型失效。
                self.workdir.invalidate();
            }
        }
    }

    /// 复制成功后的非侵入式反馈，不抢终端焦点。
    fn render_copy_feedback(&mut self, ui: &Ui) {
        let Some(until) = self.copy_flash_until else {
            return;
        };
        let now = ui.ctx().input(|i| i.time);
        if now >= until {
            self.copy_flash_until = None;
            return;
        }
        ui.ctx()
            .request_repaint_after(Duration::from_secs_f64((until - now).min(0.2)));
        let theme = crate::theme::current_theme();
        egui::Area::new(egui::Id::new("copy_feedback"))
            .order(egui::Order::Foreground)
            .interactable(false)
            .anchor(egui::Align2::RIGHT_TOP, egui::vec2(-16.0, 16.0))
            .show(ui.ctx(), |ui| {
                egui::Frame::new()
                    .fill(theme.bg_elevated.gamma_multiply(0.96))
                    .stroke(egui::Stroke::new(1.0, theme.accent))
                    .corner_radius(7.0)
                    .inner_margin(egui::Margin::symmetric(10, 6))
                    .show(ui, |ui| {
                        ui.label(
                            egui::RichText::new("COPIED")
                                .monospace()
                                .size(10.0)
                                .color(theme.accent),
                        );
                    });
            });
    }
}

/// 一帧内的终端输入动作（闭包内收集，闭包外统一应用到工作目录跟踪器）。
enum InputAction {
    /// 已写入 PTY 的字节。
    Bytes(Vec<u8>),
    /// 已写入的可见文本（含 IME Commit 上屏的中文）。
    Text(String),
    /// 粘贴（工作目录跟踪器失效）。
    Paste,
    /// 图片粘贴请求（释放粘贴键且本帧无文本粘贴事件；闭包外读剪贴板）。
    ImagePasteRequest,
    /// 复制当前终端选区。
    CopySelection,
}

/// 测试用网格文本读取。
///
/// 非测试构建不需要（正式代码走行级增量缓存不调它），`#[cfg(test)]` 下
/// `dead_code` 误报用 allow 压住——它是给回归测试用的，不是无用代码。
#[cfg(test)]
#[allow(dead_code)]
pub(crate) fn tests_grid_text(session: &Session) -> String {
    use alacritty_terminal::term::cell::Flags;
    let term_arc = session.term();
    let guard = term_arc.lock();
    let content = guard.renderable_content();
    let mut lines: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut started = false;
    let mut prev_grid_line: i32 = i32::MIN;
    for item in content.display_iter {
        let cell = item.cell;
        if cell.flags.contains(Flags::WIDE_CHAR_SPACER) || cell.flags.contains(Flags::HIDDEN) {
            continue;
        }
        if item.point.line.0 != prev_grid_line {
            if started {
                lines.push(current.trim_end().to_string());
            }
            current = String::new();
            started = true;
            prev_grid_line = item.point.line.0;
        }
        current.push(cell.c);
    }
    if started {
        lines.push(current.trim_end().to_string());
    }
    lines.join("\n")
}

// ==================== 辅助函数 ====================

/// 读取终端当前视口的纯文本行（去除 VT 属性与尾随空格）。
///
/// 这里只在 `pwd` 输出校正期间调用；正常渲染仍使用行级损坏缓存，避免
/// 每帧遍历全部 cell。
fn visible_terminal_rows(session: &Session) -> Vec<String> {
    let term_arc = session.term();
    let guard = term_arc.lock();
    let content = guard.renderable_content();
    let mut rows = Vec::new();
    let mut current_line: Option<i32> = None;
    let mut current = String::new();

    for item in content.display_iter {
        let line = item.point.line.0;
        if current_line != Some(line) {
            if current_line.is_some() {
                rows.push(current.trim_end().to_string());
            }
            current_line = Some(line);
            current.clear();
        }
        let cell = item.cell;
        if cell.flags.contains(Flags::WIDE_CHAR_SPACER) || cell.flags.contains(Flags::HIDDEN) {
            continue;
        }
        current.push(cell.c);
    }
    if current_line.is_some() {
        rows.push(current.trim_end().to_string());
    }
    rows
}

/// 滚轮事件的优先目标：全屏应用的鼠标协议优先于本地 scrollback。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WheelTarget {
    ApplicationMouse,
    AlternateScroll,
    Scrollback,
}

fn wheel_target(mode: TermMode) -> WheelTarget {
    if mode.intersects(TermMode::MOUSE_MODE) {
        WheelTarget::ApplicationMouse
    } else if mode.contains(TermMode::ALT_SCREEN | TermMode::ALTERNATE_SCROLL) {
        WheelTarget::AlternateScroll
    } else {
        WheelTarget::Scrollback
    }
}

fn mouse_wheel_direction(delta_y: f32) -> Option<MouseWheelDirection> {
    if delta_y > 0.0 {
        Some(MouseWheelDirection::Up)
    } else if delta_y < 0.0 {
        Some(MouseWheelDirection::Down)
    } else {
        None
    }
}

/// 一次 egui 滚轮事件应转换成多少个离散 xterm 滚轮按键。
///
/// 滚轮是“意图”而非距离：一次滚轮手势应只上报少量按键（如 1)，而不是按
/// 滚动像素距离折算成多次。按像素折算会让 macOS 触控板的 Point 事件
/// （单帧几十像素）一次上报十几次，Vim 里直接翻过几屏、定位不到想看的行。
fn mouse_wheel_steps(unit: egui::MouseWheelUnit, delta_y: f32) -> usize {
    let magnitude = delta_y.abs();
    if !magnitude.is_finite() || magnitude == 0.0 {
        return 0;
    }

    match unit {
        // 离散滚轮刻度：直接按刻度数上报（通常为 1）。
        egui::MouseWheelUnit::Line => magnitude.ceil().clamp(1.0, 3.0) as usize,
        // 触控板/高精度滚轮：一律视为一次手势、只发 1 次，避免惯性滚动刷屏。
        egui::MouseWheelUnit::Point => 1,
        // 整页滚动：只发 1 次，由应用自己决定翻多少。
        egui::MouseWheelUnit::Page => 1,
    }
}

/// 屏幕坐标 → 当前终端视口 cell 坐标（从零开始）。
fn terminal_cell_from_screen(
    pos: egui::Pos2,
    inner: Rect,
    cell_width: f32,
    cell_height: f32,
    cols: usize,
    rows: usize,
) -> (usize, usize) {
    let x = (pos.x - inner.left()).clamp(0.0, inner.width().max(0.0));
    let y = (pos.y - inner.top()).clamp(0.0, inner.height().max(0.0));
    let col = (x / cell_width.max(1.0)).floor() as usize;
    let row = (y / cell_height.max(1.0)).floor() as usize;
    (
        col.min(cols.saturating_sub(1)),
        row.min(rows.saturating_sub(1)),
    )
}

impl TerminalView {
    /// 悬浮在 OSC8 链接段上时：手指光标 + 单击经 `open_url` 打开。
    ///
    /// 只处理左键单击（`clicked_by(Primary)`），且指针必须落在某段链接的
    /// 矩形内；段宽用缓存 Galley 实测宽（与绘制同一宽度），不按字符估算。
    /// `file://` 与 `http(s)://` 都直通 `open_url`（egui-winit 调 `open`）。
    fn open_hovered_hyperlink(&mut self, ui: &Ui, inner: Rect, display_offset: usize) {
        let Some(pointer) = ui.input(|i| i.pointer.hover_pos()) else {
            return;
        };
        if !inner.contains(pointer) {
            return;
        }
        let cell_width = self.cell_width;
        let cell_height = self.cell_height;
        let mut hovered: Option<String> = None;
        for v in 0..self.rows as usize {
            let grid_line = v as i32 - display_offset as i32;
            let Some(cache) = self.rows_cache.get(&grid_line) else {
                continue;
            };
            let row_top = inner.min.y + v as f32 * cell_height;
            for run in &cache.runs {
                let Some(url) = run.link.as_deref() else {
                    continue;
                };
                let width = run.galley.size().x;
                if width <= 0.0 {
                    continue;
                }
                let rect = Rect::from_min_size(
                    egui::pos2(inner.min.x + run.start_col as f32 * cell_width, row_top),
                    egui::vec2(width, cell_height),
                );
                if rect.contains(pointer) {
                    hovered = Some(url.to_owned());
                    break;
                }
            }
            if hovered.is_some() {
                break;
            }
        }
        let Some(url) = hovered else {
            return;
        };
        ui.output_mut(|o| o.cursor_icon = egui::CursorIcon::PointingHand);
        if ui.input(|i| i.pointer.button_clicked(egui::PointerButton::Primary)) {
            ui.ctx().open_url(egui::OpenUrl::same_tab(url));
        }
    }
}

/// 下划线变体矢量线（SGR 4:2/4:3/4:4/4:5；Single 由 Galley 直接画）。
///
/// 双线 = 基线 + 基线-2px 两条 1px；波浪 = 振幅 1px 的 8 段折线（段宽<8px
/// 退化为单线）；点线 = 1px 点 + 2px 空、虚线 = 3px 线 + 2px 空（dash 手工
/// 分段，egui 无虚线 stroke）。颜色由调用方按 SGR58/前景解好传入。
fn paint_underline_variant(
    painter: &egui::Painter,
    x: f32,
    base_y: f32,
    width: f32,
    style: UnderlineStyle,
    color: Color32,
) {
    match style {
        UnderlineStyle::None | UnderlineStyle::Single => {}
        UnderlineStyle::Double => {
            for dy in [0.0, -2.0] {
                painter.line_segment(
                    [
                        egui::pos2(x, base_y + dy),
                        egui::pos2(x + width, base_y + dy),
                    ],
                    Stroke::new(1.0, color),
                );
            }
        }
        UnderlineStyle::Curly => {
            if width < 8.0 {
                painter.line_segment(
                    [egui::pos2(x, base_y), egui::pos2(x + width, base_y)],
                    Stroke::new(1.0, color),
                );
                return;
            }
            let segs = 8;
            let mut prev = egui::pos2(x, base_y);
            for i in 1..=segs {
                let t = i as f32 / segs as f32;
                // 正弦一周期：0→+1→0→-1→0，振幅 1px。
                let dy = (t * std::f32::consts::TAU).sin();
                let next = egui::pos2(x + width * t, base_y + dy);
                painter.line_segment([prev, next], Stroke::new(1.0, color));
                prev = next;
            }
        }
        UnderlineStyle::Dotted => {
            let mut cx = x;
            while cx < x + width {
                let end = (cx + 1.0).min(x + width);
                painter.line_segment(
                    [egui::pos2(cx, base_y), egui::pos2(end, base_y)],
                    Stroke::new(1.0, color),
                );
                cx += 3.0;
            }
        }
        UnderlineStyle::Dashed => {
            let mut cx = x;
            while cx < x + width {
                let end = (cx + 3.0).min(x + width);
                painter.line_segment(
                    [egui::pos2(cx, base_y), egui::pos2(end, base_y)],
                    Stroke::new(1.0, color),
                );
                cx += 5.0;
            }
        }
    }
}

/// egui 按键 → xterm 鼠标 button（从本帧 input 取按下的键；未知按左键）。
fn pressed_mouse_button(ui: &Ui) -> MouseButton {
    let button = ui.input(|i| {
        i.pointer
            .button_pressed(egui::PointerButton::Secondary)
            .then_some(egui::PointerButton::Secondary)
            .or_else(|| {
                i.pointer
                    .button_pressed(egui::PointerButton::Middle)
                    .then_some(egui::PointerButton::Middle)
            })
    });
    match button {
        Some(egui::PointerButton::Secondary) => MouseButton::Right,
        Some(egui::PointerButton::Middle) => MouseButton::Middle,
        _ => MouseButton::Left,
    }
}

/// 屏幕坐标 → 当前视口对应的网格坐标。
fn selection_point_from_screen(
    pos: egui::Pos2,
    inner: Rect,
    cell_width: f32,
    cell_height: f32,
    display_offset: usize,
    cols: usize,
    rows: usize,
) -> SelectionPoint {
    let x = (pos.x - inner.left()).clamp(0.0, inner.width().max(0.0));
    let y = (pos.y - inner.top()).clamp(0.0, inner.height().max(0.0));
    let col = (x / cell_width.max(1.0)).floor() as usize;
    let row = (y / cell_height.max(1.0)).floor() as usize;
    SelectionPoint {
        grid_line: row.min(rows.saturating_sub(1)) as i32 - display_offset as i32,
        col: col.min(cols.saturating_sub(1)),
    }
}

/// 从网格中提取选区文本，遵循 alacritty 的软换行、宽字符和组合字符语义。
fn selection_to_text(
    grid: &alacritty_terminal::grid::Grid<alacritty_terminal::term::cell::Cell>,
    selection: TerminalSelection,
    cols: usize,
) -> String {
    let (start, end) = if selection.anchor <= selection.focus {
        (selection.anchor, selection.focus)
    } else {
        (selection.focus, selection.anchor)
    };
    // 选区是建立时的 grid_line 快照，网格可能因 resize/scrollback 裁剪而缩小；
    // alacritty 的 Storage 越界防护仅 debug_assert，release 下会直接 panic——
    // 复制前校验范围（有效网格行号 = [-history_size, screen_lines)），越界放弃复制。
    let history = grid.history_size() as i32;
    let screen = grid.screen_lines() as i32;
    // cols 来自渲染器的快照；在 resize 或测试/异常调用下可能为 0，或大于当前网格宽度。
    // 后续会访问 Column(cols - 1)，因此必须在索引前拒绝不一致的快照。
    if cols == 0 || cols > grid.columns() || start.grid_line < -history || end.grid_line >= screen {
        return String::new();
    }
    let mut output = String::new();
    for grid_line in start.grid_line..=end.grid_line {
        let Some((mut from, to)) = selection.columns_for_line(grid_line, cols) else {
            continue;
        };
        let row = &grid[if grid_line >= 0 {
            alacritty_terminal::index::Line::from(grid_line as usize)
        } else {
            alacritty_terminal::index::Line::from(0) - grid_line.unsigned_abs() as usize
        }];
        if from < to
            && row[alacritty_terminal::index::Column(from)]
                .flags
                .contains(Flags::WIDE_CHAR_SPACER)
            && from > 0
        {
            // 选中宽字符的第二个 cell 时，把主字符一并纳入复制。
            from -= 1;
        }
        let line_length = row.line_length().0.min(to);
        let mut line = String::new();
        for col in from..line_length {
            let cell = &row[alacritty_terminal::index::Column(col)];
            if cell
                .flags
                .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
            {
                continue;
            }
            if cell.flags.contains(Flags::HIDDEN) {
                line.push(' ');
            } else {
                line.push(cell.c);
                if let Some(zero_width) = cell.zerowidth() {
                    line.extend(zero_width.iter().copied());
                }
            }
        }
        output.push_str(&line);
        if grid_line != end.grid_line
            && !row[alacritty_terminal::index::Column(cols - 1)]
                .flags
                .contains(Flags::WRAPLINE)
        {
            output.push('\n');
        }
    }
    output
}

/// 锁内构建单个网格行的渲染数据（段 + 背景 + hash）。
///
/// `grid_line` 为网格行号（滚动到 scrollback 时为负）。复用 `display_iter` 的
/// 单行语义：占位格跳过、颜色解析、背景段合并、文本段合并。
/// 注意：不再对光标 cell 做反色——Block 光标最终由光标色实心矩形覆盖，反色不可见，
/// 剔除后光标行内容 hash 稳定，光标移动不触发行重建。
///
/// 列定位关键：`Segment.start_col` 记录段首终端列；宽字符（CJK/emoji，占双列）
/// 与半角字符**不混排**——宽字符起新段，后续半角另起新段。
/// 这样同段内所有字符字宽一致（半角 1 列 / 宽字符 2 列），分段绘制时
/// `x = start_col * cell_width` 即精确对齐，无累积漂移。
/// （历史 bug：整行一个 LayoutJob 让 egui 按 fallback 字体实际 advance 排字，
/// CJK 实际宽度 ≠ 2×cell，后续字符整体左移，光标越打越远。）
#[allow(clippy::too_many_arguments)]
fn build_line_data(
    grid: &alacritty_terminal::grid::Grid<alacritty_terminal::term::cell::Cell>,
    grid_line: i32,
    cols: usize,
    colors: &Colors,
    default_fg: Rgb,
    default_bg: Rgb,
    default_bg_egui: Color32,
) -> LineData {
    let mut segments: Vec<Segment> = Vec::new();
    let mut backgrounds: Vec<BgRect> = Vec::new();
    let mut hash: u64 = 0;
    // `Line` 的 tuple 构造器不公开，负行号（scrollback）用 `Line(0) - n` 构造。
    let row = &grid[if grid_line >= 0 {
        alacritty_terminal::index::Line::from(grid_line as usize)
    } else {
        alacritty_terminal::index::Line::from(0) - grid_line.unsigned_abs() as usize
    }];

    for (col, cell) in row.into_iter().enumerate().take(cols) {
        // 解析颜色（含粗体 → 亮色映射）。
        let mut fg = resolve_color(
            cell.fg,
            colors,
            default_fg,
            cell.flags.contains(Flags::BOLD),
        );
        let mut bg = resolve_color(cell.bg, colors, default_bg, false);
        let bold = cell.flags.contains(Flags::BOLD);
        let italic = cell.flags.contains(Flags::ITALIC);
        let underline = underline_style_of(cell.flags);
        let strikeout = cell.flags.contains(Flags::STRIKEOUT);
        // SGR 58 下划线颜色（`Color::Spec` 直接 RGB，其余走调色板解析）。
        let underline_color = cell.underline_color().map(|c| match c {
            AColor::Spec(rgb) => to_egui(rgb),
            other => resolve_color(other, colors, default_fg, false),
        });

        // INVERSE 反色。
        if cell.flags.contains(Flags::INVERSE) {
            std::mem::swap(&mut fg, &mut bg);
        }
        // DIM 减暗（粗体不减）。
        if cell.flags.contains(Flags::DIM) && !bold {
            fg = Color32::from_rgb(fg.r() / 2, fg.g() / 2, fg.b() / 2);
        }

        // 背景段合并（默认背景不绘制）。
        push_background(&mut backgrounds, col, bg, default_bg_egui);

        let leading_spacer = cell.flags.contains(Flags::LEADING_WIDE_CHAR_SPACER);
        let wide_spacer = cell.flags.contains(Flags::WIDE_CHAR_SPACER);
        if wide_spacer && !leading_spacer {
            // 普通宽字符占位格由前一个宽字符的 glyph 提供视觉宽度，
            // 不再追加文本，但必须进入哈希以跟踪其背景/属性变化。
            mix_cell_hash(
                &mut hash,
                cell.c,
                CellStyle {
                    fg,
                    bg,
                    bold,
                    italic,
                    underline,
                    strikeout,
                    underline_color,
                },
                cell.zerowidth(),
            );
            continue;
        }

        // 文本段合并（宽字符起新段：字宽与半角不同，不混排）。
        let text = if cell.flags.contains(Flags::HIDDEN) || leading_spacer {
            ' '
        } else {
            cell.c
        };
        let zero_width = if cell.flags.contains(Flags::HIDDEN) || leading_spacer {
            None
        } else {
            cell.zerowidth()
        };
        // 主宽字符（WIDE_CHAR 标志）占双列：后续半角另起新段。
        // 行尾换行占位（LEADING）与隐藏字符视作半角空格宽度。
        let is_wide = !leading_spacer
            && !cell.flags.contains(Flags::HIDDEN)
            && cell.flags.contains(Flags::WIDE_CHAR);
        // OSC8 超链接：同 URI 才合并（`push_or_merge` 判 link 相等）。
        let link = cell
            .hyperlink()
            .map(|h| h.uri().to_owned())
            .filter(|u| !u.is_empty());
        if let Some(url) = link.as_deref() {
            hash = hash.wrapping_mul(131).wrapping_add(url.len() as u64);
            for b in url.bytes() {
                hash = hash.wrapping_mul(131).wrapping_add(b as u64);
            }
        }
        push_or_merge(
            &mut segments,
            col,
            text,
            zero_width,
            is_wide,
            CellStyle {
                fg,
                bg,
                bold,
                italic,
                underline,
                strikeout,
                underline_color,
            },
            link,
            &mut hash,
        );
    }

    LineData {
        hash,
        segments,
        backgrounds,
    }
}

/// 追加一个背景 cell；只有颜色相同且列号紧邻时才允许合并。
fn push_background(backgrounds: &mut Vec<BgRect>, col: usize, color: Color32, default_bg: Color32) {
    if color == default_bg {
        return;
    }
    if let Some(last) = backgrounds.last_mut() {
        if last.color == color && last.end == col {
            last.end = col + 1;
            return;
        }
    }
    backgrounds.push(BgRect {
        start: col,
        end: col + 1,
        color,
    });
}

/// 解析终端颜色为 egui 颜色（Catppuccin 调色板 + xterm 256 色表）。
///
/// 优先级：程序直接指定颜色（Spec）> OSC 动态覆盖（term.colors）> 内置调色板。
fn resolve_color(color: AColor, colors: &Colors, default: Rgb, bold: bool) -> Color32 {
    match color {
        AColor::Spec(rgb) => to_egui(rgb),
        AColor::Named(n) => {
            // 背景始终用主题色（OSC 11 背景覆盖不生效——zsh 主题常设深色背景，
            // 否则浅色主题下终端仍为深色）。
            if n == NamedColor::Background {
                return to_egui(crate::theme::current_theme().term_bg);
            }
            // OSC 覆盖优先（其余颜色仍尊重终端程序动态改色）。
            if let Some(rgb) = colors[n as usize] {
                return to_egui(rgb);
            }
            match n {
                NamedColor::Foreground => to_egui(default),
                NamedColor::Background => unreachable!(),
                NamedColor::Cursor => to_egui(crate::theme::current_theme().term_cursor),
                _ => {
                    let mut idx = n as usize;
                    // 粗体时将基本色映射到亮色（参照 Alacritty 默认行为）。
                    if bold && idx < 8 {
                        idx += 8;
                    }
                    if idx < 16 {
                        to_egui(crate::theme::current_theme().term_palette[idx])
                    } else {
                        // 其余命名色（Dim 系等）用 256 色表兜底。
                        to_egui(crate::theme::xterm256(
                            idx as u8,
                            crate::theme::current_theme().term_palette,
                        ))
                    }
                }
            }
        }
        AColor::Indexed(i) => {
            // OSC 覆盖优先。
            if let Some(rgb) = colors[i as usize] {
                return to_egui(rgb);
            }
            to_egui(crate::theme::xterm256(
                i,
                crate::theme::current_theme().term_palette,
            ))
        }
    }
}

/// VT 下划线变体 → 渲染样式（SGR 4 系列全集；无下划线为 None）。
fn underline_style_of(flags: Flags) -> UnderlineStyle {
    if flags.contains(Flags::DOUBLE_UNDERLINE) {
        UnderlineStyle::Double
    } else if flags.contains(Flags::UNDERCURL) {
        UnderlineStyle::Curly
    } else if flags.contains(Flags::DOTTED_UNDERLINE) {
        UnderlineStyle::Dotted
    } else if flags.contains(Flags::DASHED_UNDERLINE) {
        UnderlineStyle::Dashed
    } else if flags.contains(Flags::UNDERLINE) {
        UnderlineStyle::Single
    } else {
        UnderlineStyle::None
    }
}

/// alacritty Rgb → egui Color32。
fn to_egui(rgb: Rgb) -> Color32 {
    Color32::from_rgb(rgb.r, rgb.g, rgb.b)
}

/// cell 的文本样式（用于段合并判断与哈希）。
#[derive(Clone, Copy)]
struct CellStyle {
    fg: Color32,
    bg: Color32,
    bold: bool,
    italic: bool,
    underline: UnderlineStyle,
    strikeout: bool,
    underline_color: Option<Color32>,
}

impl CellStyle {
    fn key(self) -> u64 {
        u64::from(self.fg.r())
            ^ (u64::from(self.fg.g()) << 8)
            ^ (u64::from(self.fg.b()) << 16)
            ^ (u64::from(self.bg.r()) << 32)
            ^ (u64::from(self.bg.g()) << 40)
            ^ (u64::from(self.bg.b()) << 48)
            ^ (u64::from(self.bold) << 24)
            ^ (u64::from(self.italic) << 25)
            ^ ((self.underline as u64 % 7) << 26)
            ^ (u64::from(self.strikeout) << 29)
            ^ self
                .underline_color
                .map(|c| u64::from(c.r()) ^ (u64::from(c.g()) << 8) ^ (u64::from(c.b()) << 16))
                .unwrap_or(0)
                .wrapping_mul(31)
    }
}

/// 合并或追加一个 cell 到段列表。
///
/// 合并条件：相同样式 **且** 字宽一致（宽字符与半角不混排），且当前 cell
/// **不是宽字符**——宽字符恒单独成段。
/// 同段内字宽一致 → 分段绘制 `x = start_col * cell_width` 精确对齐，
/// 无字体实际 advance 的累积漂移（见 `CachedRun`）。
/// 宽字符必须单独成段：字体对 CJK 的 advance 是 1em（13px），双列宽是
/// 2×cell_width（SF Mono 13px 字号下 16.1px），若同段连续排字，段内每字
/// 少 3.1px，5 个字就漂 15px——表现为「中文越打越多，光标离文字越远、
/// 文字与后面内容之间出现一片空白」。单字符段按终端列定位后，段内无排字，
/// 每个宽字符精确落在自己的双列起点。
#[allow(clippy::too_many_arguments)]
fn push_or_merge(
    segments: &mut Vec<Segment>,
    col: usize,
    c: char,
    zero_width: Option<&[char]>,
    is_wide: bool,
    style: CellStyle,
    link: Option<String>,
    hash: &mut u64,
) {
    if let Some(last) = segments.last_mut() {
        if !is_wide
            && !last.is_wide
            && last.fg == style.fg
            && last.bold == style.bold
            && last.italic == style.italic
            && last.underline == style.underline
            && last.strikeout == style.strikeout
            && last.underline_color == style.underline_color
            && last.link == link
        {
            last.text.push(c);
            if let Some(zero_width) = zero_width {
                last.text.extend(zero_width.iter().copied());
            }
            mix_cell_hash(hash, c, style, zero_width);
            return;
        }
    }
    segments.push(Segment {
        start_col: col,
        text: c.to_string(),
        fg: style.fg,
        bold: style.bold,
        italic: style.italic,
        underline: style.underline,
        strikeout: style.strikeout,
        underline_color: style.underline_color,
        is_wide,
        link,
    });
    if let Some(zero_width) = zero_width {
        if let Some(last) = segments.last_mut() {
            last.text.extend(zero_width.iter().copied());
        }
    }
    mix_cell_hash(hash, c, style, zero_width);
}

/// 将影响行绘制的 cell 属性加入缓存指纹。
fn mix_cell_hash(hash: &mut u64, c: char, style: CellStyle, zero_width: Option<&[char]>) {
    *hash = hash.wrapping_mul(131).wrapping_add(style.key());
    *hash = hash.wrapping_mul(131).wrapping_add(c as u64);
    if let Some(zero_width) = zero_width {
        for c in zero_width {
            *hash = hash.wrapping_mul(131).wrapping_add(*c as u64);
        }
    }
}

/// 样式 → 哈希键。
#[allow(dead_code)]
fn style_key(
    fg: Color32,
    bg: Color32,
    bold: bool,
    italic: bool,
    underline: UnderlineStyle,
    underline_color: Option<Color32>,
    strikeout: bool,
) -> u64 {
    CellStyle {
        fg,
        bg,
        bold,
        italic,
        underline,
        strikeout,
        underline_color,
    }
    .key()
}

/// 为单个文本段 layout 单行 Galley（按段样式）。
fn layout_segment(ui: &Ui, seg: &Segment, font_size: f32) -> std::sync::Arc<egui::Galley> {
    ui.fonts_mut(|f| {
        f.layout_job(singleline_job(
            &seg.text,
            font_size,
            seg.fg,
            seg.bold,
            seg.italic,
            seg.underline,
            seg.underline_color,
            seg.strikeout,
        ))
    })
}

/// 宽字符段的 Galley 缓存键；非宽段、或带零宽组合符的宽字符返回 `None`。
///
/// 组合符（基符 + 变音/emoji 连接符）必须整体 shaping，不能拆成单字符 Galley，
/// 这类宽字符不入缓存、随行重建直接 layout（列定位仍是单字符段，不受影响）。
fn wide_glyph_key(seg: &Segment) -> Option<WideGlyphKey> {
    if !seg.is_wide {
        return None;
    }
    let mut chars = seg.text.chars();
    let c = chars.next()?;
    if chars.next().is_some() {
        return None;
    }
    Some((
        c,
        seg.fg,
        seg.italic,
        seg.underline,
        seg.underline_color,
        seg.strikeout,
    ))
}

/// 为单个同宽文本段构建单行 LayoutJob（不换行，按给定样式）。
#[allow(clippy::too_many_arguments)]
fn singleline_job(
    text: &str,
    font_size: f32,
    fg: Color32,
    bold: bool,
    italic: bool,
    underline: UnderlineStyle,
    underline_color: Option<Color32>,
    strikeout: bool,
) -> LayoutJob {
    // 粗体 = 前景增亮 30%（封顶 255）+ egui 暂无合成粗体，不做描边仿粗
    // （描边会让 CJK 笔画糊）。亮色映射仍由 `resolve_color` 的调色板负责，
    // 这里只处理 Spec/真彩色的粗体增亮。
    let fg = if bold {
        Color32::from_rgb(
            fg.r().saturating_add((255 - fg.r()) / 3),
            fg.g().saturating_add((255 - fg.g()) / 3),
            fg.b().saturating_add((255 - fg.b()) / 3),
        )
    } else {
        fg
    };
    let format = TextFormat {
        font_id: FontId::monospace(font_size),
        color: fg,
        italics: italic,
        // Galley 只画 Single：其它变体由 paint 侧矢量线画，避免双线重叠。
        underline: if underline == UnderlineStyle::Single {
            Stroke::new(1.0, underline_color.unwrap_or(fg))
        } else {
            Stroke::NONE
        },
        strikethrough: if strikeout {
            Stroke::new(1.0, fg)
        } else {
            Stroke::NONE
        },
        ..Default::default()
    };
    let mut job = LayoutJob::single_section(text.to_owned(), format);
    job.wrap.max_width = f32::INFINITY;
    job.break_on_newline = false;
    job.halign = egui::Align::LEFT;
    job
}

/// 将段列表构建为 egui LayoutJob（仅测试用；正式渲染走分段列定位）。
#[allow(dead_code)]
fn build_job(segments: &[Segment], font_size: f32) -> LayoutJob {
    let mut job = LayoutJob::default();
    for seg in segments {
        let format = TextFormat {
            font_id: FontId::monospace(font_size),
            color: seg.fg,
            italics: seg.italic,
            underline: if seg.underline == UnderlineStyle::Single {
                Stroke::new(1.0, seg.underline_color.unwrap_or(seg.fg))
            } else {
                Stroke::NONE
            },
            strikethrough: if seg.strikeout {
                Stroke::new(1.0, seg.fg)
            } else {
                Stroke::NONE
            },
            ..Default::default()
        };
        job.append(&seg.text, 0.0, format);
    }
    job
}

/// 判断字符是否可安全写入终端。
///
/// Text 事件本身已经是用户输入文本；只过滤 ASCII 控制字符，以及输入法
/// 在退格等按键中偶尔附带的零宽空格/BOM。不能把整个 Unicode 格式字符区
/// 都丢掉：变体选择符和零宽连接符是 emoji、部分文字系统的有效组成部分。
fn is_printable_text_char(c: char) -> bool {
    !c.is_ascii_control() && c != '\u{200b}' && c != '\u{feff}'
}

/// egui 键 → 终端字符键（仅无文本时兜底使用）。
fn map_char_key(key: &egui::Key, shift: bool) -> Option<Key> {
    use egui::Key as E;
    let v = *key as u8;
    // 字母与数字键（枚举判别值连续，按声明顺序）。
    if (E::A as u8..=E::Z as u8).contains(&v) {
        let c = (v - E::A as u8 + b'a') as char;
        return Some(Key::Char(if shift { c.to_ascii_uppercase() } else { c }));
    }
    if (E::Num0 as u8..=E::Num9 as u8).contains(&v) {
        let c = if shift {
            match key {
                E::Num0 => ')',
                E::Num1 => '!',
                E::Num2 => '@',
                E::Num3 => '#',
                E::Num4 => '$',
                E::Num5 => '%',
                E::Num6 => '^',
                E::Num7 => '&',
                E::Num8 => '*',
                E::Num9 => '(',
                _ => unreachable!("数字键范围内只能出现 Num0..Num9"),
            }
        } else {
            (v - E::Num0 as u8 + b'0') as char
        };
        return Some(Key::Char(c));
    }
    let c = match key {
        E::Space => ' ',
        E::Minus => {
            if shift {
                '_'
            } else {
                '-'
            }
        }
        E::Equals => {
            if shift {
                '+'
            } else {
                '='
            }
        }
        E::Comma => {
            if shift {
                '<'
            } else {
                ','
            }
        }
        E::Period => {
            if shift {
                '>'
            } else {
                '.'
            }
        }
        E::Slash => {
            if shift {
                '?'
            } else {
                '/'
            }
        }
        E::Semicolon => {
            if shift {
                ':'
            } else {
                ';'
            }
        }
        E::Quote => {
            if shift {
                '"'
            } else {
                '\''
            }
        }
        E::Backtick => {
            if shift {
                '~'
            } else {
                '`'
            }
        }
        E::Backslash => {
            if shift {
                '|'
            } else {
                '\\'
            }
        }
        E::OpenBracket => {
            if shift {
                '{'
            } else {
                '['
            }
        }
        E::CloseBracket => {
            if shift {
                '}'
            } else {
                ']'
            }
        }
        E::Colon => ':',
        E::Plus => '+',
        E::Pipe => '|',
        E::Questionmark => '?',
        E::Exclamationmark => '!',
        E::OpenCurlyBracket => '{',
        E::CloseCurlyBracket => '}',
        _ => return None,
    };
    Some(Key::Char(c))
}

/// 构造括号粘贴载荷。
///
/// 粘贴内容属于不可信输入；若保留其中的 ESC，文本内的
/// `ESC[201~` 可以提前关闭括号粘贴，让后续换行或控制序列脱离编辑缓冲区。
/// 删除 ESC 后，原始序列会变成普通文本，协议边界只由这里追加的结束标记提供。
fn bracketed_paste_payload(text: &str) -> String {
    let sanitized: String = text.chars().filter(|&c| c != '\x1b').collect();
    format!("\x1b[200~{sanitized}\x1b[201~")
}

/// 将拖入的路径转换为可直接交给 POSIX shell 的文本。
///
/// 常见路径保持原样，包含空格、引号或 shell 特殊字符的路径使用单引号；
/// 单引号本身用 shell 的 `'\''` 组合拆分，确保拖入路径只会作为一个参数，
/// 不会因为文件名内容被解释成额外的命令或重定向。
fn shell_escape_path(path: &Path) -> String {
    let text = path.to_string_lossy();
    if text.chars().all(is_unquoted_shell_path_char) {
        return text.into_owned();
    }

    let mut escaped = String::with_capacity(text.len() + 2);
    escaped.push('\'');
    for character in text.chars() {
        if character == '\'' {
            escaped.push_str("'\\''");
        } else {
            escaped.push(character);
        }
    }
    escaped.push('\'');
    escaped
}

/// 不需要引号时允许出现在路径中的字符。
fn is_unquoted_shell_path_char(character: char) -> bool {
    character.is_alphanumeric()
        || matches!(
            character,
            '/' | '_' | '-' | '.' | '~' | '@' | '%' | '+' | '=' | ':' | ','
        )
}

/// 将同一拖放操作中的多个路径拼接为一段终端输入。
fn dropped_paths_text(paths: &[std::path::PathBuf]) -> Option<String> {
    let text = paths
        .iter()
        .map(|path| shell_escape_path(path))
        .collect::<Vec<_>>()
        .join(" ");
    (!text.is_empty()).then_some(text)
}

/// egui 键 → 终端特殊键。
fn map_special_key(key: &egui::Key) -> Option<Key> {
    use egui::Key as E;
    match key {
        E::Enter => Some(Key::Enter),
        E::Tab => Some(Key::Tab),
        E::Backspace => Some(Key::Backspace),
        E::Escape => Some(Key::Escape),
        E::ArrowUp => Some(Key::Up),
        E::ArrowDown => Some(Key::Down),
        E::ArrowLeft => Some(Key::Left),
        E::ArrowRight => Some(Key::Right),
        E::Home => Some(Key::Home),
        E::End => Some(Key::End),
        E::PageUp => Some(Key::PageUp),
        E::PageDown => Some(Key::PageDown),
        E::Insert => Some(Key::Insert),
        E::Delete => Some(Key::Delete),
        _ => {
            // F 键（F1-F35 判别值连续）。
            let v = *key as u8;
            if v >= E::F1 as u8 && v <= E::F35 as u8 {
                Some(Key::F(v - E::F1 as u8 + 1))
            } else {
                None
            }
        }
    }
}

/// 终端窗口本地处理的翻页组合。
///
/// 只接受纯 Shift，避免拦截 Shift+Alt/Ctrl 等应继续交给终端程序的
/// 修饰键序列；Command 组合也留给应用级快捷键处理。
fn scrollback_key(key: &egui::Key, modifiers: egui::Modifiers) -> Option<Scroll> {
    if !modifiers.shift || modifiers.alt || modifiers.ctrl || modifiers.command {
        return None;
    }
    match key {
        egui::Key::PageUp => Some(Scroll::PageUp),
        egui::Key::PageDown => Some(Scroll::PageDown),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mino_core::terminal::{Session, SessionOptions};
    use std::cell::RefCell;
    use std::path::PathBuf;
    use std::rc::Rc;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    #[derive(Debug)]
    struct TestDroppedFile {
        path: PathBuf,
    }

    impl egui::DroppedFile for TestDroppedFile {
        fn path(&self) -> &std::path::Path {
            &self.path
        }

        fn bytes(&self) -> Result<Vec<u8>, String> {
            Ok(Vec::new())
        }
    }

    /// 将终端可见区域转为文本。
    fn grid_text(session: &Session) -> String {
        use alacritty_terminal::term::cell::Flags;
        let term_arc = session.term();
        let guard = term_arc.lock();
        let content = guard.renderable_content();
        let mut lines: Vec<String> = Vec::new();
        let mut current = String::new();
        let mut started = false;
        let mut prev_grid_line: i32 = i32::MIN;
        for item in content.display_iter {
            let cell = item.cell;
            if cell.flags.contains(Flags::WIDE_CHAR_SPACER) || cell.flags.contains(Flags::HIDDEN) {
                continue;
            }
            if item.point.line.0 != prev_grid_line {
                if started {
                    lines.push(current.trim_end().to_string());
                }
                current = String::new();
                started = true;
                prev_grid_line = item.point.line.0;
            }
            current.push(cell.c);
        }
        if started {
            lines.push(current.trim_end().to_string());
        }
        lines.join("\n")
    }

    /// 等待终端文本包含子串。
    fn wait_text(
        view: &Rc<RefCell<TerminalView>>,
        harness: &mut egui_kittest::Harness,
        needle: &str,
    ) -> bool {
        let deadline = Instant::now() + Duration::from_secs(8);
        while Instant::now() < deadline {
            harness.step();
            let text = grid_text(view.borrow().session());
            if text.contains(needle) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(60));
        }
        false
    }

    /// 模拟真实按键：Key 事件 + Text 事件（与 egui-winit 行为一致）。
    fn send_key(harness: &mut egui_kittest::Harness, key: egui::Key, text: Option<&str>) {
        harness.event(egui::Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::default(),
        });
        if let Some(t) = text {
            harness.event(egui::Event::Text(t.to_string()));
        }
    }

    #[test]
    fn 拖入路径按shell安全格式化() {
        assert_eq!(
            shell_escape_path(Path::new("/tmp/report.txt")),
            "/tmp/report.txt"
        );
        assert_eq!(
            shell_escape_path(Path::new("/tmp/Project Files/app's.app")),
            "'/tmp/Project Files/app'\\''s.app'"
        );
        assert_eq!(
            dropped_paths_text(&[
                PathBuf::from("/tmp/report.txt"),
                PathBuf::from("/tmp/Project Files"),
                PathBuf::from("/Applications/Mino.app"),
            ])
            .as_deref(),
            Some("/tmp/report.txt '/tmp/Project Files' /Applications/Mino.app")
        );
    }

    /// 回归：Finder 拖入文件、目录或应用时，应把路径写到当前终端，
    /// 不自动回车执行，并正确处理包含空格的路径。
    #[test]
    fn 拖入文件目录应用路径写入终端() {
        let session = Session::spawn_local(
            SessionOptions::default(),
            80,
            24,
            Arc::new(|_ev: &SessionEvent| {}),
        )
        .expect("创建本地终端失败");
        let view = Rc::new(RefCell::new(TerminalView::new(session)));
        let view_show = view.clone();
        let mut harness = egui_kittest::Harness::new_ui(move |ui| {
            view_show.borrow_mut().show(ui);
        });
        assert!(wait_text(&view, &mut harness, "mino"), "zsh 未就绪");

        // 先让 egui 记录拖放结束时的鼠标位置；原生 dropped_files 本身不携带坐标。
        let drop_pos = egui::pos2(120.0, 120.0);
        harness.event(egui::Event::PointerMoved(drop_pos));
        harness.step();
        for path in [
            "/tmp/report.txt",
            "/tmp/Project Files",
            "/Applications/Mino.app",
        ] {
            harness
                .input_mut()
                .dropped_files
                .push(Arc::new(TestDroppedFile {
                    path: PathBuf::from(path),
                }));
        }
        harness.step();

        assert!(
            wait_text(&view, &mut harness, "'/tmp/Project Files'")
                && grid_text(view.borrow().session()).contains("/Applications/Mino.app"),
            "拖入路径未写入终端，终端内容：\n{}",
            grid_text(view.borrow().session())
        );
        // 没有发送回车：路径仍在当前输入行中，后续可继续编辑或手动执行。
        let text = grid_text(view.borrow().session());
        assert!(
            text.lines().any(|line| line.contains("/tmp/report.txt")),
            "拖放不应自动执行命令，终端内容：\n{text}"
        );
    }

    /// 鼠标拖动终端网格应建立稳定的选区（回归：终端曾只有键盘焦点，
    /// 任何拖动都不会产生可复制文本）。
    #[test]
    fn 鼠标拖选建立终端选区() {
        let session = Session::spawn_local(
            SessionOptions::default(),
            80,
            24,
            Arc::new(|_ev: &SessionEvent| {}),
        )
        .expect("创建本地终端失败");
        let view = Rc::new(RefCell::new(TerminalView::new(session)));
        let view_show = view.clone();
        let mut harness = egui_kittest::Harness::new_ui(move |ui| {
            view_show.borrow_mut().show(ui);
        });
        assert!(wait_text(&view, &mut harness, "mino"), "zsh 未就绪");

        let start = egui::pos2(12.0, 14.0);
        let end = egui::pos2(150.0, 14.0);
        harness.event(egui::Event::PointerMoved(start));
        harness.event(egui::Event::PointerButton {
            pos: start,
            button: egui::PointerButton::Primary,
            pressed: true,
            modifiers: egui::Modifiers::NONE,
        });
        harness.step();
        harness.event(egui::Event::PointerMoved(end));
        harness.step();
        harness.event(egui::Event::PointerButton {
            pos: end,
            button: egui::PointerButton::Primary,
            pressed: false,
            modifiers: egui::Modifiers::NONE,
        });
        harness.step();

        assert!(view.borrow().selection.is_some(), "拖选后应存在终端选区");
    }

    /// 退格键应删除已输入字符（回归测试：曾出现删除键异常）。
    #[test]
    fn 退格键删除输入字符() {
        let session = Session::spawn_local(
            SessionOptions::default(),
            80,
            24,
            Arc::new(|_ev: &SessionEvent| {}),
        )
        .expect("创建本地终端失败");
        let view = Rc::new(RefCell::new(TerminalView::new(session)));
        let view_show = view.clone();
        let mut harness = egui_kittest::Harness::new_ui(move |ui| {
            view_show.borrow_mut().show(ui);
        });

        // 等待 zsh 提示符出现。
        assert!(
            wait_text(&view, &mut harness, "mino"),
            "zsh 未就绪，终端内容：\n{}",
            grid_text(view.borrow().session())
        );

        // 输入 abc。
        send_key(&mut harness, egui::Key::A, Some("a"));
        send_key(&mut harness, egui::Key::B, Some("b"));
        send_key(&mut harness, egui::Key::C, Some("c"));
        assert!(
            wait_text(&view, &mut harness, "abc"),
            "输入 abc 失败，终端内容：\n{}",
            grid_text(view.borrow().session())
        );

        // 按退格：模拟真实环境的 Key 事件 + 输入法产生的零宽空格 Text 事件。
        send_key(&mut harness, egui::Key::Backspace, Some("\u{200b}"));
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut deleted = false;
        while Instant::now() < deadline {
            harness.step();
            let text = grid_text(view.borrow().session());
            // zsh 回显行应变为 "ab"（末尾 abc → ab），且不应出现多余空格。
            if let Some(line) = text.lines().find(|l| l.ends_with("ab")) {
                if !line.ends_with("abc") {
                    deleted = true;
                    break;
                }
            }
            std::thread::sleep(Duration::from_millis(60));
        }
        assert!(
            deleted,
            "退格未删除字符（或插入了异常字符），终端内容：\n{}",
            grid_text(view.borrow().session())
        );
    }

    /// 普通字符键不应产生重复或异常字节。
    #[test]
    fn 普通字符单次写入() {
        let session = Session::spawn_local(
            SessionOptions::default(),
            80,
            24,
            Arc::new(|_ev: &SessionEvent| {}),
        )
        .expect("创建本地终端失败");
        let view = Rc::new(RefCell::new(TerminalView::new(session)));
        let view_show = view.clone();
        let mut harness = egui_kittest::Harness::new_ui(move |ui| {
            view_show.borrow_mut().show(ui);
        });
        assert!(wait_text(&view, &mut harness, "mino"), "zsh 未就绪");

        send_key(&mut harness, egui::Key::A, Some("a"));
        assert!(
            wait_text(&view, &mut harness, "a"),
            "字符 a 未显示，终端内容：\n{}",
            grid_text(view.borrow().session())
        );
        // 不应有重复 "aa"：只检查当前输入行（最后一行），
        // 避免被提示符中的主机名（CI 为随机 UUID，可能含 "aa"）误报。
        let text = grid_text(view.borrow().session());
        let last_line = text.lines().last().unwrap_or("");
        assert!(
            !last_line.contains("aa"),
            "字符重复写入，最后一行：{last_line:?}，终端内容：\n{text}"
        );
    }

    /// Tab 属于终端输入，不应被 egui 当作焦点导航键；否则 shell 处理 Tab 后
    /// 终端会短暂失去焦点，紧接着的 Ctrl+C 可能被吞掉。
    #[test]
    fn tab保持终端焦点() {
        let session = Session::spawn_local(
            SessionOptions::default(),
            80,
            24,
            Arc::new(|_ev: &SessionEvent| {}),
        )
        .expect("创建本地终端失败");
        let view = Rc::new(RefCell::new(TerminalView::new(session)));
        let view_show = view.clone();
        let mut harness = egui_kittest::Harness::new_ui(move |ui| {
            view_show.borrow_mut().show(ui);
            // SSH 标签页的终端后面还有悬浮 SFTP 按钮；它是可聚焦控件，
            // 正是远端 Tab 被 egui 焦点导航抢走的实际布局。
            let _ = ui.button("after-terminal");
        });
        assert!(wait_text(&view, &mut harness, "mino"), "zsh 未就绪");

        assert_eq!(
            harness.ctx.memory(|m| m.focused()),
            Some(egui::Id::new("terminal_view")),
            "终端初始应持有焦点"
        );
        harness.event(egui::Event::Key {
            key: egui::Key::Tab,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        });
        harness.step();

        assert_eq!(
            harness.ctx.memory(|m| m.focused()),
            Some(egui::Id::new("terminal_view")),
            "Tab 发送给 shell 后终端焦点不应被 egui 转移"
        );
    }

    /// 向上滚动查看 scrollback 后渲染不得崩溃（回归测试：display_iter 的
    /// scrollback 行是负网格行号，曾 cast 成 usize 触发 capacity overflow 闪退）。
    #[test]
    fn 滚动scrollback后渲染不崩溃() {
        use alacritty_terminal::grid::Scroll;

        let session = Session::spawn_local(
            SessionOptions::default(),
            80,
            24,
            Arc::new(|_ev: &SessionEvent| {}),
        )
        .expect("创建本地终端失败");
        let view = Rc::new(RefCell::new(TerminalView::new(session)));
        let view_show = view.clone();
        let mut harness = egui_kittest::Harness::new_ui(move |ui| {
            view_show.borrow_mut().show(ui);
        });
        assert!(wait_text(&view, &mut harness, "mino"), "zsh 未就绪");

        // 执行 `seq 40` 输出 40 行，超过 24 行视口，产生 scrollback。
        view.borrow().session().write(b"seq 40\r");
        let deadline = Instant::now() + Duration::from_secs(8);
        let mut done = false;
        while Instant::now() < deadline {
            harness.step();
            let text = grid_text(view.borrow().session());
            if text.lines().any(|l| l.trim_end() == "40") {
                done = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(60));
        }
        assert!(
            done,
            "seq 40 输出未就绪，终端内容：\n{}",
            grid_text(view.borrow().session())
        );

        // 滚动前视口顶行（seq 输出靠近末尾的数字）。
        let top_before: u32 = grid_text(view.borrow().session())
            .lines()
            .next()
            .and_then(|l| l.trim().parse().ok())
            .unwrap_or(0);

        // 向上滚动 5 行（进入 scrollback，出现负网格行号）。
        {
            let term = view.borrow().session().term();
            let mut guard = term.lock();
            guard.grid_mut().scroll_display(Scroll::Delta(5));
        }

        // 渲染若干帧：修复前负行号 cast 成 usize 后 resize 行缓存会
        // capacity overflow panic（本测试直接失败）。
        for _ in 0..6 {
            harness.step();
        }

        // 滚动后视口顶行应显示更早的输出（数字更小），验证显示行号换算正确。
        let top_after: u32 = grid_text(view.borrow().session())
            .lines()
            .next()
            .and_then(|l| l.trim().parse().ok())
            .unwrap_or(0);
        assert!(
            top_after < top_before,
            "滚动后视口应显示更早的输出行（{top_before} → {top_after}）"
        );
    }

    /// 找一条以 `first` 开头的可见行，返回（显示行号, [(终端列, 字符)]）。
    fn grid_row_starting_with(
        session: &Session,
        first: char,
    ) -> Option<(usize, Vec<(usize, char)>)> {
        let term_arc = session.term();
        let guard = term_arc.lock();
        let content = guard.renderable_content();
        let display_offset = content.display_offset as i32;
        let mut rows: Vec<(i32, Vec<(usize, char)>)> = Vec::new();
        for item in content.display_iter {
            let line = item.point.line.0;
            if rows.last().map(|(l, _)| *l) != Some(line) {
                rows.push((line, Vec::new()));
            }
            let cell = item.cell;
            if cell.flags.contains(Flags::WIDE_CHAR_SPACER) || cell.flags.contains(Flags::HIDDEN) {
                continue;
            }
            rows.last_mut()
                .expect("上面刚压入一行")
                .1
                .push((item.point.column.0, cell.c));
        }
        rows.into_iter()
            .find(|(_, cells)| cells.first().map(|(_, c)| *c) == Some(first))
            .map(|(line, cells)| ((line + display_offset).max(0) as usize, cells))
    }

    /// 回归：中文宽字符按终端列（双列）绘制，不在字符间留下累积空白。
    ///
    /// 用户现象：中文越打越多，光标离文字越来越远、中文与后面内容之间出现
    /// 一片空白。根因：字体对 CJK 的 advance 是 1em（13px），而一个宽字符占
    /// 两列（2×cell_width ≈ 16.1px）；连续的宽字符若放进同一个 Galley，egui
    /// 按字体实际 advance 排字，段内每字少 3.1px、越打越左漂，直到下一个按
    /// 终端列绝对定位的分段才复位——漂移量全变成可见空白。
    /// 断言（像素级）：①5 个中文字各自一个墨迹簇、起点贴住各自双列起点；
    /// ②中文末笔到后续半角字符的间距 < 一个 cell（无空白漂移）。
    #[test]
    #[cfg(target_os = "macos")] // 依赖系统中文 fallback 字体（PingFang/STHeiti）
    fn 中文宽字符按终端列对齐() {
        let session = Session::spawn_local(
            SessionOptions::default(),
            80,
            24,
            Arc::new(|_ev: &SessionEvent| {}),
        )
        .expect("创建本地终端失败");
        let view = Rc::new(RefCell::new(TerminalView::new(session)));
        let view_show = view.clone();
        let mut harness = egui_kittest::Harness::new_ui(move |ui| {
            view_show.borrow_mut().show(ui);
        });
        // 字体必须在首帧前装好：cell_width 与宽字符 Galley 都按首帧字体缓存
        // （kittest 默认字体没有中文字形，也量不出真实 cell_width）。
        crate::setup_fonts(&harness.ctx);
        assert!(wait_text(&view, &mut harness, "mino"), "zsh 未就绪");
        // 字体链必须真能画中文：缺字形时测到的是占位符宽度，断言无意义。
        // （`set_fonts` 在下一帧 begin_pass 生效，故须等跑过帧再查。）
        assert!(
            harness
                .ctx
                .fonts_mut(|f| f.has_glyphs(&FontId::monospace(13.0), "中")),
            "等宽字体链缺少中文字形，无法验证中文列对齐"
        );

        // 输出一行「中中中中中ab」：5 个中文占 0/2/4/6/8 列，半角紧随其后。
        view.borrow()
            .session()
            .write("printf '中中中中中ab\\n'\r".as_bytes());
        let deadline = Instant::now() + Duration::from_secs(8);
        while Instant::now() < deadline {
            harness.step();
            if grid_row_starting_with(view.borrow().session(), '中').is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(60));
        }
        // 再多跑一帧让该行进入渲染，然后按下帧同一时刻的网格读行号。
        harness.step();
        let (row_v, cells) =
            grid_row_starting_with(view.borrow().session(), '中').expect("终端未出现中文输出行");
        assert_eq!(
            &cells[..7],
            &[
                (0, '中'),
                (2, '中'),
                (4, '中'),
                (6, '中'),
                (8, '中'),
                (10, 'a'),
                (11, 'b')
            ],
            "中文输出行的网格列不符"
        );

        // 渲染该行后按像素核对墨迹列（终端列 → 屏幕列的唯一映射是
        // `inner.left() + col * cell_width`）。
        let img = harness.render().expect("渲染失败");
        let (cell_width, cell_height) = {
            let v = view.borrow();
            (v.cell_width, v.cell_height)
        };
        let bg = crate::theme::current_theme().term_bg;
        // 终端面板边界：kittest 的 CentralPanel 外层留 8px 白边，视图用
        // `max_rect()` 铺满面板——取非留白像素的包围盒即面板矩形。
        let margin_bg = *img.get_pixel(2, 2);
        let (mut x0, mut x1, mut y0, mut y1) = (u32::MAX, 0, u32::MAX, 0);
        for (x, y, p) in img.enumerate_pixels() {
            if *p != margin_bg {
                x0 = x0.min(x);
                x1 = x1.max(x);
                y0 = y0.min(y);
                y1 = y1.max(y);
            }
        }
        let outer = Rect::from_min_max(
            egui::pos2(x0 as f32, y0 as f32),
            egui::pos2(x1 as f32 + 1.0, y1 as f32 + 1.0),
        );
        let inner = outer.shrink(PADDING);
        let row_top = inner.top() + row_v as f32 * cell_height;
        let band = (row_top.max(0.0) as u32)..((row_top + cell_height) as u32);
        let is_ink = |x: u32| {
            band.clone().any(|y| {
                let p = img.get_pixel(x, y);
                let d = |a: u8, b: u8| (i32::from(a) - i32::from(b)).abs();
                d(p[0], bg.r).max(d(p[1], bg.g)).max(d(p[2], bg.b)) > 24
            })
        };
        // 行内墨迹列 → 连续墨迹簇。
        let mut runs: Vec<(u32, u32)> = Vec::new();
        for x in (0..img.width()).filter(|x| is_ink(*x)) {
            match runs.last_mut() {
                Some(last) if last.1 + 1 == x => last.1 = x,
                _ => runs.push((x, x)),
            }
        }
        let cjk_end = inner.left() + 10.0 * cell_width;
        let cjk: Vec<(u32, u32)> = runs
            .iter()
            .copied()
            .filter(|(start, _)| (*start as f32) < cjk_end)
            .collect();
        assert_eq!(
            cjk.len(),
            5,
            "5 个中文字应各自成一个墨迹簇（宽字漂移会粘连或留空），实际 {cjk:?}"
        );
        for (i, (start, _)) in cjk.iter().enumerate() {
            let got = *start as f32 - cjk[0].0 as f32;
            let expect = 2.0 * i as f32 * cell_width;
            assert!(
                (got - expect).abs() <= 2.5,
                "第 {i} 个中文字墨迹起点距首字 {got:.1}px，应为 {expect:.1}px（双列）"
            );
        }
        let a_ink = runs
            .iter()
            .map(|(start, _)| *start)
            .find(|x| (*x as f32) >= cjk_end)
            .expect("未找到中文之后的半角字符墨迹");
        let gap = a_ink as f32 - cjk.last().expect("已断言非空").1 as f32;
        assert!(
            gap < cell_width,
            "中文末笔 {a_ink}px 到后续半角墨迹之间有 {gap:.1}px 空白（应 < 一个 cell {cell_width:.1}px）"
        );
    }
}

#[cfg(test)]
mod deadlock_tests {
    use super::*;
    use mino_core::terminal::{Session, SessionOptions};
    use std::cell::RefCell;
    use std::rc::Rc;
    use std::sync::Arc;

    /// 回归测试：滚轮事件不应在 ui.input 闭包内触发 request_repaint（会自死锁 panic）。
    #[test]
    fn 滚轮滚动不死锁() {
        let session = Session::spawn_local(
            SessionOptions::default(),
            80,
            24,
            Arc::new(|_ev: &SessionEvent| {}),
        )
        .expect("创建本地终端失败");
        let view = Rc::new(RefCell::new(TerminalView::new(session)));
        let view_show = view.clone();
        let mut harness = egui_kittest::Harness::new_ui(move |ui| {
            view_show.borrow_mut().show(ui);
        });
        // 跑几帧让 zsh 就绪。
        for _ in 0..6 {
            harness.step();
            std::thread::sleep(std::time::Duration::from_millis(50));
        }

        // 注入滚轮事件（Point/Line/Page 三种单位）。滚轮只在指针位于终端时处理。
        harness.event(egui::Event::PointerMoved(egui::pos2(100.0, 100.0)));
        harness.step();
        for unit in [
            egui::MouseWheelUnit::Point,
            egui::MouseWheelUnit::Line,
            egui::MouseWheelUnit::Page,
        ] {
            harness.event(egui::Event::MouseWheel {
                unit,
                delta: egui::Vec2::new(0.0, 3.0),
                modifiers: egui::Modifiers::default(),
                phase: egui::TouchPhase::Move,
            });
            harness.step();
            harness.step();
        }
        // 若修复失效，此处会在 10 秒死锁后 panic；到达这里说明通过。
    }
}

#[cfg(test)]
mod mouse_wheel_tests {
    use super::*;

    #[test]
    fn 鼠标上报优先于替代屏和scrollback() {
        assert_eq!(
            wheel_target(
                TermMode::MOUSE_REPORT_CLICK
                    | TermMode::SGR_MOUSE
                    | TermMode::ALT_SCREEN
                    | TermMode::ALTERNATE_SCROLL,
            ),
            WheelTarget::ApplicationMouse
        );
        assert_eq!(
            wheel_target(TermMode::ALT_SCREEN | TermMode::ALTERNATE_SCROLL),
            WheelTarget::AlternateScroll
        );
        assert_eq!(wheel_target(TermMode::NONE), WheelTarget::Scrollback);
    }

    /// 回归：程序订阅鼠标上报后（omp `/tree`/选择框都开 1000/1006），
    /// 点击必须透传 SGR 序列给程序，不能再被本地选区吞掉；未订阅时本地
    /// 拖选行为不变（由 `鼠标拖选建立终端选区` 覆盖）。
    #[test]
    fn 鼠标上报开启时点击透传程序() {
        use mino_core::terminal::{Session, SessionOptions};
        use std::cell::RefCell;
        use std::rc::Rc;
        use std::sync::Arc;
        let session = Session::spawn_local(
            SessionOptions::default(),
            80,
            24,
            Arc::new(|_ev: &SessionEvent| {}),
        )
        .expect("创建本地终端失败");
        let view = Rc::new(RefCell::new(TerminalView::new(session)));
        let view_show = view.clone();
        let mut harness = egui_kittest::Harness::new_ui(move |ui| {
            view_show.borrow_mut().show(ui);
        });
        harness.run_steps(6);
        // 程序侧打开鼠标上报 + SGR：直接调 VT 层解析申请人造输出（走
        // `Processor::advance`，与远程 `remote_loop` 同一条管线；不能用
        // `session.write`——那是往从机方向写，会被 shell 吃掉输入）。
        view.borrow()
            .session()
            .inject_program_output_for_test(b"\x1b[?1000h\x1b[?1006h");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            harness.step();
            let mode = *view.borrow().session().term().lock().mode();
            if mode.contains(TermMode::MOUSE_REPORT_CLICK | TermMode::SGR_MOUSE) {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "DECSET 1000/1006 未生效，mode={mode:?}"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        // 编码层：左键点击 (0,0) 应为 SGR 按下+释放。
        let mode = TermMode::MOUSE_REPORT_CLICK | TermMode::SGR_MOUSE;
        assert_eq!(
            keys::encode_mouse_click(
                MouseButton::Left,
                MouseEventKind::Press,
                Mods::default(),
                mode,
                0,
                0
            )
            .unwrap(),
            b"\x1b[<0;1;1M"
        );
        // 视图分支：上报开启时点击不建本地选区（已透传给程序）。
        let pos = egui::pos2(60.0, 40.0);
        harness.event(egui::Event::PointerMoved(pos));
        harness.event(egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed: true,
            modifiers: egui::Modifiers::NONE,
        });
        harness.step();
        harness.event(egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed: false,
            modifiers: egui::Modifiers::NONE,
        });
        harness.step();
        assert!(
            view.borrow().selection.is_none(),
            "鼠标上报开启时点击应透传程序，不建本地选区"
        );
    }

    /// 回归：omp 发出的 OSC8 超链接（`tui.hyperlinks=always` 下路径/URL
    /// 全包链接）必须进段缓存并可点击；同 URI 相邻 cell 合并、不同 URI 另起段。
    #[test]
    fn 超链接分段与点击() {
        use mino_core::terminal::{Session, SessionOptions};
        use std::cell::RefCell;
        use std::rc::Rc;
        use std::sync::Arc;
        let session = Session::spawn_local(
            SessionOptions::default(),
            80,
            24,
            Arc::new(|_ev: &SessionEvent| {}),
        )
        .expect("创建本地终端失败");
        let view = Rc::new(RefCell::new(TerminalView::new(session)));
        let view_show = view.clone();
        let mut harness = egui_kittest::Harness::new_ui(move |ui| {
            view_show.borrow_mut().show(ui);
        });
        harness.run_steps(6);
        // 两段不同 URI 的链接 + 中间普通文本。
        view.borrow().session().inject_program_output_for_test(
            b"\x1b]8;;https://a.example/\x1b\\AAA\x1b]8;;\x1b\\ mid \x1b]8;;https://b.example/\x1b\\BBB\x1b]8;;\x1b\\",
        );
        harness.run_steps(6);
        // 段缓存里应出现两段带链接的 run（同 URI 合并、文本段无链接）。
        let mut links: Vec<String> = Vec::new();
        {
            let v = view.borrow();
            for cache in v.rows_cache.values() {
                for run in &cache.runs {
                    if let Some(url) = run.link.as_deref() {
                        links.push(format!("{}:{}", url, run.galley.text()));
                    }
                }
            }
        }
        assert!(
            links
                .iter()
                .any(|s| s.contains("https://a.example") && s.contains("AAA")),
            "第一段链接缺失：{links:?}"
        );
        assert!(
            links
                .iter()
                .any(|s| s.contains("https://b.example") && s.contains("BBB")),
            "第二段链接缺失：{links:?}"
        );
        assert!(
            !links.iter().any(|s| s.contains("mid")),
            "普通文本不应带链接：{links:?}"
        );
    }

    /// 回归：程序 OSC52 复制（如 omp yank 整段代码）必须落系统剪贴板；
    /// 失败走 toast 通道，不静默丢失。
    #[test]
    fn 程序复制写入系统剪贴板() {
        use crate::clip_image::ClipboardReader;
        use mino_core::terminal::{Session, SessionEvent, SessionOptions};
        use std::sync::{Arc, Mutex};

        struct RecordingClipboard {
            text: Arc<Mutex<Option<String>>>,
        }
        impl ClipboardReader for RecordingClipboard {
            fn clipboard_text(&mut self) -> Option<String> {
                self.text.lock().unwrap().clone()
            }
            fn set_clipboard_text(&mut self, text: &str) -> Result<(), String> {
                *self.text.lock().unwrap() = Some(text.to_owned());
                Ok(())
            }
            fn clipboard_file_paths(&self) -> Vec<std::path::PathBuf> {
                Vec::new()
            }
            fn clipboard_image(&mut self) -> Option<(usize, usize, Vec<u8>)> {
                None
            }
        }
        let recording: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));

        let session = Session::spawn_local(
            SessionOptions::default(),
            80,
            24,
            Arc::new(|_ev: &SessionEvent| {}),
        )
        .expect("创建本地终端失败");
        let mut view = TerminalView::new(session);
        view.set_clipboard_for_test(Box::new(RecordingClipboard {
            text: recording.clone(),
        }));
        // 程序侧发 OSC52 store（走 VT 解析器，与真实程序同管线）。
        view.session()
            .inject_program_output_for_test(b"\x1b]52;c;aGVsbG8td29ybGQ=\x07");
        view.drain_background_events();
        assert_eq!(
            recording.lock().unwrap().as_deref(),
            Some("hello-world"),
            "OSC52 store 应写入系统剪贴板"
        );
        // 读回管线：ClipboardLoad 用当前剪贴板文本回写（默认配置下程序
        // 发不出 load，这里只验 formatter 语义不进 drain）。
        let _ = recording;
    }

    /// 回归：Bell 不再静默丢失（视觉脉冲 0.6s）；ResetTitle 清空程序
    /// 设置过的标题（此前 `Event::ResetTitle` 在 `_ => false` 被丢弃）。
    #[test]
    fn 铃声与标题重置有响应() {
        use mino_core::terminal::{Session, SessionOptions};
        use std::sync::Arc;
        let session = Session::spawn_local(
            SessionOptions::default(),
            80,
            24,
            Arc::new(|_ev: &SessionEvent| {}),
        )
        .expect("创建本地终端失败");
        let mut view = TerminalView::new(session);
        view.session()
            .inject_program_output_for_test(b"\x1b]0;hello-title\x07");
        view.drain_background_events();
        assert_eq!(view.session_title(), "hello-title");
        // BEL 响铃：脉冲置位。
        view.session().inject_program_output_for_test(b"\x07");
        view.drain_background_events();
        assert!(view.bell_until.is_some(), "Bell 应置位视觉脉冲");
        // 空标题重置：缓存清空。
        view.session()
            .inject_program_output_for_test(b"\x1b]0;\x07");
        view.drain_background_events();
        assert!(
            view.session_title().is_empty(),
            "ResetTitle 应清空标题缓存，实际：{:?}",
            view.session_title()
        );
    }

    /// 回归：程序订阅 kitty 键盘（`CSI > 1 u`）后，非可打印键走 CSI-u；
    /// 可打印字符仍走 legacy（kitty 要求文本键发原文）。
    #[test]
    fn kitty订阅后按键走csi_u() {
        use mino_core::terminal::{Session, SessionOptions};
        use std::sync::Arc;
        let session = Session::spawn_local(
            SessionOptions::default(),
            80,
            24,
            Arc::new(|_ev: &SessionEvent| {}),
        )
        .expect("创建本地终端失败");
        let view = TerminalView::new(session);
        // 程序订阅 disambiguate（flag=1）。
        view.session().inject_program_output_for_test(b"\x1b[>1u");
        let mode = *view.session().term().lock().mode();
        assert!(
            mode.contains(TermMode::DISAMBIGUATE_ESC_CODES),
            "kitty 订阅未生效，mode={mode:?}"
        );
        // 编码层按该 mode 分流（与 `handle_input` 同一 `mode` 变量）。
        assert_eq!(
            keys::encode_key(Key::Enter, Mods::default(), mode).unwrap(),
            b"\x1b[13u"
        );
        assert_eq!(
            keys::encode_key(Key::Char('a'), Mods::default(), mode).unwrap(),
            b"a"
        );
    }

    /// 回归：SGR 下划线变体（4:2 双线/4:3 波浪）与 SGR58 下划线色必须进
    /// 段缓存（omp 的诊断/链接色用它们）；粗体增亮不断言像素，只断样式管线。
    #[test]
    fn 下划线变体与颜色进段缓存() {
        use mino_core::terminal::{Session, SessionEvent, SessionOptions};
        use std::sync::Arc;
        let session = Session::spawn_local(
            SessionOptions::default(),
            80,
            24,
            Arc::new(|_ev: &SessionEvent| {}),
        )
        .expect("创建本地终端失败");
        let view = TerminalView::new(session);
        // 红色波浪下划线 + 文本。
        view.session()
            .inject_program_output_for_test(b"\x1b[4:3m\x1b[58:2::255:0:0mWAVY\x1b[0m");
        // 双下划线 + 文本。
        view.session()
            .inject_program_output_for_test(b"\x1b[4:2mDOUBLE\x1b[0m");
        let term = view.session().term();
        let guard = term.lock();
        let mut content = guard.renderable_content();
        assert!(
            content.display_iter.any(|item| item.cell.c == 'W'),
            "VT 层应收到波浪线文本"
        );
        drop(guard);
        // 渲染管线：跑一帧 build_line_data（走 show 太重，直接调行构建）。
        // 简化：断 VT flags 进了段（经 inject 的 Term 状态）。
        let term = view.session().term();
        let guard = term.lock();
        let mut saw_curly = false;
        let mut saw_double = false;
        for item in guard.renderable_content().display_iter {
            let flags = item.cell.flags;
            if item.cell.c == 'W'
                && flags.contains(alacritty_terminal::term::cell::Flags::UNDERCURL)
            {
                saw_curly = true;
            }
            if item.cell.c == 'D'
                && flags.contains(alacritty_terminal::term::cell::Flags::DOUBLE_UNDERLINE)
            {
                saw_double = true;
            }
        }
        assert!(saw_curly, "波浪线 flag 应到达 cell");
        assert!(saw_double, "双下划线 flag 应到达 cell");
        // 段构建：变体 + SGR58 色进 Segment（全网格扫描定位 W/D 行，
        // `renderable_content` 的 display 行号 ≠ grid 行号，不能硬编码 0）。
        let grid = guard.grid();
        let colors = guard.colors();
        let fg = colors[alacritty_terminal::vte::ansi::NamedColor::Foreground]
            .unwrap_or(crate::theme::current_theme().term_fg);
        let bg = crate::theme::current_theme().term_bg;
        let mut wavy_line: Option<i32> = None;
        let mut dbl_line: Option<i32> = None;
        for line in -(grid.history_size() as i32)..grid.screen_lines() as i32 {
            let row = &grid[if line >= 0 {
                alacritty_terminal::index::Line::from(line as usize)
            } else {
                alacritty_terminal::index::Line::from(0) - line.unsigned_abs() as usize
            }];
            let text: String = row.into_iter().map(|c| c.c).collect();
            if text.contains('W') {
                wavy_line = Some(line);
            }
            if text.contains('D') {
                dbl_line = Some(line);
            }
        }
        let data = build_line_data(
            grid,
            wavy_line.expect("网格里应有 W 行"),
            80,
            colors,
            fg,
            bg,
            Color32::from_rgb(bg.r, bg.g, bg.b),
        );
        let wavy = data.segments.iter().find(|s| s.text.contains('W'));
        let data2 = build_line_data(
            grid,
            dbl_line.expect("网格里应有 D 行"),
            80,
            colors,
            fg,
            bg,
            Color32::from_rgb(bg.r, bg.g, bg.b),
        );
        let dbl = data2.segments.iter().find(|s| s.text.contains('D'));
        assert_eq!(
            wavy.map(|s| s.underline),
            Some(UnderlineStyle::Curly),
            "波浪线应进段样式"
        );
        assert_eq!(
            wavy.and_then(|s| s.underline_color),
            Some(Color32::from_rgb(255, 0, 0)),
            "SGR58 红色应进段下划线色"
        );
        assert_eq!(
            dbl.map(|s| s.underline),
            Some(UnderlineStyle::Double),
            "双下划线应进段样式"
        );
    }

    /// 回归：DECRQM 查 2026 必须回“不支持”（`CSI ? 2026 ; 0 $ y`），不能回
    /// “已重置”（`…; 2 $ y`）。回 2 会让 omp 判定终端支持同步更新并全程包
    /// BSU/ESU，而 alacritty 0.26 的 VT 层对 2026 是空实现（set/unset 都是
    /// `()`），回执与能力不一致。
    #[test]
    fn 同步更新查询回不支持() {
        use mino_core::terminal::{Session, SessionOptions};
        use std::sync::Arc;
        let session = Session::spawn_local(
            SessionOptions::default(),
            80,
            24,
            Arc::new(|_ev: &SessionEvent| {}),
        )
        .expect("创建本地终端失败");
        let view = TerminalView::new(session);
        // DECRQM 2026（`CSI ? 2026 $ p`）：VT 层经 PtyWrite 直接回写。
        view.session()
            .inject_program_output_for_test(b"\x1b[?2026$p");
        let events = view.session().drain_events();
        let reply: String = events
            .iter()
            .filter_map(|e| match e {
                SessionEvent::PtyWrite(text) => Some(text.clone()),
                _ => None,
            })
            .collect();
        assert!(
            reply.contains("\x1b[?2026;0$y"),
            "2026 应回不支持（0），实际：{reply:?}"
        );
    }

    #[test]
    fn 小幅point滚轮不会被截断() {
        // 滚轮是“意图”而非距离：任何有效的滚轮事件都只发一次，由 Vim 自己
        // 决定滚动行数；大 delta 不再按像素折算成多次，避免一次手势翻过几屏。
        assert_eq!(mouse_wheel_steps(egui::MouseWheelUnit::Point, 0.25), 1);
        assert_eq!(mouse_wheel_steps(egui::MouseWheelUnit::Point, 80.0), 1);
        assert_eq!(mouse_wheel_steps(egui::MouseWheelUnit::Line, -3.0), 3);
        assert_eq!(mouse_wheel_steps(egui::MouseWheelUnit::Line, -30.0), 3);
        assert_eq!(mouse_wheel_steps(egui::MouseWheelUnit::Page, 1.0), 1);
        assert_eq!(mouse_wheel_steps(egui::MouseWheelUnit::Point, 0.0), 0);
    }
}

#[cfg(test)]
mod enter_tests {
    use super::*;
    use mino_core::terminal::{Session, SessionOptions};
    use std::cell::RefCell;
    use std::rc::Rc;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    fn grid_text(session: &Session) -> String {
        use alacritty_terminal::term::cell::Flags;
        let term_arc = session.term();
        let guard = term_arc.lock();
        let content = guard.renderable_content();
        let mut lines: Vec<String> = Vec::new();
        let mut current = String::new();
        let mut started = false;
        let mut prev_grid_line: i32 = i32::MIN;
        for item in content.display_iter {
            let cell = item.cell;
            if cell.flags.contains(Flags::WIDE_CHAR_SPACER) || cell.flags.contains(Flags::HIDDEN) {
                continue;
            }
            if item.point.line.0 != prev_grid_line {
                if started {
                    lines.push(current.trim_end().to_string());
                }
                current = String::new();
                started = true;
                prev_grid_line = item.point.line.0;
            }
            current.push(cell.c);
        }
        if started {
            lines.push(current.trim_end().to_string());
        }
        lines.join("\n")
    }

    /// 回车应执行已输入的命令（回归测试：用户报告回车不执行）。
    #[test]
    fn 回车执行命令() {
        let session = Session::spawn_local(
            SessionOptions::default(),
            80,
            24,
            Arc::new(|_ev: &SessionEvent| {}),
        )
        .expect("创建本地终端失败");
        let view = Rc::new(RefCell::new(TerminalView::new(session)));
        let view_show = view.clone();
        let mut harness = egui_kittest::Harness::new_ui(move |ui| {
            view_show.borrow_mut().show(ui);
        });

        // 等待 zsh 就绪。
        let deadline = Instant::now() + Duration::from_secs(8);
        let mut ready = false;
        while Instant::now() < deadline {
            harness.step();
            if grid_text(view.borrow().session()).contains("mino") {
                ready = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(60));
        }
        assert!(ready, "zsh 未就绪");

        // 输入 echo HELLO。
        for (key, ch) in [
            (egui::Key::E, "e"),
            (egui::Key::C, "c"),
            (egui::Key::H, "h"),
            (egui::Key::O, "o"),
        ] {
            harness.event(egui::Event::Key {
                key,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::default(),
            });
            harness.event(egui::Event::Text(ch.to_string()));
        }
        harness.event(egui::Event::Text(" ".to_string()));
        for (key, ch) in [
            (egui::Key::H, "h"),
            (egui::Key::E, "e"),
            (egui::Key::L, "l"),
            (egui::Key::L, "l"),
            (egui::Key::O, "o"),
        ] {
            harness.event(egui::Event::Key {
                key,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::default(),
            });
            harness.event(egui::Event::Text(ch.to_string()));
        }

        // 按回车。
        harness.event(egui::Event::Key {
            key: egui::Key::Enter,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::default(),
        });

        // 等待 HELLO 输出出现（命令被执行）。
        let deadline = Instant::now() + Duration::from_secs(8);
        let mut executed = false;
        while Instant::now() < deadline {
            harness.step();
            if grid_text(view.borrow().session()).contains("hello") {
                executed = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(60));
        }
        assert!(
            executed,
            "回车未执行命令，终端内容：\n{}",
            grid_text(view.borrow().session())
        );
    }

    /// 回归：粘贴 cd 会让输入模型失效，随后执行 pwd 仍应以终端实际输出
    /// 校正当前目录，不能继续把 SFTP 定位在启动目录。
    #[test]
    fn pwd输出校正粘贴cd后的目录() {
        let base =
            std::env::temp_dir().join(format!("mino-terminal-pwd-test-{}", std::process::id()));
        std::fs::create_dir_all(&base).unwrap();
        let session = Session::spawn_local(
            SessionOptions {
                working_directory: Some(std::env::temp_dir()),
                ..Default::default()
            },
            80,
            24,
            Arc::new(|_ev: &SessionEvent| {}),
        )
        .expect("创建本地终端失败");
        let view = Rc::new(RefCell::new(TerminalView::new(session)));
        let view_show = view.clone();
        let mut harness = egui_kittest::Harness::new_ui(move |ui| {
            view_show.borrow_mut().show(ui);
        });
        harness.run_steps(12);

        // 用独立输出确认 shell 已经可以接收输入，不能用目录名中的
        // “mino”作为就绪条件（测试临时目录本身也可能含有该字符串）。
        harness.event(egui::Event::Text("printf __MINO_TERMINAL_READY__".into()));
        harness.event(egui::Event::Key {
            key: egui::Key::Enter,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::default(),
        });
        let ready_deadline = Instant::now() + Duration::from_secs(8);
        let mut ready = false;
        while Instant::now() < ready_deadline {
            harness.step();
            if grid_text(view.borrow().session()).contains("__MINO_TERMINAL_READY__") {
                ready = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(60));
        }
        assert!(ready, "zsh 未就绪");

        // 粘贴 cd，模拟截图中的“跟踪器此前已经失效”场景。
        harness.event(egui::Event::Paste(format!("cd {}", base.display())));
        harness.event(egui::Event::Key {
            key: egui::Key::Enter,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::default(),
        });
        harness.run_steps(6);

        // 只依赖 pwd 输出恢复，不依赖输入模型重新推导 cd。
        harness.event(egui::Event::Text("pwd".into()));
        harness.event(egui::Event::Key {
            key: egui::Key::Enter,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::default(),
        });
        let expected = std::fs::canonicalize(&base).unwrap();
        let expected_text = expected.to_string_lossy().into_owned();
        let deadline = Instant::now() + Duration::from_secs(8);
        let mut corrected = false;
        while Instant::now() < deadline {
            harness.step();
            if view.borrow().current_directory().as_deref() == Some(expected_text.as_str()) {
                corrected = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(60));
        }
        assert!(
            corrected,
            "pwd 输出后目录未校正，当前目录：{:?}",
            view.borrow().current_directory()
        );
        std::fs::remove_dir_all(base).ok();
    }

    /// 回归（用户报告“定位只有 pwd 后才好用”）：输入跟踪失效后，
    /// `request_fresh_pwd` 应自动注入 `pwd` 并把目录校正到真实值，
    /// 不需要用户先手输一次 `pwd`。
    #[test]
    fn 定位自动pwd探测校正目录() {
        let base =
            std::env::temp_dir().join(format!("mino-terminal-auto-pwd-{}", std::process::id()));
        std::fs::create_dir_all(&base).unwrap();
        let session = Session::spawn_local(
            SessionOptions {
                working_directory: Some(std::env::temp_dir()),
                ..Default::default()
            },
            80,
            24,
            Arc::new(|_ev: &SessionEvent| {}),
        )
        .expect("创建本地终端失败");
        let view = Rc::new(RefCell::new(TerminalView::new(session)));
        let view_show = view.clone();
        let mut harness = egui_kittest::Harness::new_ui(move |ui| {
            view_show.borrow_mut().show(ui);
        });
        harness.run_steps(12);

        harness.event(egui::Event::Text("printf __MINO_TERMINAL_READY__".into()));
        harness.event(egui::Event::Key {
            key: egui::Key::Enter,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::default(),
        });
        let ready_deadline = Instant::now() + Duration::from_secs(8);
        let mut ready = false;
        while Instant::now() < ready_deadline {
            harness.step();
            if grid_text(view.borrow().session()).contains("__MINO_TERMINAL_READY__") {
                ready = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(60));
        }
        assert!(ready, "zsh 未就绪");

        // 粘贴 cd 让跟踪器失效（与线上“别名/函数/补全后定位不准”同根因）。
        harness.event(egui::Event::Paste(format!("cd {}", base.display())));
        harness.event(egui::Event::Key {
            key: egui::Key::Enter,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::default(),
        });
        harness.run_steps(6);

        // 此时跟踪器仍停在旧目录；定位探测应自动注入 pwd 并校正。
        let before = view.borrow().current_directory();
        let expected = std::fs::canonicalize(&base).unwrap();
        let expected_text = expected.to_string_lossy().into_owned();
        assert_ne!(before.as_deref(), Some(expected_text.as_str()));

        // 探测注入需要经过一帧终端渲染（输出校正管线在 show 内）。
        assert!(view.borrow_mut().request_fresh_pwd());
        assert!(!view.borrow().auto_pwd_ready());
        let deadline = Instant::now() + Duration::from_secs(8);
        let mut corrected = false;
        while Instant::now() < deadline {
            harness.step();
            if view.borrow().auto_pwd_ready()
                && view.borrow().current_directory().as_deref() == Some(expected_text.as_str())
            {
                corrected = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(60));
        }
        assert!(
            corrected,
            "自动 pwd 探测后目录未校正，当前目录：{:?}",
            view.borrow().current_directory()
        );
        std::fs::remove_dir_all(base).ok();
    }

    /// 有未执行输入时不得注入 `pwd`（避免污染用户正在编辑的命令行）。
    #[test]
    fn 定位有输入时不注入pwd() {
        use mino_core::terminal::{Session, SessionOptions};
        use std::sync::Arc;

        let session = Session::spawn_local(
            SessionOptions::default(),
            80,
            24,
            Arc::new(|_ev: &SessionEvent| {}),
        )
        .expect("创建本地终端失败");
        let mut view = TerminalView::new(session);
        view.workdir.push_text("echo hi");
        assert!(!view.request_fresh_pwd(), "输入行非空时不应注入 pwd");
        assert!(view.auto_pwd_ready());
    }
}

#[cfg(test)]
mod osc_tests {
    use super::*;

    /// 浅色主题兼容：终端程序（如 zsh 主题）通过 OSC 11 设置深色背景时，
    /// 背景应强制跟随主题色（否则浅色主题下终端仍为深色）。
    #[test]
    #[allow(non_snake_case)]
    fn 背景色忽略OSC覆盖() {
        use alacritty_terminal::term::color::Colors as TermColors;
        use alacritty_terminal::vte::ansi::{Color as TermColor, NamedColor as Named};

        let mut colors = TermColors::default();
        // 模拟 zsh 主题发送 OSC 11 设置深色背景。
        colors[Named::Background] = Some(Rgb {
            r: 0x1a,
            g: 0x1a,
            b: 0x1a,
        });

        // 任意主题下：背景应为主题色，而非 OSC 深色。
        let theme_bg = crate::theme::current_theme().term_bg;
        let resolved = resolve_color(
            TermColor::Named(Named::Background),
            &colors,
            theme_bg,
            false,
        );
        assert_eq!(
            resolved,
            Color32::from_rgb(theme_bg.r, theme_bg.g, theme_bg.b),
            "背景应跟随主题，忽略 OSC 覆盖"
        );

        // 前景仍尊重 OSC（程序控制文字颜色是合理行为）。
        colors[Named::Foreground] = Some(Rgb {
            r: 0x00,
            g: 0xff,
            b: 0x00,
        });
        let resolved_fg = resolve_color(
            TermColor::Named(Named::Foreground),
            &colors,
            theme_bg,
            false,
        );
        assert_eq!(resolved_fg, Color32::from_rgb(0x00, 0xff, 0x00));
    }
}

#[cfg(test)]
mod paste_tests {
    use super::*;

    /// 括号粘贴内容中的伪造结束序列不能提前关闭协议边界。
    #[test]
    fn 括号粘贴移除内嵌转义字符() {
        let payload = bracketed_paste_payload("echo safe\x1b[201~\n下一行");
        assert_eq!(
            payload, "\x1b[200~echo safe[201~\n下一行\x1b[201~",
            "内嵌 ESC 应被移除，换行仍保留"
        );
        assert_eq!(
            payload.matches("\x1b[201~").count(),
            1,
            "载荷中只能保留由终端生成的结束标记"
        );
    }

    /// 图片粘贴接缝：Fake 剪贴板给像素 → 本地会话直接写 `@token`。
    #[test]
    fn 图片粘贴写入token() {
        use crate::clip_image::ClipboardReader;
        use mino_core::terminal::{Session, SessionOptions};
        use std::cell::RefCell;
        use std::rc::Rc;
        use std::sync::Arc;
        use std::time::{Duration, Instant};

        struct FakeClipboard;
        impl ClipboardReader for FakeClipboard {
            fn clipboard_text(&mut self) -> Option<String> {
                None
            }
            fn set_clipboard_text(&mut self, _text: &str) -> Result<(), String> {
                Ok(())
            }
            fn clipboard_file_paths(&self) -> Vec<std::path::PathBuf> {
                Vec::new()
            }
            fn clipboard_image(&mut self) -> Option<(usize, usize, Vec<u8>)> {
                Some((2, 1, vec![255u8; 8]))
            }
        }

        let session = Session::spawn_local(
            SessionOptions::default(),
            80,
            24,
            Arc::new(|_ev: &SessionEvent| {}),
        )
        .expect("创建本地终端失败");
        let view = Rc::new(RefCell::new(TerminalView::new(session)));
        view.borrow_mut()
            .set_clipboard_for_test(Box::new(FakeClipboard));
        let view_show = view.clone();
        let mut harness = egui_kittest::Harness::new_ui(move |ui| {
            view_show.borrow_mut().show(ui);
        });
        // 等 zsh 就绪。
        let deadline = Instant::now() + Duration::from_secs(8);
        let mut ready = false;
        while Instant::now() < deadline {
            harness.step();
            let text = tests_grid_text(view.borrow().session());
            if text.contains("mino") {
                ready = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(60));
        }
        assert!(ready, "zsh 未就绪");

        // 释放 Cmd+V 且无 Paste 事件 → 触发图片粘贴。
        // 焦点说明：`Harness::new_ui` 首帧即聚焦终端（`initialized` 分支），
        // `wait_text` 的步进已让焦点稳定；此处不再额外点按（点击会清选区，
        // 与图片逻辑无关）。若焦点丢失，`handle_input` 整段跳过是预期行为，
        // 用 `has_focus` 前置断言而非事后猜。
        assert!(
            harness
                .ctx
                .memory(|m| m.has_focus(egui::Id::new("terminal_view"))),
            "终端应持有焦点，否则图片粘贴不会触发"
        );
        harness.event(egui::Event::Key {
            key: egui::Key::V,
            physical_key: None,
            pressed: false,
            repeat: false,
            modifiers: egui::Modifiers::COMMAND,
        });
        // PTY 回显异步到达：轮询等待落盘文件名上屏。注意 token 中的 `@`
        // 可能被 zsh `oh-my-zsh` 主题渲染过滤，断言只看文件名与后缀。
        let deadline = Instant::now() + Duration::from_secs(8);
        while Instant::now() < deadline {
            harness.step();
            let text = tests_grid_text(view.borrow().session());
            if text.contains("mino-") && text.contains(".png") {
                break;
            }
            std::thread::sleep(Duration::from_millis(60));
        }

        let text = tests_grid_text(view.borrow().session());
        assert!(
            text.contains("mino-") && text.contains(".png"),
            "图片粘贴应写入落盘文件名，终端内容：\n{text}"
        );
        // 落盘文件真实存在：直接按落盘目录扫描（终端回显经 zsh 主题
        // 二次渲染，`@` 前缀与路径可能被截断/重排，不能从回显反推路径）。
        let staged: Vec<_> = std::fs::read_dir(crate::clip_image::paste_dir())
            .map(|entries| {
                entries
                    .filter_map(|entry| entry.ok())
                    .map(|entry| entry.path())
                    .filter(|path| path.extension().and_then(|e| e.to_str()) == Some("png"))
                    .collect()
            })
            .unwrap_or_default();
        assert!(
            !staged.is_empty(),
            "落盘目录应有 png：{}",
            crate::clip_image::paste_dir().display()
        );
        for path in staged {
            let _ = std::fs::remove_file(&path);
        }
    }

    /// 空剪贴板释放粘贴键：静默无操作，不写 PTY、不 toast。
    #[test]
    fn 空剪贴板释放粘贴键无操作() {
        use crate::clip_image::ClipboardReader;
        use mino_core::terminal::{Session, SessionOptions};
        use std::cell::RefCell;
        use std::rc::Rc;
        use std::sync::Arc;
        use std::time::{Duration, Instant};

        struct EmptyClipboard;
        impl ClipboardReader for EmptyClipboard {
            fn clipboard_text(&mut self) -> Option<String> {
                None
            }
            fn set_clipboard_text(&mut self, _text: &str) -> Result<(), String> {
                Ok(())
            }
            fn clipboard_file_paths(&self) -> Vec<std::path::PathBuf> {
                Vec::new()
            }
            fn clipboard_image(&mut self) -> Option<(usize, usize, Vec<u8>)> {
                None
            }
        }

        let session = Session::spawn_local(
            SessionOptions::default(),
            80,
            24,
            Arc::new(|_ev: &SessionEvent| {}),
        )
        .expect("创建本地终端失败");
        let view = Rc::new(RefCell::new(TerminalView::new(session)));
        view.borrow_mut()
            .set_clipboard_for_test(Box::new(EmptyClipboard));
        let view_show = view.clone();
        let mut harness = egui_kittest::Harness::new_ui(move |ui| {
            view_show.borrow_mut().show(ui);
        });
        let deadline = Instant::now() + Duration::from_secs(8);
        while Instant::now() < deadline {
            harness.step();
            if tests_grid_text(view.borrow().session()).contains("mino") {
                break;
            }
            std::thread::sleep(Duration::from_millis(60));
        }
        harness.event(egui::Event::Key {
            key: egui::Key::V,
            physical_key: None,
            pressed: false,
            repeat: false,
            modifiers: egui::Modifiers::COMMAND,
        });
        harness.run_steps(6);
        assert!(
            view.borrow_mut().take_image_paste_error().is_none(),
            "空剪贴板不应产生错误"
        );
        assert!(
            view.borrow_mut().take_pending_image().is_none(),
            "空剪贴板不应产生待上传"
        );
    }
}

#[cfg(test)]
mod background_tests {
    use super::*;

    /// 同色背景被默认背景隔开时不能跨越中间列合并。
    #[test]
    fn 背景段只合并相邻列() {
        let default = Color32::BLACK;
        let accent = Color32::from_rgb(10, 20, 30);
        let mut backgrounds = Vec::new();
        push_background(&mut backgrounds, 0, accent, default);
        push_background(&mut backgrounds, 1, default, default);
        push_background(&mut backgrounds, 2, accent, default);

        assert_eq!(backgrounds.len(), 2);
        assert_eq!((backgrounds[0].start, backgrounds[0].end), (0, 1));
        assert_eq!((backgrounds[1].start, backgrounds[1].end), (2, 3));
    }

    /// 显式背景变化必须使行缓存指纹变化，即使文本和前景完全相同。
    #[test]
    fn 背景色参与行指纹() {
        let fg = Color32::WHITE;
        let first = style_key(
            fg,
            Color32::BLACK,
            false,
            false,
            UnderlineStyle::None,
            None,
            false,
        );
        let second = style_key(
            fg,
            Color32::from_rgb(1, 2, 3),
            false,
            false,
            UnderlineStyle::None,
            None,
            false,
        );
        assert_ne!(first, second);
    }
}

#[cfg(test)]
mod cell_semantics_tests {
    use super::*;
    use alacritty_terminal::grid::Grid;
    use alacritty_terminal::index::{Column, Line};
    use alacritty_terminal::term::cell::Cell;

    fn selection(start: (i32, usize), end: (i32, usize)) -> TerminalSelection {
        TerminalSelection {
            anchor: SelectionPoint {
                grid_line: start.0,
                col: start.1,
            },
            focus: SelectionPoint {
                grid_line: end.0,
                col: end.1,
            },
        }
    }

    #[test]
    fn 组合字符随主字符渲染和复制() {
        let mut grid = Grid::<Cell>::new(2, 6, 0);
        grid[Line(0)][Column(0)].c = 'e';
        grid[Line(0)][Column(0)].push_zerowidth('\u{301}');
        grid[Line(0)][Column(1)].c = 'x';

        let text = selection_to_text(&grid, selection((0, 0), (0, 1)), 6);
        assert_eq!(text, "e\u{301}x");
    }

    #[test]
    fn 隐藏字符保留等宽空白() {
        let mut grid = Grid::<Cell>::new(2, 6, 0);
        grid[Line(0)][Column(0)].c = 'a';
        grid[Line(0)][Column(1)].c = 'x';
        grid[Line(0)][Column(1)].flags.insert(Flags::HIDDEN);
        grid[Line(0)][Column(2)].c = 'b';

        let text = selection_to_text(&grid, selection((0, 0), (0, 2)), 6);
        assert_eq!(text, "a b");
    }

    #[test]
    fn 软换行不插入额外换行符() {
        let mut grid = Grid::<Cell>::new(2, 4, 0);
        for (column, c) in "abcd".chars().enumerate() {
            grid[Line(0)][Column(column)].c = c;
        }
        grid[Line(0)][Column(3)].flags.insert(Flags::WRAPLINE);
        grid[Line(1)][Column(0)].c = 'e';

        let text = selection_to_text(&grid, selection((0, 0), (1, 0)), 4);
        assert_eq!(text, "abcde");
    }

    #[test]
    fn 局部选择保留有意义的尾随空格() {
        let mut grid = Grid::<Cell>::new(2, 6, 0);
        grid[Line(0)][Column(0)].c = 'a';
        grid[Line(0)][Column(1)].c = ' ';
        grid[Line(0)][Column(2)].c = 'b';

        let text = selection_to_text(&grid, selection((0, 0), (0, 1)), 6);
        assert_eq!(text, "a ");
    }

    #[test]
    fn 过期选区越界时安全返回空串() {
        // 选区是建立时的 grid_line 快照；resize/scrollback 裁剪后网格缩小，
        // 快照可能悬空。alacritty Storage 越界防护仅 debug_assert，
        // release 下索引越界会 panic——越界时须安全返回空串（放弃复制）。
        let grid = Grid::<Cell>::new(2, 6, 0);
        // 网格只有 2 行可视 + 0 行 scrollback，快照却引用第 5 行。
        assert_eq!(selection_to_text(&grid, selection((5, 0), (5, 1)), 6), "");
        // 快照引用 scrollback 深处（history=0 时负行号同样越界）。
        assert_eq!(selection_to_text(&grid, selection((-3, 0), (-3, 1)), 6), "");
        // 列快照为空或大于当前网格宽度时，同样放弃复制，避免 Column(cols - 1) 越界。
        assert_eq!(selection_to_text(&grid, selection((0, 0), (0, 1)), 0), "");
        assert_eq!(selection_to_text(&grid, selection((0, 0), (0, 1)), 7), "");
    }

    #[test]
    fn 渲染器保留隐藏与跨行宽字符列位() {
        let theme = crate::theme::current_theme();
        let mut grid = Grid::<Cell>::new(1, 4, 0);
        grid[Line(0)][Column(0)].c = 'a';
        grid[Line(0)][Column(1)].c = 'x';
        grid[Line(0)][Column(1)].flags.insert(Flags::HIDDEN);
        grid[Line(0)][Column(2)].c = 'b';
        grid[Line(0)][Column(3)]
            .flags
            .insert(Flags::LEADING_WIDE_CHAR_SPACER);

        let data = build_line_data(
            &grid,
            0,
            4,
            &Colors::default(),
            theme.term_fg,
            theme.term_bg,
            to_egui(theme.term_bg),
        );
        let text: String = data
            .segments
            .iter()
            .map(|segment| segment.text.as_str())
            .collect();
        assert_eq!(text, "a b ");
    }

    /// 回归：中文输入越多光标漂移越远——整行 LayoutJob 按 fallback 字体实际
    /// advance 排字，CJK 实际宽度 ≠ 2×cell，后续字符整体左移。
    /// 修复要求：宽字符**每个 cell 单独成段**、半角另起新段，分段按终端列定位绘制。
    /// 宽字符不能合并同类段：段内仍按字体实际 advance 排字，CJK 的 1em（13px）
    /// 比双列（2×cell_width ≈ 16.1px）窄，连续排字每字少 3.1px，越长漂越远。
    #[test]
    fn 中文与半角分段列定位() {
        let theme = crate::theme::current_theme();
        let mut grid = Grid::<Cell>::new(1, 8, 0);
        // "现在ab"：中(0,宽) 中(2,宽) a(4) b(5)。
        grid[Line(0)][Column(0)].c = '现';
        grid[Line(0)][Column(0)].flags.insert(Flags::WIDE_CHAR);
        grid[Line(0)][Column(1)].c = ' ';
        grid[Line(0)][Column(1)]
            .flags
            .insert(Flags::WIDE_CHAR_SPACER);
        grid[Line(0)][Column(2)].c = '在';
        grid[Line(0)][Column(2)].flags.insert(Flags::WIDE_CHAR);
        grid[Line(0)][Column(3)].c = ' ';
        grid[Line(0)][Column(3)]
            .flags
            .insert(Flags::WIDE_CHAR_SPACER);
        grid[Line(0)][Column(4)].c = 'a';
        grid[Line(0)][Column(5)].c = 'b';

        let data = build_line_data(
            &grid,
            0,
            8,
            &Colors::default(),
            theme.term_fg,
            theme.term_bg,
            to_egui(theme.term_bg),
        );
        // 每个宽字符独立成段（按自身双列起点定位），半角自成一列：[现][在][ab  ]。
        let texts: Vec<&str> = data.segments.iter().map(|s| s.text.as_str()).collect();
        assert_eq!(texts, vec!["现", "在", "ab  "], "宽字符须逐 cell 成段");
        let cols: Vec<usize> = data.segments.iter().map(|s| s.start_col).collect();
        assert_eq!(cols, vec![0, 2, 4], "分段起点 = 终端列");
        assert!(data.segments[0].is_wide && data.segments[1].is_wide);
        assert!(!data.segments[2].is_wide);
        // 每个宽字符段都是单字符 → 可走 (字符, 样式) Galley 缓存。
        assert_eq!(wide_glyph_key(&data.segments[0]).map(|k| k.0), Some('现'));
        assert_eq!(wide_glyph_key(&data.segments[1]).map(|k| k.0), Some('在'));
        assert_eq!(wide_glyph_key(&data.segments[2]), None);
    }

    /// 同色纯半角仍合并为一段（分段绘制不增加 layout 开销）。
    #[test]
    fn 纯半角行合并为一段() {
        let theme = crate::theme::current_theme();
        let mut grid = Grid::<Cell>::new(1, 4, 0);
        for (i, c) in "abcd".chars().enumerate() {
            grid[Line(0)][Column(i)].c = c;
        }
        let data = build_line_data(
            &grid,
            0,
            4,
            &Colors::default(),
            theme.term_fg,
            theme.term_bg,
            to_egui(theme.term_bg),
        );
        assert_eq!(data.segments.len(), 1);
        assert_eq!(data.segments[0].text, "abcd");
        assert_eq!(data.segments[0].start_col, 0);
    }
}

#[cfg(test)]
mod ime_backspace_tests {
    use super::*;
    use mino_core::terminal::{Session, SessionOptions};
    use std::cell::RefCell;
    use std::rc::Rc;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    fn grid_text(session: &Session) -> String {
        use alacritty_terminal::term::cell::Flags;
        let term_arc = session.term();
        let guard = term_arc.lock();
        let content = guard.renderable_content();
        let mut lines: Vec<String> = Vec::new();
        let mut current = String::new();
        let mut started = false;
        let mut prev_grid_line: i32 = i32::MIN;
        for item in content.display_iter {
            let cell = item.cell;
            if cell.flags.contains(Flags::WIDE_CHAR_SPACER) || cell.flags.contains(Flags::HIDDEN) {
                continue;
            }
            if item.point.line.0 != prev_grid_line {
                if started {
                    lines.push(current.trim_end().to_string());
                }
                current = String::new();
                started = true;
                prev_grid_line = item.point.line.0;
            }
            current.push(cell.c);
        }
        if started {
            lines.push(current.trim_end().to_string());
        }
        lines.join("\n")
    }

    fn send_key(harness: &mut egui_kittest::Harness, key: egui::Key, text: Option<&str>) {
        harness.event(egui::Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::default(),
        });
        if let Some(t) = text {
            harness.event(egui::Event::Text(t.to_string()));
        }
    }

    /// 回归测试（用户报告"删除键插入空格"）：输入法（如微信输入法）在退格时
    /// 伴随发送"空格" Text 事件，不应插入空格。
    #[test]
    fn 退格伴随空格文本不插入() {
        let session = Session::spawn_local(
            SessionOptions::default(),
            80,
            24,
            Arc::new(|_ev: &SessionEvent| {}),
        )
        .expect("创建本地终端失败");
        let view = Rc::new(RefCell::new(TerminalView::new(session)));
        let view_show = view.clone();
        let mut harness = egui_kittest::Harness::new_ui(move |ui| {
            view_show.borrow_mut().show(ui);
        });

        // 等 zsh 就绪。
        let deadline = Instant::now() + Duration::from_secs(8);
        let mut ready = false;
        while Instant::now() < deadline {
            harness.step();
            if grid_text(view.borrow().session()).contains("mino") {
                ready = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(60));
        }
        assert!(ready, "zsh 未就绪");

        // 输入 abc。
        send_key(&mut harness, egui::Key::A, Some("a"));
        send_key(&mut harness, egui::Key::B, Some("b"));
        send_key(&mut harness, egui::Key::C, Some("c"));
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut typed = false;
        while Instant::now() < deadline {
            harness.step();
            if grid_text(view.borrow().session()).contains("abc") {
                typed = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(60));
        }
        assert!(typed, "输入 abc 失败");

        // 退格（伴随空格 Text——输入法产物）。
        send_key(&mut harness, egui::Key::Backspace, Some(" "));
        // 等待 zsh 回显更新为 ab。
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut deleted = false;
        while Instant::now() < deadline {
            harness.step();
            let text = grid_text(view.borrow().session());
            if text.lines().any(|l| l.ends_with("ab")) {
                deleted = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(60));
        }
        assert!(
            deleted,
            "退格后应为 ab，终端内容：\n{}",
            grid_text(view.borrow().session())
        );

        // 不得出现"ab "（退格伴随的空格被丢弃）。
        let text = grid_text(view.borrow().session());
        let last_line = text.lines().last().unwrap_or("");
        assert!(
            !last_line.contains("ab "),
            "退格不应插入空格，最后一行：{last_line:?}"
        );
    }

    /// 中文输入法组字不提前上屏：Preedit 只存不写，Commit 才写入终端。
    ///
    /// 回归用户报告"中文输入法状态下输入不了中文、进去的都是英文"：
    /// 组字期的拼音字母若当普通 Text 写入 PTY，命令行会被拼音污染；
    /// Commit 的中文若被过滤/丢弃，用户看到的就只剩英文。
    #[test]
    #[allow(non_snake_case)]
    fn 中文输入法组字与上屏() {
        let session = Session::spawn_local(
            SessionOptions::default(),
            80,
            24,
            Arc::new(|_ev: &SessionEvent| {}),
        )
        .expect("创建本地终端失败");
        let view = Rc::new(RefCell::new(TerminalView::new(session)));
        let view_show = view.clone();
        let mut harness = egui_kittest::Harness::new_ui(move |ui| {
            view_show.borrow_mut().show(ui);
        });

        // 等 zsh 就绪。
        let deadline = Instant::now() + Duration::from_secs(8);
        let mut ready = false;
        while Instant::now() < deadline {
            harness.step();
            if grid_text(view.borrow().session()).contains("mino") {
                ready = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(60));
        }
        assert!(ready, "zsh 未就绪");

        // 1. 组字期：Preedit("ni") + 拼音字母的零散 Text 都不应写入终端。
        harness.event(egui::Event::Ime(egui::ImeEvent::Preedit {
            text: "ni".to_string(),
            active_range_chars: Some(0..2),
        }));
        harness.event(egui::Event::Text("n".to_string()));
        harness.event(egui::Event::Text("i".to_string()));
        harness.run_steps(6);
        assert_eq!(
            view.borrow().ime_preedit.as_ref().map(|p| p.text.as_str()),
            Some("ni"),
            "预编辑串应暂存待渲染"
        );
        let text = grid_text(view.borrow().session());
        assert!(
            !text.lines().any(|l| l.contains("ni")),
            "组字期拼音不应写入终端，终端内容：\n{text}"
        );

        // 2. 选词上屏：Commit("你") 必须写入终端并回显。
        harness.event(egui::Event::Ime(egui::ImeEvent::Commit("你".to_string())));
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut committed = false;
        while Instant::now() < deadline {
            harness.step();
            if grid_text(view.borrow().session()).contains("你") {
                committed = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(60));
        }
        assert!(
            committed,
            "Commit 中文应写入终端，终端内容：\n{}",
            grid_text(view.borrow().session())
        );
        assert!(view.borrow().ime_preedit.is_none(), "上屏后预编辑串应清空");

        // 3. 取消组字：空 Preedit 不写 PTY、不留痕。
        harness.event(egui::Event::Ime(egui::ImeEvent::Preedit {
            text: "hao".to_string(),
            active_range_chars: Some(0..3),
        }));
        harness.run_steps(3);
        assert!(view.borrow().ime_preedit.is_some());
        harness.event(egui::Event::Ime(egui::ImeEvent::Preedit {
            text: String::new(),
            active_range_chars: None,
        }));
        harness.run_steps(3);
        assert!(
            view.borrow().ime_preedit.is_none(),
            "空 Preedit 应清空预编辑串"
        );
        let text = grid_text(view.borrow().session());
        assert!(
            !text.contains("hao"),
            "取消的组字串不应写入终端，终端内容：\n{text}"
        );
    }

    /// 终端聚焦时每帧声明 IME 意图（`PlatformOutput::ime`），否则真机上
    /// `winit::Window::set_ime_allowed(true)` 永不触发、中文输入法起不来。
    ///
    /// 回归用户报告"输入法是中文状态但输入不了中文"的另一半根因：
    /// 只处理 Commit 事件不够——OS 根本不会发组字事件。
    #[test]
    #[allow(clippy::single_range_in_vec_init, non_snake_case)]
    fn 聚焦终端声明IME意图() {
        let session = Session::spawn_local(
            SessionOptions::default(),
            80,
            24,
            Arc::new(|_ev: &SessionEvent| {}),
        )
        .expect("创建本地终端失败");
        let view = Rc::new(RefCell::new(TerminalView::new(session)));
        let view_show = view.clone();
        let mut harness = egui_kittest::Harness::new_ui(move |ui| {
            view_show.borrow_mut().show(ui);
        });
        harness.run_steps(6);
        let ime = harness.output().platform_output.ime;
        assert!(
            ime.is_some(),
            "聚焦的终端必须声明 IME 意图，否则输入法无法激活"
        );
        let ime = ime.expect("已断言 Some");
        assert_eq!(
            ime.purpose,
            egui::IMEPurpose::Terminal,
            "终端 IME 应用途声明 Terminal 语义"
        );
        // egui-winit 0.36 只用 `rect` 定位候选窗（忽略 cursor_rect）：
        // rect 必须是光标 cell 级小矩形。曾传整个终端区域，候选窗落在
        // 终端左下角一带、远离实际输入位置（用户报告"输入法不在输入位置附近"）。
        let (cell_w, cell_h) = (view.borrow().cell_width, view.borrow().cell_height);
        assert!(cell_w > 0.0 && cell_h > 0.0, "cell 尺寸应在首帧后就绪");
        assert!(
            (ime.rect.width() - cell_w).abs() < 1.0 && (ime.rect.height() - cell_h).abs() < 1.0,
            "IME rect 应为光标 cell 大小（{cell_w:.1}x{cell_h:.1}），实际：{:?}",
            ime.rect
        );
        assert_eq!(
            ime.rect, ime.cursor_rect,
            "rect 与 cursor_rect 应一致（后端只消费 rect）"
        );
    }

    /// 终端光标位置快照（网格行，网格列，display_offset）。
    fn cursor_disp_pos(session: &Session) -> (i32, i32, usize) {
        let term_arc = session.term();
        let guard = term_arc.lock();
        let content = guard.renderable_content();
        (
            content.cursor.point.line.0,
            content.cursor.point.column.0 as i32,
            content.display_offset,
        )
    }

    /// 候选窗跟随终端光标：egui-winit 只用 `IMEOutput.rect` 调
    /// `set_ime_cursor_area` 定位候选窗，光标移动后 rect 必须同步移动。
    ///
    /// 回归用户报告"输入法不在输入位置附近、而在终端左下方"：
    /// 根因是 rect 曾传整个终端区域，macOS 按该矩形左下角放候选窗。
    #[test]
    #[allow(non_snake_case)]
    fn IME候选窗跟随终端光标() {
        let session = Session::spawn_local(
            SessionOptions::default(),
            80,
            24,
            Arc::new(|_ev: &SessionEvent| {}),
        )
        .expect("创建本地终端失败");
        let view = Rc::new(RefCell::new(TerminalView::new(session)));
        let view_show = view.clone();
        let mut harness = egui_kittest::Harness::new_ui(move |ui| {
            view_show.borrow_mut().show(ui);
        });
        // 等 shell 启动输出稳定（光标连续多帧不动），否则 prompt 绘制
        // 干扰"移动前后"的差值比较。
        let deadline = Instant::now() + Duration::from_secs(8);
        let mut last = cursor_disp_pos(view.borrow().session());
        let mut stable = 0;
        while stable < 3 && Instant::now() < deadline {
            harness.step();
            std::thread::sleep(Duration::from_millis(100));
            let cur = cursor_disp_pos(view.borrow().session());
            if cur == last {
                stable += 1;
            } else {
                stable = 0;
                last = cur;
            }
        }
        let before = harness
            .output()
            .platform_output
            .ime
            .expect("聚焦终端应声明 IME 意图")
            .rect;
        let (r0, c0, o0) = last;
        // 右移 5 列、下移 2 行（CSI 直接进 PTY，不依赖 shell 回显）。
        view.borrow().session().write(b"\x1b[5C\x1b[2B");
        std::thread::sleep(Duration::from_millis(200));
        harness.run_steps(6);
        let after = harness
            .output()
            .platform_output
            .ime
            .expect("移动后仍应声明 IME 意图")
            .rect;
        let (r1, c1, o1) = cursor_disp_pos(view.borrow().session());
        assert_ne!((r1, c1), (r0, c0), "CSI 光标移动未生效，差值比较无意义");
        let (cell_w, cell_h) = (view.borrow().cell_width, view.borrow().cell_height);
        assert!(
            ((after.min.x - before.min.x) - (c1 - c0) as f32 * cell_w).abs() < 1.0,
            "候选窗应随光标列移动：列差 {}，rect x 差 {:.1}，实际 {:?} / {:?}",
            c1 - c0,
            after.min.x - before.min.x,
            before,
            after
        );
        assert!(
            ((after.min.y - before.min.y) - ((r1 + o1 as i32) - (r0 + o0 as i32)) as f32 * cell_h)
                .abs()
                < 1.0,
            "候选窗应随光标行移动：显示行差 {}，rect y 差 {:.1}，实际 {:?} / {:?}",
            (r1 + o1 as i32) - (r0 + o0 as i32),
            after.min.y - before.min.y,
            before,
            after
        );
    }

    /// 前台弹窗打开（`input_enabled=false`）时不声明 IME：输入法候选窗
    /// 不应跟随一个不接受输入的后台终端。
    #[test]
    #[allow(non_snake_case)]
    fn 弹窗打开时不声明IME意图() {
        let session = Session::spawn_local(
            SessionOptions::default(),
            80,
            24,
            Arc::new(|_ev: &SessionEvent| {}),
        )
        .expect("创建本地终端失败");
        // show_with_input(false) 即设置弹窗打开时的渲染路径：终端仍渲染
        // 后台输出，但不消费输入、也不声明 IME。
        let view = Rc::new(RefCell::new(TerminalView::new(session)));
        let view_show = view.clone();
        let mut harness = egui_kittest::Harness::new_ui(move |ui| {
            view_show.borrow_mut().show_with_input(ui, false);
        });
        harness.run_steps(6);
        assert!(
            harness.output().platform_output.ime.is_none(),
            "后台终端不应声明 IME 意图"
        );
    }

    #[test]
    fn 修饰键回退映射保留移位符号() {
        assert_eq!(map_char_key(&egui::Key::Num1, true), Some(Key::Char('!')));
        assert_eq!(map_char_key(&egui::Key::Minus, true), Some(Key::Char('_')));
        assert_eq!(
            map_char_key(&egui::Key::OpenBracket, true),
            Some(Key::Char('{'))
        );
        assert_eq!(map_char_key(&egui::Key::Slash, true), Some(Key::Char('?')));
        assert_eq!(map_char_key(&egui::Key::Num1, false), Some(Key::Char('1')));
    }
}

#[cfg(test)]
mod query_response_tests {
    use super::*;
    use mino_core::terminal::{Session, SessionEvent, SessionOptions};
    use std::sync::Arc;

    fn test_view() -> TerminalView {
        let session = Session::spawn_local(
            SessionOptions {
                shell: Some("/bin/cat".to_string()),
                ..Default::default()
            },
            80,
            24,
            Arc::new(|_e: &SessionEvent| {}),
        )
        .expect("创建本地终端失败");
        TerminalView::new(session)
    }

    /// 程序查询终端配色时必须给出真实颜色。
    ///
    /// 索引语义（alacritty `term::color`）：0-255 调色板、256 前景、
    /// 257 背景、258 光标。曾因会话层丢弃 `Event::ColorRequest`，
    /// `printf '\e]11;?\a'` 永远收不到答复——查询终端背景色的 TUI
    /// （omp 启动即查 OSC 11）只能按“未知终端”回退。
    #[test]
    #[allow(non_snake_case)]
    fn 颜色查询按索引返回主题颜色() {
        let view = test_view();
        let theme = crate::theme::current_theme();
        assert_eq!(view.query_color(257), theme.term_bg, "257 应为背景色");
        assert_eq!(view.query_color(256), theme.term_fg, "256 应为前景色");
        assert_eq!(view.query_color(258), theme.term_cursor, "258 应为光标色");
        assert_eq!(
            view.query_color(1),
            crate::theme::xterm256(1, theme.term_palette),
            "0-255 应走调色板"
        );
        // 越界索引（Dim/Bright 变体）不能 panic，也不能编造颜色。
        assert_eq!(view.query_color(10_000), theme.term_fg);
    }

    /// OSC 覆盖（程序自己设过的颜色）优先于内置调色板——终端的答复
    /// 必须与实际渲染一致，否则查询方拿到的颜色和屏幕上看到的不是一回事。
    #[test]
    #[allow(non_snake_case)]
    fn 颜色查询优先使用OSC覆盖() {
        use std::time::{Duration, Instant};
        let script = std::env::temp_dir().join(format!("mino-osc-set-{}.sh", std::process::id()));
        std::fs::write(
            &script,
            "#!/bin/sh\nprintf '\\033]11;rgb:1111/2222/3333\\007'\nsleep 5\n",
        )
        .expect("写测试脚本失败");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).ok();
        }
        let session = Session::spawn_local(
            SessionOptions {
                shell: Some(script.to_string_lossy().into_owned()),
                ..Default::default()
            },
            80,
            24,
            Arc::new(|_e: &SessionEvent| {}),
        )
        .expect("创建本地终端失败");
        let view = TerminalView::new(session);
        let expected = Rgb {
            r: 0x11,
            g: 0x22,
            b: 0x33,
        };
        let deadline = Instant::now() + Duration::from_secs(8);
        let mut actual = view.query_color(257);
        while Instant::now() < deadline && actual != expected {
            std::thread::sleep(Duration::from_millis(30));
            actual = view.query_color(257);
        }
        assert_eq!(
            actual, expected,
            "OSC 11 设置的背景色应优先于主题色，实际：{actual:?}"
        );
    }
}

#[cfg(test)]
mod capability_response_tests {
    use super::*;
    use mino_core::terminal::{Session, SessionEvent, SessionOptions};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    /// python3 是否可用（端到端用例需要带超时地读 PTY）。
    fn python3_available() -> bool {
        std::process::Command::new("python3")
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    /// 端到端：程序发终端查询后，mino 必须把应答写回 PTY。
    ///
    /// 覆盖两条曾经完全缺失的链路：
    /// - OSC 11 背景色查询（`ColorRequest` 曾被会话层丢弃 → 永不应答）
    /// - `DECSET 1004` 焦点上报（窗口焦点变化时发 `ESC [ I` / `ESC [ O`）
    #[test]
    #[allow(non_snake_case)]
    fn 终端查询与焦点上报有应答() {
        if !python3_available() {
            eprintln!("跳过：缺少 python3");
            return;
        }
        let tag = std::process::id();
        let reply = std::env::temp_dir().join(format!("mino-cap-reply-{tag}.bin"));
        let script = std::env::temp_dir().join(format!("mino-cap-probe-{tag}"));
        let _ = std::fs::remove_file(&reply);
        // 脚本：声明自身已就绪 → 打开焦点上报并查询背景色 → 带超时累积 stdin。
        std::fs::write(
            &script,
            format!(
                r#"#!/usr/bin/env python3
import os, select, sys, termios, time, tty
path = {path:?}
fd = sys.stdin.fileno()
# PTY 默认为规范模式：不关掉 ICANON，无换行的转义序列应答会卡在行缓冲里。
tty.setcbreak(fd)
os.write(1, b"\x1b[?1004h\x1b]11;?\x07\x1b]0;ready\x07")
data = b""
deadline = time.time() + 10
while time.time() < deadline:
    ready, _, _ = select.select([fd], [], [], 0.1)
    if ready:
        chunk = os.read(fd, 4096)
        if not chunk:
            break
        data += chunk
        with open(path, "wb") as handle:
            handle.write(data)
        if b"\x1b[I" in data and b"]11;" in data:
            break
"#,
                path = reply.display().to_string()
            ),
        )
        .expect("写探测脚本失败");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).ok();
        }

        let session = Session::spawn_local(
            SessionOptions {
                shell: Some(script.to_string_lossy().into_owned()),
                working_directory: Some(std::env::temp_dir()),
                ..Default::default()
            },
            80,
            24,
            Arc::new(|_e: &SessionEvent| {}),
        )
        .expect("创建本地终端失败");
        let mut view = TerminalView::new(session);

        let deadline = Instant::now() + Duration::from_secs(12);
        let mut received = Vec::new();
        let mut focused = false;
        while Instant::now() < deadline {
            // 回写后台事件（颜色/尺寸应答走这条路径）。
            view.drain_background_events();
            // 用终端真实模式驱动（程序发了 `DECSET 1004` 后才会出现该位）。
            view.last_mode = {
                let term = view.session().term();
                let mode = term.lock().renderable_content().mode;
                mode
            };
            // 程序打开 DECSET 1004 后，模拟窗口先失焦再获焦。
            if view.last_mode.contains(TermMode::FOCUS_IN_OUT) && !focused {
                focused = true;
                view.report_focus_change(false);
                view.report_focus_change(true);
            }
            if let Ok(data) = std::fs::read(&reply) {
                received = data;
                if received.windows(3).any(|w| w == b"]11")
                    && received.windows(3).any(|w| w == b"\x1b[I")
                {
                    break;
                }
            }
            std::thread::sleep(Duration::from_millis(20));
        }

        let text = String::from_utf8_lossy(&received).into_owned();
        assert!(
            text.contains("]11;rgb:"),
            "OSC 11 背景色查询应有应答，实际收到：{text:?}"
        );
        assert!(
            text.contains("\x1b[I"),
            "DECSET 1004 获焦事件应上报，实际收到：{text:?}"
        );
        let _ = std::fs::remove_file(&reply);
        let _ = std::fs::remove_file(&script);
    }
}
