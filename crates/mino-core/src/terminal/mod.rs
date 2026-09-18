//! 终端会话：PTY + VT 仿真封装（基于 alacritty_terminal）。
//!
//! 本地会话由 alacritty 的 EventLoop 驱动（PTY 读取线程）；
//! 远程会话由自建 tokio 读循环驱动（SSH channel 数据 → vte 解析器）。

pub mod keys;

use std::collections::HashMap;
use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use alacritty_terminal::event::{Event, EventListener, WindowSize};
use alacritty_terminal::event_loop::{EventLoop, Msg};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::{self, Term};
use alacritty_terminal::tty;

pub use alacritty_terminal::term::TermMode;
use alacritty_terminal::vte::ansi::{Processor, StdSyncHandler};

/// 终端尺寸（实现 alacritty 的 `Dimensions`）。
#[derive(Clone, Copy, Debug)]
pub struct TermSize {
    pub rows: usize,
    pub cols: usize,
}

impl Dimensions for TermSize {
    fn total_lines(&self) -> usize {
        self.rows
    }

    fn screen_lines(&self) -> usize {
        self.rows
    }

    fn columns(&self) -> usize {
        self.cols
    }
}

/// 会话事件（后台线程 → UI 线程的通知）。
#[derive(Clone)]
pub enum SessionEvent {
    /// 终端有新内容，需要重绘。
    Wakeup,
    /// 窗口标题变化。
    Title(String),
    /// 子进程退出。
    ChildExit,
    /// 终端请求应用写入数据（如粘贴内容回传）。
    PtyWrite(String),
    /// 终端铃响。
    Bell,
    /// 程序查询某个颜色（OSC 4/10/11/12）。
    ///
    /// VT 仿真层不知道真实调色板（那是渲染层的主题），所以只把查询交给
    /// UI：UI 用当前主题颜色调用 `formatter` 得到应答串，再写回 PTY。
    /// 丢掉这个事件会让 `printf '\e]11;?\a'` 永远收不到答复——查询终端
    /// 配色的 TUI（omp、neovim、fzf 等）会一直按“未知终端”回退。
    ColorRequest {
        /// 颜色索引：0-255 为调色板，`NamedColor` 的 Foreground/Background/Cursor 取更大值。
        index: usize,
        /// 由 alacritty 提供的应答格式化函数（输入 Rgb，输出完整转义序列）。
        formatter: Arc<dyn Fn(alacritty_terminal::vte::ansi::Rgb) -> String + Send + Sync>,
    },
    /// 程序查询文本区像素尺寸（CSI 14 t）。
    TextAreaSizeRequest(Arc<dyn Fn(WindowSize) -> String + Send + Sync>),
    /// 程序请求恢复默认窗口标题（`OSC 0/1/2` 空值或 `TitleStack` 弹空；
    /// alacritty 的 `Event::ResetTitle`，此前在 `_ => false` 被静默丢弃，
    /// 标题会永久停在程序设置过的旧值）。
    ResetTitle,
    /// 程序请求写入系统剪贴板（OSC52 store，`]`+`c`/`p`/`s` 剪贴板类型已由
    /// VT 层解析为 `ClipboardType`，这里只透传文本；默认配置只接受 copy）。
    ClipboardStore {
        /// 剪贴板类型（`Clipboard` / `Selection`，透传给 UI 层记录）。
        clipboard: alacritty_terminal::term::ClipboardType,
        /// 解码后的文本内容。
        text: String,
    },
    /// 程序请求读取系统剪贴板（OSC52 load；默认配置拒绝，事件不会到达）。
    ClipboardLoad {
        /// 剪贴板类型。
        clipboard: alacritty_terminal::term::ClipboardType,
        /// 应答格式化函数（输入剪贴板文本，输出完整 OSC52 回复序列）。
        formatter: Arc<dyn Fn(&str) -> String + Send + Sync>,
    },
}

impl std::fmt::Debug for SessionEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SessionEvent::Wakeup => f.write_str("Wakeup"),
            SessionEvent::Title(title) => f.debug_tuple("Title").field(title).finish(),
            SessionEvent::ChildExit => f.write_str("ChildExit"),
            SessionEvent::PtyWrite(text) => f.debug_tuple("PtyWrite").field(text).finish(),
            SessionEvent::Bell => f.write_str("Bell"),
            SessionEvent::ColorRequest { index, .. } => f
                .debug_struct("ColorRequest")
                .field("index", index)
                .finish(),
            SessionEvent::TextAreaSizeRequest(_) => f.write_str("TextAreaSizeRequest"),
            SessionEvent::ClipboardStore { text, .. } => {
                f.debug_tuple("ClipboardStore").field(text).finish()
            }
            SessionEvent::ClipboardLoad { .. } => f.write_str("ClipboardLoad"),
            SessionEvent::ResetTitle => f.write_str("ResetTitle"),
        }
    }
}

/// 事件回调：后台有数据时由监听器线程调用（用于触发 UI 重绘）。
pub type EventHandler = Arc<dyn Fn(&SessionEvent) + Send + Sync>;

/// 合并式唤醒通知：两次 `drain_events` 之间最多回调一次。
///
/// 每次回调都会经 `EventLoopProxy` 唤醒 UI 并请求重绘（一次 Context 写锁 +
/// 一条重绘缘由）。远程读循环按 SSH 数据包逐包到达，`cat` 大文件时可达
/// 每帧数十包——逐包回调会把重绘请求放大到与包数同阶，而一帧只需要一次
/// 重绘。标志位由 `Session::drain_events` 复位，语义与本地 alacritty
/// 事件循环的 `Event::Wakeup` 合并完全一致。
pub(crate) fn notify_wakeup(shared: &Shared, on_event: &EventHandler) {
    if !shared.wakeup.swap(true, Ordering::AcqRel) {
        (on_event)(&SessionEvent::Wakeup);
    }
}

/// 读取本地 shell 当前工作目录（macOS `PROC_PIDVNODEPATHINFO`）。
///
/// 内核态真实值，与 shell 是否内建、是否输出无关：`source`/别名/函数、
/// 粘贴多行、tmux 嵌套下的 `cd` 都能正确反映。
///
/// `pid` 是 PTY 子进程（macOS 上是 `/usr/bin/login`，见下 `spawn_local`）：
/// 先读它的直接子进程（`exec` 后的真实 shell），读不到才回退读自身——
/// `login -flp` 自身常驻根目录附近，读它会得到永远不变的 `/`。
/// 只在 `waitpid(WNOHANG)==0`（仍是当前进程的子进程）时查询，防止 PID
/// 复用后读到陌生进程的目录。
#[cfg(all(unix, target_os = "macos"))]
fn child_current_dir(pid: i32) -> Option<PathBuf> {
    if let Some(shell_pid) = login_shell_child(pid) {
        if let Some(dir) = proc_cwd(shell_pid) {
            return Some(dir);
        }
    }
    proc_cwd(pid)
}

/// `login` 进程的直接子进程（`exec` 后的真实 shell）。
///
/// `waitpid` 只确认“仍是我的子进程”（防 PID 复用），不确认身份；
/// 之后用 `proc_listchildpids` 枚举直接子进程并取第一个——`login -f`
/// 只 `exec` 一个 shell，无多子进程歧义。
#[cfg(all(unix, target_os = "macos"))]
fn login_shell_child(pid: i32) -> Option<i32> {
    let mut status = 0;
    if unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) } != 0 {
        return None;
    }
    let mut children = [0 as libc::pid_t; 16];
    let count = unsafe {
        libc::proc_listchildpids(
            pid,
            children.as_mut_ptr() as *mut libc::c_void,
            (children.len() * std::mem::size_of::<libc::pid_t>()) as i32,
        )
    };
    if count <= 0 {
        return None;
    }
    let child = children[0];
    if child <= 0 {
        return None;
    }
    Some(child)
}

/// 单个 PID 的内核 cwd（`PROC_PIDVNODEPATHINFO` 的 `pvi_cdir`）。
#[cfg(all(unix, target_os = "macos"))]
fn proc_cwd(pid: i32) -> Option<PathBuf> {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;
    let raw = proc_vnode_path(pid)?;
    let len = raw.iter().position(|&b| b == 0).unwrap_or(raw.len());
    if len == 0 {
        return None;
    }
    let dir = PathBuf::from(OsStr::from_bytes(&raw[..len]));
    if dir.is_absolute() {
        Some(dir)
    } else {
        None
    }
}

/// 单个 PID 的内核 cwd 原始字节（NUL 结尾，`MAXPATHLEN`=1024）。
///
/// `vip_path` 在 libc 中为 `[[c_char; 32]; 32]`（绕老 rustc 定长限制），
/// 按连续 1024 字节读。调用方保证 PID 身份：直接子进程（login）用
/// `waitpid(WNOHANG)` 确认存活；真实 shell 是 login 存活期间的直接子进程，
/// 其 PID 在此期间不会被系统回收复用（父进程未 wait 的僵尸/运行中进程
/// 的 PID 不会分配给他人）。
#[cfg(all(unix, target_os = "macos"))]
fn proc_vnode_path(pid: i32) -> Option<[u8; 1024]> {
    let mut info: libc::proc_vnodepathinfo = unsafe { std::mem::zeroed() };
    let ret = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDVNODEPATHINFO,
            0,
            &mut info as *mut _ as *mut libc::c_void,
            std::mem::size_of::<libc::proc_vnodepathinfo>() as libc::c_int,
        )
    };
    if ret as usize != std::mem::size_of::<libc::proc_vnodepathinfo>() {
        return None;
    }
    // 按连续 1024 字节读 NUL 截断（即 MAXPATHLEN）。
    let raw = info.pvi_cdir.vip_path.as_ptr() as *const u8;
    Some(unsafe { *(raw as *const [u8; 1024]) })
}

/// 只向仍由当前进程持有的子进程发送信号。
///
/// 不能仅凭保存下来的裸 PID 调用 kill：shell 退出并被回收后，PID 可能
/// 已被系统分配给别的进程。waitpid 的 WNOHANG 结果同时确认子进程身份，
/// 失败或已退出时保持安静，不触碰可能复用该 PID 的进程。
#[cfg(unix)]
fn signal_child_if_running(pid: i32, signal: i32) {
    let mut status = 0;
    let state = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
    if state == 0 {
        unsafe {
            libc::kill(pid, signal);
        }
    }
}

/// 会话共享状态（监听器与 UI 线程共同访问）。
#[derive(Default)]
pub(crate) struct Shared {
    pub(crate) title: Mutex<String>,
    pub(crate) exited: Mutex<bool>,
    pub(crate) pending: Mutex<Vec<SessionEvent>>,
    /// Wakeup 只表示“终端有新内容”，多个通知可合并成一次。
    pub(crate) wakeup: AtomicBool,
}

/// 把 DECRQM 2026（同步更新）的应答改写为“不支持”。
///
/// alacritty 0.26 对 2026 的 set/unset 是空实现（`()`），DECRQM 却按
/// “已重置”（`2`）回执——程序（omp 实测、nvim 类 TUI）据此判定终端支持
/// 同步更新并逐帧包 BSU/ESU，而 mino 在 BSU..ESU 之间照常渲染中间态，
/// 半成品画面直接上屏。回执必须与能力一致：统一回 `0`（NotSupported，
/// 与 VT 层同格式 `CSI ? mode ; state $ y`）。
///
/// 只动这一条应答（其他模式的能力回执是正确的），且覆盖任意包内容——
/// 程序常把 2026 查询与 kitty/OSC 11/DA1 等混在同一次写入里到达，
/// 按“整包等于查询”匹配的旧实现从未命中（该缺陷的根因）。
fn rewrite_sync_update_reply(text: &mut String) {
    const MARK: &str = "\x1b[?2026;";
    if !text.contains(MARK) {
        return;
    }
    for state in ["1$y", "2$y", "3$y", "4$y"] {
        let from = format!("{MARK}{state}");
        if text.contains(&from) {
            *text = text.replace(&from, "\x1b[?2026;0$y");
        }
    }
}

/// alacritty 事件监听器：把事件记录到共享状态并通知回调。
pub struct Listener {
    pub(crate) shared: Arc<Shared>,
    pub(crate) on_event: EventHandler,
}

impl EventListener for Listener {
    fn send_event(&self, event: Event) {
        if matches!(event, Event::Wakeup) {
            notify_wakeup(&self.shared, &self.on_event);
            return;
        }
        // 只在锁内更新共享状态；UI 回调可能触发事件循环唤醒，不能在
        // 持有 pending 锁时调用，否则 UI 线程 drain_events 与后台线程
        // 的回调路径会形成不必要的锁竞争，严重时表现为窗口卡死。
        let should_notify = {
            let mut pending = self.shared.pending.lock().unwrap();
            // 非 Wakeup 事件需要保留顺序，但后台标签页可能长时间不可见；
            // 普通状态事件设置上限，避免异常事件无限增长。PtyWrite 是
            // 用户数据，不能因状态事件已满而丢弃；相邻写回合并以减少
            // 队列条目数量，数据本身仍完整保留。
            const MAX_PENDING_EVENTS: usize = 256;
            match event {
                Event::Title(title) => {
                    *self.shared.title.lock().unwrap() = title.clone();
                    // 标题是最新状态，不能像 Bell 一样在队列满时直接丢弃：
                    // TerminalView 只从队列更新缓存，丢弃后窗口标题会永久停留
                    // 在旧值。已有标题直接就地替换；队列满时优先回收一个
                    // 可丢弃的 Bell，仍保持普通状态事件队列有界。
                    if let Some(existing) = pending
                        .iter_mut()
                        .find(|event| matches!(event, SessionEvent::Title(_)))
                    {
                        *existing = SessionEvent::Title(title);
                        true
                    } else if pending.len() < MAX_PENDING_EVENTS {
                        pending.push(SessionEvent::Title(title));
                        true
                    } else if let Some(index) = pending
                        .iter()
                        .position(|event| matches!(event, SessionEvent::Bell))
                    {
                        pending[index] = SessionEvent::Title(title);
                        true
                    } else {
                        // 仅剩用户写回数据/不可丢弃状态时允许多出一个标题槽位，
                        // 不能为了维持计数上限而丢失标题或破坏输入数据。
                        pending.push(SessionEvent::Title(title));
                        true
                    }
                }
                Event::ChildExit(_) => {
                    *self.shared.exited.lock().unwrap() = true;
                    // 子进程退出是生命周期状态，不能像 Bell 一样在队列满时
                    // 静默丢弃：否则状态栏可能一直显示“已连接”，直到下一次
                    // 无关输入才被动刷新。相同事件只保留一份；队列满时优先
                    // 回收一个可丢弃的 Bell，必要时允许重要状态暂时超出上限。
                    if pending
                        .iter()
                        .any(|event| matches!(event, SessionEvent::ChildExit))
                    {
                        true
                    } else if pending.len() < MAX_PENDING_EVENTS {
                        pending.push(SessionEvent::ChildExit);
                        true
                    } else if let Some(index) = pending
                        .iter()
                        .position(|event| matches!(event, SessionEvent::Bell))
                    {
                        pending[index] = SessionEvent::ChildExit;
                        true
                    } else {
                        pending.push(SessionEvent::ChildExit);
                        true
                    }
                }
                Event::PtyWrite(mut text) => {
                    // DECRQM 2026（同步更新）应答必须回"不支持"：alacritty 的
                    // VT 层对 2026 的 set/unset 是空实现（不缓冲任何输出），
                    // 却按"已重置"回 `CSI ? 2026 ; 2 $ y`——程序（omp 实测、
                    // nvim 类 TUI）据此判定终端支持同步更新并全程用 BSU/ESU
                    // 包裹每次重绘；mino 在 BSU..ESU 之间照常渲染，半成品画面
                    // 直接上屏（用户现象："整个输出流都变错乱了"）。此处是
                    // 本地/远程/测试三条路径的唯一应答出口，统一改写。
                    rewrite_sync_update_reply(&mut text);
                    // PtyWrite 是终端能力应答（DA/kitty/DECRQM/OSC 查询回执），
                    // 不是用户数据：单条很短（<100B），但多 tab 高输出并发时
                    // 后台标签长期不消费会无界追加；旧注释称"用户数据"有误。
                    // 字节上限 64KB：超限丢最旧的应答（程序按"未知终端"回退，
                    // 不破坏终端状态），保证后台队列内存有界。
                    const MAX_PTY_WRITE_BYTES: usize = 64 * 1024;
                    let queued: usize = pending
                        .iter()
                        .filter_map(|event| match event {
                            SessionEvent::PtyWrite(text) => Some(text.len()),
                            _ => None,
                        })
                        .sum();
                    let mut overflow = queued
                        .saturating_add(text.len())
                        .saturating_sub(MAX_PTY_WRITE_BYTES);
                    // 丢最旧的应答给新应答腾位；单条超限则直接丢新应答。
                    let accepted = if overflow == 0 {
                        true
                    } else {
                        pending.retain_mut(|event| {
                            if overflow == 0 {
                                return true;
                            }
                            let SessionEvent::PtyWrite(previous) = event else {
                                return true;
                            };
                            let drop = previous.len().min(overflow);
                            previous.drain(..drop);
                            overflow -= drop;
                            !previous.is_empty()
                        });
                        overflow == 0
                    };
                    if accepted {
                        if let Some(SessionEvent::PtyWrite(previous)) = pending.last_mut() {
                            previous.push_str(&text);
                        } else {
                            pending.push(SessionEvent::PtyWrite(text));
                        }
                    }
                    accepted
                }
                Event::Bell if pending.len() < MAX_PENDING_EVENTS => {
                    pending.push(SessionEvent::Bell);
                    true
                }
                Event::Bell => false,
                // 颜色/尺寸查询：仿真层不知道真实调色板与像素尺寸，转交 UI
                // 才是唯一能给出正确答案的地方。查询本身很轻（一条短转义
                // 序列），保留完整语义直接入队；队列满时按普通状态事件丢弃
                // （丢一条查询只会让程序回退默认值，不会破坏终端状态）。
                Event::ColorRequest(index, formatter) => {
                    if pending.len() < MAX_PENDING_EVENTS {
                        pending.push(SessionEvent::ColorRequest { index, formatter });
                        true
                    } else {
                        false
                    }
                }
                Event::TextAreaSizeRequest(formatter) => {
                    if pending.len() >= MAX_PENDING_EVENTS {
                        false
                    } else {
                        pending.push(SessionEvent::TextAreaSizeRequest(formatter));
                        true
                    }
                }
                // OSC52 剪贴板写入：程序复制（如 omp 的 yank）必须到达系统
                // 剪贴板，否则“复制了但 ⌘V 粘不出来”。文本可能较长（整段
                // 代码），与 PtyWrite 同级保护：不受状态事件上限限制。
                Event::ClipboardStore(clipboard, text) => {
                    pending.push(SessionEvent::ClipboardStore { clipboard, text });
                    true
                }
                // OSC52 剪贴板读取：默认配置拒绝（OnlyCopy），几乎不会到达；
                // 到达则按普通状态事件入队（丢一条只让程序读不到剪贴板）。
                Event::ClipboardLoad(clipboard, formatter)
                    if pending.len() < MAX_PENDING_EVENTS =>
                {
                    pending.push(SessionEvent::ClipboardLoad {
                        clipboard,
                        formatter,
                    });
                    true
                }
                // 标题重置是最新状态（与 Title 同级）：已有 Title 就地替换，
                // 否则按普通状态事件入队；队列满时回收一个可丢弃的 Bell。
                Event::ResetTitle => {
                    if let Some(existing) = pending
                        .iter_mut()
                        .find(|event| matches!(event, SessionEvent::Title(_)))
                    {
                        *existing = SessionEvent::ResetTitle;
                        true
                    } else if pending.len() < MAX_PENDING_EVENTS {
                        pending.push(SessionEvent::ResetTitle);
                        true
                    } else if let Some(index) = pending
                        .iter()
                        .position(|event| matches!(event, SessionEvent::Bell))
                    {
                        pending[index] = SessionEvent::ResetTitle;
                        true
                    } else {
                        pending.push(SessionEvent::ResetTitle);
                        true
                    }
                }
                // 不要为未入队事件用 pending.last() 通知：队列已满时会
                // 误重复通知上一次事件，造成无意义的重绘。
                _ => false,
            }
        };

        // 回调只作为“有事件需要 UI 尽快轮询”的重绘信号；实际事件数据
        // 统一从 pending 队列读取，避免复制大型 PtyWrite 字符串。
        if should_notify {
            (self.on_event)(&SessionEvent::Wakeup);
        }
    }
}

/// 写目标：非阻塞写入字节到会话（本地 → EventLoopSender，远程 → 命令队列）。
pub type WriteFn = Arc<dyn Fn(&[u8]) + Send + Sync>;

#[derive(Clone)]
pub struct Writer(WriteFn);

impl Writer {
    pub fn new(f: impl Fn(&[u8]) + Send + Sync + 'static) -> Self {
        Self(Arc::new(f))
    }

    pub fn write(&self, bytes: &[u8]) {
        (self.0)(bytes);
    }
}

/// 尺寸调整目标：非阻塞通知后台调整窗口尺寸。
#[derive(Clone)]
pub struct Resizer(Arc<dyn Fn(u16, u16) + Send + Sync>);

impl Resizer {
    pub fn new(f: impl Fn(u16, u16) + Send + Sync + 'static) -> Self {
        Self(Arc::new(f))
    }

    pub fn resize(&self, cols: u16, rows: u16) {
        (self.0)(cols, rows);
    }
}

/// 关闭目标。
#[derive(Clone)]
pub struct Shuttor(Arc<dyn Fn() + Send + Sync>);

impl Shuttor {
    pub fn new(f: impl Fn() + Send + Sync + 'static) -> Self {
        Self(Arc::new(f))
    }

    pub fn shutdown(&self) {
        (self.0)();
    }
}

/// 终端会话（本地或远程 shell 的统一封装）。
pub struct Session {
    term: Arc<FairMutex<Term<Listener>>>,
    shared: Arc<Shared>,
    writer: Writer,
    resizer: Resizer,
    shuttor: Shuttor,
    /// 是否为远程会话（远程不启用本地补全：无本机文件系统对应）。
    is_remote: bool,
    /// 本地会话的 PTY 读取线程（远程会话为 None）。
    pty_thread: Option<
        JoinHandle<(
            EventLoop<tty::Pty, Listener>,
            alacritty_terminal::event_loop::State,
        )>,
    >,
    /// 本地 shell 子进程 PID（仅 unix：关闭时兜底 SIGKILL，远程会话为 None）。
    #[cfg(unix)]
    child_pid: Option<i32>,
}

/// 会话创建参数（本地）。
#[derive(Default)]
pub struct SessionOptions {
    /// 要启动的 shell 程序，None 表示系统默认 shell。
    pub shell: Option<String>,
    /// 启动目录。
    pub working_directory: Option<PathBuf>,
    /// 附加环境变量（追加到继承的环境，alacritty 为覆盖语义）。
    pub env: HashMap<String, String>,
}

impl Session {
    /// 构造会话（由本地/远程创建逻辑调用）。
    pub(crate) fn new(
        term: Arc<FairMutex<Term<Listener>>>,
        shared: Arc<Shared>,
        writer: Writer,
        resizer: Resizer,
        shuttor: Shuttor,
        is_remote: bool,
        #[allow(unused_variables)] child_pid: Option<i32>,
    ) -> Session {
        Session {
            term,
            shared,
            writer,
            resizer,
            shuttor,
            is_remote,
            pty_thread: None,
            #[cfg(unix)]
            child_pid,
        }
    }

    /// 是否为远程会话（决定本地补全等本机能力是否可用）。
    pub fn is_remote(&self) -> bool {
        self.is_remote
    }

    /// 启动一个本地 PTY 会话。
    ///
    /// `on_event` 会在 PTY 线程收到数据时被调用（UI 层用它请求重绘）。
    pub fn spawn_local(
        options: SessionOptions,
        cols: u16,
        rows: u16,
        on_event: EventHandler,
    ) -> io::Result<Session> {
        let shared = Arc::new(Shared::default());
        let config = term::Config {
            kitty_keyboard: true,
            ..term::Config::default()
        };

        // 创建 PTY（macOS/Unix 平台）。
        let shell = options
            .shell
            .map(|program| tty::Shell::new(program, Vec::new()));
        let pty_options = tty::Options {
            shell,
            working_directory: options.working_directory,
            drain_on_exit: true,
            env: options.env,
        };
        let window_size = WindowSize {
            num_lines: rows,
            num_cols: cols,
            cell_width: 1,
            cell_height: 1,
        };
        let pty = tty::new(&pty_options, window_size, 0)?;
        // 子进程 PID：alacritty 的 Pty 析构只发 SIGHUP 后 `wait()`，shell 偶发
        // 不响应 SIGHUP 会让关闭流程永久阻塞；Session 关闭时用它兜底 SIGKILL
        // （仅 unix 有此机制，Windows ConPTY 无对应问题）。
        #[cfg(unix)]
        let child_pid = pty.child().id() as i32;

        // 创建终端状态机。
        let term = Arc::new(FairMutex::new(Term::new(
            config,
            &TermSize {
                rows: rows as usize,
                cols: cols as usize,
            },
            Listener {
                shared: shared.clone(),
                // Term 自身负责解析 VT 后产生 Title/PtyWrite/Bell 等事件；
                // 不能使用空监听器，否则本地终端虽然能显示字符，却会静默
                // 丢失终端能力应答、标题变化和响铃。
                on_event: on_event.clone(),
            },
        )));

        // 创建事件循环（PTY 读取线程）。
        let event_loop = match EventLoop::new(
            term.clone(),
            Listener {
                shared: shared.clone(),
                on_event: on_event.clone(),
            },
            pty,
            true,
            false,
        ) {
            Ok(event_loop) => event_loop,
            Err(err) => {
                // pty 随错误路径析构，先杀 shell 避免其 wait 阻塞。
                #[cfg(unix)]
                signal_child_if_running(child_pid, libc::SIGKILL);
                return Err(err);
            }
        };
        let channel = event_loop.channel();
        let pty_thread: JoinHandle<(
            EventLoop<tty::Pty, Listener>,
            alacritty_terminal::event_loop::State,
        )> = event_loop.spawn();

        // 写 / 缩放 / 关闭 都走 EventLoopSender（克隆共享）。
        let writer_channel = channel.clone();
        let resizer_channel = channel.clone();
        let shuttor_channel = channel.clone();
        let writer = Writer::new(move |bytes: &[u8]| {
            // 通道本身就要所有权，这里一次小拷贝是底线；批量合并由调用方
            // 在调用前完成（`TerminalView::handle_input` 把一帧的输入攒成
            // 一次 `write`，滚轮 steps 循环这类 N 次发送已消除）。
            let _ = writer_channel.send(Msg::Input(bytes.to_vec().into()));
        });
        let resizer = Resizer::new(move |cols: u16, rows: u16| {
            let _ = resizer_channel.send(Msg::Resize(WindowSize {
                num_lines: rows,
                num_cols: cols,
                cell_width: 1,
                cell_height: 1,
            }));
        });
        let shuttor = Shuttor::new(move || {
            let _ = shuttor_channel.send(Msg::Shutdown);
        });

        #[cfg(unix)]
        let child_pid_arg = Some(child_pid);
        #[cfg(not(unix))]
        let child_pid_arg: Option<i32> = None;
        let mut session =
            Session::new(term, shared, writer, resizer, shuttor, false, child_pid_arg);
        session.pty_thread = Some(pty_thread);
        Ok(session)
    }

    /// PTY 读取线程是否已退出（用于诊断写入失效问题）。
    pub fn pty_thread_finished(&self) -> bool {
        self.pty_thread
            .as_ref()
            .map(|t| t.is_finished())
            .unwrap_or(false)
    }

    /// 访问终端状态机（渲染时锁定读取）。
    pub fn term(&self) -> Arc<FairMutex<Term<Listener>>> {
        self.term.clone()
    }

    /// 写入数据（键盘输入、粘贴内容等）。
    pub fn write(&self, bytes: &[u8]) {
        self.writer.write(bytes);
    }

    /// 调整终端尺寸（窗口 resize 时调用）。
    pub fn resize(&mut self, cols: u16, rows: u16) {
        self.resize_grid(cols, rows);
        // 通知后台（PTY/SSH channel）。
        self.resizer.resize(cols, rows);
    }

    /// 只同步终端状态机网格，不通知后台。
    ///
    /// 窗口拖拽中每帧都在变化：本地网格必须立即跟上（否则字越界/显示错位），
    /// 但后台通知（本地 `SIGWINCH`/PTY ioctl、远程 SSH `window_change` 网络
    /// 包）可以节流——由 `TerminalView` 按 50ms 合并后补发最终尺寸。
    /// 无条件重入开销为一次 `Term::resize`（网格引用更新，空网格时便宜）。
    pub fn resize_grid(&mut self, cols: u16, rows: u16) {
        // 同步更新终端状态机网格。
        self.term.lock().resize(TermSize {
            rows: rows as usize,
            cols: cols as usize,
        });
    }

    /// 只通知后台当前网格尺寸（拖拽节流的 trailing 补发）。
    pub fn notify_backend_size(&self, cols: u16, rows: u16) {
        self.resizer.resize(cols, rows);
    }

    /// 取出所有待处理事件（UI 每帧轮询）。
    pub fn drain_events(&self) -> Vec<SessionEvent> {
        // 必须先清除通知标记，再取得队列。若先 take 队列、最后才清除标记，
        // 后台线程可能在两步之间发出 Wakeup，看到旧的 true 而跳过回调，
        // 随后又被这里的 store(false) 覆盖，导致终端内容已经更新却没有下一帧。
        self.shared.wakeup.store(false, Ordering::Release);
        let mut pending = self.shared.pending.lock().unwrap();
        std::mem::take(&mut *pending)
    }

    /// 测试用：把程序侧输出直接喂给 VT 解析器（与远程 `remote_loop` 同管线）。
    ///
    /// `session.write` 是往从机方向写（会被 shell 吃掉输入），开鼠标上报
    /// 这类 DECSET 必须从程序侧进入解析器；公开的 `Term` API 没有 mode
    /// setter，只能走字节流注入。
    pub fn inject_program_output_for_test(&self, bytes: &[u8]) {
        // 解析器必须**跨调用持久**（与远程 `remote_loop` 同一语义）：分片
        // 边界常常落在转义序列中间，逐次新建 `Processor` 会丢掉半截状态，
        // 后半段被当作文本吞进屏幕（按 PTY 分片注入字节的测试会因此看到
        // 转义残骸）。
        thread_local! {
            static PARSER: std::cell::RefCell<Processor<StdSyncHandler>> =
                std::cell::RefCell::new(Processor::new());
        }
        PARSER.with(|parser| {
            let mut guard = self.term.lock();
            parser.borrow_mut().advance(&mut *guard, bytes);
        });
    }

    /// 当前窗口标题。
    pub fn title(&self) -> String {
        self.shared.title.lock().unwrap().clone()
    }

    /// 子进程是否已退出。
    pub fn has_exited(&self) -> bool {
        *self.shared.exited.lock().unwrap()
    }
    /// 本地 shell 子进程的当前工作目录（macOS 内核查询）。
    ///
    /// 内核态真实值：跟踪器只认“本视图键入的可见文本 + 回车”，`source`、
    /// 别名/函数、粘贴多行、`cd -`、远程嵌套会话里的 `cd` 都追踪不到。
    /// 远程会话与非 macOS 恒返回 `None`（无子进程 / 无该内核接口）。
    /// 返回前做 `canonicalize`，与跟踪器的规范化路径可比（`/tmp` → `/private/tmp`）。
    #[cfg(all(unix, target_os = "macos"))]
    pub fn child_current_dir(&self) -> Option<PathBuf> {
        if self.is_remote {
            return None;
        }
        let pid = self.child_pid?;
        let dir = child_current_dir(pid)?;
        Some(std::fs::canonicalize(&dir).unwrap_or(dir))
    }

    /// 非 macOS 的占位实现（无 `PROC_PIDVNODEPATHINFO` 内核接口）。
    #[cfg(not(all(unix, target_os = "macos")))]
    pub fn child_current_dir(&self) -> Option<PathBuf> {
        None
    }

    /// 关闭会话。
    pub fn shutdown(self) {
        self.shuttor.shutdown();
    }
}

impl Drop for Session {
    /// 会话被丢弃时优雅关闭后台线程。
    fn drop(&mut self) {
        #[cfg(unix)]
        if let Some(pid) = self.child_pid {
            // 先给 shell 优雅退出机会（SIGHUP 让它保存状态正常退出）。
            signal_child_if_running(pid, libc::SIGHUP);
            // 兜底：shell 偶发不响应 SIGHUP，而 alacritty Pty 析构的
            // `wait()` 会随之永久阻塞（关闭标签页时 UI 线程卡死），
            // 延时 SIGKILL 保证其必然退出；发送前重新确认子进程身份，
            // 防止 shell 退出后 PID 被其它进程复用。
            std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_millis(300));
                signal_child_if_running(pid, libc::SIGKILL);
            });
        }
        self.shuttor.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    /// 回调不能在 pending 锁仍被持有时执行，否则回调路径触及会话事件
    /// 队列会自我阻塞，最终让 UI 看起来像卡死。
    #[test]
    fn 事件回调在释放队列锁后执行() {
        let shared = Arc::new(Shared::default());
        let callback_shared = shared.clone();
        let listener = Listener {
            shared: shared.clone(),
            on_event: Arc::new(move |_event| {
                assert!(
                    callback_shared.pending.try_lock().is_ok(),
                    "事件回调执行时不应继续持有 pending 锁"
                );
            }),
        };

        listener.send_event(Event::Wakeup);
        assert!(shared.wakeup.load(Ordering::Acquire));
        assert!(shared.pending.lock().unwrap().is_empty());
    }

    #[test]
    fn 队列已满时保留写回数据且不重复旧事件通知() {
        let shared = Arc::new(Shared::default());
        let callback_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let callback_count_clone = callback_count.clone();
        let listener = Listener {
            shared: shared.clone(),
            on_event: Arc::new(move |_event| {
                callback_count_clone.fetch_add(1, Ordering::Relaxed);
            }),
        };

        // 填满普通状态事件队列。
        for _ in 0..256 {
            listener.send_event(Event::Bell);
        }
        let callbacks_at_capacity = callback_count.load(Ordering::Relaxed);
        assert_eq!(callbacks_at_capacity, 256);

        // 队列满时丢弃可合并的 Bell，但不能重复回调上一次 Bell。
        listener.send_event(Event::Bell);
        assert_eq!(
            callback_count.load(Ordering::Relaxed),
            callbacks_at_capacity
        );

        // 标题是最新状态，队列满时也必须保留，不能让窗口标题永久停在旧值。
        listener.send_event(Event::Title("新标题".into()));
        assert_eq!(
            callback_count.load(Ordering::Relaxed),
            callbacks_at_capacity + 1
        );
        {
            let pending = shared.pending.lock().unwrap();
            assert_eq!(pending.len(), 256);
            assert!(pending
                .iter()
                .any(|event| matches!(event, SessionEvent::Title(title) if title == "新标题")));
        }

        // PtyWrite 是用户数据，不受普通事件上限影响；相邻写回合并，
        // 数据仍须完整保留。
        listener.send_event(Event::PtyWrite("payload".into()));
        listener.send_event(Event::PtyWrite("!".into()));
        assert_eq!(
            callback_count.load(Ordering::Relaxed),
            callbacks_at_capacity + 3
        );
        let pending = shared.pending.lock().unwrap();
        assert!(pending
            .iter()
            .any(|event| { matches!(event, SessionEvent::PtyWrite(text) if text == "payload!") }));
    }

    /// 回归：PtyWrite 队列字节有界——后台标签长期不消费时丢最旧的终端能力
    /// 应答，不无界追加；超限后新应答仍通知 UI 轮询（程序按"未知终端"回退，
    /// 不破坏终端状态）。用户现象：多 tab 高输出并发时整窗冻结。
    #[test]
    fn 写回队列超限丢最旧应答且有界() {
        let shared = Arc::new(Shared::default());
        let callback_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let callback_count_clone = callback_count.clone();
        let listener = Listener {
            shared: shared.clone(),
            on_event: Arc::new(move |_event| {
                callback_count_clone.fetch_add(1, Ordering::Relaxed);
            }),
        };
        // 每条 1KB，先压 64 条占满 64KB 上限。
        let chunk = "x".repeat(1024);
        for _ in 0..64 {
            listener.send_event(Event::PtyWrite(chunk.clone()));
        }
        let bytes: usize = shared
            .pending
            .lock()
            .unwrap()
            .iter()
            .filter_map(|event| match event {
                SessionEvent::PtyWrite(text) => Some(text.len()),
                _ => None,
            })
            .sum();
        assert_eq!(bytes, 64 * 1024, "64 条 1KB 应答应占满上限");
        // 再压一条：丢最旧 1KB、新数据完整保留，总量仍为上限。
        listener.send_event(Event::PtyWrite("NEW".into()));
        let pending = shared.pending.lock().unwrap();
        let bytes: usize = pending
            .iter()
            .filter_map(|event| match event {
                SessionEvent::PtyWrite(text) => Some(text.len()),
                _ => None,
            })
            .sum();
        assert_eq!(bytes, 64 * 1024, "超限后总量应仍为上限");
        let merged: String = pending
            .iter()
            .filter_map(|event| match event {
                SessionEvent::PtyWrite(text) => Some(text.clone()),
                _ => None,
            })
            .collect();
        assert!(merged.ends_with("NEW"), "新应答必须保留");
        assert_eq!(merged.len(), 64 * 1024);
    }

    #[test]
    fn 队列已满时仍通知子进程退出() {
        let shared = Arc::new(Shared::default());
        let callback_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let callback_count_clone = callback_count.clone();
        let listener = Listener {
            shared: shared.clone(),
            on_event: Arc::new(move |_event| {
                callback_count_clone.fetch_add(1, Ordering::Relaxed);
            }),
        };

        for _ in 0..256 {
            listener.send_event(Event::Bell);
        }
        listener.send_event(Event::ChildExit(std::process::ExitStatus::default()));

        assert!(shared.exited.lock().unwrap().to_owned());
        assert_eq!(callback_count.load(Ordering::Relaxed), 257);
        assert!(shared
            .pending
            .lock()
            .unwrap()
            .iter()
            .any(|event| matches!(event, SessionEvent::ChildExit)));
    }

    /// 程序的终端查询必须进入事件队列，不能被会话层静默丢弃。
    ///
    /// 仿真层不知道真实调色板与像素尺寸，只有 UI 能回答；曾把
    /// `Event::ColorRequest` 当无关事件丢掉，导致 OSC 10/11/12、OSC 4
    /// 查询永远没有答复，查询终端配色的 TUI 只能按“未知终端”回退。
    #[test]
    fn 颜色与尺寸查询进入事件队列() {
        use alacritty_terminal::vte::ansi::Rgb;

        let shared = Arc::new(Shared::default());
        let listener = Listener {
            shared: shared.clone(),
            on_event: Arc::new(|_event| {}),
        };
        let color_formatter: Arc<dyn Fn(Rgb) -> String + Send + Sync> = Arc::new(|color| {
            format!(
                "\x1b]11;rgb:{:02x}/{:02x}/{:02x}\x07",
                color.r, color.g, color.b
            )
        });
        listener.send_event(Event::ColorRequest(257, color_formatter));
        listener.send_event(Event::TextAreaSizeRequest(Arc::new(|size| {
            format!("\x1b[4;{};{}t", size.num_lines, size.num_cols)
        })));

        let pending = shared.pending.lock().unwrap();
        assert_eq!(pending.len(), 2, "两条查询都应入队");
        match &pending[0] {
            SessionEvent::ColorRequest { index, formatter } => {
                assert_eq!(*index, 257);
                assert_eq!(
                    formatter(Rgb { r: 1, g: 2, b: 3 }),
                    "\x1b]11;rgb:01/02/03\x07"
                );
            }
            other => panic!("首个事件应为 ColorRequest，实际 {other:?}"),
        }
        match &pending[1] {
            SessionEvent::TextAreaSizeRequest(formatter) => {
                let text = formatter(WindowSize {
                    num_lines: 30,
                    num_cols: 100,
                    cell_width: 8,
                    cell_height: 16,
                });
                assert_eq!(text, "\x1b[4;30;100t");
            }
            other => panic!("第二个事件应为 TextAreaSizeRequest，实际 {other:?}"),
        }
    }

    /// 高频输出只需触发一次唤醒回调，不能按输出块无限追加 Wakeup 事件。
    #[test]
    fn 高频唤醒事件合并() {
        let shared = Arc::new(Shared::default());
        let callback_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let callback_count_clone = callback_count.clone();
        let listener = Listener {
            shared: shared.clone(),
            on_event: Arc::new(move |_event| {
                callback_count_clone.fetch_add(1, Ordering::Relaxed);
            }),
        };

        for _ in 0..10_000 {
            listener.send_event(Event::Wakeup);
        }
        assert_eq!(callback_count.load(Ordering::Relaxed), 1);
        assert!(shared.pending.lock().unwrap().is_empty());
        assert!(shared.wakeup.load(Ordering::Acquire));

        let events = {
            let pending = shared.pending.lock().unwrap();
            pending.len()
        };
        assert_eq!(events, 0);
        let _ = Session {
            term: Arc::new(FairMutex::new(Term::new(
                term::Config {
                    kitty_keyboard: true,
                    ..term::Config::default()
                },
                &TermSize { rows: 1, cols: 1 },
                Listener {
                    shared: shared.clone(),
                    on_event: Arc::new(|_| {}),
                },
            ))),
            shared: shared.clone(),
            writer: Writer::new(|_| {}),
            resizer: Resizer::new(|_, _| {}),
            shuttor: Shuttor::new(|| {}),
            is_remote: false,
            pty_thread: None,
            #[cfg(unix)]
            child_pid: None,
        }
        .drain_events();
        assert!(!shared.wakeup.load(Ordering::Acquire));
    }

    /// 远程读循环按 SSH 数据包逐包到达（`cat` 大文件时每帧数十包），
    /// 逐包回调会把重绘请求放大到与包数同阶；`notify_wakeup` 必须与本地
    /// `Event::Wakeup` 走同一套合并语义（一次 drain 间隔内最多一次）。
    #[test]
    fn 远程数据包合并为一次重绘通知() {
        let shared = Arc::new(Shared::default());
        let callback_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = callback_count.clone();
        let on_event: EventHandler = Arc::new(move |_event| {
            counter.fetch_add(1, Ordering::Relaxed);
        });

        // 模拟连续三个 SSH 数据包：只应产生一次回调。
        for _ in 0..3 {
            notify_wakeup(&shared, &on_event);
        }
        assert_eq!(callback_count.load(Ordering::Relaxed), 1);
        assert!(shared.wakeup.load(Ordering::Acquire));

        // drain 周期复位（与 `Session::drain_events` 的第一步同语义）后，
        // 下一个数据包必须再次触发一次回调——否则终端内容更新后没有下一帧。
        shared.wakeup.store(false, Ordering::Release);
        notify_wakeup(&shared, &on_event);
        assert_eq!(callback_count.load(Ordering::Relaxed), 2);
    }
}
