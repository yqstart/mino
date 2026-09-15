//! Mino 应用主体：布局、连接管理与状态。
//!
//! 视觉参照 Warp：分层深色背景、终端绿与琥珀强调、圆角幽灵按钮、
//! 标签页底部指示条与扫光动效（动效细节见 `crate::anim`）。

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::{Duration, Instant};

use eframe::egui;
use mino_core::config::{Auth, HostConfig, HostProfile, ProjectProfile};
use mino_core::ssh::sftp::{connect_sftp_with_handler, SftpEvent, SftpHandle};
use mino_core::ssh::{connect_remote_with_cancel, ConnectCancel, ConnectResult};
use mino_core::terminal::{Session, SessionEvent, SessionOptions};
use mino_core::updater::{check_for_update, UpdateInfo};
use tokio::sync::mpsc::{Receiver, UnboundedReceiver};

use crate::anim;
use crate::dialog;
use crate::views::sftp_view::SftpView;
use crate::views::terminal_view::TerminalView;

/// 应用对外展示名称。
pub const PRODUCT_NAME: &str = "Mino";

/// 新建连接表单状态。
struct ConnectForm {
    name: String,
    host: String,
    port: String,
    user: String,
    auth_kind: usize, // 0=密码 1=私钥
    password: String,
    key_path: String,
    passphrase: String,
    /// 名称输入框是否已聚焦（对话框打开时自动聚焦）。
    name_focused: bool,
}

impl Default for ConnectForm {
    fn default() -> Self {
        Self {
            name: String::new(),
            host: String::new(),
            // 默认 root / 22 端口，可修改（大多数服务器默认入口）。
            port: "22".into(),
            user: "root".into(),
            auth_kind: 0,
            password: String::new(),
            key_path: String::new(),
            passphrase: String::new(),
            name_focused: false,
        }
    }
}

/// 项目新增/编辑表单状态（设置弹窗「项目管理」卡片内展开）。
///
/// `index` 为 `None` 表示新增，`Some(i)` 表示编辑第 i 个项目。
/// `name_error`/`path_error` 记录上次保存校验结果，驱动输入框红边框。
struct ProjectEdit {
    index: Option<usize>,
    name: String,
    path: String,
    command: String,
    name_error: bool,
    path_error: bool,
}

/// 更新下载事件（后台线程 → UI）。
enum DownloadEvent {
    Progress { downloaded: u64, total: Option<u64> },
    Done(PathBuf),
    Error(String),
}

/// 下载中的状态。
struct DownloadState {
    info: UpdateInfo,
    downloaded: u64,
    total: Option<u64>,
}

/// 更新状态机。
enum UpdateState {
    Idle,
    Checking,
    Available(UpdateInfo),
    UpToDate,
    Failed,
    Downloading(DownloadState),
    Downloaded { info: UpdateInfo, dmg_path: PathBuf },
    Installing(UpdateInfo),
    Installed,
    Error(String),
}

/// 更新弹窗内产生的用户动作。
enum UpdateAction {
    Dismiss,
    StartDownload(UpdateInfo),
    CancelDownload,
    Install { dmg_path: PathBuf },
    Retry,
}

/// 轻提示（非模态 Toast）。
struct Toast {
    message: String,
    is_error: bool,
    /// 首次渲染时记录时间戳（NAN 表示尚未记录）。
    start: f64,
}

/// 单个终端标签页（本地或远程会话）。
pub struct TerminalTab {
    /// 标签稳定身份，不随 Vec 中的插入、删除或移动变化。
    id: u64,
    label: String,
    terminal: TerminalView,
    sftp: Option<SftpView>,
    /// SFTP 面板是否展开（tabby 形式：默认收起，终端右上角悬浮按钮切换）。
    sftp_open: bool,
    /// SFTP“定位到终端位置”等待中的终端 `pwd` 探测。
    ///
    /// 面板只表达“用户想要定位”，真正的目录要等 shell 的 `pwd` 输出把
    /// 跟踪器校正到真实值后才能导航（输入跟踪在 Tab/粘贴/别名/函数等
    /// 场景下会失效，直接用旧推测值就是“只有 pwd 后才好用”的根因）。
    /// 等待期间每帧检查 `TerminalView::auto_pwd_ready()`，拿到结果就
    /// 导航；超时则取消探测、用已知目录回退并给明确反馈。
    locate_pending: Option<LocatePending>,
}

/// 等待中的一次 SFTP 定位探测。
struct LocatePending {
    /// 发起定位时的帧时间（egui `input.time`，秒）。
    started_at: f64,
}

/// 等待中的远程图片粘贴（本地中转 → SFTP 上传 → 远端 token 写回）。
struct PendingImagePaste {
    /// 目标标签稳定身份（上传完成时按 id 找 tab，关闭后丢弃）。
    tab_id: u64,
    /// 远端全路径（发起时锁定，不跟随目录切换）。
    remote_path: String,
    /// 已发起上传时的传输 id（`Done/Error` 按 id 匹配）。
    transfer_id: u64,
}

impl TerminalTab {
    fn new(id: u64, label: String, terminal: TerminalView) -> Self {
        Self {
            id,
            label,
            terminal,
            sftp: None,
            sftp_open: false,
            locate_pending: None,
        }
    }

    /// 标签/状态栏标题：`(显示文本, 全路径悬浮提示)`。
    ///
    /// - 本地标签：当前目录的**末级文件夹名**（与 zsh `%c` 提示符一致，home
    ///   显示 `~`）+ 全路径提示。目录以 shell 子进程的内核 cwd 为准
    ///   （`Session::child_current_dir`，每帧读取无缓存延迟）；
    ///   **不**采用 shell 上报的窗口标题——oh-my-zsh 的标题是截断过的
    ///   `%15<..<%~%<<`，既非末级目录名也拿不到完整路径。
    /// - 远程标签：主机名（远端目录由 sshd 决定、本地跟踪器不适用，主机名
    ///   才是用户认得的身份），无悬浮提示；`label` 仅在目录未知时兜底。
    fn title(&self) -> (String, Option<String>) {
        match self.local_dir() {
            Some((name, full)) => (name, Some(full)),
            None => (self.label.clone(), None),
        }
    }

    /// 本地会话的 `(末级目录名, 全路径)`；远程会话返回 `None`。
    ///
    /// 数据源是 `TerminalView::effective_local_directory`（内核 cwd 优先、
    /// 跟踪值回退）：输入跟踪在粘贴/补全/别名等场景下会停在旧目录，
    /// 标题不能依赖它；也不采用 shell 上报的窗口标题（oh-my-zsh 的标题是
    /// 截断过的 `%15<..<%~%<<`）。
    fn local_dir(&self) -> Option<(String, String)> {
        let full = self.terminal.effective_local_directory()?;
        Some((dir_display_name(&full), full))
    }
}

/// 目录的展示名：只保留最末级文件夹名（与 zsh `%c` 提示符语义一致）。
///
/// - home 显示 `~`、根目录显示 `/`（`%c` 同样如此，避免标签写着用户名）；
/// - 尾随 `/` 先剥掉，`/Users/me/proj/` 与 `/Users/me/proj` 同名。
fn dir_display_name(path: &str) -> String {
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() {
        return "/".to_string();
    }
    if let Some(home) = std::env::var_os("HOME") {
        let home = home.to_string_lossy();
        if !home.is_empty() && trimmed == home.trim_end_matches('/') {
            return "~".to_string();
        }
    }
    match Path::new(trimmed).file_name() {
        Some(name) => name.to_string_lossy().into_owned(),
        None => trimmed.to_string(),
    }
}

/// 一次 SSH/SFTP 连接的身份与状态。
struct SftpConnection {
    connection_id: u64,
    handle: SftpHandle,
    rx: Receiver<SftpEvent>,
    host: String,
    home: Option<String>,
}

/// 标签页：普通终端（含本地/远程）。设置改为独立弹窗（`show_settings`），
/// 不再作为 tab。
///
/// `Vec<Box<TerminalTab>>` 用 Box 包裹：TerminalTab 体积大，
/// Box 避免 Vec 各槽位按最大元素对齐造成内存浪费
/// （clippy `large_enum_variant` 等价警告——`Tab` enum 之前也是用 Box）。
pub type Tab = Box<TerminalTab>;

/// 应用状态。
pub struct MinoApp {
    tabs: Vec<Tab>,
    active_tab: usize,
    /// 连接成功后创建的标签稳定身份（用于挂载 SFTP）。
    pending_tab: Option<u64>,
    /// 当前等待 SSH 结果的连接身份。
    pending_connection_id: Option<u64>,
    /// 标签与连接身份分配器。
    next_id: u64,
    /// 主机行最近一次点击（时间, 行索引），自实现双击检测。
    last_row_click: Option<(f64, usize)>,
    /// 设置中的当前主机焦点（单击后保持，双击连接）。
    selected_host: Option<usize>,
    /// 设置弹窗是否打开（`⌘,` 或齿轮按钮切换；Esc/× 关闭）。
    show_settings: bool,
    /// 项目打开面板是否打开（`⌘O` 切换；Esc/回车关闭）。
    show_projects: bool,
    /// 项目搜索过滤词（快捷菜单与 ⌘O 面板共用）。
    project_filter: String,
    /// 过滤后项目列表的选中下标（⌘O 面板上下键导航用）。
    project_selected: usize,
    /// 最近一帧的 egui::Context（`new_local_tab` 等非 UI 闭包内构造时使用）。
    last_ctx: egui::Context,
    config: HostConfig,
    config_path: PathBuf,
    /// 原配置无法读取且备份也失败时，禁止用空配置覆盖原文件。
    config_write_blocked: bool,
    show_new_conn: bool,
    /// 新建连接弹窗关闭后是否恢复此前被其遮住的设置窗口。
    settings_before_new_conn: bool,
    form: ConnectForm,
    /// 项目新增/编辑表单（`None` 表示未展开）。
    project_edit: Option<ProjectEdit>,
    pending: Option<UnboundedReceiver<ConnectResult>>,
    /// 当前 SSH 连接建立阶段的取消句柄；替换或销毁等待中的连接时立即取消。
    pending_connect_cancel: Option<ConnectCancel>,
    pending_label: String,
    toast: Option<Toast>,
    /// 进行中的 SFTP 连接（句柄 + 事件流 + 主机名——主机名随连接绑定，
    /// 多连接并发时不会串到别的标签页上）。
    pending_sftp: Option<SftpConnection>,
    /// SFTP 已就绪但 SSH 标签尚未创建，等待挂载。
    ready_sftp: Option<SftpConnection>,
    /// SFTP 连接错误（状态栏持久显示，toast 易被忽略）。
    sftp_error: Option<String>,
    /// 等待中的远程图片粘贴（tab 身份 + 本地中转路径 + 远端文件名 + 传输 id）。
    ///
    /// 终端只产出本地中转文件；上传由面板 `SFTP` 句柄执行，`Done` 后再向
    /// 该 tab 的 PTY 写远端 `@token`。单槽：新粘贴覆盖旧等待（旧中转留
    /// `/tmp` 自清，不阻塞新图）。
    pending_image_paste: Option<PendingImagePaste>,
    update_state: UpdateState,
    update_rx: Option<std::sync::mpsc::Receiver<Result<Option<UpdateInfo>, String>>>,
    download_rx: Option<std::sync::mpsc::Receiver<DownloadEvent>>,
    /// 当前下载的取消标记；取消动作会通知后台线程停止读取并清理临时文件。
    download_cancel: Option<Arc<AtomicBool>>,
    /// 当前下载的独立临时路径，用于取消时立即清理。
    download_path: Option<PathBuf>,
    /// 下载路径序号，避免重试与旧线程共享同一文件。
    download_sequence: u64,
    /// 本次检查是否为用户手动触发（决定是否弹提示）。
    manual_update: bool,
    /// 安装脚本已启动，到该时间点关闭应用重启。
    restart_at: Option<f64>,
    /// 安装脚本状态文件路径（原子写入后由 UI 轮询）。
    install_result_path: Option<PathBuf>,
    /// 安装脚本仍在使用的 DMG；安装失败/超时时由 UI 清理。
    install_dmg_path: Option<PathBuf>,
    /// 安装脚本启动时间，用于检测脚本无响应。
    install_started_at: Option<f64>,
    /// 性能 HUD 是否显示（`⌥P` 切换；默认展示）。
    show_perf_hud: bool,
    /// 终端自管 GPU 渲染资源（`None` = 无 wgpu 后端，走 egui Shape 路径）。
    terminal_gpu: Option<std::sync::Arc<crate::views::terminal_gpu::TerminalGpu>>,
    /// 帧耗时统计（UI 线程打点）。
    perf: crate::perf::PerfStats,
    /// 中文 fallback 字体后台加载器（生产路径由 `main` 注入；测试为空，
    /// 中文由需要它的测试自己 `setup_fonts` + `wait_ready`）。
    cjk_fonts: Option<crate::CjkFontLoader>,
    /// 应用构造时刻（启动打点基准：首帧耗时、终端就绪耗时）。
    created_at: std::time::Instant,
    /// 首帧耗时是否已记录（只记一次）。
    first_frame_reported: bool,
    /// 终端就绪耗时是否已记录（首个会话挂上标签时记一次）。
    terminal_ready_reported: bool,
    /// 后台创建中的本地终端会话（PTY fork + shell 启动不再堵住首帧）。
    pending_local: Option<PendingLocalSpawn>,
}

/// 正在后台创建的本地终端会话。
///
/// 本地会话创建要 fork PTY 并等待 shell（oh-my-zsh 用户可达数百毫秒），
/// 同步做会让窗口首帧与 ⌘T 都明显卡顿；改为后台线程创建 + 帧内轮询挂载。
/// 测试构建仍走同步路径（kittest 的 step 语义不等待后台线程），
/// 轮询挂载逻辑由 `本地终端异步就绪后挂载标签` 直接驱动 `poll_local_spawn` 覆盖。
struct PendingLocalSpawn {
    rx: std::sync::mpsc::Receiver<std::io::Result<Session>>,
    /// 打开后自动执行的启动命令（项目收藏；取首个非空行）。
    command: String,
}

/// 本地终端会话选项：默认工作目录为 home，注入 TERM 与颜色环境变量。
#[allow(dead_code)]
fn local_session_options() -> SessionOptions {
    local_session_options_at(std::env::var("HOME").ok().map(PathBuf::from))
}

/// 指定工作目录的本地终端会话选项（项目收藏打开用；`None` 时由 PTY 继承进程 cwd）。
fn local_session_options_at(dir: Option<PathBuf>) -> SessionOptions {
    SessionOptions {
        working_directory: dir,
        // TERM 必须显式注入：从 GUI/Finder/Dock 启动的进程继承 `TERM=dumb`，
        // alacritty 的 `setup_env()` 只在其主应用入口调用，mino 未调用 →
        // zsh 的 zle 判定非交互终端，删除回显走「原地空格覆盖」（删不掉+冒空格）、
        // 行编辑/回车行为异常。注入 xterm-256color 恢复完整交互（同 Miro Code 修复）。
        // 注意：勿注入 locale（LANG/LC_ALL），会引发回车不执行（实测回归）。
        // macOS `ls` 默认不输出颜色（无 CLICOLOR 环境变量），文件/目录全白；
        // 注入后按 LSCOLORS 着色区分（与 Terminal.app/iTerm2 行为一致，不篡改 shell）。
        // LSCOLORS 为深色终端优化：目录=亮青、符号链接=紫红、可执行=红、
        // socket=绿、管道=黄、块/字符设备=蓝（默认底色）。
        env: [
            ("TERM".to_string(), "xterm-256color".to_string()),
            // omp 的颜色档位：`COLORTERM=truecolor/24bit` 才判 24（`getColorMode`
            // 实证）；`TERM=xterm-256color` 只判 8。mino 渲染层本就走真彩
            // （`resolve_color` 直接 RGB），此前缺这一行让 omp 全程降级 256 色。
            ("COLORTERM".to_string(), "truecolor".to_string()),
            ("CLICOLOR".to_string(), "1".to_string()),
            ("LSCOLORS".to_string(), "Gxfxcxdxbxegedabagacad".to_string()),
        ]
        .into(),
        ..Default::default()
    }
}

/// 终端右上角悬浮 SFTP 开关按钮（tabby 风格；纯函数避免借用冲突，
/// 返回是否被点击）。打开时 accent 填充，关闭时浮层底 + 边框。
fn sftp_floating_button(ui: &mut egui::Ui, open: bool) -> bool {
    let theme = crate::theme::current_theme();
    let area = ui.max_rect();
    let btn_size = egui::vec2(60.0, 24.0);
    // 与终端内容内边距一致（PADDING=10），悬浮于终端区域右上角。
    let btn_rect = egui::Rect::from_min_size(
        egui::pos2(area.right() - 10.0 - btn_size.x, area.top() + 10.0),
        btn_size,
    );
    let (fill, stroke, fg) = if open {
        (theme.accent, egui::Stroke::NONE, egui::Color32::WHITE)
    } else {
        (
            theme.bg_elevated,
            egui::Stroke::new(1.0, theme.border),
            theme.text_secondary,
        )
    };
    let button = egui::Button::new(egui::RichText::new("SFTP").size(12.0).color(fg))
        .fill(fill)
        .stroke(stroke)
        .corner_radius(crate::theme::tokens::RADIUS_SM)
        .min_size(btn_size);
    ui.put(btn_rect, button)
        .on_hover_cursor(egui::CursorIcon::PointingHand)
        .on_hover_text(if open {
            "关闭 SFTP 面板"
        } else {
            "打开 SFTP 面板"
        })
        .clicked()
}

/// 标签栏最右侧设置图标。22×22 纯图标按钮，无 unicode 齿轮字形依赖。
///
/// 用矢量圆环与八根短齿绘制，跨平台字体不会出现方框，视觉上也更贴近
/// Mino 的控制台/仪表盘语气。
/// 包装为 `egui::Button` 以便被 kittest 通过 `Role::Button` 找到。
/// 点击调用方负责打开设置弹窗。
fn settings_gear_button(ui: &mut egui::Ui) -> bool {
    let theme = crate::theme::current_theme();
    let btn_size = 22.0;
    // 无文字 Button（透明 fill 覆盖默认背景）——kittest 通过 Role::Button 找到此控件。
    let btn = egui::Button::new("")
        .fill(egui::Color32::TRANSPARENT)
        .stroke(egui::Stroke::NONE)
        .min_size(egui::vec2(btn_size, btn_size));
    let response = ui
        .add(btn)
        .on_hover_cursor(egui::CursorIcon::PointingHand)
        .on_hover_text("设置（⌘,）");
    let rect = response.rect;
    if ui.is_rect_visible(rect) {
        if response.hovered() {
            // hover：白色 8% 圆角底（与全局控件 hover 一致）。
            ui.painter().rect_filled(
                rect,
                crate::theme::tokens::RADIUS_ITEM,
                egui::Color32::from_rgba_unmultiplied(255, 255, 255, 18),
            );
        }
        let icon_color = if response.hovered() {
            theme.accent
        } else {
            theme.text_secondary
        };
        let center = rect.center();
        let core = theme.bg_panel;
        for i in 0..8 {
            let angle = i as f32 * std::f32::consts::TAU / 8.0;
            let dir = egui::vec2(angle.cos(), angle.sin());
            ui.painter().line_segment(
                [center + dir * 5.2, center + dir * 7.0],
                egui::Stroke::new(1.5, icon_color),
            );
        }
        ui.painter()
            .circle_stroke(center, 5.0, egui::Stroke::new(1.5, icon_color));
        ui.painter().circle_filled(center, 2.0, core);
    }
    response.clicked()
}

/// 标签栏快速 SSH 连接按钮。22×22 纯图标按钮（">_" 终端符号，业界通用的
/// "命令行/终端"标识），风格与齿轮一致：次要色、hover 白 8% 圆角底。
/// 点击弹出已保存主机列表（`Popup::menu` 自行管理开关状态，Id 需稳定）。
fn ssh_quick_button(ui: &mut egui::Ui) -> egui::Response {
    let theme = crate::theme::current_theme();
    let btn_size = 22.0;
    let btn = egui::Button::new(
        egui::RichText::new(">_")
            .monospace()
            .size(11.0)
            .color(theme.text_secondary),
    )
    .fill(egui::Color32::TRANSPARENT)
    .stroke(egui::Stroke::NONE)
    .min_size(egui::vec2(btn_size, btn_size));
    let response = ui
        .add(btn)
        .on_hover_cursor(egui::CursorIcon::PointingHand)
        .on_hover_text("快速连接已保存主机");
    if response.hovered() && ui.is_rect_visible(response.rect) {
        // hover：白色 8% 圆角底（与全局控件 hover 一致）。
        ui.painter().rect_filled(
            response.rect,
            crate::theme::tokens::RADIUS_ITEM,
            egui::Color32::from_rgba_unmultiplied(255, 255, 255, 18),
        );
    }
    response
}

/// 标签栏项目收藏按钮。22×22 纯矢量文件夹图标（圆角矩形主体 + 左上标签突起），
/// 风格与齿轮/`>_ `一致：次要色描边、hover 白 8% 圆角底。
/// 禁用 unicode 文件夹符号（SF Mono 缺字形会变方块）。
/// 点击弹出项目收藏菜单（`Popup::menu` 自行管理开关状态，Id 需稳定）。
fn project_quick_button(ui: &mut egui::Ui) -> egui::Response {
    let theme = crate::theme::current_theme();
    let btn_size = 22.0;
    let btn = egui::Button::new("")
        .fill(egui::Color32::TRANSPARENT)
        .stroke(egui::Stroke::NONE)
        .min_size(egui::vec2(btn_size, btn_size));
    let response = ui
        .add(btn)
        .on_hover_cursor(egui::CursorIcon::PointingHand)
        .on_hover_text("打开项目（⌘O）");
    let rect = response.rect;
    if ui.is_rect_visible(rect) {
        if response.hovered() {
            ui.painter().rect_filled(
                rect,
                crate::theme::tokens::RADIUS_ITEM,
                egui::Color32::from_rgba_unmultiplied(255, 255, 255, 18),
            );
        }
        let icon_color = if response.hovered() {
            theme.accent
        } else {
            theme.text_secondary
        };
        let center = rect.center();
        // 文件夹主体：12×8.5 圆角矩形；标签突起在左上。
        let body =
            egui::Rect::from_center_size(center + egui::vec2(0.0, 1.0), egui::vec2(12.0, 8.5));
        ui.painter().rect_stroke(
            body,
            2.0,
            egui::Stroke::new(1.4, icon_color),
            egui::StrokeKind::Inside,
        );
        let tab = egui::Rect::from_min_size(
            egui::pos2(body.left() + 1.0, body.top() - 2.5),
            egui::vec2(5.0, 3.0),
        );
        ui.painter().rect_stroke(
            tab,
            1.0,
            egui::Stroke::new(1.4, icon_color),
            egui::StrokeKind::Inside,
        );
    }
    response
}

/// 当前 macOS 架构 → 发布产物命名（release.yml 约定）。
fn macos_arch() -> &'static str {
    match std::env::consts::ARCH {
        "aarch64" => "arm64",
        "x86_64" => "x64",
        other => other,
    }
}

/// 备份主机配置并强制 0600（备份可能含明文密码/私钥口令，而源文件
/// 权限不可信——手建或旧版本可能是 0644，copy 会保留源权限位）。
/// chmod 失败时删除备份并返回错误（宁可禁止覆盖原文件也不留明文副本）。
#[cfg(unix)]
fn backup_config(src: &Path, dst: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::copy(src, dst)?;
    match std::fs::set_permissions(dst, std::fs::Permissions::from_mode(0o600)) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(dst);
            Err(e)
        }
    }
}

#[cfg(not(unix))]
fn backup_config(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::copy(src, dst).map(|_| ())
}

/// 读取与配置文件同目录的崩溃日志（`crash.log`），取走后归档为 `crash.log.1`。
///
/// 归档而不是删除：日志是用户报告闪退的唯一证据，不能因为提示过一次就丢。
/// 每次启动只提示一次，避免归档前的每次启动都被同一个旧崩溃打扰。
/// 路径跟随 `config_path`，测试传入隔离路径时不会读到用户真实日志。
fn take_crash_report(config_path: &Path) -> Option<PathBuf> {
    let dir = config_path.parent()?;
    let log = dir.join("crash.log");
    let size = std::fs::metadata(&log).ok()?.len();
    if size == 0 {
        return None;
    }
    let archived = dir.join("crash.log.1");
    let _ = std::fs::rename(&log, &archived);
    Some(archived)
}

/// 更新工作目录：进程内复用同一私有目录（0700），目录名含 pid 与纳秒
/// 时间戳不可预测。此前固定使用 `/tmp/mino-update`：/tmp 的 sticky 位
/// 不保护子目录内容，其他本地用户可预建该目录（0777）后替换 install.sh
/// 或预放符号链接，随后被本应用以当前用户权限执行/写入。
fn update_dir() -> Result<PathBuf, String> {
    use std::sync::OnceLock;
    static DIR: OnceLock<Result<PathBuf, String>> = OnceLock::new();
    DIR.get_or_init(|| {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let base = std::env::temp_dir();
        for attempt in 0..32u32 {
            let dir = base.join(format!(
                "mino-update-{}-{nanos:x}-{attempt}",
                std::process::id()
            ));
            let mut builder = std::fs::DirBuilder::new();
            // 目录必须在创建时就是 0700；先 create_dir_all 再 chmod 会留下
            // 可被其他本地用户抢先写入的窗口，而且还会把预先存在的目录当成成功。
            #[cfg(unix)]
            std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
            match builder.create(&dir) {
                Ok(()) => return Ok(dir),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => {
                    return Err(format!("创建安全更新目录失败：{error}"));
                }
            }
        }
        Err("创建安全更新目录失败：临时目录名冲突".into())
    })
    .clone()
}

/// 下载缓存目录下的 dmg 路径。
fn temp_dmg_path(file_name: &str, sequence: u64) -> Result<PathBuf, String> {
    Ok(update_dir()?.join(format!("{file_name}.{sequence}.part")))
}

/// 安装脚本：挂载 dmg → 与应用握手 → 等待主程序退出 → 替换 .app → 重启。
/// 优先安装到 /Applications，失败回退 ~/Applications。第三个参数是状态文件，
/// 使用临时文件 + mv 原子写入，避免 UI 读到半行状态。
const INSTALL_SCRIPT: &str = r#"#!/bin/sh
set -u
DMG="$1"
MOUNT="$2"
RESULT="$3"
FINAL=""
RESULT_TMP="$RESULT.$$"
write_result() {
  printf '%s\n' "$1" > "$RESULT_TMP" 2>/dev/null || return 0
  mv -f "$RESULT_TMP" "$RESULT" 2>/dev/null || true
}
fail() {
  write_result "error:$1"
  exit 1
}
install() {
  TARGET="$1"
  OLD="$TARGET.old"
  rm -rf "$OLD" 2>/dev/null
  if [ -d "$TARGET" ] && ! mv "$TARGET" "$OLD" 2>/dev/null; then
    return 1
  fi
  if ! ditto "$SRC" "$TARGET" 2>/dev/null; then
    rm -rf "$TARGET" 2>/dev/null
    [ -d "$OLD" ] && mv "$OLD" "$TARGET" 2>/dev/null
    return 1
  fi
  rm -rf "$OLD" 2>/dev/null
  FINAL="$TARGET"
  return 0
}
mkdir -p "$MOUNT" || fail "创建挂载目录失败"
hdiutil attach "$DMG" -nobrowse -readonly -mountpoint "$MOUNT" >/dev/null 2>&1 || fail "挂载 DMG 失败"
SRC="$MOUNT/Mino.app"
[ -d "$SRC" ] || { hdiutil detach "$MOUNT" -quiet >/dev/null 2>&1 || true; fail "DMG 中未找到 Mino.app"; }
# 发布脚本会对 .app 做签名；安装前验证整个 bundle，防止下载包被篡改
# 或替换为未签名内容后直接以用户权限执行。
if ! codesign --verify --deep --strict "$SRC" >/dev/null 2>&1; then
  hdiutil detach "$MOUNT" -quiet >/dev/null 2>&1 || true
  fail "应用签名校验失败"
fi
# 先通知应用已完成挂载和源文件校验；应用收到后退出，脚本再替换正在运行的旧版本。
write_result "ready"
i=0
while [ $i -lt 50 ]; do
  if ! pgrep -x mino-app >/dev/null 2>&1; then break; fi
  sleep 0.2
  i=$((i+1))
done
if pgrep -x mino-app >/dev/null 2>&1; then
  hdiutil detach "$MOUNT" -quiet >/dev/null 2>&1 || true
  fail "等待旧版本退出超时"
fi
install "/Applications/Mino.app" || install "$HOME/Applications/Mino.app" || { hdiutil detach "$MOUNT" -quiet >/dev/null 2>&1 || true; fail "替换应用失败"; }
hdiutil detach "$MOUNT" -quiet >/dev/null 2>&1 || true
rmdir "$MOUNT" 2>/dev/null || true
rm -f "$DMG" 2>/dev/null
open "$FINAL" || fail "启动新版本失败"
write_result "success"
"#;

impl MinoApp {
    /// 创建应用（启动本地终端会话）。
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        #[cfg(test)]
        {
            Self::new_with_config_inner(cc, test_config_path("default"), false)
        }
        #[cfg(not(test))]
        {
            Self::new_with_config(cc, mino_core::config::default_config_path())
        }
    }

    /// 指定配置文件路径创建应用。
    ///
    /// 测试必须走这里传入隔离路径——曾发生测试直接读写并删除用户真实的
    /// `~/.config/mino/hosts.toml`（default_config_path），运行一次测试
    /// 主机列表就丢一次（表现为"更新后主机全部消失"）。
    pub fn new_with_config(cc: &eframe::CreationContext<'_>, config_path: PathBuf) -> Self {
        Self::new_with_config_inner(cc, config_path, cfg!(not(test)))
    }

    /// 构造应用的内部实现；测试构建关闭自动更新，避免网络与后台线程污染 UI 测试。
    fn new_with_config_inner(
        cc: &eframe::CreationContext<'_>,
        config_path: PathBuf,
        auto_update: bool,
    ) -> Self {
        let ctx = cc.egui_ctx.clone();

        // 终端自管 GPU 渲染资源：只有持有 wgpu 后端时才有
        // （`Harness::new_ui` 的测试没有，天然覆盖 egui 回退路径）。
        let terminal_gpu = cc
            .wgpu_render_state
            .as_ref()
            .map(|state| std::sync::Arc::new(crate::views::terminal_gpu::TerminalGpu::new(state)));

        // 加载失败不能静默按空配置启动：文件存在但解析失败时先备份原文，
        // 避免后续保存把用户主机列表覆盖掉。
        let (config, load_message, config_write_blocked) = match HostConfig::load(&config_path) {
            Ok(c) => (c, None, false),
            Err(e) => {
                let existed = config_path.exists();
                if !existed && e.kind() == std::io::ErrorKind::NotFound {
                    (HostConfig::default(), None, false)
                } else if existed {
                    let bak = config_path.with_extension("toml.bak");
                    match backup_config(&config_path, &bak) {
                        Ok(()) => {
                            log::error!("主机配置加载失败（原文已备份为 {bak:?}）：{e}");
                            (
                                HostConfig::default(),
                                Some(format!("主机配置读取失败，原文已备份为 {}", bak.display())),
                                false,
                            )
                        }
                        Err(be) => {
                            log::error!("备份主机配置到 {bak:?} 失败：{be}");
                            log::error!("主机配置加载失败，已禁止覆盖原文件：{e}");
                            (
                                HostConfig::default(),
                                Some(format!(
                                    "主机配置读取失败且备份失败，已禁止覆盖原文件：{be}"
                                )),
                                true,
                            )
                        }
                    }
                } else {
                    log::error!("主机配置加载失败，已禁止覆盖原文件：{e}");
                    (
                        HostConfig::default(),
                        Some(format!("主机配置读取失败，已禁止覆盖原文件：{e}")),
                        true,
                    )
                }
            }
        };

        // 主题恢复：已保存的主题名 → 对应下标；未知/空则默认第一套
        // （曾启动硬编码 set_theme(0)，切换皮肤退出重进永远回到原来的）。
        let initial_theme = crate::theme::theme_index_by_name(config.theme.trim()).unwrap_or(0);
        crate::theme::set_theme(&ctx, initial_theme);

        let mut app = Self {
            tabs: Vec::new(),
            active_tab: 0,
            pending_tab: None,
            pending_connection_id: None,
            next_id: 1,
            last_row_click: None,
            selected_host: None,
            show_settings: false,
            show_projects: false,
            project_filter: String::new(),
            project_selected: 0,
            last_ctx: cc.egui_ctx.clone(),
            config,
            config_path,
            config_write_blocked,
            show_new_conn: false,
            settings_before_new_conn: false,
            form: ConnectForm::default(),
            project_edit: None,
            pending: None,
            pending_connect_cancel: None,
            pending_label: String::new(),
            toast: None,
            pending_sftp: None,
            ready_sftp: None,
            sftp_error: None,
            pending_image_paste: None,
            update_state: UpdateState::Idle,
            update_rx: None,
            download_rx: None,
            download_cancel: None,
            download_path: None,
            download_sequence: 0,
            manual_update: false,
            restart_at: None,
            install_result_path: None,
            install_dmg_path: None,
            install_started_at: None,
            show_perf_hud: true,
            perf: crate::perf::PerfStats::new(),
            terminal_gpu,
            cjk_fonts: None,
            created_at: std::time::Instant::now(),
            first_frame_reported: false,
            terminal_ready_reported: false,
            pending_local: None,
        };
        // 启动时自动检查更新（后台线程，延迟 3 秒，静默）。
        if let Some(message) = load_message {
            app.show_toast(message, true);
        }
        // 上次运行崩溃过：把日志路径告诉用户（闪退时窗口直接消失，用户
        // 除了这个提示没有任何线索），同时归档以免每次启动都提示。
        // 测试构建跳过：隔离配置目录里没有真实崩溃日志，也不能让测试
        // 读到 /tmp/crash.log 这类无关文件（行为由 `take_crash_report`
        // 自己的单元测试覆盖）。
        #[cfg(not(test))]
        if let Some(report) = take_crash_report(&app.config_path) {
            app.show_toast(
                format!("上次运行发生崩溃，日志：{}", report.display()),
                true,
            );
        }
        if auto_update {
            app.start_update_check(true, &ctx);
        }
        // 初始一个本地终端 tab；设置改弹窗（`show_settings`）。
        app.new_local_tab(&ctx);
        app
    }

    /// 弹出轻提示（自动淡出）。
    fn show_toast(&mut self, message: impl Into<String>, is_error: bool) {
        self.toast = Some(Toast {
            message: message.into(),
            is_error,
            start: f64::NAN,
        });
    }

    /// 新建本地终端标签页并激活。
    fn new_local_tab(&mut self, ctx: &egui::Context) {
        let home = std::env::var("HOME").ok().map(PathBuf::from);
        self.new_local_tab_at(ctx, home, "");
    }

    /// 打开项目收藏：新建以项目目录为工作目录的本地标签，启动命令非空时自动执行。
    ///
    /// 目录不存在（被删/移动/外接盘拔出）时只 toast，不建 tab。
    fn open_project(&mut self, ctx: &egui::Context, project: &ProjectProfile) {
        if !project.path.is_dir() {
            self.show_toast(format!("项目目录不存在：{}", project.path.display()), true);
            return;
        }
        self.new_local_tab_at(ctx, Some(project.path.clone()), &project.command);
    }

    /// 带目录与启动命令的本地标签构造（`new_local_tab` 与 `open_project` 共用）。
    ///
    /// 生产构建在后台线程创建会话（PTY fork + shell 启动/oh-my-zsh 初始化
    /// 可达数百毫秒，同步做会推迟首帧、也让 ⌘T 明显卡顿），本帧只登记
    /// `pending_local`，就绪后由 `poll_local_spawn` 挂上标签。
    fn new_local_tab_at(&mut self, ctx: &egui::Context, dir: Option<PathBuf>, command: &str) {
        let options = local_session_options_at(dir);
        let command = command.to_string();

        // 测试构建同步创建：kittest 的 `run_steps` 不等待后台线程，
        // 大量用例在 `MinoApp::new` 后立即断言标签存在。异步挂载路径由
        // `本地终端异步就绪后挂载标签` 直接驱动 `poll_local_spawn` 覆盖。
        #[cfg(test)]
        {
            let on_event = Arc::new(move |_ev: &SessionEvent| {});
            let _ = ctx;
            match Session::spawn_local(options, 80, 24, on_event) {
                Ok(session) => self.mount_local_session(session, &command),
                Err(e) => {
                    log::error!("启动本地终端失败：{e}");
                    self.show_toast(format!("启动本地终端失败：{e}"), true);
                }
            }
        }

        #[cfg(not(test))]
        {
            let (tx, rx) = std::sync::mpsc::channel();
            let spawn_ctx = ctx.clone();
            let notify_ctx = ctx.clone();
            std::thread::spawn(move || {
                let on_event = Arc::new(move |_ev: &SessionEvent| spawn_ctx.request_repaint());
                let result = Session::spawn_local(options, 80, 24, on_event);
                let _ = tx.send(result);
                // 结果到达即唤醒 UI 去取（本帧之外才有结果，必须自己请求重绘）。
                notify_ctx.request_repaint();
            });
            self.pending_local = Some(PendingLocalSpawn { rx, command });
            // 让"正在启动终端…"占位立刻可见。
            ctx.request_repaint();
        }
    }

    /// 把已创建的本地会话挂上标签栏（异步与同步路径共用）。
    fn mount_local_session(&mut self, session: Session, command: &str) {
        let mut view = TerminalView::new(session);
        view.set_gpu(self.terminal_gpu.clone());
        // 启动命令只取首个非空行：多行粘贴会被 shell 逐行执行，
        // 配置里换行只可能是误粘贴，不应多行注入。
        if let Some(line) = command.lines().map(str::trim).find(|l| !l.is_empty()) {
            view.session().write(format!("{line}\n").as_bytes());
        }
        let id = self.allocate_id();
        let tab = Box::new(TerminalTab::new(id, "本地终端".into(), view));
        self.tabs.push(tab);
        self.active_tab = self.tabs.len() - 1;
        if !self.terminal_ready_reported {
            self.terminal_ready_reported = true;
            self.perf
                .set_terminal_ready_ms(self.created_at.elapsed().as_secs_f32() * 1000.0);
        }
    }

    /// 处理后台创建中的本地终端：就绪即挂标签，失败即提示。
    ///
    /// 每帧调用；无等待中的会话时零成本。
    fn poll_local_spawn(&mut self, ctx: &egui::Context) {
        let Some(pending) = self.pending_local.as_ref() else {
            return;
        };
        let result = match pending.rx.try_recv() {
            Ok(result) => result,
            Err(std::sync::mpsc::TryRecvError::Empty) => return,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                self.pending_local = None;
                self.show_toast("启动本地终端失败：会话创建线程异常退出", true);
                return;
            }
        };
        let command = self
            .pending_local
            .take()
            .map(|pending| pending.command)
            .unwrap_or_default();
        match result {
            Ok(session) => {
                self.mount_local_session(session, &command);
                ctx.request_repaint();
            }
            Err(e) => {
                log::error!("启动本地终端失败：{e}");
                self.show_toast(format!("启动本地终端失败：{e}"), true);
            }
        }
    }

    /// 注入中文 fallback 字体后台加载器（生产路径由 `main` 调用）。
    pub fn set_cjk_font_loader(&mut self, loader: crate::CjkFontLoader) {
        self.cjk_fonts = Some(loader);
    }

    /// 收藏当前终端目录为项目（⌘D 与快捷菜单空态入口共用）。
    ///
    /// 仅本地标签可用：目录取 `TerminalView::fresh_local_directory`
    /// （内核 cwd 优先、跟踪值回退，不走标题用的 TTL 缓存——用户按 ⌘D 的
    /// 那一刻目录可能刚变过，缓存 300ms 的旧值会收藏错地方）——纯跟踪值在
    /// 粘贴/补全/别名场景下会停在启动目录，标题与 SFTP 定位都不依赖它；
    /// 去重只看规范路径（重名允许），默认名取末级目录名。
    fn bookmark_current_directory(&mut self) {
        let Some(tab) = self.tabs.get(self.active_tab) else {
            self.show_toast("没有可收藏的终端", true);
            return;
        };
        if tab.terminal.session().is_remote() {
            self.show_toast("仅支持收藏本地终端目录", true);
            return;
        }
        let Some(cwd) = tab.terminal.fresh_local_directory() else {
            self.show_toast("当前目录未知，稍后再试", true);
            return;
        };
        let canonical = match std::fs::canonicalize(&cwd) {
            Ok(p) => p,
            Err(e) => {
                self.show_toast(format!("无法读取当前目录：{e}"), true);
                return;
            }
        };
        let duplicate =
            self.config.projects.iter().any(|p| {
                std::fs::canonicalize(&p.path).is_ok_and(|existing| existing == canonical)
            });
        if duplicate {
            self.show_toast("已收藏过该目录", true);
            return;
        }
        // 默认名 = 末级目录名；根目录无 file_name 时用全路径本身。
        let name = canonical
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| canonical.to_string_lossy().into_owned());
        self.config.projects.push(ProjectProfile {
            name: name.clone(),
            path: canonical,
            command: String::new(),
        });
        if !self.save_config() {
            self.config.projects.pop();
            return;
        }
        self.show_toast(format!("已收藏「{name}」"), false);
    }

    fn close_tab(&mut self, index: usize) {
        if index >= self.tabs.len() {
            return;
        }
        let tab_id = self.tabs[index].id;
        if self.pending_tab == Some(tab_id) {
            self.pending_tab = None;
            self.close_ready_sftp(tab_id);
        }
        if self
            .pending_sftp
            .as_ref()
            .is_some_and(|connection| connection.connection_id == tab_id)
        {
            self.close_pending_sftp(tab_id);
        }
        // 已挂载的 SFTP：显式 close，置位取消标志立即中止进行中的传输
        // 并清理半成品（此前只靠 handle drop，传输会继续跑到完成）。
        if let Some(sftp) = self.tabs[index].sftp.as_ref() {
            sftp.close();
        }
        self.tabs.remove(index);
        if self.tabs.is_empty() {
            self.active_tab = 0;
        } else if self.active_tab >= self.tabs.len() {
            self.active_tab = self.tabs.len() - 1;
        } else if self.active_tab > index {
            self.active_tab -= 1;
        }
    }

    /// 分配不会随标签列表变化的身份。
    fn allocate_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1).max(1);
        id
    }

    /// 关闭指定连接的待处理 SFTP 会话。
    fn close_pending_sftp(&mut self, connection_id: u64) {
        if self
            .pending_sftp
            .as_ref()
            .is_some_and(|connection| connection.connection_id == connection_id)
        {
            if let Some(connection) = self.pending_sftp.take() {
                connection.handle.close();
            }
        }
    }

    /// 关闭指定连接的已就绪但尚未挂载的 SFTP 会话。
    fn close_ready_sftp(&mut self, connection_id: u64) {
        if self
            .ready_sftp
            .as_ref()
            .is_some_and(|connection| connection.connection_id == connection_id)
        {
            if let Some(connection) = self.ready_sftp.take() {
                connection.handle.close();
            }
        }
    }

    /// 启动后台更新检查（delay=true 时延迟 3 秒，避免影响启动）。
    fn start_update_check(&mut self, delay: bool, ctx: &egui::Context) {
        let (tx, rx) = std::sync::mpsc::channel();
        let current = env!("CARGO_PKG_VERSION").to_string();
        let arch = macos_arch().to_string();
        let repaint_ctx = ctx.clone();
        std::thread::spawn(move || {
            if delay {
                std::thread::sleep(Duration::from_secs(3));
            }
            let result = check_for_update(&current, mino_core::updater::DEFAULT_REPO, &arch);
            let _ = tx.send(result);
            repaint_ctx.request_repaint();
        });
        self.update_rx = Some(rx);
        self.update_state = UpdateState::Checking;
        self.manual_update = !delay;
    }

    /// 处理更新检查结果。
    fn poll_update(&mut self) {
        let mut result = None;
        if let Some(rx) = &self.update_rx {
            while let Ok(r) = rx.try_recv() {
                result = Some(r);
            }
        }
        if let Some(result) = result {
            self.update_rx = None;
            match result {
                Ok(Some(info)) => self.update_state = UpdateState::Available(info),
                Ok(None) => {
                    self.update_state = UpdateState::UpToDate;
                    if self.manual_update {
                        self.show_toast(
                            format!("已是最新版本 v{}", env!("CARGO_PKG_VERSION")),
                            false,
                        );
                    }
                }
                Err(e) => {
                    self.update_state = UpdateState::Failed;
                    if self.manual_update {
                        self.show_toast(format!("检查更新失败：{e}"), true);
                    } else {
                        log::debug!("检查更新失败：{e}");
                    }
                }
            }
            self.manual_update = false;
        }
    }

    /// 开始下载更新资产。
    fn start_download(&mut self, info: UpdateInfo, ctx: &egui::Context) {
        self.cancel_download();
        let (tx, rx) = std::sync::mpsc::channel();
        let url = info.asset_url.clone();
        let sequence = self.download_sequence;
        self.download_sequence = self.download_sequence.wrapping_add(1);
        let dest = match temp_dmg_path(&info.asset_name, sequence) {
            Ok(path) => path,
            Err(error) => {
                self.update_state = UpdateState::Error(error);
                ctx.request_repaint();
                return;
            }
        };
        let cancel = Arc::new(AtomicBool::new(false));
        let thread_cancel = cancel.clone();
        let thread_dest = dest.clone();
        let repaint_ctx = ctx.clone();
        std::thread::spawn(move || {
            let mut last_repaint = Instant::now() - Duration::from_secs(1);
            let result = mino_core::updater::download_asset_with_cancel(
                &url,
                &thread_dest,
                &thread_cancel,
                |done, total| {
                    let _ = tx.send(DownloadEvent::Progress {
                        downloaded: done,
                        total,
                    });
                    if last_repaint.elapsed() >= Duration::from_millis(50) {
                        repaint_ctx.request_repaint();
                        last_repaint = Instant::now();
                    }
                },
            );
            if thread_cancel.load(Ordering::Relaxed) {
                let _ = std::fs::remove_file(&thread_dest);
                return;
            }
            repaint_ctx.request_repaint();
            match result {
                Ok(()) => {
                    let _ = tx.send(DownloadEvent::Done(thread_dest));
                }
                Err(e) => {
                    let _ = tx.send(DownloadEvent::Error(e));
                }
            }
        });
        self.download_rx = Some(rx);
        self.download_cancel = Some(cancel);
        self.download_path = Some(dest);
        self.update_state = UpdateState::Downloading(DownloadState {
            info,
            downloaded: 0,
            total: None,
        });
    }

    /// 处理下载进度/结果。
    fn poll_download(&mut self, ctx: &egui::Context) {
        let mut events = Vec::new();
        if let Some(rx) = &self.download_rx {
            while let Ok(ev) = rx.try_recv() {
                events.push(ev);
            }
        }
        for ev in events {
            let info = match &self.update_state {
                UpdateState::Downloading(s) => Some(s.info.clone()),
                _ => None,
            };
            let Some(info) = info else {
                continue;
            };
            match ev {
                DownloadEvent::Progress { downloaded, total } => {
                    self.update_state = UpdateState::Downloading(DownloadState {
                        info,
                        downloaded,
                        total,
                    });
                    ctx.request_repaint();
                }
                DownloadEvent::Done(path) => {
                    self.download_rx = None;
                    self.download_cancel = None;
                    self.download_path = None;
                    self.update_state = UpdateState::Downloaded {
                        info,
                        dmg_path: path,
                    };
                }
                DownloadEvent::Error(e) => {
                    self.download_rx = None;
                    self.download_cancel = None;
                    self.download_path = None;
                    self.update_state = UpdateState::Error(e);
                }
            }
        }
    }

    /// 取消下载并清理当前临时文件。
    fn cancel_download(&mut self) {
        if let Some(cancel) = self.download_cancel.take() {
            cancel.store(true, Ordering::Relaxed);
        }
        if let Some(path) = self.download_path.take() {
            let _ = std::fs::remove_file(path);
        }
        self.download_rx = None;
    }

    /// 启动安装脚本并安排重启。
    fn install_update(&mut self, ctx: &egui::Context, dmg_path: PathBuf) {
        let UpdateState::Downloaded { info, .. } = &self.update_state else {
            return;
        };
        let info = info.clone();
        let dir = match update_dir() {
            Ok(dir) => dir,
            Err(error) => {
                let _ = std::fs::remove_file(&dmg_path);
                self.update_state = UpdateState::Error(error);
                return;
            }
        };
        let sequence = self.download_sequence;
        self.download_sequence = self.download_sequence.wrapping_add(1);
        let mount = dir.join(format!("mount-{}-{sequence}", std::process::id()));
        let result_path = dir.join(format!(
            "install-result-{}-{sequence}.txt",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&result_path);
        match launch_installer(&dmg_path, &mount, &result_path) {
            Ok(()) => {
                self.update_state = UpdateState::Installing(info);
                self.install_result_path = Some(result_path);
                self.install_dmg_path = Some(dmg_path);
                self.install_started_at = Some(anim::now(ctx));
                self.restart_at = None;
                ctx.request_repaint_after(Duration::from_millis(100));
            }
            Err(e) => {
                let _ = std::fs::remove_file(&dmg_path);
                self.update_state = UpdateState::Error(format!("启动安装脚本失败：{e}"));
            }
        }
    }

    /// 清理安装失败后不再会被脚本使用的 DMG。
    fn cleanup_install_dmg(&mut self) {
        if let Some(path) = self.install_dmg_path.take() {
            let _ = std::fs::remove_file(path);
        }
    }

    /// 轮询安装脚本状态；仅收到握手或最终成功状态后才安排退出。
    fn poll_install(&mut self, ctx: &egui::Context) {
        if !matches!(self.update_state, UpdateState::Installing(_)) {
            return;
        }
        let Some(result_path) = self.install_result_path.clone() else {
            self.cleanup_install_dmg();
            self.update_state = UpdateState::Error("安装脚本缺少状态文件".into());
            return;
        };
        let status = match std::fs::read_to_string(result_path) {
            Ok(status) => status.trim().to_string(),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(e) => {
                self.cleanup_install_dmg();
                self.update_state = UpdateState::Error(format!("读取安装状态失败：{e}"));
                self.install_result_path = None;
                self.install_started_at = None;
                return;
            }
        };
        if status == "ready" || status == "success" {
            // ready 表示脚本已完成挂载与校验，应用退出后脚本才会继续替换旧版本。
            self.update_state = UpdateState::Installed;
            self.restart_at = Some(anim::now(ctx) + 0.9);
            self.install_started_at = None;
            ctx.request_repaint_after(Duration::from_millis(50));
            return;
        }
        if let Some(message) = status.strip_prefix("error:") {
            self.cleanup_install_dmg();
            self.update_state = UpdateState::Error(format!("安装失败：{message}"));
            self.install_result_path = None;
            self.install_started_at = None;
            return;
        }
        if self
            .install_started_at
            .is_some_and(|started| anim::now(ctx) - started > 120.0)
        {
            self.cleanup_install_dmg();
            self.update_state = UpdateState::Error("安装脚本 120 秒内未返回状态".into());
            self.install_result_path = None;
            self.install_started_at = None;
            return;
        }
        ctx.request_repaint_after(Duration::from_millis(100));
    }

    /// 保存主机配置到磁盘，并把失败反馈给用户。
    fn save_config(&mut self) -> bool {
        if self.config_write_blocked {
            self.show_toast("主机配置保存已阻止：原文件未能备份", true);
            return false;
        }
        match self.config.save(&self.config_path) {
            Ok(()) => true,
            Err(e) => {
                log::warn!("保存主机配置失败：{e}");
                self.show_toast(format!("保存主机配置失败：{e}"), true);
                false
            }
        }
    }

    /// 切换主题并持久化到配置（退出重进保持所选皮肤）。
    ///
    /// 保存失败时回退到原主题并 toast 提示：内存态与落盘态必须一致，
    /// 否则本次看着切成功了、重启又回到原来的（用户反馈的根因）。
    fn apply_theme_and_persist(&mut self, ctx: &egui::Context, index: usize) {
        let previous = self.config.theme.clone();
        let name = crate::theme::THEMES[index.min(crate::theme::THEMES.len() - 1)].name;
        crate::theme::set_theme(ctx, index);
        self.config.theme = name.to_string();
        if !self.save_config() {
            // 落盘失败：内存态回滚，保持与磁盘一致。
            self.config.theme = previous;
            let rollback = crate::theme::theme_index_by_name(self.config.theme.trim()).unwrap_or(0);
            crate::theme::set_theme(ctx, rollback);
            return;
        }
        self.show_toast(format!("主题：{name}"), false);
    }

    /// 打开一个全新的连接表单。
    ///
    /// “新建连接”始终代表新建配置，不能沿用上一次输入的主机、密码或私钥
    /// 口令；关闭对话框后再次打开也必须回到默认值。
    fn open_new_connection(&mut self) {
        self.form = ConnectForm::default();
        self.last_row_click = None;
        // 新建连接弹窗和设置弹窗都是居中的模态窗口；从设置里的入口
        // 打开时必须先收起设置，否则设置层会盖住新建连接表单。
        self.settings_before_new_conn = self.show_settings;
        self.show_settings = false;
        self.show_new_conn = true;
    }

    /// 处理进行中的连接结果。
    fn poll_connection(&mut self, ctx: &egui::Context) {
        let mut result = None;
        if let Some(rx) = &mut self.pending {
            while let Ok(r) = rx.try_recv() {
                result = Some(r);
            }
        }
        if let Some(result) = result {
            self.pending = None;
            // 结果已经到达，连接建立阶段结束；不要再保留取消句柄。
            self.pending_connect_cancel = None;
            match result {
                ConnectResult::Connected(session) => {
                    let connection_id = self
                        .pending_connection_id
                        .take()
                        .unwrap_or_else(|| self.allocate_id());
                    let mut view = TerminalView::new(session);
                    view.set_gpu(self.terminal_gpu.clone());
                    self.tabs.push(Box::new(TerminalTab::new(
                        connection_id,
                        self.pending_label.clone(),
                        view,
                    )));
                    self.active_tab = self.tabs.len() - 1;
                    self.pending_tab = Some(connection_id);
                    self.mount_ready_sftp();
                    self.show_toast(format!("已连接到 {}", self.pending_label), false);
                    ctx.request_repaint();
                }
                ConnectResult::Failed(e) => {
                    self.pending_connection_id = None;
                    if let Some(connection) = self.pending_sftp.take() {
                        connection.handle.close();
                    }
                    if let Some(connection) = self.ready_sftp.take() {
                        connection.handle.close();
                    }
                    log::error!("连接失败：{e}");
                    self.show_toast(format!("连接失败：{e}"), true);
                    ctx.request_repaint();
                }
            }
        }
    }

    /// 发起远程连接（同时启动 SFTP 连接）。
    fn start_connect(&mut self, ctx: &egui::Context, profile: HostProfile) {
        self.last_row_click = None;
        if let Some(connection) = self.pending_sftp.take() {
            connection.handle.close();
        }
        if let Some(connection) = self.ready_sftp.take() {
            connection.handle.close();
        }
        if let Some(cancel) = self.pending_connect_cancel.take() {
            cancel.cancel();
        }
        self.pending = None;
        self.pending_tab = None;
        let connection_id = self.allocate_id();
        self.pending_connection_id = Some(connection_id);
        // 标签展示新建连接时填写的名称，不把用户名和远程当前路径带进来。
        let label = profile.name.clone();
        let terminal_ctx = ctx.clone();
        let on_event = Arc::new(move |_ev: &SessionEvent| {
            terminal_ctx.request_repaint();
        });
        let (_thread, rx, connect_cancel) = connect_remote_with_cancel(&profile, 80, 24, on_event);
        self.pending = Some(rx);
        self.pending_connect_cancel = Some(connect_cancel);
        self.pending_label = label.clone();

        let sftp_ctx = ctx.clone();
        let on_sftp_event = Arc::new(move || {
            sftp_ctx.request_repaint();
        });
        let (_sftp_thread, sftp_handle, sftp_rx) =
            connect_sftp_with_handler(&profile, on_sftp_event);
        // 主机名随本连接绑定，避免与并发连接串台。
        self.pending_sftp = Some(SftpConnection {
            connection_id,
            handle: sftp_handle,
            rx: sftp_rx,
            host: label,
            home: None,
        });
        self.sftp_error = None;
    }

    /// 将已就绪的 SFTP 会话挂载到 SSH 标签页。
    fn mount_ready_sftp(&mut self) {
        let Some(connection_id) = self.pending_tab else {
            return;
        };
        let Some(connection) = self.ready_sftp.take() else {
            return;
        };
        if connection.connection_id != connection_id {
            connection.handle.close();
            return;
        }
        let Some(tab) = self.tabs.iter_mut().find(|tab| tab.id == connection_id) else {
            connection.handle.close();
            self.pending_tab = None;
            return;
        };
        {
            let home = connection.home.unwrap_or_else(|| "/".to_string());
            tab.terminal.set_remote_current_directory(&home);
            tab.sftp = Some(SftpView::new_at_path(
                &connection.host,
                connection.handle,
                connection.rx,
                &home,
            ));
        }
        // SFTP 已经挂载到目标标签；保留 pending_tab 会让后续关闭标签或
        // 无关连接结果继续把它误当成“等待挂载”的目标。
        self.pending_tab = None;
    }

    /// SFTP“定位到终端位置”：本地读内核 cwd 直接导航，远程先 `pwd` 探测。
    ///
    /// 用户期望“在某一路径下点定位就能到当前目录”，但终端输入跟踪在
    /// Tab/粘贴/别名/函数/`cd -`/复合命令等场景下会失效或保守放弃，
    /// 直接用旧推测值导航就是“只有 pwd 后才好用”的根因。本地会话直接读
    /// shell 子进程的内核 cwd（无注入、无等待）；远程仍在空闲提示符下
    /// 注入一条 `pwd`，用 shell 真正的输出校正后再导航。
    fn begin_locate_terminal(tab: &mut TerminalTab, ctx: &egui::Context) {
        let Some(sftp) = tab.sftp.as_mut() else {
            return;
        };
        // SFTP 面板要求终端上下文才显示定位入口；本地 tab 同样支持
        // （current_directory 本地恒为 Some）。
        if tab.terminal.current_directory().is_none() {
            return;
        }
        // 本地会话直接读 shell 子进程的内核 cwd（source/别名/函数等场景下
        // 输入跟踪早已失效，`pwd` 探测还要往用户终端里注命令；内核值无
        // 注入、无延迟，直接导航）。
        if !tab.terminal.session().is_remote() {
            if let Some(dir) = tab.terminal.session().child_current_dir() {
                sftp.locate_terminal_directory(&dir.to_string_lossy());
                ctx.request_repaint();
                return;
            }
        }
        if tab.terminal.request_fresh_pwd() {
            // 探测已注入：等待终端输出（见 `poll_locate_pending`），
            // 这几帧内保持重绘，pwd 回显/输出到达后第一时间导航。
            tab.locate_pending = Some(LocatePending {
                started_at: ctx.input(|i| i.time),
            });
            ctx.request_repaint();
        } else {
            // 不适合探测（全屏应用/有未执行输入/已有探测在途）：直接用
            // 已知目录回退，行为与此前一致，不让用户觉得“点了没反应”。
            if let Some(path) = tab.terminal.current_directory() {
                sftp.locate_terminal_directory(&path);
                ctx.request_repaint();
            }
        }
    }

    /// 每帧推进等待中的定位探测：拿到 `pwd` 输出就导航，超时则回退。
    fn poll_locate_pending(&mut self, ctx: &egui::Context) {
        // 定位超时：远端高延迟/输出被全屏应用吞掉时不能无限等待；
        // 取消探测、用已知目录回退并给明确反馈。
        const LOCATE_TIMEOUT_SECS: f64 = 3.0;
        let now = ctx.input(|i| i.time);
        // locate_pending 按标签独立保存；SFTP 未挂载的标签直接清理。
        for tab in &mut self.tabs {
            let Some(pending) = tab.locate_pending.as_ref() else {
                continue;
            };
            let Some(sftp) = tab.sftp.as_mut() else {
                tab.locate_pending = None;
                tab.terminal.cancel_fresh_pwd();
                continue;
            };
            if tab.terminal.auto_pwd_ready() {
                tab.locate_pending = None;
                if let Some(path) = tab.terminal.current_directory() {
                    sftp.locate_terminal_directory(&path);
                }
                ctx.request_repaint();
            } else if now - pending.started_at >= LOCATE_TIMEOUT_SECS {
                tab.locate_pending = None;
                tab.terminal.cancel_fresh_pwd();
                if let Some(path) = tab.terminal.current_directory() {
                    sftp.locate_terminal_directory(&path);
                }
                ctx.request_repaint();
            } else {
                // 等待输出期间保持重绘，pwd 结果到达后下一帧即导航。
                ctx.request_repaint();
            }
        }
    }

    /// 每帧推进远程图片粘贴：终端产出本地中转 → 经面板 SFTP 上传 →
    /// `Done` 后向该 tab 写远端 `@token`。
    ///
    /// - 无 SFTP / 已关闭：`toast` 提示并丢弃（不写 broken 路径），
    ///   本地中转留 `/tmp` 自清；
    /// - 上传中：复用面板传输进度条（`begin_transfer` 已自动展开）；
    /// - `tab` 已关闭：按 id 找不到即丢弃。
    fn poll_image_paste(&mut self, ctx: &egui::Context) {
        // 借用分离：先把各 tab 的产出与错误收集到局部，再统一处理
        // （`show_toast` 要 `&mut self`，不能在 `&mut self.tabs` 循环内调用）。
        struct NewUpload {
            tab_id: u64,
            local: PathBuf,
        }
        let mut new_uploads: Vec<NewUpload> = Vec::new();
        let mut no_sftp_tabs: Vec<u64> = Vec::new();
        let mut closed_sftp_tabs: Vec<u64> = Vec::new();
        let mut paste_errors: Vec<String> = Vec::new();
        for tab in &mut self.tabs {
            if let Some(local) = tab.terminal.take_pending_image() {
                // 本地会话已在 `TerminalView::handle_image_paste` 内直接写入，
                // 能到这里的一定是远程（`take_pending_image` 本地恒为 `None`）。
                match tab.sftp.as_ref() {
                    None => no_sftp_tabs.push(tab.id),
                    Some(sftp) if sftp.is_closed() => closed_sftp_tabs.push(tab.id),
                    Some(_) => new_uploads.push(NewUpload {
                        tab_id: tab.id,
                        local,
                    }),
                }
            }
            if let Some(message) = tab.terminal.take_image_paste_error() {
                paste_errors.push(message);
            }
            if let Some(message) = tab.terminal.take_clipboard_write_error() {
                paste_errors.push(message);
            }
        }
        for tab_id in no_sftp_tabs {
            let _ = tab_id;
            self.show_toast("远程会话需先连接 SFTP 才能粘贴图片", true);
        }
        for tab_id in closed_sftp_tabs {
            let _ = tab_id;
            self.show_toast("SFTP 已关闭，图片未能上传", true);
        }
        for message in paste_errors {
            self.show_toast(format!("图片粘贴失败：{message}"), true);
        }
        // 1. 新中转经面板 SFTP 发起上传（复用传输进度条，自动展开）。
        for upload in new_uploads {
            let Some(tab) = self.tabs.iter_mut().find(|tab| tab.id == upload.tab_id) else {
                continue;
            };
            let Some(sftp) = tab.sftp.as_mut() else {
                self.show_toast("远程会话需先连接 SFTP 才能粘贴图片", true);
                continue;
            };
            let name = upload
                .local
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| "mino-paste.png".to_string());
            let remote = crate::views::sftp_view::join_path(sftp.current_remote_dir(), &name);
            let transfer_id = sftp.upload_local_file(&upload.local);
            self.pending_image_paste = Some(PendingImagePaste {
                tab_id: tab.id,
                remote_path: remote,
                transfer_id,
            });
            ctx.request_repaint();
        }
        // 3. 上传完成即写远端 token（`SftpView::poll_events` 已消费事件，
        // 此处按传输 id 在面板传输记录中确认完成/失败）。
        let Some(pending) = self.pending_image_paste.take() else {
            return;
        };
        let Some(tab) = self.tabs.iter_mut().find(|tab| tab.id == pending.tab_id) else {
            return;
        };
        let Some(sftp) = tab.sftp.as_mut() else {
            self.show_toast("SFTP 已关闭，图片未能上传", true);
            return;
        };
        match sftp.transfer_result(pending.transfer_id) {
            Some(true) => {
                let token = format!(
                    "@{}",
                    crate::clip_image::shell_escape_for_token(&pending.remote_path)
                );
                tab.terminal.session().write(token.as_bytes());
                tab.terminal.push_pasted_text(&token);
                ctx.request_repaint();
            }
            Some(false) => {
                self.show_toast("图片上传失败，详见 SFTP 面板", true);
            }
            None => {
                // 传输仍在进行中：放回等待，下一帧继续。
                self.pending_image_paste = Some(pending);
            }
        }
    }

    /// 处理 SFTP 连接结果。
    fn poll_sftp(&mut self) {
        let mut ready = false;
        let mut failed: Option<String> = None;
        let mut closed = false;
        if let Some(connection) = &mut self.pending_sftp {
            while let Ok(ev) = connection.rx.try_recv() {
                match ev {
                    SftpEvent::Ready { home: path } => {
                        ready = true;
                        connection.home = Some(path);
                    }
                    SftpEvent::Failed(e) => failed = Some(e),
                    // 连接中途关闭（如被服务器断开）：不能继续等待，
                    // 否则状态栏会永远停在"SFTP 连接中…"。
                    SftpEvent::Closed => closed = true,
                    _ => {}
                }
            }
        }
        // SFTP 可能先于 SSH 终端就绪；在等待终端连接结果期间仍要轮询
        // ready_sftp，否则服务器随后断开时该连接会被遗留到挂载阶段。
        if let Some(connection) = &mut self.ready_sftp {
            while let Ok(ev) = connection.rx.try_recv() {
                match ev {
                    SftpEvent::Failed(e) => failed = Some(e),
                    SftpEvent::Closed => closed = true,
                    _ => {}
                }
            }
        }
        let err = failed.or(closed.then(|| "连接中断".to_string()));
        if err.is_none() && ready {
            self.ready_sftp = self.pending_sftp.take();
            self.mount_ready_sftp();
        }
        if let Some(e) = err {
            if let Some(connection) = self.pending_sftp.take() {
                connection.handle.close();
            }
            if let Some(connection) = self.ready_sftp.take() {
                connection.handle.close();
            }
            // 状态栏持久显示（toast 一闪而过容易忽略）。
            self.sftp_error = Some(format!("SFTP 连接失败：{e}"));
            self.show_toast(self.sftp_error.clone().unwrap(), true);
        }
    }

    /// 设置弹窗：Mino 控制台风格的统一外壳。
    ///
    /// 不使用 egui 默认标题栏，标题、关闭按钮与内容卡片共用同一个内边距
    /// 基线，避免出现“系统头部一套边距、弹窗内容另一套边距”的错位。
    fn settings_panel(&mut self, ctx: &egui::Context) {
        let theme = crate::theme::current_theme();
        let mut open = self.show_settings;
        let mut close_requested = false;
        egui::Window::new("settings_panel")
            .open(&mut open)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, -20.0])
            .default_size([600.0, 540.0])
            .min_size([520.0, 420.0])
            .max_size([720.0, 620.0])
            .resizable(true)
            .collapsible(false)
            .title_bar(false)
            .frame(dialog::shell_frame(theme))
            .show(ctx, |ui| {
                // ==================== 自绘头部：logo + 标题 / ESC + 关闭 ====================
                let (header_rect, _) = ui.allocate_exact_size(
                    egui::vec2(ui.available_width(), dialog::HEADER_H),
                    egui::Sense::hover(),
                );
                ui.painter().rect_filled(
                    header_rect,
                    egui::CornerRadius {
                        nw: 14,
                        ne: 14,
                        sw: 0,
                        se: 0,
                    },
                    theme.bg_header,
                );
                ui.painter().line_segment(
                    [header_rect.left_bottom(), header_rect.right_bottom()],
                    egui::Stroke::new(1.0, theme.border),
                );

                let mut header = ui.new_child(
                    egui::UiBuilder::new()
                        .max_rect(header_rect)
                        .layout(egui::Layout::left_to_right(egui::Align::Center)),
                );
                header.add_space(14.0);
                draw_logo_mark_static(&mut header, dialog::HEADER_LOGO);
                header.add_space(10.0);
                dialog::header_title(
                    &mut header,
                    "settings_header_title",
                    "设置",
                    "主机 · 项目 · 外观 · 关于",
                );
                header.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.add_space(12.0);
                    if dialog::close_icon_button(ui, "关闭设置（Esc）") {
                        close_requested = true;
                    }
                    ui.add_space(4.0);
                    // 同样固定 ESC 胶囊的尺寸，防止 Frame 在横向布局中纵向填满头部。
                    let esc_rect = ui
                        .allocate_exact_size(egui::vec2(34.0, 24.0), egui::Sense::hover())
                        .0;
                    ui.painter().rect_filled(esc_rect, 5.0, theme.bg_elevated);
                    ui.painter().rect_stroke(
                        esc_rect,
                        5.0,
                        egui::Stroke::new(1.0, theme.border),
                        egui::StrokeKind::Inside,
                    );
                    let mut esc = ui.new_child(
                        egui::UiBuilder::new()
                            .id_salt("settings_header_esc")
                            .max_rect(esc_rect)
                            .layout(egui::Layout::centered_and_justified(
                                egui::Direction::TopDown,
                            )),
                    );
                    esc.label(
                        egui::RichText::new("ESC")
                            .monospace()
                            .size(8.0)
                            .color(theme.text_muted),
                    );
                });

                ui.add_space(12.0);

                egui::Frame::new()
                    .inner_margin(egui::Margin::symmetric(14, 0))
                    .show(ui, |ui| {
                        egui::ScrollArea::vertical()
                            .id_salt("settings_scroll")
                            .auto_shrink([false, true])
                            .scroll_bar_visibility(
                                egui::scroll_area::ScrollBarVisibility::VisibleWhenNeeded,
                            )
                            .show(ui, |ui| {
                                ui.set_width(ui.available_width());
                                ui.spacing_mut().item_spacing.y = 0.0;
                                // ============ 主机管理 ============
                                let host_count = format!("{} 台", self.config.hosts.len());
                                Self::settings_card(ui, "主机管理", Some(&host_count), |ui| {
                                    self.host_sidebar(ui);
                                });

                                // ============ 项目管理 ============
                                let project_count = format!("{} 个", self.config.projects.len());
                                Self::settings_card(
                                    ui,
                                    "项目管理",
                                    Some(&project_count),
                                    |ui| {
                                        self.project_manager(ui);
                                    },
                                );

                                // ============ 外观 ============
                                Self::settings_card(ui, "外观", None, |ui| {
                                    ui.horizontal(|ui| {
                                        ui.label(
                                            egui::RichText::new("主题")
                                                .size(12.0)
                                                .color(theme.text_muted),
                                        );
                                        ui.add_space(8.0);
                                        let current = crate::theme::current_theme().name;
                                        egui::ComboBox::from_id_salt("settings_theme_switcher")
                                            .selected_text(
                                                egui::RichText::new(current)
                                                    .color(theme.text_primary),
                                            )
                                            .show_ui(ui, |ui| {
                                                for (i, t) in
                                                    crate::theme::THEMES.iter().enumerate()
                                                {
                                                    let selected = current == t.name;
                                                    if ui
                                                        .selectable_label(
                                                            selected,
                                                            egui::RichText::new(t.name).color(
                                                                if selected {
                                                                    theme.accent
                                                                } else {
                                                                    theme.text_primary
                                                                },
                                                            ),
                                                        )
                                                        .clicked()
                                                    {
                                                        let theme_ctx = ui.ctx().clone();
                                                        self.apply_theme_and_persist(&theme_ctx, i);
                                                    }
                                                }
                                            });
                                    });
                                    ui.add_space(4.0);
                                    ui.label(
                                        egui::RichText::new("⌥1 — ⌥3 快速切换主题")
                                            .monospace()
                                            .size(10.0)
                                            .color(theme.text_muted),
                                    );
                                });
                                ui.add_space(10.0);

                                // ============ 关于 ============
                                Self::settings_card(ui, "关于", None, |ui| {
                                    ui.horizontal(|ui| {
                                        ui.spacing_mut().item_spacing.x = 8.0;
                                        ui.label(
                                            egui::RichText::new(format!(
                                                "{PRODUCT_NAME} v{}",
                                                env!("CARGO_PKG_VERSION")
                                            ))
                                            .strong()
                                            .size(12.5)
                                            .color(theme.text_primary),
                                        );
                                        ui.label(
                                            egui::RichText::new("STABLE")
                                                .monospace()
                                                .size(9.0)
                                                .color(theme.accent),
                                        );
                                    });
                                    ui.add_space(10.0);
                                    let (label, dot, pulse) = match &self.update_state {
                                        UpdateState::Idle => ("检查更新", None, false),
                                        UpdateState::Checking => {
                                            ("检查中…", Some(theme.text_muted), false)
                                        }
                                        UpdateState::Available(_) => {
                                            ("新版本可用", Some(theme.accent2), true)
                                        }
                                        UpdateState::UpToDate => {
                                            ("已是最新", Some(theme.success), false)
                                        }
                                        UpdateState::Failed => {
                                            ("检查失败", Some(theme.danger), false)
                                        }
                                        UpdateState::Downloading(_) => {
                                            ("正在下载", Some(theme.accent), false)
                                        }
                                        UpdateState::Downloaded { .. } => {
                                            ("准备安装", Some(theme.accent), true)
                                        }
                                        UpdateState::Installing(_) => {
                                            ("安装中…", Some(theme.accent), false)
                                        }
                                        UpdateState::Installed => {
                                            ("已更新", Some(theme.success), false)
                                        }
                                        UpdateState::Error(_) => {
                                            ("更新出错", Some(theme.danger), true)
                                        }
                                    };
                                    ui.horizontal(|ui| {
                                        ui.spacing_mut().item_spacing.x = 8.0;
                                        if let Some(color) = dot {
                                            status_dot(ui, color, pulse);
                                        }
                                        if ui
                                            .add(
                                                egui::Button::new(
                                                    egui::RichText::new(label)
                                                        .size(12.0)
                                                        .color(theme.text_primary),
                                                )
                                                .fill(theme.bg_elevated)
                                                .stroke(egui::Stroke::new(1.0, theme.border))
                                                .corner_radius(crate::theme::tokens::RADIUS_ITEM)
                                                .min_size(egui::vec2(88.0, 28.0)),
                                            )
                                            .on_hover_text("检查更新")
                                            .clicked()
                                            && matches!(
                                                self.update_state,
                                                UpdateState::Idle
                                                    | UpdateState::UpToDate
                                                    | UpdateState::Failed
                                                    | UpdateState::Error(_)
                                            )
                                        {
                                            self.start_update_check(false, ctx);
                                        }
                                    });
                                    ui.add_space(10.0);
                                    dialog::hairline(ui);
                                    ui.add_space(10.0);
                                    // 性能 HUD 开关（调试用）。
                                    ui.horizontal(|ui| {
                                        ui.label(
                                            egui::RichText::new("性能 HUD")
                                                .size(12.0)
                                                .color(theme.text_secondary),
                                        );
                                        ui.with_layout(
                                            egui::Layout::right_to_left(egui::Align::Center),
                                            |ui| {
                                                let mut enabled = self.show_perf_hud;
                                                if ui
                                                    .checkbox(&mut enabled, "")
                                                    .on_hover_text("显示帧耗时 / FPS（⌥P 切换）")
                                                    .changed()
                                                {
                                                    self.show_perf_hud = enabled;
                                                }
                                            },
                                        );
                                    });
                                });
                            });
                    });
            });
        if close_requested {
            open = false;
        }
        // Esc 关闭 / × 关闭 / 闭包内主动关闭（如双击主机行连接成功）都生效：
        // open 由 egui 回写用户关闭动作，self.show_settings 记录闭包内的主动关闭。
        self.show_settings = open && self.show_settings;
    }

    /// 设置弹窗的统一内容卡片：标题 + 右侧计数 + 发丝线 + 内容。
    fn settings_card(
        ui: &mut egui::Ui,
        title: &str,
        count: Option<&str>,
        body: impl FnOnce(&mut egui::Ui),
    ) {
        let theme = crate::theme::current_theme();
        dialog::card_frame(theme).show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 8.0;
                ui.label(
                    egui::RichText::new(title)
                        .strong()
                        .size(13.0)
                        .color(theme.text_primary),
                );
                if let Some(count) = count {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(
                            egui::RichText::new(count)
                                .monospace()
                                .size(10.0)
                                .color(theme.text_muted),
                        );
                    });
                }
            });
            ui.add_space(8.0);
            dialog::hairline(ui);
            ui.add_space(10.0);
            body(ui);
        });
        ui.add_space(10.0);
    }

    /// 标签栏 ">_" 快捷按钮弹出的主机菜单：单击主机行直接发起连接
    /// （与设置弹窗主机行的双击不同——快捷入口单击即连，无需二次确认）。
    /// 无已保存主机时提示并可一键打开新建连接对话框。
    fn host_quick_menu(&mut self, ui: &mut egui::Ui) {
        let theme = crate::theme::current_theme();
        const MENU_W: f32 = 256.0;
        const ROW_H: f32 = 40.0;
        const AVATAR: f32 = 22.0;
        const ROW_PAD_X: f32 = 10.0;
        const TEXT_GAP: f32 = 8.0;
        ui.set_min_width(MENU_W);
        ui.set_max_width(MENU_W);
        ui.spacing_mut().item_spacing.y = 2.0;

        if self.config.hosts.is_empty() {
            ui.add_space(8.0);
            ui.vertical_centered(|ui| {
                ui.label(
                    egui::RichText::new("暂无已保存主机")
                        .size(12.0)
                        .color(theme.text_muted),
                );
                ui.add_space(6.0);
                if ui
                    .button(
                        egui::RichText::new("新建连接")
                            .size(12.0)
                            .color(theme.text_primary),
                    )
                    .clicked()
                {
                    self.open_new_connection();
                    ui.close();
                }
            });
            ui.add_space(8.0);
            return;
        }

        ui.add_space(2.0);
        let mut connect: Option<HostProfile> = None;
        for (i, host) in self.config.hosts.iter().enumerate() {
            let row_id = egui::Id::new(("quick_host", i));
            // 先占满菜单宽度，hover 高亮才能贴齐左右内边距（曾按内容包围盒
            // expand，短名称行高亮左右留白、像一块浮岛）。
            let (row_rect, _) = ui.allocate_exact_size(
                egui::vec2(ui.available_width(), ROW_H),
                egui::Sense::hover(),
            );
            let highlight = row_rect.shrink2(egui::vec2(4.0, 1.0));
            let hovered = ui.rect_contains_pointer(highlight);
            if hovered {
                ui.painter().rect_filled(
                    highlight,
                    crate::theme::tokens::RADIUS_ITEM,
                    egui::Color32::from_rgba_unmultiplied(255, 255, 255, 16),
                );
            }

            // 行内容：头像垂直居中 + 名称/地址左对齐截断。
            // 不用 add_sized——其内部 Layout::centered_and_justified 会把短
            // 名称水平居中，和长地址错位（回归：ssh快捷菜单行左对齐）。
            let mut inner = ui.new_child(
                egui::UiBuilder::new()
                    .id_salt(row_id)
                    .max_rect(row_rect)
                    .layout(egui::Layout::left_to_right(egui::Align::Center)),
            );
            inner.spacing_mut().item_spacing = egui::vec2(0.0, 0.0);
            inner.add_space(ROW_PAD_X);
            let (avatar_rect, _) =
                inner.allocate_exact_size(egui::vec2(AVATAR, AVATAR), egui::Sense::hover());
            let initial = host.name.chars().next().unwrap_or('?');
            dialog::paint_avatar(inner.painter(), avatar_rect, initial, theme, false);
            inner.add_space(TEXT_GAP);
            let text_rect = inner
                .allocate_exact_size(
                    egui::vec2(
                        (row_rect.width() - ROW_PAD_X * 2.0 - AVATAR - TEXT_GAP).max(80.0),
                        28.0,
                    ),
                    egui::Sense::hover(),
                )
                .0;
            let mut text = inner.new_child(
                egui::UiBuilder::new()
                    .id_salt(row_id.with("text"))
                    .max_rect(text_rect)
                    .layout(egui::Layout::top_down(egui::Align::Min)),
            );
            text.spacing_mut().item_spacing.y = 1.0;
            text.add_space(1.0);
            text.add(
                egui::Label::new(
                    egui::RichText::new(&host.name)
                        .size(12.5)
                        .color(theme.text_primary),
                )
                .truncate(),
            );
            text.add(
                egui::Label::new(
                    egui::RichText::new(format!("{}@{}", host.user, host.host))
                        .size(10.5)
                        .color(theme.text_secondary),
                )
                .truncate(),
            );

            // 整行点击区（显式 interact + 稳定 Id，注册在内容之后）。
            let resp = ui
                .interact(row_rect, row_id.with("click"), egui::Sense::click())
                .on_hover_cursor(egui::CursorIcon::PointingHand);
            if resp.clicked() {
                connect = Some(host.clone());
                ui.close();
            }
        }
        ui.add_space(2.0);
        if let Some(profile) = connect {
            self.start_connect(ui.ctx(), profile);
        }
    }

    /// 按搜索词过滤项目（名称/路径子串，大小写不敏感），返回原下标。
    ///
    /// 快捷菜单与 ⌘O 面板共用，保证两处过滤语义一致。
    fn filtered_project_indices(&self) -> Vec<usize> {
        let query = self.project_filter.trim().to_lowercase();
        if query.is_empty() {
            return (0..self.config.projects.len()).collect();
        }
        self.config
            .projects
            .iter()
            .enumerate()
            .filter(|(_, p)| {
                p.name.to_lowercase().contains(&query)
                    || p.path.to_string_lossy().to_lowercase().contains(&query)
            })
            .map(|(i, _)| i)
            .collect()
    }

    /// 标签栏项目按钮弹出的收藏菜单：首行搜索 + 项目列表，单击即打开为新终端标签。
    ///
    /// 行结构复制 `host_quick_menu`（MENU_W 256、ROW_H 40、先 allocate 满宽再绘内容、
    /// 显式 `interact` + 稳定 Id、内容之后注册点击）。
    fn project_quick_menu(&mut self, ui: &mut egui::Ui) {
        let theme = crate::theme::current_theme();
        const MENU_W: f32 = 256.0;
        const ROW_H: f32 = 40.0;
        const AVATAR: f32 = 22.0;
        const ROW_PAD_X: f32 = 10.0;
        const TEXT_GAP: f32 = 8.0;
        ui.set_min_width(MENU_W);
        ui.set_max_width(MENU_W);
        ui.spacing_mut().item_spacing.y = 2.0;

        if self.config.projects.is_empty() {
            ui.add_space(8.0);
            ui.vertical_centered(|ui| {
                ui.label(
                    egui::RichText::new("暂无收藏项目")
                        .size(12.0)
                        .color(theme.text_muted),
                );
                ui.add_space(6.0);
                if ui
                    .button(
                        egui::RichText::new("收藏当前目录（⌘D）")
                            .size(12.0)
                            .color(theme.text_primary),
                    )
                    .clicked()
                {
                    self.bookmark_current_directory();
                    ui.close();
                }
                if ui
                    .button(
                        egui::RichText::new("管理项目…")
                            .size(12.0)
                            .color(theme.text_primary),
                    )
                    .clicked()
                {
                    self.show_settings = true;
                    ui.close();
                }
            });
            ui.add_space(8.0);
            return;
        }

        ui.add_space(2.0);
        let search_id = egui::Id::new("project_quick_search");
        let search_resp = dialog::form_input(
            ui,
            search_id,
            &mut self.project_filter,
            "搜索项目",
            ui.available_width(),
            false,
            false,
        );
        if search_resp.changed() {
            self.project_selected = 0;
        }
        ui.memory_mut(|m| m.request_focus(search_id));

        let matched = self.filtered_project_indices();
        if matched.is_empty() {
            ui.add_space(4.0);
            ui.vertical_centered(|ui| {
                ui.label(
                    egui::RichText::new("无匹配项目")
                        .size(12.0)
                        .color(theme.text_muted),
                );
            });
            ui.add_space(4.0);
            return;
        }

        let mut open: Option<ProjectProfile> = None;
        egui::ScrollArea::vertical()
            .id_salt("project_quick_scroll")
            .max_height(320.0)
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 2.0;
                for idx in matched {
                    let project = self.config.projects[idx].clone();
                    let row_id = egui::Id::new(("quick_project", idx));
                    let (row_rect, _) = ui.allocate_exact_size(
                        egui::vec2(ui.available_width(), ROW_H),
                        egui::Sense::hover(),
                    );
                    let highlight = row_rect.shrink2(egui::vec2(4.0, 1.0));
                    if ui.rect_contains_pointer(highlight) {
                        ui.painter().rect_filled(
                            highlight,
                            crate::theme::tokens::RADIUS_ITEM,
                            egui::Color32::from_rgba_unmultiplied(255, 255, 255, 16),
                        );
                    }
                    let mut inner = ui.new_child(
                        egui::UiBuilder::new()
                            .id_salt(row_id)
                            .max_rect(row_rect)
                            .layout(egui::Layout::left_to_right(egui::Align::Center)),
                    );
                    inner.spacing_mut().item_spacing = egui::vec2(0.0, 0.0);
                    inner.add_space(ROW_PAD_X);
                    let (avatar_rect, _) =
                        inner.allocate_exact_size(egui::vec2(AVATAR, AVATAR), egui::Sense::hover());
                    let initial = project.name.chars().next().unwrap_or('?');
                    dialog::paint_avatar(inner.painter(), avatar_rect, initial, theme, false);
                    inner.add_space(TEXT_GAP);
                    let text_rect = inner
                        .allocate_exact_size(
                            egui::vec2(
                                (row_rect.width() - ROW_PAD_X * 2.0 - AVATAR - TEXT_GAP).max(80.0),
                                28.0,
                            ),
                            egui::Sense::hover(),
                        )
                        .0;
                    let mut text = inner.new_child(
                        egui::UiBuilder::new()
                            .id_salt(row_id.with("text"))
                            .max_rect(text_rect)
                            .layout(egui::Layout::top_down(egui::Align::Min)),
                    );
                    text.spacing_mut().item_spacing.y = 1.0;
                    text.add_space(1.0);
                    text.add(
                        egui::Label::new(
                            egui::RichText::new(&project.name)
                                .size(12.5)
                                .color(theme.text_primary),
                        )
                        .truncate(),
                    );
                    let path_text = project.path.to_string_lossy().into_owned();
                    text.add(
                        egui::Label::new(
                            egui::RichText::new(&path_text)
                                .size(10.5)
                                .color(theme.text_secondary),
                        )
                        .truncate(),
                    );
                    let resp = ui
                        .interact(row_rect, row_id.with("click"), egui::Sense::click())
                        .on_hover_cursor(egui::CursorIcon::PointingHand);
                    if resp.clicked() {
                        open = Some(project);
                        ui.close();
                    }
                }
            });
        ui.add_space(2.0);
        if let Some(project) = open {
            self.open_project(ui.ctx(), &project);
        }
    }

    /// 切换项目打开面板（⌘O；新建连接模态时不响应，防弹窗叠加）。
    fn toggle_projects(&mut self) {
        if self.show_new_conn {
            return;
        }
        self.show_projects = !self.show_projects;
        if self.show_projects {
            self.project_filter.clear();
            self.project_selected = 0;
        }
    }

    /// ⌘O 项目打开面板：可搜索的项目列表，回车/单击即打开为新终端标签。
    ///
    /// 居中无标题栏弹窗（`dialog::shell_frame` 外壳，头部为设置弹窗的
    /// logo+标题+ESC 胶囊简化版）。行样式与快捷菜单一致（头像+名称+路径），
    /// 键盘选中的行 accent 软底。
    fn projects_panel(&mut self, ctx: &egui::Context) {
        let theme = crate::theme::current_theme();
        // ==================== 键盘导航（每帧先处理） ====================
        let matched = self.filtered_project_indices();
        if !matched.is_empty() {
            if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowDown)) {
                self.project_selected = (self.project_selected + 1) % matched.len();
            }
            if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowUp)) {
                self.project_selected = (self.project_selected + matched.len() - 1) % matched.len();
            }
        }
        let mut open_idx: Option<usize> = None;
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Enter))
            && !matched.is_empty()
        {
            open_idx = Some(matched[self.project_selected.min(matched.len() - 1)]);
        }
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape)) {
            self.show_projects = false;
        }
        if let Some(idx) = open_idx {
            let project = self.config.projects[idx].clone();
            self.show_projects = false;
            self.project_filter.clear();
            self.project_selected = 0;
            self.open_project(ctx, &project);
            return;
        }
        if !self.show_projects {
            return;
        }

        let mut close_requested = false;
        let mut open_clicked: Option<ProjectProfile> = None;
        egui::Window::new("projects_panel")
            .anchor(egui::Align2::CENTER_CENTER, [0.0, -20.0])
            .default_size([480.0, 320.0])
            .max_size([480.0, 420.0])
            .resizable(false)
            .collapsible(false)
            .title_bar(false)
            .frame(dialog::shell_frame(theme))
            .show(ctx, |ui| {
                // ==================== 自绘头部 ====================
                let (header_rect, _) = ui.allocate_exact_size(
                    egui::vec2(ui.available_width(), dialog::HEADER_H),
                    egui::Sense::hover(),
                );
                ui.painter().rect_filled(
                    header_rect,
                    egui::CornerRadius {
                        nw: 14,
                        ne: 14,
                        sw: 0,
                        se: 0,
                    },
                    theme.bg_header,
                );
                ui.painter().line_segment(
                    [header_rect.left_bottom(), header_rect.right_bottom()],
                    egui::Stroke::new(1.0, theme.border),
                );
                let mut header = ui.new_child(
                    egui::UiBuilder::new()
                        .max_rect(header_rect)
                        .layout(egui::Layout::left_to_right(egui::Align::Center)),
                );
                header.add_space(14.0);
                draw_logo_mark_static(&mut header, dialog::HEADER_LOGO);
                header.add_space(10.0);
                dialog::header_title(
                    &mut header,
                    "projects_header_title",
                    "打开项目",
                    "名称 · 路径",
                );
                header.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.add_space(12.0);
                    if dialog::close_icon_button(ui, "关闭（Esc）") {
                        close_requested = true;
                    }
                    ui.add_space(4.0);
                    let esc_rect = ui
                        .allocate_exact_size(egui::vec2(34.0, 24.0), egui::Sense::hover())
                        .0;
                    ui.painter().rect_filled(esc_rect, 5.0, theme.bg_elevated);
                    ui.painter().rect_stroke(
                        esc_rect,
                        5.0,
                        egui::Stroke::new(1.0, theme.border),
                        egui::StrokeKind::Inside,
                    );
                    let mut esc = ui.new_child(
                        egui::UiBuilder::new()
                            .id_salt("projects_header_esc")
                            .max_rect(esc_rect)
                            .layout(egui::Layout::centered_and_justified(
                                egui::Direction::TopDown,
                            )),
                    );
                    esc.label(
                        egui::RichText::new("ESC")
                            .monospace()
                            .size(8.0)
                            .color(theme.text_muted),
                    );
                });

                ui.add_space(10.0);
                egui::Frame::new()
                    .inner_margin(egui::Margin::symmetric(14, 0))
                    .show(ui, |ui| {
                        // 搜索框（与快捷菜单共用 filter，输入变化时选中归零）。
                        let search_id = egui::Id::new("projects_panel_search");
                        let search_resp = dialog::form_input(
                            ui,
                            search_id,
                            &mut self.project_filter,
                            "搜索项目",
                            ui.available_width(),
                            false,
                            false,
                        );
                        if search_resp.changed() {
                            self.project_selected = 0;
                        }
                        ui.memory_mut(|m| m.request_focus(search_id));
                        ui.add_space(8.0);

                        let matched = self.filtered_project_indices();
                        if self.config.projects.is_empty() {
                            ui.vertical_centered(|ui| {
                                ui.add_space(12.0);
                                ui.label(
                                    egui::RichText::new("暂无收藏项目")
                                        .size(12.0)
                                        .color(theme.text_muted),
                                );
                                ui.add_space(4.0);
                                ui.label(
                                    egui::RichText::new("⌘D 收藏当前终端目录")
                                        .monospace()
                                        .size(10.5)
                                        .color(theme.text_secondary),
                                );
                                ui.add_space(12.0);
                            });
                            return;
                        }
                        if matched.is_empty() {
                            ui.vertical_centered(|ui| {
                                ui.add_space(12.0);
                                ui.label(
                                    egui::RichText::new("无匹配项目")
                                        .size(12.0)
                                        .color(theme.text_muted),
                                );
                                ui.add_space(12.0);
                            });
                            return;
                        }
                        egui::ScrollArea::vertical()
                            .id_salt("projects_panel_scroll")
                            .max_height(280.0)
                            .show(ui, |ui| {
                                ui.spacing_mut().item_spacing.y = 2.0;
                                const ROW_H: f32 = 40.0;
                                const AVATAR: f32 = 22.0;
                                for (pos, idx) in matched.iter().enumerate() {
                                    let project = self.config.projects[*idx].clone();
                                    let row_id = egui::Id::new(("projects_panel_row", *idx));
                                    let (row_rect, _) = ui.allocate_exact_size(
                                        egui::vec2(ui.available_width(), ROW_H),
                                        egui::Sense::hover(),
                                    );
                                    let highlight = row_rect.shrink2(egui::vec2(4.0, 1.0));
                                    let selected = pos == self.project_selected;
                                    if selected {
                                        ui.painter().rect_filled(
                                            highlight,
                                            crate::theme::tokens::RADIUS_ITEM,
                                            theme.accent_soft,
                                        );
                                    } else if ui.rect_contains_pointer(highlight) {
                                        ui.painter().rect_filled(
                                            highlight,
                                            crate::theme::tokens::RADIUS_ITEM,
                                            egui::Color32::from_rgba_unmultiplied(
                                                255, 255, 255, 16,
                                            ),
                                        );
                                    }
                                    let mut inner = ui.new_child(
                                        egui::UiBuilder::new()
                                            .id_salt(row_id)
                                            .max_rect(row_rect)
                                            .layout(egui::Layout::left_to_right(
                                                egui::Align::Center,
                                            )),
                                    );
                                    inner.spacing_mut().item_spacing = egui::vec2(0.0, 0.0);
                                    inner.add_space(10.0);
                                    let (avatar_rect, _) = inner.allocate_exact_size(
                                        egui::vec2(AVATAR, AVATAR),
                                        egui::Sense::hover(),
                                    );
                                    let initial = project.name.chars().next().unwrap_or('?');
                                    dialog::paint_avatar(
                                        inner.painter(),
                                        avatar_rect,
                                        initial,
                                        theme,
                                        selected,
                                    );
                                    inner.add_space(8.0);
                                    let text_rect = inner
                                        .allocate_exact_size(
                                            egui::vec2(
                                                (row_rect.width() - 10.0 * 2.0 - AVATAR - 8.0)
                                                    .max(80.0),
                                                28.0,
                                            ),
                                            egui::Sense::hover(),
                                        )
                                        .0;
                                    let mut text = inner.new_child(
                                        egui::UiBuilder::new()
                                            .id_salt(row_id.with("text"))
                                            .max_rect(text_rect)
                                            .layout(egui::Layout::top_down(egui::Align::Min)),
                                    );
                                    text.spacing_mut().item_spacing.y = 1.0;
                                    text.add_space(1.0);
                                    text.add(
                                        egui::Label::new(
                                            egui::RichText::new(&project.name)
                                                .size(12.5)
                                                .color(theme.text_primary),
                                        )
                                        .truncate(),
                                    );
                                    let path_text = project.path.to_string_lossy().into_owned();
                                    text.add(
                                        egui::Label::new(
                                            egui::RichText::new(&path_text)
                                                .size(10.5)
                                                .color(theme.text_secondary),
                                        )
                                        .truncate(),
                                    );
                                    let resp = ui
                                        .interact(
                                            row_rect,
                                            row_id.with("click"),
                                            egui::Sense::click(),
                                        )
                                        .on_hover_cursor(egui::CursorIcon::PointingHand);
                                    if resp.clicked() {
                                        open_clicked = Some(project);
                                    }
                                }
                            });
                    });
                ui.add_space(10.0);
            });
        if close_requested {
            self.show_projects = false;
        }
        if let Some(project) = open_clicked {
            self.show_projects = false;
            self.project_filter.clear();
            self.project_selected = 0;
            self.open_project(ctx, &project);
        }
    }

    /// 渲染设置里的主机管理区。
    ///
    /// 仪器列表风：平时无线无底、行间发丝分隔，认证降级为次要文本；
    /// 选中行 accent 软底 + 左侧竖条。点击区域覆盖整行。
    fn host_sidebar(&mut self, ui: &mut egui::Ui) {
        let theme = crate::theme::current_theme();
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            dialog::section_title(ui, "已保存主机");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .add_sized(
                        egui::vec2(76.0, 26.0),
                        dialog::primary_button(theme, "新建连接"),
                    )
                    .clicked()
                {
                    self.open_new_connection();
                }
                ui.add_space(8.0);
                ui.label(
                    egui::RichText::new(format!("{} 台", self.config.hosts.len()))
                        .monospace()
                        .size(10.0)
                        .color(theme.text_muted),
                );
            });
        });
        ui.add_space(10.0);

        if self.config.hosts.is_empty() {
            let (empty_rect, _) = ui
                .allocate_exact_size(egui::vec2(ui.available_width(), 92.0), egui::Sense::hover());
            dialog::dashed_rounded_rect(
                ui,
                empty_rect,
                crate::theme::tokens::RADIUS_ITEM,
                theme.border,
            );
            let mut empty = ui.new_child(
                egui::UiBuilder::new()
                    .id_salt("empty_hosts")
                    .max_rect(empty_rect)
                    .layout(egui::Layout::top_down(egui::Align::Center)),
            );
            empty.add_space(22.0);
            empty.label(
                egui::RichText::new("暂无已保存主机")
                    .color(theme.text_secondary)
                    .size(12.5),
            );
            empty.add_space(2.0);
            empty.label(
                egui::RichText::new("⌘N 添加第一台主机")
                    .monospace()
                    .size(10.0)
                    .color(theme.text_muted),
            );
            return;
        }

        let mut remove_idx: Option<usize> = None;
        let mut connect_idx: Option<usize> = None;
        // 右侧操作列固定：认证文本 + 删除按钮永远在同一条竖线上，
        // 名称列只占用中间剩余空间，过长截断不挤乱布局。
        const ROW_H: f32 = 56.0;
        const AVATAR: f32 = 30.0;
        const AUTH_W: f32 = 72.0;
        const DELETE_SIZE: f32 = 24.0;
        const GAP: f32 = 10.0;

        let host_count = self.config.hosts.len();
        for (i, host) in self.config.hosts.iter().enumerate() {
            let row_id = egui::Id::new(("host_row", i));
            let (row_rect, _) = ui.allocate_exact_size(
                egui::vec2(ui.available_width(), ROW_H),
                egui::Sense::hover(),
            );
            // 行背景在内容之前绘制；点击区域延后到内容之后注册，避免内部
            // 名称/地址控件抢走整行点击，删除按钮随后再覆盖行点击区。
            let hover = ui.input(|input| {
                input
                    .pointer
                    .hover_pos()
                    .is_some_and(|pointer| row_rect.contains(pointer))
            });
            let selected = self.selected_host == Some(i);
            // 平时无线无底；选中 accent 软底，hover 白 6% 提亮。
            if selected {
                ui.painter().rect_filled(
                    row_rect,
                    crate::theme::tokens::RADIUS_ITEM,
                    theme.accent_soft,
                );
                ui.painter().rect_filled(
                    egui::Rect::from_min_max(
                        egui::pos2(row_rect.left() + 2.0, row_rect.top() + 10.0),
                        egui::pos2(row_rect.left() + 4.0, row_rect.bottom() - 10.0),
                    ),
                    1.0,
                    theme.accent,
                );
            } else if hover {
                ui.painter().rect_filled(
                    row_rect,
                    crate::theme::tokens::RADIUS_ITEM,
                    egui::Color32::from_rgba_unmultiplied(255, 255, 255, 14),
                );
            }
            // 行间发丝线（末行不画），左右与内容对齐。
            if i + 1 < host_count {
                let y = row_rect.bottom();
                ui.painter().line_segment(
                    [
                        egui::pos2(row_rect.left() + 12.0, y),
                        egui::pos2(row_rect.right() - 12.0, y),
                    ],
                    egui::Stroke::new(1.0, theme.border.gamma_multiply(0.5)),
                );
            }

            let content_rect = row_rect.shrink2(egui::vec2(12.0, 6.0));
            let right_w = AUTH_W + GAP + DELETE_SIZE;
            let identity_width = (content_rect.width() - AVATAR - GAP - right_w - GAP).max(72.0);
            let mut inner = ui.new_child(
                egui::UiBuilder::new()
                    .id_salt(row_id.with("content"))
                    .max_rect(content_rect)
                    .layout(egui::Layout::left_to_right(egui::Align::Center)),
            );
            inner.spacing_mut().item_spacing = egui::vec2(0.0, 0.0);
            let (avatar_rect, _) =
                inner.allocate_exact_size(egui::vec2(AVATAR, AVATAR), egui::Sense::hover());
            let initial = host.name.chars().next().unwrap_or('?');
            dialog::paint_avatar(inner.painter(), avatar_rect, initial, theme, selected);
            inner.add_space(GAP);

            let identity_rect = inner
                .allocate_exact_size(egui::vec2(identity_width, AVATAR), egui::Sense::hover())
                .0;
            let mut identity = inner.new_child(
                egui::UiBuilder::new()
                    .id_salt(row_id.with("identity"))
                    .max_rect(identity_rect)
                    .layout(egui::Layout::top_down(egui::Align::Min)),
            );
            identity.spacing_mut().item_spacing.y = 2.0;
            identity.add_space(1.0);
            identity.add(
                egui::Label::new(
                    egui::RichText::new(&host.name)
                        .strong()
                        .size(13.0)
                        .color(theme.text_primary),
                )
                .truncate(),
            );
            identity.add(
                egui::Label::new(
                    egui::RichText::new(format!("{}@{}:{}", host.user, host.host, host.port))
                        .monospace()
                        .size(10.5)
                        .color(theme.text_muted),
                )
                .truncate(),
            );

            // 认证方式降级为次要文本（无胶囊），右对齐固定列。
            let auth_label = if matches!(host.auth, Auth::Key { .. }) {
                "SSH KEY"
            } else {
                "PASSWORD"
            };
            let auth_rect = egui::Rect::from_min_size(
                egui::pos2(
                    content_rect.right() - DELETE_SIZE - GAP - AUTH_W,
                    row_rect.center().y - 8.0,
                ),
                egui::vec2(AUTH_W, 16.0),
            );
            let mut auth = ui.new_child(
                egui::UiBuilder::new()
                    .id_salt(row_id.with("auth"))
                    .max_rect(auth_rect)
                    .layout(egui::Layout::right_to_left(egui::Align::Center)),
            );
            auth.label(
                egui::RichText::new(auth_label)
                    .monospace()
                    .size(9.0)
                    .color(theme.text_muted),
            );

            let row_response = ui
                .interact(row_rect, row_id, egui::Sense::click())
                .on_hover_cursor(egui::CursorIcon::PointingHand);

            // 删除按钮最后注册，覆盖整行点击区，避免点击删除时先触发连接。
            let del_rect = egui::Rect::from_min_size(
                egui::pos2(
                    content_rect.right() - DELETE_SIZE,
                    row_rect.center().y - DELETE_SIZE * 0.5,
                ),
                egui::vec2(DELETE_SIZE, DELETE_SIZE),
            );
            let del_resp = ui
                .interact(del_rect, row_id.with("del"), egui::Sense::click())
                .on_hover_cursor(egui::CursorIcon::PointingHand);
            if del_resp.hovered() {
                ui.painter()
                    .rect_filled(del_rect, 6.0, theme.danger.gamma_multiply(0.18));
            }
            let icon_color = if del_resp.hovered() {
                theme.danger
            } else {
                theme.text_muted
            };
            let icon = del_rect.shrink(8.0);
            ui.painter().line_segment(
                [icon.left_top(), icon.right_bottom()],
                egui::Stroke::new(1.4, icon_color),
            );
            ui.painter().line_segment(
                [icon.right_top(), icon.left_bottom()],
                egui::Stroke::new(1.4, icon_color),
            );
            if del_resp.clicked() {
                remove_idx = Some(i);
            }
            if row_response.clicked() {
                self.selected_host = Some(i);
                let now = ui.input(|i| i.time);
                if let Some((t, idx)) = self.last_row_click {
                    if idx == i && now - t < 0.3 {
                        connect_idx = Some(i);
                    }
                }
                self.last_row_click = Some((now, i));
            }
        }
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.add_space(2.0);
            ui.label(
                egui::RichText::new("单击选中 · 双击连接")
                    .size(10.5)
                    .color(theme.text_muted),
            );
        });
        if let Some(i) = connect_idx {
            let profile = self.config.hosts[i].clone();
            // 从设置弹窗双击连接成功后关闭弹窗，直接进入终端。
            self.show_settings = false;
            self.last_row_click = None;
            self.start_connect(ui.ctx(), profile);
        }
        if let Some(i) = remove_idx {
            let removed = self.config.hosts.remove(i);
            self.selected_host = None;
            self.last_row_click = None;
            if !self.save_config() {
                self.config.hosts.insert(i, removed);
            }
        }
    }
    /// 渲染设置里的项目管理区（主机管理与外观之间）。
    ///
    /// 行布局复用 `host_sidebar` 模式（56px 行、头像+名称/路径两行、hover 提亮，
    /// 无选中竖条）；右侧 ↑ ↓ 改 删四个 24px 操作按钮（改/删为单字 + 悬浮说明）；
    /// 删除直接生效（与主机行 🗑 一致，不弹确认）；底部展开新增/编辑表单。
    fn project_manager(&mut self, ui: &mut egui::Ui) {
        let theme = crate::theme::current_theme();
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            dialog::section_title(ui, "已收藏项目");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .add_sized(
                        egui::vec2(76.0, 26.0),
                        dialog::primary_button(theme, "新增项目"),
                    )
                    .clicked()
                {
                    self.project_edit = Some(ProjectEdit {
                        index: None,
                        name: String::new(),
                        path: String::new(),
                        command: String::new(),
                        name_error: false,
                        path_error: false,
                    });
                }
            });
        });
        ui.add_space(10.0);

        if self.config.projects.is_empty() && self.project_edit.is_none() {
            let (empty_rect, _) = ui
                .allocate_exact_size(egui::vec2(ui.available_width(), 92.0), egui::Sense::hover());
            dialog::dashed_rounded_rect(
                ui,
                empty_rect,
                crate::theme::tokens::RADIUS_ITEM,
                theme.border,
            );
            let mut empty = ui.new_child(
                egui::UiBuilder::new()
                    .id_salt("empty_projects")
                    .max_rect(empty_rect)
                    .layout(egui::Layout::top_down(egui::Align::Center)),
            );
            empty.add_space(22.0);
            empty.label(
                egui::RichText::new("暂无收藏项目")
                    .color(theme.text_secondary)
                    .size(12.5),
            );
            empty.add_space(2.0);
            empty.label(
                egui::RichText::new("⌘D 收藏当前终端目录")
                    .monospace()
                    .size(10.0)
                    .color(theme.text_muted),
            );
            return;
        }

        let mut remove_idx: Option<usize> = None;
        let mut move_up: Option<usize> = None;
        let mut move_down: Option<usize> = None;
        let mut edit_idx: Option<usize> = None;
        const ROW_H: f32 = 56.0;
        const AVATAR: f32 = 30.0;
        const ACT_SIZE: f32 = 24.0;
        const ACT_GAP: f32 = 6.0;
        const GAP: f32 = 10.0;

        let project_count = self.config.projects.len();
        for (i, project) in self.config.projects.iter().enumerate() {
            let row_id = egui::Id::new(("project_row", i));
            let (row_rect, _) = ui.allocate_exact_size(
                egui::vec2(ui.available_width(), ROW_H),
                egui::Sense::hover(),
            );
            let hover = ui.input(|input| {
                input
                    .pointer
                    .hover_pos()
                    .is_some_and(|pointer| row_rect.contains(pointer))
            });
            if hover {
                ui.painter().rect_filled(
                    row_rect,
                    crate::theme::tokens::RADIUS_ITEM,
                    egui::Color32::from_rgba_unmultiplied(255, 255, 255, 14),
                );
            }
            if i + 1 < project_count {
                let y = row_rect.bottom();
                ui.painter().line_segment(
                    [
                        egui::pos2(row_rect.left() + 12.0, y),
                        egui::pos2(row_rect.right() - 12.0, y),
                    ],
                    egui::Stroke::new(1.0, theme.border.gamma_multiply(0.5)),
                );
            }

            let content_rect = row_rect.shrink2(egui::vec2(12.0, 6.0));
            let actions_w = ACT_SIZE * 4.0 + ACT_GAP * 3.0;
            let identity_width = (content_rect.width() - AVATAR - GAP - actions_w - GAP).max(72.0);
            let mut inner = ui.new_child(
                egui::UiBuilder::new()
                    .id_salt(row_id.with("content"))
                    .max_rect(content_rect)
                    .layout(egui::Layout::left_to_right(egui::Align::Center)),
            );
            inner.spacing_mut().item_spacing = egui::vec2(0.0, 0.0);
            let (avatar_rect, _) =
                inner.allocate_exact_size(egui::vec2(AVATAR, AVATAR), egui::Sense::hover());
            let initial = project.name.chars().next().unwrap_or('?');
            dialog::paint_avatar(inner.painter(), avatar_rect, initial, theme, false);
            inner.add_space(GAP);

            let identity_rect = inner
                .allocate_exact_size(egui::vec2(identity_width, AVATAR), egui::Sense::hover())
                .0;
            let mut identity = inner.new_child(
                egui::UiBuilder::new()
                    .id_salt(row_id.with("identity"))
                    .max_rect(identity_rect)
                    .layout(egui::Layout::top_down(egui::Align::Min)),
            );
            identity.spacing_mut().item_spacing.y = 2.0;
            identity.add_space(1.0);
            identity.add(
                egui::Label::new(
                    egui::RichText::new(&project.name)
                        .strong()
                        .size(13.0)
                        .color(theme.text_primary),
                )
                .truncate(),
            );
            let path_text = project.path.to_string_lossy().into_owned();
            identity.add(
                egui::Label::new(
                    egui::RichText::new(&path_text)
                        .monospace()
                        .size(10.5)
                        .color(theme.text_muted),
                )
                .truncate(),
            );

            // 右侧操作列：↑ 上移 / ↓ 下移 / 改 编辑 / 删 删除（24px，悬浮说明）。
            let acts = [("↑", "上移"), ("↓", "下移"), ("改", "编辑"), ("删", "删除")];
            for (k, (label, tip)) in acts.iter().enumerate() {
                let act_rect = egui::Rect::from_min_size(
                    egui::pos2(
                        content_rect.right() - actions_w + k as f32 * (ACT_SIZE + ACT_GAP),
                        row_rect.center().y - ACT_SIZE * 0.5,
                    ),
                    egui::vec2(ACT_SIZE, ACT_SIZE),
                );
                let act_resp = ui
                    .interact(
                        act_rect,
                        row_id.with(("project_act", k)),
                        egui::Sense::click(),
                    )
                    .on_hover_text(*tip)
                    .on_hover_cursor(egui::CursorIcon::PointingHand);
                let danger = k == 3;
                if act_resp.hovered() {
                    ui.painter().rect_filled(
                        act_rect,
                        6.0,
                        if danger {
                            theme.danger.gamma_multiply(0.18)
                        } else {
                            egui::Color32::from_rgba_unmultiplied(255, 255, 255, 18)
                        },
                    );
                }
                let icon_color = if act_resp.hovered() {
                    if danger {
                        theme.danger
                    } else {
                        theme.accent
                    }
                } else {
                    theme.text_muted
                };
                ui.painter().text(
                    act_rect.center(),
                    egui::Align2::CENTER_CENTER,
                    *label,
                    egui::FontId::proportional(12.0),
                    icon_color,
                );
                if act_resp.clicked() {
                    match k {
                        0 => move_up = Some(i),
                        1 => move_down = Some(i),
                        2 => edit_idx = Some(i),
                        _ => remove_idx = Some(i),
                    }
                }
            }
        }
        if let Some(i) = edit_idx {
            let (name, path, command) = {
                let p = &self.config.projects[i];
                (
                    p.name.clone(),
                    p.path.to_string_lossy().into_owned(),
                    p.command.clone(),
                )
            };
            self.project_edit = Some(ProjectEdit {
                index: Some(i),
                name,
                path,
                command,
                name_error: false,
                path_error: false,
            });
        }
        if let Some(i) = move_up {
            if i > 0 {
                self.config.projects.swap(i - 1, i);
                if !self.save_config() {
                    self.config.projects.swap(i - 1, i);
                }
            }
        }
        if let Some(i) = move_down {
            if i + 1 < self.config.projects.len() {
                self.config.projects.swap(i, i + 1);
                if !self.save_config() {
                    self.config.projects.swap(i, i + 1);
                }
            }
        }
        if let Some(i) = remove_idx {
            let removed = self.config.projects.remove(i);
            if !self.save_config() {
                self.config.projects.insert(i, removed);
            }
            // 正在编辑的行被删 → 关闭表单。
            if self
                .project_edit
                .as_ref()
                .is_some_and(|e| e.index == Some(i))
            {
                self.project_edit = None;
            }
        }

        // 新增/编辑表单（take 出来渲染，避免与 save/toast 的 &mut self 冲突）。
        let mut edit = self.project_edit.take();
        let mut close_form = false;
        if let Some(e) = edit.as_mut() {
            ui.add_space(8.0);
            dialog::hairline(ui);
            ui.add_space(10.0);
            dialog::section_title(
                ui,
                if e.index.is_some() {
                    "编辑项目"
                } else {
                    "新增项目"
                },
            );
            ui.add_space(6.0);
            let field_w = ui.available_width();
            dialog::field_label(ui, "名称");
            let name_resp = dialog::form_input(
                ui,
                egui::Id::new("project_edit_name"),
                &mut e.name,
                "如：mino",
                field_w,
                false,
                e.name_error,
            );
            if name_resp.changed() {
                e.name_error = false;
            }
            dialog::field_label(ui, "路径");
            let path_resp = dialog::form_input(
                ui,
                egui::Id::new("project_edit_path"),
                &mut e.path,
                "/Users/me/proj",
                field_w,
                false,
                e.path_error,
            );
            if path_resp.changed() {
                e.path_error = false;
            }
            dialog::field_label(ui, "启动命令（可选）");
            dialog::form_input(
                ui,
                egui::Id::new("project_edit_command"),
                &mut e.command,
                "打开后自动执行，如：npm run dev",
                field_w,
                false,
                false,
            );
            ui.add_space(6.0);
            let mut save = false;
            let mut cancel = false;
            let mut use_cwd = false;
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 8.0;
                if ui
                    .add_sized(
                        egui::vec2(76.0, 28.0),
                        dialog::primary_button(theme, "保存"),
                    )
                    .clicked()
                {
                    save = true;
                }
                if ui
                    .add_sized(
                        egui::vec2(76.0, 28.0),
                        dialog::secondary_button(theme, "取消"),
                    )
                    .clicked()
                {
                    cancel = true;
                }
                if ui
                    .add(dialog::secondary_button(theme, "使用当前终端目录"))
                    .clicked()
                {
                    use_cwd = true;
                }
            });
            if use_cwd {
                match self
                    .tabs
                    .get(self.active_tab)
                    .filter(|t| !t.terminal.session().is_remote())
                    .and_then(|t| t.terminal.effective_local_directory())
                {
                    Some(dir) => e.path = dir,
                    None => self.show_toast("当前无本地终端目录", true),
                }
            }
            if cancel {
                close_form = true;
            } else if save {
                e.name_error = e.name.trim().is_empty();
                e.path_error = !std::path::Path::new(e.path.trim()).is_dir();
                if e.name_error || e.path_error {
                    self.show_toast("请检查项目名称与目录", true);
                } else {
                    let profile = ProjectProfile {
                        name: e.name.trim().to_owned(),
                        path: PathBuf::from(e.path.trim()),
                        command: e.command.trim().to_owned(),
                    };
                    match e.index {
                        Some(idx) if idx < self.config.projects.len() => {
                            let old = std::mem::replace(&mut self.config.projects[idx], profile);
                            if !self.save_config() {
                                self.config.projects[idx] = old;
                            } else {
                                close_form = true;
                            }
                        }
                        _ => {
                            self.config.projects.push(profile);
                            if !self.save_config() {
                                self.config.projects.pop();
                            } else {
                                close_form = true;
                            }
                        }
                    }
                }
            }
        }
        if close_form {
            edit = None;
        }
        self.project_edit = edit;
    }

    /// 渲染新建连接对话框。
    fn connect_dialog(&mut self, ctx: &egui::Context) {
        let theme = crate::theme::current_theme();
        let field_width = 308.0;
        let mut port_error = false;
        let mut open = self.show_new_conn;
        let mut to_connect: Option<HostProfile> = None;
        let mut canceled = false;
        if !self.show_new_conn {
            return;
        }
        // 端口错误态：输入框描红（比 toast 更贴近问题位置）。
        let port_invalid =
            !self.form.port.trim().is_empty() && self.form.port.trim().parse::<u16>().is_err();
        egui::Window::new("connect_dialog")
            .open(&mut open)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, -20.0])
            .resizable(false)
            .collapsible(false)
            .title_bar(false)
            .frame(dialog::shell_frame(theme))
            .show(ctx, |ui| {
                // ==================== 自绘头部 ====================
                let (header_rect, _) = ui.allocate_exact_size(
                    egui::vec2(ui.available_width(), dialog::HEADER_H),
                    egui::Sense::hover(),
                );
                ui.painter().rect_filled(
                    header_rect,
                    egui::CornerRadius {
                        nw: 14,
                        ne: 14,
                        sw: 0,
                        se: 0,
                    },
                    theme.bg_header,
                );
                ui.painter().line_segment(
                    [header_rect.left_bottom(), header_rect.right_bottom()],
                    egui::Stroke::new(1.0, theme.border),
                );
                let mut header = ui.new_child(
                    egui::UiBuilder::new()
                        .max_rect(header_rect)
                        .layout(egui::Layout::left_to_right(egui::Align::Center)),
                );
                header.add_space(14.0);
                draw_logo_mark(&mut header, dialog::HEADER_LOGO);
                header.add_space(10.0);
                dialog::header_title(
                    &mut header,
                    "connect_header_title",
                    "新建连接",
                    "保存后可在主机列表中一键连接",
                );
                header.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.add_space(12.0);
                    if dialog::close_icon_button(ui, "关闭（Esc）") {
                        canceled = true;
                    }
                });

                egui::Frame::new()
                    .inner_margin(egui::Margin::symmetric(18, 14))
                    .show(ui, |ui| {
                        ui.set_min_width(field_width);
                        ui.spacing_mut().item_spacing.y = 0.0;
                        dialog::section_title(ui, "连接身份");
                        ui.add_space(8.0);
                        let name_id = egui::Id::new("conn_form_name");
                        if !self.form.name_focused {
                            ui.memory_mut(|m| m.request_focus(name_id));
                            self.form.name_focused = true;
                        }
                        dialog::field_label(ui, "名称");
                        ui.add_space(4.0);
                        dialog::form_input(
                            ui,
                            name_id,
                            &mut self.form.name,
                            "连接名称（可选）",
                            field_width,
                            false,
                            false,
                        );
                        ui.add_space(10.0);
                        dialog::field_label(ui, "用户名");
                        ui.add_space(4.0);
                        dialog::form_input(
                            ui,
                            egui::Id::new("conn_form_user"),
                            &mut self.form.user,
                            "用户名，例如 root",
                            field_width,
                            false,
                            false,
                        );
                        ui.add_space(14.0);
                        dialog::hairline(ui);
                        ui.add_space(14.0);
                        dialog::section_title(ui, "网络地址");
                        ui.add_space(8.0);
                        dialog::field_label(ui, "主机");
                        ui.add_space(4.0);
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = 8.0;
                            dialog::form_input(
                                ui,
                                egui::Id::new("conn_form_host"),
                                &mut self.form.host,
                                "主机名或 IP",
                                218.0,
                                false,
                                false,
                            );
                            dialog::form_input(
                                ui,
                                egui::Id::new("conn_form_port"),
                                &mut self.form.port,
                                "22",
                                82.0,
                                false,
                                port_error || port_invalid,
                            );
                        });
                        if port_error || port_invalid {
                            ui.add_space(3.0);
                            ui.label(
                                egui::RichText::new("端口必须是 1–65535 的数字")
                                    .size(11.0)
                                    .color(theme.danger),
                            );
                        }
                        ui.add_space(14.0);
                        dialog::hairline(ui);
                        ui.add_space(14.0);
                        dialog::section_title(ui, "认证方式");
                        ui.add_space(8.0);
                        dialog::auth_segmented(ui, &mut self.form.auth_kind);
                        ui.add_space(10.0);
                        if self.form.auth_kind == 0 {
                            dialog::field_label(ui, "密码");
                            ui.add_space(4.0);
                            dialog::form_input(
                                ui,
                                egui::Id::new("conn_form_password"),
                                &mut self.form.password,
                                "密码",
                                field_width,
                                true,
                                false,
                            );
                        } else {
                            dialog::field_label(ui, "私钥文件");
                            ui.add_space(4.0);
                            dialog::form_input(
                                ui,
                                egui::Id::new("conn_form_key"),
                                &mut self.form.key_path,
                                "私钥文件路径",
                                field_width,
                                false,
                                false,
                            );
                            ui.add_space(10.0);
                            dialog::field_label(ui, "私钥口令（可选）");
                            ui.add_space(4.0);
                            dialog::form_input(
                                ui,
                                egui::Id::new("conn_form_pass"),
                                &mut self.form.passphrase,
                                "私钥口令（可选）",
                                field_width,
                                true,
                                false,
                            );
                        }
                        ui.add_space(16.0);
                        dialog::hairline(ui);
                        ui.add_space(12.0);
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.spacing_mut().item_spacing.x = 8.0;
                            if ui.add(dialog::primary_button(theme, "连接")).clicked() {
                                let port: u16 = match self.form.port.trim().parse() {
                                    Ok(port @ 1..=65535) => port,
                                    _ => {
                                        port_error = true;
                                        22
                                    }
                                };
                                if port_error {
                                    self.show_toast("端口必须是 1-65535 的数字", true);
                                    return;
                                }
                                if self.form.auth_kind == 1 && self.form.key_path.trim().is_empty()
                                {
                                    self.show_toast("请填写私钥文件路径", true);
                                    return;
                                }
                                let host = self.form.host.trim().to_string();
                                let user = self.form.user.trim().to_string();
                                let auth = if self.form.auth_kind == 0 {
                                    Auth::Password(self.form.password.clone())
                                } else {
                                    Auth::Key {
                                        path: PathBuf::from(self.form.key_path.trim()),
                                        passphrase: if self.form.passphrase.is_empty() {
                                            None
                                        } else {
                                            Some(self.form.passphrase.clone())
                                        },
                                    }
                                };
                                let profile = HostProfile {
                                    name: if self.form.name.trim().is_empty() {
                                        host.clone()
                                    } else {
                                        self.form.name.trim().to_string()
                                    },
                                    host,
                                    port,
                                    user,
                                    auth,
                                };
                                if !profile.host.is_empty() && !profile.user.is_empty() {
                                    to_connect = Some(profile);
                                } else {
                                    self.show_toast("请填写主机与用户名", true);
                                }
                            }
                            if ui.add(dialog::secondary_button(theme, "取消")).clicked() {
                                canceled = true;
                            }
                        });
                    });
            });
        if let Some(profile) = to_connect {
            self.config.hosts.push(profile.clone());
            let saved = self.save_config();
            if !saved {
                self.config.hosts.pop();
            }
            self.show_new_conn = false;
            self.show_settings = false;
            self.settings_before_new_conn = false;
            self.form.name_focused = false;
            self.start_connect(ctx, profile);
        } else if canceled || !open {
            self.show_new_conn = false;
            if self.settings_before_new_conn {
                self.show_settings = true;
            }
            self.settings_before_new_conn = false;
            self.form.name_focused = false;
        } else {
            self.show_new_conn = true;
        }
    }

    /// 渲染标签页栏（Warp 风格圆角标签 + 底部指示条）。
    fn tab_bar(&mut self, ui: &mut egui::Ui) {
        let theme = crate::theme::current_theme();
        // 标签栏即窗口拖拽区：整行注册底层 click_and_drag 背景（先注册，
        // 被后注册的控件覆盖）。
        // egui 命中规则：后注册 widget 在顶层，控件上点击/拖拽优先命中控件，
        // 标签栏空白处的操作则命中此背景：
        // - 拖动 → 发 StartDrag 让系统接管窗口移动；
        // - 双击 → 切换窗口 zoom（最大化/恢复，macOS 标题栏双击标准行为）。
        // Sense 必须带 CLICK（click_and_drag）：纯 Sense::drag 在按下瞬间就
        // 被判为 drag_started，必须等指针移动；双击是两次"按下即抬起"的点击，
        // 指针几乎不动，纯 drag 下第二次按下仍是 drag 状态，double_clicked
        // 永不为真（官方 custom_window_frame 示例同样用 click_and_drag）。
        let drag_rect = ui.max_rect();
        let drag_resp = ui.interact(
            drag_rect,
            egui::Id::new("tab_bar_drag"),
            egui::Sense::click_and_drag(),
        );
        if drag_resp.drag_started_by(egui::PointerButton::Primary) {
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::StartDrag);
        }
        if drag_resp.double_clicked() {
            let maximized = ui.ctx().input(|i| i.viewport().maximized.unwrap_or(false));
            ui.ctx()
                .send_viewport_cmd(egui::ViewportCommand::Maximized(!maximized));
        }
        ui.horizontal(|ui| {
            // macOS 原生红绿灯位于内容视图之上，不参与 egui 布局；
            // 预留同等宽度，避免第一个标签被系统按钮遮住。
            #[cfg(target_os = "macos")]
            ui.add_space(80.0);
            let mut switch_to: Option<usize> = None;
            let mut close_idx: Option<usize> = None;
            for (i, tab) in self.tabs.iter().enumerate() {
                // 本地标签显示当前目录的末级名，全路径作悬浮提示。
                let (title, title_tooltip) = tab.title();
                let selected = i == self.active_tab;
                // 数字索引前缀：与终端主流 Tab 习惯一致（1, 2, ...）。
                let prefix = format!("{} ", i + 1);
                // 选中底必须画在内容之前（Frame 先铺底再放内容）：曾把不透明
                // 面板色画在内容之后，激活 tab 的文字被整块盖住、完全不可见。
                let sel_alpha = anim::smooth_bool(
                    ui.ctx(),
                    egui::Id::new(("tab_sel", i)),
                    selected,
                    anim::SPEED_FAST,
                );
                let active_fill = egui::Color32::from_rgba_unmultiplied(
                    theme.accent.r(),
                    theme.accent.g(),
                    theme.accent.b(),
                    (26.0 * sel_alpha) as u8,
                );
                let active_stroke = egui::Stroke::new(
                    1.0,
                    egui::Color32::from_rgba_unmultiplied(
                        theme.accent.r(),
                        theme.accent.g(),
                        theme.accent.b(),
                        (96.0 * sel_alpha) as u8,
                    ),
                );
                let row = ui
                    .scope_builder(egui::UiBuilder::new().id_salt(("tab", i)), |ui| {
                        egui::Frame::new()
                            .fill(active_fill)
                            .stroke(active_stroke)
                            .corner_radius(crate::theme::tokens::RADIUS_ITEM)
                            .inner_margin(egui::Margin::symmetric(2, 3))
                            .show(ui, |ui| {
                                ui.horizontal(|ui| {
                                    ui.add_space(4.0);
                                    // 数字索引（muted 色，提示序号）。
                                    ui.label(
                                        egui::RichText::new(&prefix)
                                            .size(11.5)
                                            .color(if selected {
                                                theme.accent2
                                            } else {
                                                theme.text_muted.gamma_multiply(0.7)
                                            })
                                            .monospace(),
                                    );
                                    // 无边框透明按钮（selectable_label 选中自带边框，
                                    // 与手绘高亮叠加会形成"双重框"）。
                                    let title_resp = ui.add(
                                        egui::Button::new(
                                            egui::RichText::new(title).size(12.5).color(
                                                if selected {
                                                    theme.accent
                                                } else {
                                                    theme.text_muted
                                                },
                                            ),
                                        )
                                        .fill(egui::Color32::TRANSPARENT)
                                        .stroke(egui::Stroke::NONE)
                                        .corner_radius(crate::theme::tokens::RADIUS_ITEM),
                                    );
                                    // 悬浮展示终端当前目录的全路径。
                                    let title_resp = match &title_tooltip {
                                        Some(full) => title_resp.on_hover_text(full),
                                        None => title_resp,
                                    };
                                    if title_resp.clicked() {
                                        switch_to = Some(i);
                                    }
                                    ui.add_space(1.0);
                                    if ui
                                        .add(
                                            egui::Button::new(egui::RichText::new("×").color(
                                                if selected {
                                                    theme.accent.gamma_multiply(0.85)
                                                } else {
                                                    theme.text_muted
                                                },
                                            ))
                                            .fill(egui::Color32::TRANSPARENT)
                                            .stroke(egui::Stroke::NONE)
                                            .min_size(egui::vec2(18.0, 18.0))
                                            .corner_radius(4.0),
                                        )
                                        .on_hover_text("关闭标签页")
                                        .clicked()
                                    {
                                        close_idx = Some(i);
                                    }
                                    ui.add_space(2.0);
                                });
                            });
                    })
                    .response;

                // hover 底（动画过渡）：使用主题 accent 的低透明度叠加，保持终端
                // 控制台的色彩语气；激活 tab 的底色和描边已由上方 Frame 先铺好。
                let hover = row.hovered() && !selected;
                let hover_alpha = anim::smooth_bool(
                    ui.ctx(),
                    egui::Id::new(("tab_hover", i)),
                    hover,
                    anim::SPEED_FAST,
                );
                if hover_alpha > 0.01 {
                    ui.painter().rect_filled(
                        row.rect.expand2(egui::vec2(1.0, 2.0)),
                        crate::theme::tokens::RADIUS_ITEM,
                        egui::Color32::from_rgba_unmultiplied(
                            theme.accent.r(),
                            theme.accent.g(),
                            theme.accent.b(),
                            (18.0 * hover_alpha) as u8,
                        ),
                    );
                }
                // 底部指示条（宽度随选中状态动画）：用 accent 色和短距离辉光
                // 表示当前焦点，避免白色线条破坏终端配色。
                let bar_w = anim::smooth(
                    ui.ctx(),
                    egui::Id::new(("tab_bar_w", i)),
                    if selected {
                        row.rect.width() * 0.58
                    } else {
                        0.0
                    },
                    anim::SPEED_NORMAL,
                );
                if bar_w > 0.5 {
                    let bar = egui::Rect::from_center_size(
                        egui::pos2(row.rect.center().x, row.rect.bottom() - 1.0),
                        egui::vec2(bar_w, 2.0),
                    );
                    let glow = egui::Rect::from_center_size(
                        egui::pos2(bar.center().x, bar.center().y),
                        egui::vec2(bar_w + 8.0, 5.0),
                    );
                    ui.painter().rect_filled(
                        glow,
                        2.0,
                        egui::Color32::from_rgba_unmultiplied(
                            theme.accent.r(),
                            theme.accent.g(),
                            theme.accent.b(),
                            (42.0 * sel_alpha) as u8,
                        ),
                    );
                    ui.painter().rect_filled(
                        bar,
                        1.0,
                        egui::Color32::from_rgba_unmultiplied(
                            theme.accent.r(),
                            theme.accent.g(),
                            theme.accent.b(),
                            (220.0 * sel_alpha.max(0.25)) as u8,
                        ),
                    );
                }
            }
            // 新建本地终端标签（＋）。
            if ui
                .add(
                    egui::Button::new(egui::RichText::new("＋").color(theme.text_muted))
                        .fill(egui::Color32::TRANSPARENT)
                        .stroke(egui::Stroke::NONE)
                        .corner_radius(crate::theme::tokens::RADIUS_ITEM),
                )
                .on_hover_text("新建本地终端（⌘T）")
                .clicked()
            {
                self.new_local_tab(ui.ctx());
            }

            // 项目收藏（文件夹图标）：点击弹出收藏菜单，单击项目行即打开为新终端标签。
            // push_id 固定按钮 Id——Popup::menu 的开关状态按 Id 记忆，
            // 自动 Id 帧间漂移会让菜单闪断（与 ssh_quick 同理）。
            let project_btn_resp = ui.push_id("project_quick", project_quick_button).inner;
            egui::Popup::menu(&project_btn_resp).show(|ui| self.project_quick_menu(ui));

            // 快速 SSH 连接（">_" 图标）：点击弹出已保存主机列表，单击主机行
            // 直接发起连接（不需要进设置弹窗双击）。push_id 固定按钮 Id——
            // Popup::menu 的开关状态按 Id 记忆，自动 Id 帧间漂移会让菜单闪断。
            let ssh_btn_resp = ui.push_id("ssh_quick", ssh_quick_button).inner;
            egui::Popup::menu(&ssh_btn_resp).show(|ui| self.host_quick_menu(ui));

            // 顶到右侧：设置齿轮（设置弹窗入口）。
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if settings_gear_button(ui) && !self.show_new_conn {
                    self.show_settings = true;
                }
            });

            if let Some(i) = switch_to {
                self.active_tab = i;
            }
            if let Some(i) = close_idx {
                self.close_tab(i);
            }
        });
    }

    /// 切换设置弹窗开关状态。
    fn toggle_settings(&mut self) {
        // 新建连接是前台模态窗口，不能让快捷键把设置窗口叠到它上面。
        if self.show_new_conn {
            return;
        }
        self.show_settings = !self.show_settings;
    }

    /// 渲染状态栏。
    fn status_bar(&mut self, ui: &mut egui::Ui) {
        let theme = crate::theme::current_theme();
        ui.horizontal(|ui| {
            ui.add_space(6.0);
            if let Some(tab) = self.tabs.get(self.active_tab) {
                let session = tab.terminal.session();
                // 与标签栏同一标题（本地为当前目录末级名，远程为主机名）。
                let (title, title_tooltip) = tab.title();
                let exited = session.has_exited();
                status_dot(ui, if exited { theme.danger } else { theme.success }, false);
                let title_label = ui.label(
                    egui::RichText::new(title)
                        .size(11.5)
                        .color(theme.text_secondary),
                );
                if let Some(full) = title_tooltip {
                    title_label.on_hover_text(full);
                }
                if exited {
                    ui.colored_label(theme.danger, "会话已退出");
                }
                // SFTP 主机名从标签页的 SFTP 视图读取（每个标签绑定自己的连接，
                // 多远程标签共存时不会显示成最后一次连接的主机名）。
                if let Some(sftp) = &tab.sftp {
                    ui.separator();
                    status_dot(ui, theme.accent2, false);
                    ui.colored_label(theme.accent2, format!("SFTP · {}", sftp.host_name()));
                }
            }
            if self.pending_sftp.is_some() {
                ui.separator();
                loading_hint(ui, "SFTP 连接中…");
            }
            if self.pending_local.is_some() {
                // ⌘T 新建终端时当前标签仍在显示：状态栏给出明确等待反馈。
                ui.separator();
                loading_hint(ui, "终端启动中…");
            }
            if let Some(e) = &self.sftp_error {
                ui.separator();
                status_dot(ui, theme.danger, false);
                // 截断显示，避免长错误把右侧 HUD 挤出状态栏；hover 看全文。
                ui.add(
                    egui::Label::new(egui::RichText::new(e).size(11.5).color(theme.danger))
                        .truncate(),
                )
                .on_hover_text(format!("{e}\n\n重新连接主机可再次尝试"));
            }
            // 性能 HUD 与左侧会话状态同处一行：剩余空间右对齐。
            if self.show_perf_hud {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    render_perf_hud(ui, &self.perf);
                });
            }
        });
    }

    /// 更新弹窗（下载进度 / 安装 / 错误统一入口）。
    fn update_dialog(&mut self, ctx: &egui::Context) {
        let theme = crate::theme::current_theme();
        let (version, notes, url, downloaded, total, error, is_downloading, is_downloaded) =
            match &self.update_state {
                UpdateState::Available(info) => (
                    Some(info.version.clone()),
                    Some(info.notes.clone()),
                    Some(info.url.clone()),
                    None,
                    None,
                    None,
                    false,
                    false,
                ),
                UpdateState::Downloading(s) => (
                    Some(s.info.version.clone()),
                    Some(s.info.notes.clone()),
                    None,
                    Some(s.downloaded),
                    s.total,
                    None,
                    true,
                    false,
                ),
                UpdateState::Downloaded { info, .. } => (
                    Some(info.version.clone()),
                    Some(info.notes.clone()),
                    None,
                    None,
                    None,
                    None,
                    false,
                    true,
                ),
                UpdateState::Installing(info) => (
                    Some(info.version.clone()),
                    None,
                    None,
                    None,
                    None,
                    None,
                    false,
                    false,
                ),
                UpdateState::Installed => (None, None, None, None, None, None, false, false),
                UpdateState::Error(e) => {
                    (None, None, None, None, None, Some(e.clone()), false, false)
                }
                _ => return,
            };
        let dmg_path = match &self.update_state {
            UpdateState::Downloaded { dmg_path, .. } => Some(dmg_path.clone()),
            _ => None,
        };
        let available_info = match &self.update_state {
            UpdateState::Available(info) => Some(info.clone()),
            _ => None,
        };
        let is_installing = matches!(self.update_state, UpdateState::Installing(_));
        let is_installed = matches!(self.update_state, UpdateState::Installed);

        let mut action: Option<UpdateAction> = None;
        egui::Window::new("发现新版本")
            .anchor(egui::Align2::CENTER_CENTER, [0.0, -20.0])
            .default_width(460.0)
            .resizable(false)
            .collapsible(false)
            .title_bar(false)
            .frame(dialog::shell_frame(theme))
            .show(ctx, |ui| {
                // ==================== 自绘头部 ====================
                let (header_rect, _) = ui.allocate_exact_size(
                    egui::vec2(ui.available_width(), dialog::HEADER_H),
                    egui::Sense::hover(),
                );
                ui.painter().rect_filled(
                    header_rect,
                    egui::CornerRadius {
                        nw: 14,
                        ne: 14,
                        sw: 0,
                        se: 0,
                    },
                    theme.bg_header,
                );
                ui.painter().line_segment(
                    [header_rect.left_bottom(), header_rect.right_bottom()],
                    egui::Stroke::new(1.0, theme.border),
                );
                let mut header = ui.new_child(
                    egui::UiBuilder::new()
                        .max_rect(header_rect)
                        .layout(egui::Layout::left_to_right(egui::Align::Center)),
                );
                header.add_space(14.0);
                draw_logo_mark_static(&mut header, dialog::HEADER_LOGO);
                header.add_space(10.0);
                dialog::header_title(
                    &mut header,
                    "update_header_title",
                    &format!("更新 {PRODUCT_NAME}"),
                    "版本更新",
                );

                egui::Frame::new()
                    .inner_margin(egui::Margin::symmetric(18, 14))
                    .show(ui, |ui| {
                        ui.set_min_width(424.0);
                        ui.spacing_mut().item_spacing.y = 0.0;

                        if let Some(e) = &error {
                            dialog::inset_frame(theme).show(ui, |ui| {
                                ui.horizontal(|ui| {
                                    status_dot(ui, theme.danger, false);
                                    ui.add_space(4.0);
                                    ui.label(
                                        egui::RichText::new(e).size(12.5).color(theme.text_primary),
                                    );
                                });
                            });
                            ui.add_space(12.0);
                        } else if is_downloading {
                            // ==================== 下载进度 ====================
                            let fraction = match total {
                                Some(t) if t > 0 => downloaded.unwrap_or(0) as f32 / t as f32,
                                _ => f32::NAN,
                            };
                            let percent = if fraction.is_finite() {
                                format!("{:.0}%", fraction.clamp(0.0, 1.0) * 100.0)
                            } else {
                                String::new()
                            };
                            ui.horizontal(|ui| {
                                ui.label(
                                    egui::RichText::new("正在下载安装包")
                                        .strong()
                                        .size(12.5)
                                        .color(theme.text_primary),
                                );
                                ui.with_layout(
                                    egui::Layout::right_to_left(egui::Align::Center),
                                    |ui| {
                                        ui.label(
                                            egui::RichText::new(&percent)
                                                .monospace()
                                                .size(12.0)
                                                .color(theme.accent),
                                        );
                                    },
                                );
                            });
                            ui.add_space(8.0);
                            progress_bar(ui, fraction);
                            ui.add_space(8.0);
                            ui.label(
                                egui::RichText::new(match total {
                                    Some(t) => format!(
                                        "{} / {}",
                                        fmt_bytes(downloaded.unwrap_or(0)),
                                        fmt_bytes(t)
                                    ),
                                    None => fmt_bytes(downloaded.unwrap_or(0)),
                                })
                                .monospace()
                                .size(11.0)
                                .color(theme.text_muted),
                            );
                            ui.add_space(12.0);
                        } else if is_downloaded {
                            ui.horizontal(|ui| {
                                status_dot(ui, theme.success, false);
                                ui.add_space(4.0);
                                ui.label(
                                    egui::RichText::new("下载完成，重启后即可生效")
                                        .strong()
                                        .size(12.5)
                                        .color(theme.text_primary),
                                );
                            });
                            ui.add_space(12.0);
                        } else if is_installing {
                            ui.horizontal(|ui| {
                                loading_hint(ui, "正在挂载并安装，请稍候…");
                            });
                            ui.add_space(12.0);
                        } else if is_installed {
                            ui.horizontal(|ui| {
                                status_dot(ui, theme.success, false);
                                ui.add_space(4.0);
                                ui.label(
                                    egui::RichText::new("安装准备完成，应用即将退出并重启")
                                        .strong()
                                        .size(12.5)
                                        .color(theme.text_primary),
                                );
                            });
                            ui.add_space(12.0);
                        } else if let Some(notes) = &notes {
                            if !notes.is_empty() {
                                // 版本徽标行：新版本 + 当前版本并排，用户一眼看清跨度。
                                ui.horizontal(|ui| {
                                    ui.spacing_mut().item_spacing.x = 8.0;
                                    if let Some(v) = &version {
                                        let (pill, _) = ui.allocate_exact_size(
                                            egui::vec2(64.0, 22.0),
                                            egui::Sense::hover(),
                                        );
                                        ui.painter().rect_filled(pill, 11.0, theme.accent_soft);
                                        ui.painter().rect_stroke(
                                            pill,
                                            11.0,
                                            egui::Stroke::new(
                                                1.0,
                                                theme.accent.gamma_multiply(0.55),
                                            ),
                                            egui::StrokeKind::Inside,
                                        );
                                        let mut badge = ui.new_child(
                                            egui::UiBuilder::new().max_rect(pill).layout(
                                                egui::Layout::centered_and_justified(
                                                    egui::Direction::LeftToRight,
                                                ),
                                            ),
                                        );
                                        badge.label(
                                            egui::RichText::new(format!("v{v}"))
                                                .strong()
                                                .size(11.0)
                                                .color(theme.accent),
                                        );
                                        ui.add_space(2.0);
                                        ui.label(
                                            egui::RichText::new("→")
                                                .size(11.0)
                                                .color(theme.text_muted),
                                        );
                                        ui.add_space(2.0);
                                    }
                                    ui.label(
                                        egui::RichText::new(format!(
                                            "当前 v{}",
                                            env!("CARGO_PKG_VERSION")
                                        ))
                                        .size(11.0)
                                        .color(theme.text_muted),
                                    );
                                });
                                ui.add_space(10.0);
                                dialog::inset_frame(theme).show(ui, |ui| {
                                    egui::ScrollArea::vertical()
                                        .id_salt("update_notes")
                                        .max_height(150.0)
                                        .auto_shrink([false, true])
                                        .show(ui, |ui| {
                                            // release 说明逐行渲染：空行留段落间距，普通行紧凑排列。
                                            let mut first = true;
                                            let mut blank = false;
                                            for line in notes.lines() {
                                                let text = line.trim();
                                                if text.is_empty() {
                                                    blank = true;
                                                    continue;
                                                }
                                                if !first {
                                                    ui.add_space(if blank { 8.0 } else { 2.0 });
                                                }
                                                first = false;
                                                blank = false;
                                                ui.label(
                                                    egui::RichText::new(text)
                                                        .size(12.5)
                                                        .color(theme.text_secondary),
                                                );
                                            }
                                        });
                                });
                                ui.add_space(12.0);
                            }
                        }

                        dialog::hairline(ui);
                        ui.add_space(12.0);
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.spacing_mut().item_spacing.x = 8.0;
                            if is_downloaded {
                                if let Some(dmg) = &dmg_path {
                                    if ui
                                        .add(dialog::primary_button(theme, "安装并重启"))
                                        .clicked()
                                    {
                                        action = Some(UpdateAction::Install {
                                            dmg_path: dmg.clone(),
                                        });
                                    }
                                }
                                if ui.add(dialog::secondary_button(theme, "取消")).clicked() {
                                    action = Some(UpdateAction::Dismiss);
                                }
                            } else if is_downloading {
                                if ui.add(dialog::secondary_button(theme, "取消")).clicked() {
                                    action = Some(UpdateAction::CancelDownload);
                                }
                            } else if error.is_some() {
                                if ui.add(dialog::primary_button(theme, "重试")).clicked() {
                                    action = Some(UpdateAction::Retry);
                                }
                                if ui.add(dialog::secondary_button(theme, "关闭")).clicked() {
                                    action = Some(UpdateAction::Dismiss);
                                }
                            } else if let Some(info) = &available_info {
                                if ui
                                    .add(dialog::primary_button(theme, "下载并安装"))
                                    .clicked()
                                {
                                    action = Some(UpdateAction::StartDownload(info.clone()));
                                }
                                if ui.add(dialog::secondary_button(theme, "稍后")).clicked() {
                                    action = Some(UpdateAction::Dismiss);
                                }
                                if let Some(url) = &url {
                                    if ui.hyperlink_to("查看完整更新说明", url).clicked() {
                                        action = Some(UpdateAction::Dismiss);
                                    }
                                }
                            }
                        });
                    });
            });

        match action {
            Some(UpdateAction::Dismiss) => {
                // 下载完成后“稍后/取消”也必须删除私有临时目录中的 DMG；
                // 否则状态回到 Idle 后路径丢失，文件会一直残留。
                if let UpdateState::Downloaded { dmg_path, .. } = &self.update_state {
                    let _ = std::fs::remove_file(dmg_path);
                }
                self.update_state = UpdateState::Idle;
            }
            Some(UpdateAction::StartDownload(info)) => self.start_download(info, ctx),
            Some(UpdateAction::CancelDownload) => {
                self.cancel_download();
                self.update_state = UpdateState::Idle;
            }
            Some(UpdateAction::Install { dmg_path }) => self.install_update(ctx, dmg_path),
            Some(UpdateAction::Retry) => self.start_update_check(false, ctx),
            None => {}
        }
    }

    /// 渲染 Toast（右下角滑入，自动淡出）。
    fn render_toast(&mut self, ctx: &egui::Context) {
        let theme = crate::theme::current_theme();
        let Some(toast) = self.toast.as_mut() else {
            return;
        };
        let now = anim::now(ctx);
        if toast.start.is_nan() {
            toast.start = now;
        }
        let elapsed = now - toast.start;
        const DURATION: f64 = 4.0;
        if elapsed >= DURATION {
            self.toast = None;
            return;
        }
        // 滑入动画期间持续重绘（约 0.32s）；动画结束后不再 60fps 循环，
        // 仅安排到期关闭帧——Toast 停留期终端无输出时整帧静止省电。
        let in_t = (elapsed / 0.32).clamp(0.0, 1.0) as f32;
        if in_t < 1.0 {
            ctx.request_repaint_after(Duration::from_millis(16));
        } else {
            ctx.request_repaint_after(Duration::from_millis(
                ((DURATION - elapsed) * 1000.0).max(16.0) as u64,
            ));
        }
        let out_t = ((DURATION - elapsed) / 0.35).clamp(0.0, 1.0) as f32;
        let alpha = anim::ease_out_cubic(in_t) * anim::ease_out_cubic(out_t);
        let slide = (1.0 - anim::ease_out_back(in_t)) * 24.0;
        let accent = if toast.is_error {
            theme.danger
        } else {
            theme.success
        };

        let mut dismiss = false;
        egui::Area::new(egui::Id::new("toast"))
            .anchor(egui::Align2::RIGHT_BOTTOM, egui::vec2(-16.0, -16.0 + slide))
            .order(egui::Order::Foreground)
            .interactable(true)
            .show(ctx, |ui| {
                ui.set_opacity(alpha);
                let frame = egui::Frame::new()
                    .fill(theme.bg_elevated)
                    .corner_radius(9.0)
                    .inner_margin(egui::Margin::symmetric(13, 9))
                    .stroke(egui::Stroke::new(1.0, accent.gamma_multiply(0.45 * alpha)));
                let response = frame.show(ui, |ui| {
                    ui.horizontal(|ui| {
                        status_dot(ui, accent, false);
                        ui.label(
                            egui::RichText::new(&toast.message)
                                .size(12.5)
                                .color(theme.text_primary),
                        );
                    });
                });
                if ui
                    .interact(
                        response.response.rect,
                        egui::Id::new("toast_click"),
                        egui::Sense::click(),
                    )
                    .on_hover_cursor(egui::CursorIcon::PointingHand)
                    .clicked()
                {
                    dismiss = true;
                }
            });
        if dismiss {
            self.toast = None;
        }
    }

    /// 无标签页时的空状态。
    fn empty_state(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let theme = crate::theme::current_theme();
        // 顶部标签栏与内容区之间保留呼吸空间，避免最后一个 tab 关闭后
        // 空状态内容紧贴窗口上沿。
        ui.add_space(32.0);
        ui.centered_and_justified(|ui| {
            ui.vertical_centered(|ui| {
                draw_logo_mark(ui, 48.0);
                ui.add_space(12.0);
                ui.label(
                    egui::RichText::new(PRODUCT_NAME)
                        .strong()
                        .size(22.0)
                        .color(theme.text_primary),
                );
                ui.add_space(2.0);
                shortcut_hint(ui, theme);
                ui.add_space(14.0);
                let btn = egui::Button::new(
                    egui::RichText::new("新建本地终端")
                        .color(crate::theme::tokens::ACCENT_FG)
                        .size(13.0),
                )
                .fill(theme.accent)
                .stroke(egui::Stroke::NONE)
                .corner_radius(crate::theme::tokens::RADIUS_SM);
                if ui.add(btn).clicked() {
                    self.new_local_tab(ctx);
                }
            });
        });
    }
}

// ==================== 绘制辅助 ====================

/// 渲染空状态中的快捷键提示。
///
/// 快捷键单独使用等宽字体并放入固定高度的键帽，避免 `⌘` 因字体回退与
/// `T` 产生不同的字面高度；说明文字继续使用比例界面字体。
fn shortcut_hint(ui: &mut egui::Ui, theme: &crate::theme::Theme) {
    const ITEM_SPACING: f32 = 5.0;
    const KEY_HORIZONTAL_PADDING: f32 = 10.0;
    let measure = |text: &str, font: egui::FontId| {
        ui.painter()
            .layout_no_wrap(text.to_owned(), font, egui::Color32::TRANSPARENT)
            .size()
            .x
    };
    let key_width =
        |key: &str| measure(key, egui::FontId::monospace(10.5)) + KEY_HORIZONTAL_PADDING;
    let label_width = |label: &str| measure(label, egui::FontId::proportional(11.5));
    let row_width = key_width("⌘T")
        + label_width("新建本地终端")
        + label_width("·")
        + key_width("⌘N")
        + label_width("新建连接")
        + label_width("·")
        + key_width("⌘O")
        + label_width("打开项目")
        + ITEM_SPACING * 6.0;
    // `ui.horizontal` 会占满父布局宽度且默认从左侧排布；根据当前 UI 的
    // 真实中心坐标补前导空间，避免嵌套布局的 available_width 造成偏移。
    let leading_space = (ui.max_rect().center().x - ui.cursor().left() - row_width * 0.5).max(0.0);
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = ITEM_SPACING;
        ui.add_space(leading_space);
        shortcut_key(ui, theme, "⌘T");
        ui.label(
            egui::RichText::new("新建本地终端")
                .size(11.5)
                .color(theme.text_muted),
        );
        ui.label(egui::RichText::new("·").size(11.5).color(theme.text_muted));
        shortcut_key(ui, theme, "⌘N");
        ui.label(
            egui::RichText::new("新建连接")
                .size(11.5)
                .color(theme.text_muted),
        );
        ui.label(egui::RichText::new("·").size(11.5).color(theme.text_muted));
        shortcut_key(ui, theme, "⌘O");
        ui.label(
            egui::RichText::new("打开项目")
                .size(11.5)
                .color(theme.text_muted),
        );
    });
}

/// 终端风格的快捷键键帽。
fn shortcut_key(ui: &mut egui::Ui, theme: &crate::theme::Theme, key: &str) {
    egui::Frame::new()
        .fill(theme.bg_elevated)
        .stroke(egui::Stroke::new(1.0, theme.accent.gamma_multiply(0.45)))
        .corner_radius(4.0)
        .inner_margin(egui::Margin::symmetric(5, 2))
        .show(ui, |ui| {
            ui.label(
                egui::RichText::new(key)
                    .font(egui::FontId::monospace(10.5))
                    .color(theme.accent),
            );
        });
}

/// 隔离的测试配置路径（每个测试独享一份临时文件）。
///
/// 绝不触碰用户真实的 `~/.config/mino/hosts.toml`——曾发生测试直接
/// 覆盖并删除用户主机配置（运行一次测试丢一次主机列表）。
#[cfg(test)]
pub(crate) fn test_config_path(tag: &str) -> PathBuf {
    static SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let sequence = SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "mino-test-config-{tag}-{}-{sequence}.toml",
        std::process::id(),
    ))
}

/// 设置窗口标题区的静态品牌标记，不响应悬浮视觉效果。
fn draw_logo_mark_static(ui: &mut egui::Ui, size: f32) -> egui::Rect {
    draw_logo_mark_impl(ui, size, false)
}

/// 品牌标记：黑色哑光底 + 柔和白/磷光绿 `>_` 几何符号。
///
/// 与应用图标（scripts/make-icon.swift）同构图；默认在 hover 时带主题 accent 辉光。
fn draw_logo_mark(ui: &mut egui::Ui, size: f32) -> egui::Rect {
    draw_logo_mark_impl(ui, size, true)
}

fn draw_logo_mark_impl(ui: &mut egui::Ui, size: f32, show_hover_glow: bool) -> egui::Rect {
    let theme = crate::theme::current_theme();
    let (rect, response) = ui.allocate_exact_size(egui::vec2(size, size), egui::Sense::hover());
    if ui.is_rect_visible(rect) {
        let painter = ui.painter();
        let center = rect.center();
        let tile = egui::Rect::from_center_size(center, egui::vec2(size * 0.80, size * 0.80));
        let radius = tile.width() * 0.22;
        let black_top = egui::Color32::from_rgb(0x16, 0x18, 0x19);
        let black_bottom = egui::Color32::from_rgb(0x05, 0x06, 0x07);
        let primary = egui::Color32::from_rgb(0xe8, 0xef, 0xeb);
        let accent = egui::Color32::from_rgb(0xb8, 0xf3, 0x4c);

        // 黑色圆角底：保持与应用图标相同的石墨层次。
        anim::paint_rounded_gradient(painter, tile, radius, black_top, black_bottom);
        painter.rect_stroke(
            tile.shrink(0.5),
            radius,
            egui::Stroke::new(1.0, egui::Color32::from_white_alpha(46)),
            egui::StrokeKind::Inside,
        );

        // `>_`：用线条绘制，避免依赖字体字形，在小尺寸下也保持清晰。
        let stroke_width = (size * 0.075).max(1.5);
        let stroke = egui::Stroke::new(stroke_width, primary);
        let chevron_left = tile.left() + tile.width() * 0.29;
        let chevron_tip = tile.left() + tile.width() * 0.44;
        let chevron_half_height = tile.height() * 0.15;
        let glyph_y = tile.top() + tile.height() * 0.47;
        let chevron_points = [
            egui::pos2(chevron_left, glyph_y - chevron_half_height),
            egui::pos2(chevron_tip, glyph_y),
            egui::pos2(chevron_left, glyph_y + chevron_half_height),
        ];
        let cursor_points = [
            egui::pos2(
                tile.left() + tile.width() * 0.54,
                glyph_y - tile.height() * 0.13,
            ),
            egui::pos2(
                tile.left() + tile.width() * 0.75,
                glyph_y - tile.height() * 0.13,
            ),
        ];

        painter.line_segment([chevron_points[0], chevron_points[1]], stroke);
        painter.line_segment([chevron_points[1], chevron_points[2]], stroke);
        painter.line_segment(cursor_points, egui::Stroke::new(stroke_width, accent));

        if show_hover_glow && response.hovered() {
            anim::paint_glow(painter, center, size * 0.9, theme.accent2);
        }
    }
    rect
}

/// 状态圆点（可选呼吸光圈）。
fn status_dot(ui: &mut egui::Ui, color: egui::Color32, pulse: bool) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(9.0, 9.0), egui::Sense::hover());
    let center = rect.center();
    ui.painter().circle_filled(center, 3.2, color);
    if pulse {
        let p = anim::pulse(ui.ctx(), 1.6);
        ui.painter().circle_stroke(
            center,
            4.0 + p * 2.4,
            egui::Stroke::new(1.0, color.gamma_multiply(0.8 - p * 0.8)),
        );
    }
}

/// 静态加载指示（accent2 圆点 + 文字）。
///
/// 替代 egui `Spinner`：其内部每帧 `request_repaint` 强制 60fps 全帧重绘
/// （连接等待/安装期间终端无输出时整帧白烧 CPU），静态点零重绘。
fn loading_hint(ui: &mut egui::Ui, text: &str) {
    let theme = crate::theme::current_theme();
    ui.horizontal(|ui| {
        let (rect, _) = ui.allocate_exact_size(egui::vec2(9.0, 9.0), egui::Sense::hover());
        ui.painter()
            .circle_filled(rect.center(), 3.2, theme.accent2);
        ui.label(
            egui::RichText::new(text)
                .size(if ui.available_height() > 24.0 {
                    12.5
                } else {
                    11.5
                })
                .color(theme.text_secondary),
        );
    });
}

/// 自定义进度条：6px 细轨道 + accent 渐变填充（未知总量时显示流动光带）。
fn progress_bar(ui: &mut egui::Ui, fraction: f32) {
    let theme = crate::theme::current_theme();
    let (rect, _) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 6.0), egui::Sense::hover());
    ui.painter()
        .rect_filled(rect, 3.0, theme.bg_panel.gamma_multiply(0.85));
    ui.painter().rect_stroke(
        rect,
        3.0,
        egui::Stroke::new(1.0, theme.border.gamma_multiply(0.6)),
        egui::StrokeKind::Inside,
    );
    let fill_w = if fraction.is_finite() {
        rect.width() * fraction.clamp(0.0, 1.0)
    } else {
        rect.width() * 0.28
    };
    let fill = egui::Rect::from_min_size(rect.min, egui::vec2(fill_w.max(4.0), rect.height()));
    if fill_w > 0.0 {
        anim::paint_rounded_gradient(
            ui.painter(),
            fill,
            fill.height() * 0.5,
            theme.accent,
            theme.accent2,
        );
        // 填充上的扫光。
        let phase = anim::sweep(ui.ctx(), 1.7);
        let band_w = fill.width() * 0.4;
        let band_x = fill.left() + (fill.width() - band_w).max(0.0) * phase;
        anim::paint_h_gradient(
            ui.painter(),
            egui::Rect::from_min_size(
                egui::pos2(band_x, fill.top()),
                egui::vec2(band_w.min(fill.width()), fill.height()),
            ),
            egui::Color32::from_white_alpha(0),
            egui::Color32::from_white_alpha(48),
        );
    }
}

/// 字节数友好显示。
fn fmt_bytes(n: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    const GB: f64 = MB * 1024.0;
    let n = n as f64;
    if n >= GB {
        format!("{:.1} GB", n / GB)
    } else if n >= MB {
        format!("{:.1} MB", n / MB)
    } else if n >= KB {
        format!("{:.1} KB", n / KB)
    } else {
        format!("{n:.0} B")
    }
}

/// 写安装脚本并启动（独立进程，应用退出后继续运行）。
fn launch_installer(dmg: &Path, mount: &Path, result_path: &Path) -> Result<(), String> {
    // 私有 0700 目录（见 update_dir）：install.sh 与其他文件均不可被
    // 其他本地用户预建/替换。
    let dir = update_dir()?;
    let script = dir.join("install.sh");
    std::fs::write(&script, INSTALL_SCRIPT).map_err(|e| e.to_string())?;
    let log_path = dir.join(format!("install-{}.log", std::process::id()));
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)
        .map_err(|e| e.to_string())?;
    let log_err = log.try_clone().map_err(|e| e.to_string())?;
    Command::new("/bin/sh")
        .arg(&script)
        .arg(dmg)
        .arg(mount)
        .arg(result_path)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log_err))
        .spawn()
        .map_err(|e| e.to_string())?;
    Ok(())
}

impl Drop for MinoApp {
    fn drop(&mut self) {
        // 窗口关闭时停止未完成的下载并清理私有临时文件；安装阶段的 DMG
        // 不能在这里删除，因为独立安装脚本仍可能正在使用它。
        self.cancel_download();
        if let UpdateState::Downloaded { dmg_path, .. } = &self.update_state {
            let _ = std::fs::remove_file(dmg_path);
        }
        // eframe 关闭窗口后应用实例会先于进程退出；取消尚未完成的 SSH
        // 连接，避免后台 runtime 因等待 TCP 超时而让进程额外存活十几秒。
        if let Some(cancel) = self.pending_connect_cancel.take() {
            cancel.cancel();
        }
        if let Some(connection) = self.pending_sftp.take() {
            connection.handle.close();
        }
        if let Some(connection) = self.ready_sftp.take() {
            connection.handle.close();
        }
        for tab in &self.tabs {
            if let Some(sftp) = &tab.sftp {
                sftp.close();
            }
        }
    }
}

impl eframe::App for MinoApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        // 缓存 ctx 供非 UI 回调使用（如复制 tab 时构造 Session）。
        self.last_ctx = ctx.clone();
        self.perf.begin_frame();

        // ==================== 快捷键 ====================
        // ⌥P：切换性能 HUD。
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::ALT, egui::Key::P)) {
            self.show_perf_hud = !self.show_perf_hud;
        }
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::N))
            && !self.show_new_conn
        {
            self.open_new_connection();
        }
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::T)) {
            self.new_local_tab(&ctx);
        }
        // ⌘, 切换设置弹窗（macOS 标准"应用偏好设置"快捷键）。
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::Comma)) {
            self.toggle_settings();
        }
        // ⌘O：切换项目打开面板（新建连接模态时不响应，见 `toggle_projects`）。
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::O)) {
            self.toggle_projects();
        }
        // ⌘D：收藏当前本地终端目录为项目。
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::D)) {
            self.bookmark_current_directory();
        }
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::W))
            && !self.tabs.is_empty()
        {
            self.close_tab(self.active_tab);
        }
        for (key, theme_idx) in [
            (egui::Key::Num1, 0),
            (egui::Key::Num2, 1),
            (egui::Key::Num3, 2),
        ] {
            if ctx.input_mut(|i| i.consume_key(egui::Modifiers::ALT, key)) {
                self.apply_theme_and_persist(&ctx, theme_idx);
            }
        }
        for (key, idx) in [
            (egui::Key::Num1, 0usize),
            (egui::Key::Num2, 1),
            (egui::Key::Num3, 2),
            (egui::Key::Num4, 3),
            (egui::Key::Num5, 4),
            (egui::Key::Num6, 5),
            (egui::Key::Num7, 6),
            (egui::Key::Num8, 7),
            (egui::Key::Num9, 8),
        ] {
            if ctx.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, key))
                && idx < self.tabs.len()
            {
                self.active_tab = idx;
            }
        }

        // Esc 只关闭前台弹窗。无弹窗时必须保留事件给终端（例如 Vim 退出插入模式）。
        if (self.show_new_conn || self.show_settings || self.show_projects)
            && ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape))
        {
            if self.show_new_conn {
                self.show_new_conn = false;
                self.form.name_focused = false;
                if self.settings_before_new_conn {
                    self.show_settings = true;
                }
                self.settings_before_new_conn = false;
            } else if self.show_settings {
                self.show_settings = false;
            } else if self.show_projects {
                self.show_projects = false;
            }
        }

        // ==================== 处理异步结果 ====================
        // 所有标签都轮询后台状态，非活动标签不会积压终端写回或 SFTP 事件。
        if let Some(loader) = &mut self.cjk_fonts {
            // 中文 fallback 就绪即并入（只应用一次；未就绪时零成本）。
            // 并入会让 epaint 重建整套字体（`Context::add_font` → 下一帧
            // `fonts = None`），此前缓存的 Galley 全部指向旧图集——必须同步
            // 失效终端行缓存，否则启动期渲染的中文会永久停在乱码字形上。
            // 只负责唤醒：真正生效在下一帧 `begin_pass`（见
            // `TerminalView::font_fingerprint` 的看门狗），此处清缓存会
            // 用旧字体重建出一批乱码并永久缓存。
            if loader.poll(&ctx) {
                ctx.request_repaint();
            }
        }
        if !self.first_frame_reported {
            // 首帧耗时打点：覆盖"窗口出现→第一帧内容"的真实启动延迟。
            self.first_frame_reported = true;
            self.perf
                .set_startup_ms(self.created_at.elapsed().as_secs_f32() * 1000.0);
        }
        self.poll_local_spawn(&ctx);
        for tab in &mut self.tabs {
            tab.terminal.drain_background_events();
        }
        let mut sftp_event_received = false;
        for tab in &mut self.tabs {
            if let Some(sftp) = &mut tab.sftp {
                sftp_event_received |= sftp.poll_events();
            }
        }
        if sftp_event_received {
            ctx.request_repaint();
        }
        self.poll_connection(&ctx);
        self.poll_sftp();
        self.poll_locate_pending(&ctx);
        self.poll_image_paste(&ctx);
        self.poll_update();
        self.poll_download(&ctx);
        self.poll_install(&ctx);

        // ==================== 顶部标签页栏（合并了原 toolbar：齿轮入口在最右） ====================
        let theme = crate::theme::current_theme();
        // ==================== 标签页栏 ====================
        let tab_frame = egui::Frame::new()
            .fill(theme.bg_header)
            .inner_margin(egui::Margin {
                left: 8,
                right: 8,
                top: 2,
                bottom: 2,
            });
        egui::Panel::top("tabs").frame(tab_frame).show(ui, |ui| {
            self.tab_bar(ui);
        });

        // ==================== 状态栏 ====================
        let status_frame = egui::Frame::new()
            .fill(theme.bg_panel)
            .inner_margin(egui::Margin {
                left: 4,
                right: 10,
                top: 4,
                bottom: 4,
            });
        egui::Panel::bottom("status")
            .frame(status_frame)
            .show(ui, |ui| {
                self.status_bar(ui);
            });

        // ==================== 中央区：当前标签页 ====================
        // SFTP 面板必须是顶层面板（先于 CentralPanel 注册）：
        // egui 0.36 嵌套在 CentralPanel 内的 `Panel::right` 会把面板状态
        // 计入顶层布局，导致面板错位覆盖终端（表现为"SFTP 面板打不开"）。
        // tabby 形式：面板默认收起，只显示终端；终端右上角悬浮按钮切换
        // 展开/收起（`show_collapsible` 官方滑动动画，收起后右缘保留
        // 细拖拽把手，拖动也可重新打开）。
        // SFTP 面板宽度：默认约 40% 窗口宽、最宽 50%（曾固定 340px，
        // 大窗口下偏窄；用户要求默认 40%、上限 50%）。
        let viewport_w = ui.ctx().viewport_rect().width();
        let sftp_default_w = viewport_w * 0.40;
        // Panel 的 max_size 会把 min_size 一并压低；小窗口下 50% 可能
        // 小于 260，导致面板虽然“有最小宽度”却仍被布局压窄。
        let max_sftp_w = (viewport_w * 0.50).max(260.0);
        if let Some(tab) = self.tabs.get_mut(self.active_tab) {
            if let Some(sftp) = &mut tab.sftp {
                let terminal_cwd = tab.terminal.current_directory();
                let sftp_frame = egui::Frame::new()
                    .fill(theme.bg_panel)
                    .inner_margin(egui::Margin::symmetric(12, 10));
                let mut locate_requested = false;
                egui::Panel::right("sftp_panel")
                    .default_size(sftp_default_w)
                    // 最小宽度保证面板始终可见可操作（此前可被拖到极窄）。
                    .min_size(260.0)
                    // 最宽限制（约窗口 50%）：防止拖宽挤压终端
                    // （曾拖到 ~70% 窗口宽把终端压成一条窄带）。
                    .max_size(max_sftp_w)
                    .resizable(true)
                    .frame(sftp_frame)
                    .show_collapsible(ui, &mut tab.sftp_open, |ui| {
                        if sftp
                            .show_with_terminal_cwd(ui, terminal_cwd.as_deref())
                            .is_some()
                        {
                            locate_requested = true;
                        }
                    });
                if locate_requested {
                    Self::begin_locate_terminal(tab, ui.ctx());
                }
            }
        }
        // 设置弹窗是前台模态内容；终端仍渲染后台输出，
        // 但禁止它消费键盘、鼠标和滚轮事件，避免输入穿透。
        let terminal_input_enabled = !self.show_settings && !self.show_projects;
        egui::CentralPanel::default().show(ui, |ui| {
            if let Some(_pending) = &self.pending {
                ui.centered_and_justified(|ui| {
                    loading_hint(ui, &format!("正在连接 {} …", self.pending_label));
                });
            } else if self.tabs.is_empty() {
                if self.pending_local.is_some() {
                    // 本地会话在后台创建：显示占位而不是空状态，避免用户
                    // 看到"没有终端"的错觉（PTY fork + shell 初始化期间）。
                    ui.centered_and_justified(|ui| {
                        loading_hint(ui, "正在启动终端…");
                    });
                } else {
                    self.empty_state(ui, &ctx);
                }
            } else if let Some(tab) = self.tabs.get_mut(self.active_tab) {
                tab.terminal.show_with_input(ui, terminal_input_enabled);
                // tabby 风格：远程标签页终端右上角悬浮 SFTP 开关按钮。
                if tab.sftp.is_some() && sftp_floating_button(ui, tab.sftp_open) {
                    tab.sftp_open = !tab.sftp_open;
                }
            }
        });

        // ==================== 对话框与 Toast ====================
        if let Some(tab) = self.tabs.get_mut(self.active_tab) {
            if let Some(sftp) = &mut tab.sftp {
                sftp.show_dialog(&ctx);
            }
        }
        self.connect_dialog(&ctx);
        if self.show_settings {
            self.settings_panel(&ctx);
        }
        if self.show_projects {
            self.projects_panel(&ctx);
        }
        // 手动检查发现新版本时设置保持打开，更新弹窗后渲染
        // 以保证它位于设置窗口之上。
        self.update_dialog(&ctx);
        self.render_toast(&ctx);

        // 安装完成 → 关闭应用（脚本会拉起新版本）。
        if matches!(self.update_state, UpdateState::Installed) {
            if let Some(t) = self.restart_at {
                if anim::now(&ctx) >= t {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                } else {
                    ctx.request_repaint_after(Duration::from_millis(50));
                }
            }
        }

        // ==================== 性能 HUD ====================
        self.perf.end_frame();
        // 活动标签页的终端分段耗时喂给统计（无标签页时为 0 不影响）。
        if let Some(tab) = self.tabs.get(self.active_tab) {
            let (build, layout, paint) = tab.terminal.last_timing();
            self.perf.add_build(build);
            self.perf.add_layout(layout);
            self.perf.add_paint(paint);
            let (shapes, rebuilt, reused, upload) = tab.terminal.last_stats();
            self.perf
                .add_terminal_counts(shapes, rebuilt, reused, upload);
        }
    }
}

/// 渲染性能 HUD 文本（底部状态栏右侧）。
fn render_perf_hud(ui: &mut egui::Ui, perf: &crate::perf::PerfStats) {
    let theme = crate::theme::current_theme();
    ui.label(
        egui::RichText::new(perf.summary())
            .monospace()
            .size(11.0)
            .color(theme.text_secondary),
    )
    .on_hover_text("帧耗时 / FPS / 终端构建、布局与绘制耗时（⌥P 切换）");
}

#[cfg(test)]
mod tests {
    use super::*;
    use eframe::egui;
    use egui_kittest::Harness;

    /// 异步挂载：后台线程造好的会话经 `poll_local_spawn` 挂上标签栏。
    ///
    /// 生产构建的 `new_local_tab_at` 只登记 `pending_local`（后台线程创建
    /// 会话），挂载逻辑与测试的同步路径共用 `mount_local_session`；这里直接
    /// 驱动轮询，断言"结果到达→标签出现→状态栏占位消失"。
    #[test]
    fn 本地终端异步就绪后挂载标签() {
        use mino_core::terminal::{Session, SessionOptions};
        let config_path = test_config_path("pending-local");
        let mut harness = egui_kittest::Harness::builder()
            .with_step_dt(1.0 / 60.0)
            .build_eframe(|cc| MinoApp::new_with_config(cc, config_path.clone()));
        harness.run_steps(6);
        assert_eq!(harness.state().tabs.len(), 1, "测试构建应同步建好首个标签");
        // 模拟生产路径：把一个已建好的会话塞进 pending 队列。
        let session = Session::spawn_local(
            SessionOptions::default(),
            80,
            24,
            std::sync::Arc::new(|_ev: &mino_core::terminal::SessionEvent| {}),
        )
        .expect("创建本地终端失败");
        let (tx, rx) = std::sync::mpsc::channel();
        tx.send(Ok(session)).expect("发送会话失败");
        harness.state_mut().pending_local = Some(PendingLocalSpawn {
            rx,
            command: String::new(),
        });
        harness.run_steps(3);
        assert!(
            harness.state().pending_local.is_none(),
            "结果到达后 pending 应被消费"
        );
        assert_eq!(harness.state().tabs.len(), 2, "新会话应挂上第二个标签");
        assert!(
            harness.state().perf.summary().contains("终端"),
            "终端就绪打点应记录：{}",
            harness.state().perf.summary()
        );
        std::fs::remove_file(&config_path).ok();
    }

    use kittest::Queryable;

    /// 验证侧栏、工具栏与中央面板在 root Ui 上正常渲染。
    #[test]
    fn 面板渲染完整() {
        let mut harness = Harness::new_ui(|ui| {
            egui::Panel::left("hosts")
                .default_size(220.0)
                .resizable(true)
                .show(ui, |ui| {
                    ui.heading("主机");
                    let _ = ui.button("新建连接");
                    ui.weak("暂无已保存主机");
                });
            egui::Panel::top("toolbar").show(ui, |ui| {
                ui.label(egui::RichText::new(super::PRODUCT_NAME).strong());
                let _ = ui.button("本地终端");
            });
            egui::Panel::bottom("status").show(ui, |ui| {
                ui.label("状态栏");
            });
            egui::CentralPanel::default().show(ui, |ui| {
                ui.label("终端区域");
            });
        });
        harness.run_steps(6);

        harness.get_by_label("主机");
        harness.get_by_label("新建连接");
        harness.get_by_label("暂无已保存主机");
        harness.get_by_label(super::PRODUCT_NAME);
        harness.get_by_label("本地终端");
        harness.get_by_label("状态栏");
        harness.get_by_label("终端区域");
    }
}

#[cfg(unix)]
#[cfg(test)]
mod backup_tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn 配置损坏备份强制0600() {
        let src = test_config_path("backup-src");
        let dst = src.with_extension("toml.bak");
        let _ = std::fs::remove_file(&src);
        let _ = std::fs::remove_file(&dst);
        std::fs::write(&src, "[broken").unwrap();
        std::fs::set_permissions(&src, std::fs::Permissions::from_mode(0o644)).unwrap();

        backup_config(&src, &dst).unwrap();
        let mode = std::fs::metadata(&dst).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "配置备份必须以 0600 落盘");

        std::fs::remove_file(&src).ok();
        std::fs::remove_file(&dst).ok();
    }
}

#[cfg(test)]
mod dialog_tests {
    use super::*;

    /// 完整渲染"新建连接"对话框（含密码/私钥切换、自动聚焦），不应崩溃。
    #[test]
    fn 新建连接对话框完整渲染() {
        use kittest::Queryable;
        let mut form = ConnectForm {
            name: "测试".into(),
            host: "127.0.0.1".into(),
            port: "22".into(),
            user: "root".into(),
            auth_kind: 0,
            password: String::new(),
            key_path: String::new(),
            passphrase: String::new(),
            name_focused: false,
        };
        let mut show = true;
        let mut harness = egui_kittest::Harness::new_ui(|ui| {
            let mut open = show;
            egui::Window::new("新建连接")
                .open(&mut open)
                .resizable(false)
                .collapsible(false)
                .show(ui, |ui| {
                    egui::Grid::new("conn_form")
                        .num_columns(2)
                        .spacing([8.0, 6.0])
                        .show(ui, |ui| {
                            ui.label("名称");
                            let name_id = egui::Id::new("conn_form_name");
                            ui.add(egui::TextEdit::singleline(&mut form.name).id(name_id));
                            if !form.name_focused {
                                ui.memory_mut(|m| m.request_focus(name_id));
                                form.name_focused = true;
                            }
                            ui.end_row();
                            ui.label("主机");
                            ui.text_edit_singleline(&mut form.host);
                            ui.end_row();
                            ui.label("端口");
                            ui.text_edit_singleline(&mut form.port);
                            ui.end_row();
                            ui.label("用户名");
                            ui.text_edit_singleline(&mut form.user);
                            ui.end_row();
                            ui.label("认证方式");
                            ui.horizontal(|ui| {
                                ui.selectable_value(&mut form.auth_kind, 0, "密码");
                                ui.selectable_value(&mut form.auth_kind, 1, "私钥");
                            });
                            ui.end_row();
                            if form.auth_kind == 0 {
                                ui.label("密码");
                                ui.add(
                                    egui::TextEdit::singleline(&mut form.password).password(true),
                                );
                                ui.end_row();
                            } else {
                                ui.label("私钥路径");
                                ui.text_edit_singleline(&mut form.key_path);
                                ui.end_row();
                                ui.label("口令（可选）");
                                ui.add(
                                    egui::TextEdit::singleline(&mut form.passphrase).password(true),
                                );
                                ui.end_row();
                            }
                        });
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        let _ = ui.button("连接");
                        let _ = ui.button("取消");
                    });
                });
            show = open;
        });
        harness.run_steps(6);

        // 渲染完成，切换到私钥模式再渲染一帧。
        harness.get_by_label("私钥").click();
        harness.run_steps(6);
        harness.get_by_label("私钥路径");
    }
}

#[cfg(test)]
mod app_tests {
    use super::*;

    /// tabby 形式：远程标签页 SFTP 面板默认收起，仅显示终端；
    /// 终端右上角悬浮 SFTP 按钮点击切换面板开/关
    /// （回归测试：曾无条件显示右侧面板，宽度失控挤压终端成窄条）。
    #[test]
    fn sftp面板默认收起悬浮按钮切换() {
        use kittest::Queryable;

        let mut harness = egui_kittest::Harness::new_eframe(|cc| {
            let mut app = MinoApp::new(cc);
            // 构造带 SFTP 会话的标签页（mock 通道，无需测试 sshd）。
            let session = Session::spawn_local(
                SessionOptions::default(),
                80,
                24,
                Arc::new(|_ev: &SessionEvent| {}),
            )
            .expect("创建本地终端失败");
            let id = app.allocate_id();
            let mut tab = TerminalTab::new(id, "测试主机".into(), TerminalView::new(session));
            let (_tx, rx) = tokio::sync::mpsc::channel(128);
            let (handle_tx, _cmd_rx) = tokio::sync::mpsc::unbounded_channel();
            tab.sftp = Some(SftpView::new(
                "测试主机",
                SftpHandle::from_raw(handle_tx),
                rx,
            ));
            app.tabs.push(Box::new(tab));
            app.active_tab = app.tabs.len() - 1;
            app
        });
        harness.run_steps(6);

        // 默认：面板收起（面板内控件不可见），悬浮按钮可见。
        assert!(
            harness.root().query_all_by_label("..").next().is_none(),
            "SFTP 面板应默认收起"
        );
        harness.get_by_label("SFTP").click();
        harness.run_steps(6);
        // 点击悬浮按钮 → 面板展开。
        harness.get_by_label("..");
        // 再点 → 收起。
        harness.get_by_label("SFTP").click();
        harness.run_steps(6);
        assert!(
            harness.root().query_all_by_label("..").next().is_none(),
            "再次点击应收起面板"
        );
    }

    /// 回归：前面的标签被删除后，延迟到达的 SFTP 结果仍按稳定身份挂载，
    /// 不能按旧 Vec 下标落到另一标签。
    #[test]
    fn sftp按稳定标签身份挂载() {
        let mut harness = egui_kittest::Harness::new_eframe(|cc| {
            let mut app = MinoApp::new(cc);
            app.new_local_tab(&cc.egui_ctx);
            let target_id = app.tabs[1].id;
            app.tabs.remove(0);
            app.active_tab = 0;

            let (_event_tx, event_rx) = tokio::sync::mpsc::channel(128);
            let (command_tx, _command_rx) = tokio::sync::mpsc::unbounded_channel();
            app.pending_tab = Some(target_id);
            app.ready_sftp = Some(SftpConnection {
                connection_id: target_id,
                handle: SftpHandle::from_raw(command_tx),
                rx: event_rx,
                host: "稳定身份主机".into(),
                home: Some("/home/test".into()),
            });
            app.mount_ready_sftp();
            app
        });
        harness.run_steps(6);
        let tab = &harness.state().tabs[0];
        assert_eq!(tab.label, "本地终端");
        assert_eq!(
            tab.sftp.as_ref().map(SftpView::host_name),
            Some("稳定身份主机")
        );
    }

    /// 回归（用户报告“定位只有 pwd 后才好用”）：定位请求应先触发终端
    /// `pwd` 探测（`locate_pending`），等输出校正后再导航，而不是直接
    /// 用跟踪器里的旧推测值；不适合探测时回退到已知目录。
    #[test]
    fn 定位请求先探测再导航() {
        // 场景一：空闲提示符 → 触发探测，等待输出，不立即导航。
        let mut tab = {
            let session = Session::spawn_local(
                SessionOptions::default(),
                80,
                24,
                Arc::new(|_ev: &SessionEvent| {}),
            )
            .expect("创建本地终端失败");
            let mut tab = TerminalTab::new(1, "定位测试".into(), TerminalView::new(session));
            let (_tx, rx) = tokio::sync::mpsc::channel(128);
            let (handle_tx, _cmd_rx) = tokio::sync::mpsc::unbounded_channel();
            tab.sftp = Some(SftpView::new(
                "定位测试",
                SftpHandle::from_raw(handle_tx),
                rx,
            ));
            tab
        };
        let ctx = egui::Context::default();
        // 首帧让终端完成布局（request_fresh_pwd 依赖真实尺寸的全屏判断）。
        let mut harness = egui_kittest::Harness::new_ui(|ui| {
            tab.terminal.show(ui);
        });
        harness.run_steps(6);
        // 借用已结束（harness 只在闭包内借 tab）；后续直接操作 tab。
        drop(harness);
        MinoApp::begin_locate_terminal(&mut tab, &ctx);
        // 本地会话有内核 cwd 可读时直接导航、不注入 `pwd`（macOS 新实现）；
        // 读不到时才回退到 `pwd` 探测（Linux CI 等无 libproc 实现的平台）。
        if tab.terminal.session().child_current_dir().is_some() {
            assert!(
                tab.locate_pending.is_none(),
                "内核 cwd 可读时应直接导航，不进入 pwd 探测等待"
            );
        } else {
            assert!(
                tab.locate_pending.is_some(),
                "空闲提示符下定位应进入 pwd 探测等待"
            );
            assert!(!tab.terminal.auto_pwd_ready(), "探测注入后应等待终端输出");
        }

        // 场景二：有未执行输入 → 不注入，直接用已知目录回退。
        tab.locate_pending = None;
        tab.terminal.cancel_fresh_pwd();
        tab.terminal.push_workdir_text_for_test("echo hi");
        MinoApp::begin_locate_terminal(&mut tab, &ctx);
        assert!(
            tab.locate_pending.is_none(),
            "有未执行输入时不应进入探测等待"
        );
        assert!(tab.terminal.auto_pwd_ready());
    }

    #[test]
    fn ssh失败关闭已就绪的sftp连接() {
        let _harness = egui_kittest::Harness::new_eframe(|cc| {
            let mut app = MinoApp::new(cc);
            let (result_tx, result_rx) = tokio::sync::mpsc::unbounded_channel();
            result_tx
                .send(ConnectResult::Failed("SSH 失败".into()))
                .unwrap();
            app.pending = Some(result_rx);

            let (command_tx, mut command_rx) = tokio::sync::mpsc::unbounded_channel();
            app.ready_sftp = Some(SftpConnection {
                connection_id: 42,
                handle: SftpHandle::from_raw(command_tx),
                rx: tokio::sync::mpsc::channel(128).1,
                host: "待关闭主机".into(),
                home: None,
            });
            app.poll_connection(&cc.egui_ctx);
            assert!(matches!(
                command_rx.try_recv(),
                Ok(mino_core::ssh::sftp::SftpCmd::Shutdown)
            ));
            app
        });
    }

    /// 输入命令前缀时不再创建应用内的建议浮层，输入仍由 shell 直接处理。
    #[test]
    fn 输入命令不显示提示浮层() {
        let mut harness = egui_kittest::Harness::new_eframe(|cc| MinoApp::new(cc));
        harness.run_steps(6);
        harness.event(egui::Event::Text("ca".into()));
        harness.run_steps(6);

        assert!(
            harness
                .ctx
                .memory(|memory| memory.area_rect(egui::Id::new("completion_popup")))
                .is_none(),
            "输入命令前缀不应创建建议浮层"
        );
    }

    /// 读当前标签页终端可见文本（kittest 断言用）。
    fn app_grid_text(app: &MinoApp) -> String {
        use alacritty_terminal::term::cell::Flags;
        let tab = app.tabs.get(app.active_tab).expect("无标签页");
        let term_arc = tab.terminal.session().term();
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

    /// 完整应用（工具栏/标签栏/悬浮按钮共存）下回车应执行命令
    /// （回归测试：用户报告英文输入法下命令能输入但回车不执行）。
    #[test]
    fn 完整应用回车执行命令() {
        use std::time::{Duration, Instant};

        let mut harness = egui_kittest::Harness::new_eframe(|cc| MinoApp::new(cc));
        harness.run_steps(6);

        // 等待 zsh 就绪（提示符出现）。用 `~`（home 缩写，zsh 在 home 目录的
        // 交互提示符必含）而非 `➜`（仅 oh-my-zsh robbyrussell 主题有）——
        // GitHub runner 默认 zsh 提示符是 `...:~ runner$`，不输出 `➜`，
        // 依赖 `➜` 会让该测试在 CI 上永远超时（此前 CI 失败根因）。
        let deadline = Instant::now() + Duration::from_secs(8);
        let mut ready = false;
        while Instant::now() < deadline {
            harness.step();
            if app_grid_text(harness.state()).contains('~') {
                ready = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(60));
        }
        assert!(
            ready,
            "zsh 未就绪，终端内容：\n{}",
            app_grid_text(harness.state())
        );

        // 输入 echo hello。
        harness.event(egui::Event::Text("echo hello".into()));
        for _ in 0..6 {
            harness.step();
        }

        // 按回车。
        harness.event(egui::Event::Key {
            key: egui::Key::Enter,
            physical_key: None,
            modifiers: egui::Modifiers::NONE,
            repeat: false,
            pressed: true,
        });

        // 等待 hello 输出出现（命令被执行）。
        let deadline = Instant::now() + Duration::from_secs(8);
        let mut executed = false;
        while Instant::now() < deadline {
            harness.step();
            if app_grid_text(harness.state()).contains("hello") {
                executed = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(60));
        }
        assert!(
            executed,
            "回车未执行命令，终端内容：\n{}",
            app_grid_text(harness.state())
        );
    }

    /// 无前台弹窗时 Esc 必须到达终端；Vim 依赖它退出插入模式。
    #[test]
    fn 完整应用转义键转发给终端() {
        use std::time::{Duration, Instant};

        let mut harness = egui_kittest::Harness::new_eframe(|cc| MinoApp::new(cc));
        harness.run_steps(6);

        let deadline = Instant::now() + Duration::from_secs(8);
        let mut ready = false;
        while Instant::now() < deadline {
            harness.step();
            if app_grid_text(harness.state()).contains('~') {
                ready = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(60));
        }
        assert!(ready, "zsh 未就绪");

        // `read` 在规范输入模式下等待换行；PTY 会把收到的实际 ESC 字节
        // 回显为 `^[`。这直接验证应用级快捷键没有先消费该按键。
        harness.event(egui::Event::Text("IFS= read -r c".into()));
        harness.event(egui::Event::Key {
            key: egui::Key::Enter,
            physical_key: None,
            modifiers: egui::Modifiers::NONE,
            repeat: false,
            pressed: true,
        });
        for _ in 0..6 {
            harness.step();
        }

        harness.event(egui::Event::Key {
            key: egui::Key::Escape,
            physical_key: None,
            modifiers: egui::Modifiers::NONE,
            repeat: false,
            pressed: true,
        });

        let deadline = Instant::now() + Duration::from_secs(8);
        let mut forwarded = false;
        while Instant::now() < deadline {
            harness.step();
            if app_grid_text(harness.state())
                .lines()
                .any(|line| line.trim() == "^[")
            {
                forwarded = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(60));
        }
        assert!(
            forwarded,
            "Esc 未转发给终端，内容：\n{}",
            app_grid_text(harness.state())
        );
    }

    /// 新建连接表单默认值：用户名 root、端口 22（可修改）。
    #[test]
    fn 表单默认root与22端口() {
        let form = ConnectForm::default();
        assert_eq!(form.user, "root");
        assert_eq!(form.port, "22");
        assert!(form.name.is_empty());
        assert!(form.host.is_empty());
        assert_eq!(form.port.trim().parse::<u16>().unwrap(), 22);
    }

    /// 完整应用：⌘, 打开设置弹窗后点"新建连接"按钮打开对话框，不应崩溃。
    #[test]
    fn 点击新建连接不崩溃() {
        use kittest::Queryable;

        let mut harness = egui_kittest::Harness::new_eframe(|cc| MinoApp::new(cc));
        harness.run_steps(6);
        // ⌘, 打开设置弹窗（含"主机管理"分组与"新建连接"按钮）。
        harness.event(egui::Event::Key {
            key: egui::Key::Comma,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::COMMAND,
        });
        for _ in 0..3 {
            harness.step();
        }

        harness.get_by_label("新建连接").click();
        for _ in 0..8 {
            harness.step();
        }

        assert!(
            harness.query_all_by_label("名称").next().is_some(),
            "名称字段缺失"
        );
        assert!(
            harness.query_all_by_label("主机").next().is_some(),
            "主机字段缺失"
        );
        assert!(
            harness.query_all_by_label("用户名").next().is_some(),
            "用户名字段缺失"
        );
        assert!(
            harness.query_all_by_label("认证方式").next().is_some(),
            "认证方式缺失"
        );
        assert!(
            harness.query_all_by_label("连接").next().is_some(),
            "连接按钮缺失"
        );
        assert!(
            harness.query_all_by_label("取消").next().is_some(),
            "取消按钮缺失"
        );
    }
}

#[cfg(test)]
mod connect_tests {
    use super::*;
    use mino_core::config::Auth;

    /// 将测试 sshd 的 known_hosts 记录与 hostkey 放在同一目录（/tmp/mino-test-sshd）：
    /// hostkey 随 /tmp 清理重建时指纹记录一并消失，避免旧指纹不匹配导致测试失败。
    /// `call_once` 保证进程内只设置一次（测试并行安全）。
    static KNOWN_HOSTS_INIT: std::sync::Once = std::sync::Once::new();
    fn init_test_env() {
        KNOWN_HOSTS_INIT.call_once(|| {
            std::env::set_var("MINO_KNOWN_HOSTS", "/tmp/mino-test-sshd/known_hosts.toml");
        });
    }

    fn sshd_available() -> bool {
        use std::net::TcpStream;
        use std::time::Duration;
        TcpStream::connect_timeout(
            &"127.0.0.1:2222".parse().unwrap(),
            Duration::from_millis(500),
        )
        .is_ok()
    }

    fn test_profile() -> HostProfile {
        let key_path = std::env::var("MINO_TEST_KEY").unwrap_or_else(|_| {
            format!(
                "{}/.ssh/id_ed25519",
                std::env::var("HOME").unwrap_or_default()
            )
        });
        HostProfile {
            name: "链路测试".into(),
            host: std::env::var("MINO_TEST_HOST").unwrap_or_else(|_| "127.0.0.1".into()),
            port: std::env::var("MINO_TEST_PORT")
                .unwrap_or_else(|_| "2222".into())
                .parse()
                .unwrap(),
            user: std::env::var("MINO_TEST_USER")
                .unwrap_or_else(|_| std::env::var("USER").unwrap_or_else(|_| "root".into())),
            auth: Auth::Key {
                path: key_path.into(),
                passphrase: None,
            },
        }
    }

    /// 端到端：点击侧栏主机条目 → 远程终端 + SFTP 面板出现。
    #[test]
    fn 点击连接建立远程会话() {
        use kittest::Queryable;
        use std::time::{Duration, Instant};

        init_test_env();
        if !sshd_available() {
            eprintln!("跳过：测试 sshd 未运行（scripts/test-sshd.sh start）");
            return;
        }

        let profile = test_profile();
        if let Auth::Key { path, .. } = &profile.auth {
            if !path.exists() {
                eprintln!("跳过：测试私钥不存在");
                return;
            }
        }
        // 隔离路径：绝不覆盖用户真实的 ~/.config/mino/hosts.toml
        //（曾直接覆盖并删除用户主机列表，运行一次测试丢一次配置）。
        let config_path = test_config_path("connect-e2e");
        let config = HostConfig {
            theme: String::new(),
            hosts: vec![profile],
            projects: Vec::new(),
        };
        config.save(&config_path).expect("写入测试配置失败");

        let mut harness = egui_kittest::Harness::builder()
            .with_step_dt(1.0 / 60.0)
            .build_eframe(|cc| MinoApp::new_with_config(cc, config_path.clone()));
        harness.run_steps(6);
        // 主机行从设置弹窗里取：⌘, 打开设置弹窗。
        harness.event(egui::Event::Key {
            key: egui::Key::Comma,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::COMMAND,
        });
        for _ in 0..3 {
            harness.step();
        }

        // 双击设置 tab 内的主机行（自实现 0.3s 双击检测）。
        {
            let host_row = harness.get_by_label("链路测试");
            host_row.click();
        }
        harness.step();
        {
            let host_row = harness.get_by_label("链路测试");
            host_row.click();
        }

        let deadline = Instant::now() + Duration::from_secs(15);
        let mut connected = false;
        while Instant::now() < deadline {
            for _ in 0..5 {
                harness.step();
            }
            // SFTP 面板默认收起，连接成功后先出现终端右上角浮动按钮；
            // 展开后再断言面板标题。
            if harness.root().query_all_by_label("SFTP").next().is_some() {
                connected = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        std::fs::remove_file(&config_path).ok();

        assert!(
            connected,
            "点击连接后未出现 SFTP 浮动按钮（连接失败或崩溃）"
        );
        // 连接成功后设置弹窗应自动关闭（回归：曾保持打开遮住终端）。
        assert!(
            !harness.state().show_settings,
            "双击主机行连接成功后设置弹窗应自动关闭"
        );
        // SFTP 面板默认收起，需点击终端右上角悬浮按钮展开。
        harness.get_by_label("SFTP").click();
        for _ in 0..6 {
            harness.step();
        }
        assert!(
            harness
                .root()
                .query_all_by_label("SFTP · 链路测试")
                .next()
                .is_some(),
            "SFTP 标题应出现"
        );
        harness.get_by_label("..");
    }

    /// 回归：设置弹窗 → 新建连接 → 点"连接"后，新建连接对话框与设置弹窗
    /// 都应关闭（连接结果不影响关闭行为）。
    #[test]
    fn 新建连接后关闭设置弹窗() {
        use kittest::{NodeT, Queryable};

        let config_path = test_config_path("new-conn-dialog");
        let mut harness = egui_kittest::Harness::new_eframe(|cc| {
            MinoApp::new_with_config(cc, config_path.clone())
        });
        harness.run_steps(6);

        // ⌘, 打开设置弹窗 → 点"新建连接"。
        harness.event(egui::Event::Key {
            key: egui::Key::Comma,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::COMMAND,
        });
        for _ in 0..3 {
            harness.step();
        }
        assert!(harness.state().show_settings, "设置弹窗应打开");
        harness.get_by_label("新建连接").click();
        for _ in 0..3 {
            harness.step();
        }
        assert!(harness.state().show_new_conn, "新建连接对话框应打开");

        // 填主机（用户名默认 root、端口默认 22，无需改动）。
        // 输入框顺序：名称、用户名、主机、端口、密码。
        let inputs: Vec<_> = harness
            .root()
            .query_all_by_role(accesskit::Role::TextInput)
            .collect();
        assert!(inputs.len() >= 3, "表单应有名称/用户名/主机输入框");
        inputs[2].click();
        for _ in 0..2 {
            harness.step();
        }
        harness.event(egui::Event::Text("127.0.0.1".into()));
        for _ in 0..2 {
            harness.step();
        }

        // 点"连接" → 两个弹窗都应关闭（连接发起后失败与否不影响）。
        // 按钮文字会同时生成 Label 节点，需按 role 过滤出真正的 Button。
        let connect_btn = harness
            .root()
            .query_all_by_role(accesskit::Role::Button)
            .find(|n| n.accesskit_node().label() == Some("连接".to_string()))
            .expect("找不到连接按钮");
        connect_btn.click();
        for _ in 0..3 {
            harness.step();
        }
        assert!(
            !harness.state().show_new_conn,
            "点连接后新建连接对话框应关闭"
        );
        assert!(!harness.state().show_settings, "点连接后设置弹窗应关闭");
        // 清理测试写入的配置。
        std::fs::remove_file(&config_path).ok();
    }
}

#[cfg(test)]
mod snapshot_tests {
    use super::*;

    /// 与 connect_tests::init_test_env 相同（每模块独立 Once）。
    static KNOWN_HOSTS_INIT: std::sync::Once = std::sync::Once::new();
    fn init_test_env() {
        KNOWN_HOSTS_INIT.call_once(|| {
            std::env::set_var("MINO_KNOWN_HOSTS", "/tmp/mino-test-sshd/known_hosts.toml");
        });
    }

    fn sshd_available() -> bool {
        use std::net::TcpStream;
        use std::time::Duration;
        TcpStream::connect_timeout(
            &"127.0.0.1:2222".parse().unwrap(),
            Duration::from_millis(500),
        )
        .is_ok()
    }

    /// 连接测试 sshd 后渲染应用界面并保存截图（供视觉验证）。
    #[test]
    fn 生成连接后样式截图() {
        use kittest::Queryable;
        use mino_core::config::Auth;
        use std::time::{Duration, Instant};

        init_test_env();
        if !sshd_available() {
            eprintln!("跳过：测试 sshd 未运行");
            return;
        }

        let key_path = std::env::var("MINO_TEST_KEY").unwrap_or_else(|_| {
            format!(
                "{}/.ssh/id_ed25519",
                std::env::var("HOME").unwrap_or_default()
            )
        });
        let profile = HostProfile {
            name: "链路测试".into(),
            host: std::env::var("MINO_TEST_HOST").unwrap_or_else(|_| "127.0.0.1".into()),
            port: std::env::var("MINO_TEST_PORT")
                .unwrap_or_else(|_| "2222".into())
                .parse()
                .unwrap(),
            user: std::env::var("MINO_TEST_USER")
                .unwrap_or_else(|_| std::env::var("USER").unwrap_or_else(|_| "root".into())),
            auth: Auth::Key {
                path: key_path.into(),
                passphrase: None,
            },
        };
        if let Auth::Key { path, .. } = &profile.auth {
            if !path.exists() {
                eprintln!("跳过：测试私钥不存在");
                return;
            }
        }
        let config_path = test_config_path("snapshot");
        let config = HostConfig {
            theme: String::new(),
            hosts: vec![profile],
            projects: Vec::new(),
        };
        config.save(&config_path).expect("写入测试配置失败");

        let mut harness = egui_kittest::Harness::builder()
            .with_step_dt(1.0 / 60.0)
            .build_eframe(|cc| MinoApp::new_with_config(cc, config_path.clone()));
        harness.run_steps(6);
        // 主机行从设置弹窗里取：⌘, 打开设置弹窗。
        harness.event(egui::Event::Key {
            key: egui::Key::Comma,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::COMMAND,
        });
        for _ in 0..3 {
            harness.step();
        }
        {
            let host_row = harness.get_by_label("链路测试");
            host_row.click();
        }
        harness.step();
        {
            let host_row = harness.get_by_label("链路测试");
            host_row.click();
        }

        let mut connected = false;
        for _attempt in 0..3 {
            let deadline = Instant::now() + Duration::from_secs(10);
            while Instant::now() < deadline {
                for _ in 0..5 {
                    harness.step();
                }
                // SFTP 面板默认收起，以浮动开关作为连接成功信号。
                if harness.root().query_all_by_label("SFTP").next().is_some() {
                    connected = true;
                    break;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            if connected {
                break;
            }
            config.save(&config_path).ok();
            harness = egui_kittest::Harness::builder()
                .with_step_dt(1.0 / 60.0)
                .build_eframe(|cc| MinoApp::new_with_config(cc, config_path.clone()));
            harness.run_steps(6);
            // 主机行从设置弹窗里取：⌘, 打开设置弹窗。
            harness.event(egui::Event::Key {
                key: egui::Key::Comma,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::COMMAND,
            });
            for _ in 0..3 {
                harness.step();
            }
            {
                let host_row = harness.get_by_label("链路测试");
                host_row.click();
            }
            harness.step();
            {
                let host_row = harness.get_by_label("链路测试");
                host_row.click();
            }
        }
        assert!(connected, "连接失败，无法生成截图");

        // 连接成功后设置弹窗自动关闭，SFTP 浮按钮可直接点击。
        harness.get_by_label("SFTP").click();
        for _ in 0..6 {
            harness.step();
        }
        // 布局断言：SFTP 面板应位于窗口右侧（终端 + 面板 + 侧栏三段式）。
        let panel_title = harness
            .root()
            .query_all_by_label("SFTP · 链路测试")
            .max_by(|a, b| a.rect().left().partial_cmp(&b.rect().left()).unwrap())
            .expect("SFTP 标题应出现");
        let r = panel_title.rect();
        assert!(
            r.left() > 400.0,
            "SFTP 面板应位于窗口右半侧，实际标题 x={}",
            r.left()
        );
        // 设置入口齿轮按钮应在标签栏最右侧（> 700 视口），用 Button role 查找
        // （齿轮纯图标无文字 label）。取所有 button 中 right 最大的。
        let gear = harness
            .root()
            .query_all_by_role(accesskit::Role::Button)
            .max_by(|a, b| a.rect().right().partial_cmp(&b.rect().right()).unwrap())
            .expect("应找到按钮");
        assert!(
            gear.rect().right() > 700.0,
            "齿轮按钮应在标签栏最右侧（视口宽 800），实际 right={}",
            gear.rect().right()
        );
        // 多跑几帧让面板状态稳定后再截图（kittest 渲染器对首帧 shapes 输出有延迟）。
        for _ in 0..30 {
            harness.step();
        }

        let img = harness.render().expect("渲染失败");
        let out = "/tmp/mino_style_sftp.png";
        img.save(out).expect("保存截图失败");
        eprintln!("样式截图已保存：{out}");
    }
}

#[cfg(test)]
mod theme_tests {
    use super::*;
    use std::sync::Mutex;

    /// `CURRENT_THEME` 是进程级全局静态量：多线程并行跑测试时，
    /// 一个测试的 `set_theme` 会污染另一个测试的 `current_theme()` 断言。
    /// 本模块两个测试串行化，避免"单跑过、并跑随机挂"。
    static THEME_TEST_LOCK: Mutex<()> = Mutex::new(());

    /// 三套主题切换并渲染截图（视觉验证用）。
    #[test]
    fn 三套主题渲染截图() {
        use kittest::Queryable;

        let _guard = THEME_TEST_LOCK.lock().unwrap();
        let config_path = test_config_path("theme-shots");
        let mut harness = egui_kittest::Harness::new_eframe(|cc| {
            MinoApp::new_with_config(cc, config_path.clone())
        });
        harness.run_steps(6);
        // 主题下拉现在在设置弹窗里，先用 ⌘, 打开弹窗。
        harness.event(egui::Event::Key {
            key: egui::Key::Comma,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::COMMAND,
        });
        for _ in 0..3 {
            harness.step();
        }

        for theme_name in ["深色", "深蓝", "霓虹"] {
            // 项目管理卡片加入后设置内容变长，外观卡片可能被挤出可视区：
            // 先滚到下拉框再点，否则点击落在别的控件上、选项菜单弹不出。
            {
                let combo = harness
                    .root()
                    .query_by_role(accesskit::Role::ComboBox)
                    .expect("主题下拉不存在");
                combo.scroll_to_me();
            }
            for _ in 0..3 {
                harness.step();
            }
            harness
                .root()
                .query_by_role(accesskit::Role::ComboBox)
                .expect("主题下拉不存在")
                .click();
            for _ in 0..3 {
                harness.step();
            }
            harness
                .get_by_role_and_label(accesskit::Role::Button, theme_name)
                .click();
            for _ in 0..3 {
                harness.step();
            }
            // 切换即落盘：hosts.toml 里应记录所选主题名
            //（回归：曾只改内存，退出重进回到原来的）。
            let saved = HostConfig::load(&config_path).expect("主题切换后配置应可读");
            assert_eq!(saved.theme, theme_name, "主题选择应持久化到配置");
            let img = harness.render().expect("渲染失败");
            let out = format!("/tmp/mino_theme_{theme_name}.png");
            img.save(&out).expect("保存截图失败");
            eprintln!("已保存：{out}");
        }
        std::fs::remove_file(&config_path).ok();
    }

    /// 回归：切换皮肤后重启应用，应恢复上次所选主题而非第一套。
    #[test]
    fn 主题切换重启后保持() {
        let _guard = THEME_TEST_LOCK.lock().unwrap();
        let config_path = test_config_path("theme-persist");
        HostConfig {
            theme: String::new(),
            hosts: Vec::new(),
            projects: Vec::new(),
        }
        .save(&config_path)
        .expect("写入测试配置失败");

        // 首次启动 → 切到"霓虹"（经持久化路径写回配置）。
        let mut harness = egui_kittest::Harness::new_eframe(|cc| {
            MinoApp::new_with_config(cc, config_path.clone())
        });
        harness.run_steps(6);
        let theme_ctx = harness.state().last_ctx.clone();
        harness.state_mut().apply_theme_and_persist(&theme_ctx, 2);
        harness.run_steps(3);
        assert_eq!(crate::theme::current_theme().name, "霓虹");
        drop(harness);

        // 第二次启动（模拟退出重进）→ 应仍是"霓虹"。
        let mut harness = egui_kittest::Harness::new_eframe(|cc| {
            MinoApp::new_with_config(cc, config_path.clone())
        });
        harness.run_steps(6);
        assert_eq!(
            crate::theme::current_theme().name,
            "霓虹",
            "重启后主题应保持上次选择，而非回到默认"
        );

        std::fs::remove_file(&config_path).ok();
    }
}

#[cfg(test)]
mod crash_report_tests {
    use super::*;

    /// 崩溃日志读取：有内容才提示，取走后归档（不删——那是用户报告闪退
    /// 的唯一证据），同一个崩溃不能每次启动都提示。
    #[test]
    fn 崩溃日志取走后归档() {
        let dir = std::env::temp_dir().join(format!("mino-crash-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("创建测试目录失败");
        let config = dir.join("hosts.toml");
        let log = dir.join("crash.log");

        // 没有日志：不提示（首次启动的正常路径）。
        assert!(take_crash_report(&config).is_none());

        // 空文件：不算崩溃（写失败留下的空壳）。
        std::fs::write(&log, b"").unwrap();
        assert!(take_crash_report(&config).is_none());

        // 有内容：返回归档路径，且原文件被移走（下次启动不再提示）。
        std::fs::write(&log, b"=== crash ===\n").unwrap();
        let archived = take_crash_report(&config).expect("应识别到崩溃日志");
        assert!(archived.exists(), "归档文件应保留：{archived:?}");
        assert!(!log.exists(), "原日志应已移走，避免重复提示");
        assert!(take_crash_report(&config).is_none(), "第二次读取不应再提示");

        std::fs::remove_dir_all(&dir).ok();
    }
}

#[cfg(test)]
mod tab_tests {
    use super::*;

    /// 标签栏"＋"按钮应新建标签页（而非替换）：关闭按钮数量 1 → 2。
    #[test]
    fn 新建本地终端标签页() {
        use kittest::Queryable;

        let mut harness = egui_kittest::Harness::new_eframe(|cc| MinoApp::new(cc));
        harness.run_steps(6);
        // 本地标签标题 = 当前目录末级名（启动即 home → `~`），
        // 不采用 shell 上报的窗口标题（oh-my-zsh 的标题是截断过的路径）。
        let (title, tooltip) = harness.state().tabs[0].title();
        let home = std::env::var("HOME").expect("测试环境应有 HOME");
        assert_eq!(title, "~", "启动目录 home 应显示为 `~`");
        assert_eq!(
            tooltip.as_deref(),
            Some(home.as_str()),
            "悬浮提示应为全路径"
        );
        assert_eq!(
            harness.query_all_by_label("×").count(),
            1,
            "初始应有一个标签页"
        );

        harness.get_by_label("＋").click();
        for _ in 0..4 {
            harness.step();
        }
        assert_eq!(
            harness.query_all_by_label("×").count(),
            2,
            "点击后应有 2 个标签页"
        );
    }

    /// ⌘T 新建标签页、⌘W 关闭当前标签页。
    #[test]
    fn 快捷键新建与关闭标签页() {
        use kittest::Queryable;

        let mut harness = egui_kittest::Harness::new_eframe(|cc| MinoApp::new(cc));
        harness.run_steps(6);

        // 启动时：[本地终端]（设置改弹窗，不再是 tab）→ 1 个 ×。
        assert_eq!(
            harness.query_all_by_label("×").count(),
            1,
            "启动时只有 1 个本地终端标签"
        );

        // ⌘T 新建本地终端 → [本地终端, 新的本地终端] → 2 个 ×。
        harness.event(egui::Event::Key {
            key: egui::Key::T,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::COMMAND,
        });
        for _ in 0..4 {
            harness.step();
        }
        assert_eq!(
            harness.query_all_by_label("×").count(),
            2,
            "⌘T 后应有 2 个 × 按钮"
        );

        // ⌘W 关闭当前 → [本地终端] → 1 个 ×。
        harness.event(egui::Event::Key {
            key: egui::Key::W,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::COMMAND,
        });
        for _ in 0..4 {
            harness.step();
        }
        assert_eq!(
            harness.query_all_by_label("×").count(),
            1,
            "⌘W 后应回到 1 个 × 按钮"
        );

        // 再次 ⌘W → [] → 0 个 ×。
        harness.event(egui::Event::Key {
            key: egui::Key::W,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::COMMAND,
        });
        for _ in 0..4 {
            harness.step();
        }
        assert_eq!(
            harness.query_all_by_label("×").count(),
            0,
            "全部关闭后应无 × 按钮"
        );
    }

    /// 目录展示名 = 最末级文件夹名（zsh `%c` 语义）。
    #[test]
    fn 目录展示名只保留末级() {
        assert_eq!(dir_display_name("/Users/me/proj"), "proj");
        assert_eq!(dir_display_name("/Users/me/proj/"), "proj");
        assert_eq!(dir_display_name("/"), "/");
        assert_eq!(dir_display_name(""), "/");
        let home = std::env::var("HOME").expect("测试环境应有 HOME");
        assert_eq!(dir_display_name(&home), "~");
        assert_eq!(dir_display_name(&format!("{home}/")), "~");
        // home 内的子目录仍只显示末级名（不缩写为 `~/x`）。
        assert_eq!(dir_display_name(&format!("{home}/proj")), "proj");
    }

    /// 本地标签标题跟随终端当前目录：只显示末级文件夹名，悬浮提示给全路径。
    ///
    /// 目录来源是 shell 子进程的内核 cwd（每帧实时读取），不再依赖脆弱的
    /// 终端输入跟踪；不采用 shell 上报的窗口标题——oh-my-zsh 的标题是
    /// 截断过的 `%15<..<%~%<<`，既非末级目录名也拿不到完整路径。
    #[test]
    fn 本地标签标题跟随当前目录() {
        use std::cell::RefCell;
        use std::rc::Rc;
        use std::time::{Duration, Instant};

        let base = std::env::temp_dir().join(format!("mino-tab-title-{}", std::process::id()));
        let project = base.join("proj-alpha");
        std::fs::create_dir_all(&project).expect("创建测试目录失败");

        let home_dir = PathBuf::from(std::env::var("HOME").expect("测试环境应有 HOME"));
        let session = Session::spawn_local(
            SessionOptions {
                working_directory: Some(home_dir),
                ..Default::default()
            },
            80,
            24,
            Arc::new(|_ev: &SessionEvent| {}),
        )
        .expect("创建本地终端失败");
        let tab = Rc::new(RefCell::new(TerminalTab::new(
            1,
            "本地终端".into(),
            TerminalView::new(session),
        )));
        let show = tab.clone();
        let mut harness = egui_kittest::Harness::new_ui(move |ui| {
            show.borrow_mut().terminal.show(ui);
        });
        harness.run_steps(6);

        // 启动目录 = home → `~`（与 zsh 提示符一致），提示为全路径。
        let home = std::env::var("HOME").expect("测试环境应有 HOME");
        let (title, tooltip) = tab.borrow().title();
        assert_eq!(title, "~", "启动目录 home 应显示为 `~`");
        assert_eq!(tooltip.as_deref(), Some(home.as_str()));

        // 等 shell 就绪（独立哨兵输出，不依赖目录名）。
        harness.event(egui::Event::Text("printf __MINO_TAB_READY__".into()));
        harness.event(egui::Event::Key {
            key: egui::Key::Enter,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::default(),
        });
        let deadline = Instant::now() + Duration::from_secs(8);
        let mut ready = false;
        while Instant::now() < deadline {
            harness.step();
            let text =
                crate::views::terminal_view::tests_grid_text(tab.borrow().terminal.session());
            if text.contains("__MINO_TAB_READY__") {
                ready = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(60));
        }
        assert!(ready, "zsh 未就绪");

        // cd 进嵌套目录：标题只显示末级名，提示给完整（已规范化）路径。
        harness.event(egui::Event::Text(format!("cd {}", project.display())));
        harness.event(egui::Event::Key {
            key: egui::Key::Enter,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::default(),
        });
        let expected = std::fs::canonicalize(&project)
            .expect("规范化测试目录失败")
            .to_string_lossy()
            .into_owned();
        let deadline = Instant::now() + Duration::from_secs(8);
        let mut followed = false;
        while Instant::now() < deadline {
            harness.step();
            let (title, tooltip) = tab.borrow().title();
            if title == "proj-alpha" && tooltip.as_deref() == Some(expected.as_str()) {
                followed = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(60));
        }
        let (title, tooltip) = tab.borrow().title();
        assert!(
            followed,
            "cd 后标题未跟随目录：title={title:?} tooltip={tooltip:?}（期望 proj-alpha / {expected}）"
        );
        std::fs::remove_dir_all(base).ok();
    }

    /// 标签栏与状态栏都渲染当前目录的末级名（不再是固定 "本地终端"），
    /// 悬浮标题显示全路径。
    #[test]
    fn 标签栏与状态栏显示当前目录名() {
        use kittest::Queryable;
        use std::time::{Duration, Instant};

        let base = std::env::temp_dir().join(format!("mino-tab-label-{}", std::process::id()));
        let project = base.join("proj-beta");
        std::fs::create_dir_all(&project).expect("创建测试目录失败");

        let mut harness = egui_kittest::Harness::new_eframe(|cc| MinoApp::new(cc));
        harness.run_steps(6);
        let deadline = Instant::now() + Duration::from_secs(8);
        let mut ready = false;
        while Instant::now() < deadline {
            harness.step();
            let text = crate::views::terminal_view::tests_grid_text(
                harness.state().tabs[0].terminal.session(),
            );
            if !text.trim().is_empty() {
                ready = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(60));
        }
        assert!(ready, "zsh 未就绪");

        harness.event(egui::Event::Text(format!("cd {}", project.display())));
        harness.event(egui::Event::Key {
            key: egui::Key::Enter,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::default(),
        });
        let expected = std::fs::canonicalize(&project)
            .expect("规范化测试目录失败")
            .to_string_lossy()
            .into_owned();
        let deadline = Instant::now() + Duration::from_secs(8);
        let mut shown = false;
        while Instant::now() < deadline {
            harness.step();
            if harness.query_all_by_label("proj-beta").count() == 2 {
                shown = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(60));
        }
        let tab_title = crate::views::terminal_view::tests_grid_text(
            harness.state().tabs[0].terminal.session(),
        );
        assert!(
            shown,
            "标签栏与状态栏应各显示一次目录名，实际 “proj-beta” 出现 {} 次；终端内容：\n{tab_title}",
            harness.query_all_by_label("proj-beta").count()
        );

        // 悬浮标签标题 → 全路径提示。
        harness
            .query_all_by_label("proj-beta")
            .next()
            .expect("标签标题节点缺失")
            .hover();
        for _ in 0..4 {
            harness.step();
        }
        assert_eq!(
            harness.query_all_by_label(&expected).count(),
            1,
            "悬浮标签标题应显示全路径 {expected}"
        );
        std::fs::remove_dir_all(base).ok();
    }

    /// 跟踪器失效时标题仍跟随：`source` 别名/函数等场景下输入跟踪早已
    /// invalidate，标题数据源是内核 cwd，不能停在旧目录。
    #[test]
    fn 跟踪失效后标题仍跟随内核目录() {
        use std::time::{Duration, Instant};

        // 启动目录用 HOME 之外的独立目录，避免与 `~` 断言耦合。
        let base = std::env::temp_dir().join(format!("mino-tab-stale-{}", std::process::id()));
        let project = base.join("proj-gamma");
        std::fs::create_dir_all(&project).expect("创建测试目录失败");
        let start = std::fs::canonicalize(&base).expect("规范化测试目录失败");

        let options = SessionOptions {
            working_directory: Some(start.clone()),
            ..Default::default()
        };
        let session = Session::spawn_local(options, 80, 24, Arc::new(|_ev: &SessionEvent| {}))
            .expect("创建本地终端失败");
        let mut view = TerminalView::new(session);
        // 模拟 Tab/粘贴/方向键后的跟踪失效：此前标题会永久停在旧值。
        view.workdir_for_test().invalidate();
        let tab = TerminalTab::new(1, "本地终端".into(), view);
        let expected = std::fs::canonicalize(&project)
            .expect("规范化测试目录失败")
            .to_string_lossy()
            .into_owned();
        // 子 shell 启动需要时间（login shell 先跑完 rc 再 chdir 到启动目录）：
        // 轮询等内核 cwd 落到启动目录，避免把启动瞬间 home 误判为 bug。
        let start_name = dir_display_name(&start.to_string_lossy());
        let start_full = start.to_string_lossy().into_owned();
        let deadline = Instant::now() + Duration::from_secs(8);
        loop {
            let (title, tooltip) = tab.title();
            if title == start_name && tooltip.as_deref() == Some(start_full.as_str()) {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "子 shell 未进入启动目录：title={title:?} tooltip={tooltip:?}（期望 {start_name} / {start_full}）"
            );
            std::thread::sleep(Duration::from_millis(60));
        }

        // 经由 shell 函数里的 cd（跟踪器只认行首 `cd`，函数体内的 cd
        // 在它眼里只是普通文本 + 回车，目录推测保持不动）。
        tab.terminal
            .session()
            .write(format!("mygoto() {{ cd {} ; }}; mygoto\n", project.display()).as_bytes());
        let deadline = Instant::now() + Duration::from_secs(8);
        let mut followed = false;
        while Instant::now() < deadline {
            let (title, tooltip) = tab.title();
            if title == "proj-gamma" && tooltip.as_deref() == Some(expected.as_str()) {
                followed = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(60));
        }
        let (title, tooltip) = tab.title();
        assert!(
            followed,
            "跟踪失效后标题未跟随内核目录：title={title:?} tooltip={tooltip:?}（期望 proj-gamma / {expected}）"
        );
        std::fs::remove_dir_all(base).ok();
    }

    /// 最后一个标签关闭后，空状态不能紧贴顶部标签栏，且快捷键应作为独立键帽渲染。
    #[test]
    fn 无标签页空状态有顶部留白与快捷键键帽() {
        use kittest::Queryable;

        let mut harness = egui_kittest::Harness::new_eframe(|cc| MinoApp::new(cc));
        harness.run_steps(6);
        harness.event(egui::Event::Key {
            key: egui::Key::W,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::COMMAND,
        });
        for _ in 0..4 {
            harness.step();
        }

        let title = harness.get_by_label(PRODUCT_NAME);
        assert!(
            title.rect().top() > 110.0,
            "空状态标题应与顶部标签栏保持距离，实际 top={}",
            title.rect().top()
        );
        assert!(harness.get_by_label("⌘T").rect().height() > 0.0);
        assert!(harness.get_by_label("⌘N").rect().height() > 0.0);
        assert!(harness.get_by_label("⌘O").rect().height() > 0.0);
        let shortcut_left = harness.get_by_label("⌘T").rect().left();
        let shortcut_right = harness.get_by_label("打开项目").rect().right();
        let shortcut_center = (shortcut_left + shortcut_right) * 0.5;
        let title_center = title.rect().center().x;
        assert!(
            (shortcut_center - title_center).abs() < 10.0,
            "快捷键行应与标题居中，shortcut_center={shortcut_center}, title_center={title_center}"
        );
    }

    /// 向标签栏空白点发送一次"移动→按下→抬起"点击序列，每步后累积检查视口命令。
    /// kittest 说明：`harness.output()` 只保留最后一帧的命令（每帧覆盖），
    /// 且默认 step_dt=0.25s 会让两次点击间隔 0.75s、永远形不成双击
    /// （阈值 0.3s）——调用方须用 `with_step_dt(1/60)` 构造 Harness。
    fn click_titlebar_gap(
        harness: &mut egui_kittest::Harness<MinoApp>,
        pos: egui::Pos2,
        saw: &mut impl FnMut(&egui::ViewportCommand),
    ) {
        harness.event(egui::Event::PointerMoved(pos));
        harness.step();
        collect_viewport_cmds(harness, saw);
        harness.event(egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed: true,
            modifiers: egui::Modifiers::default(),
        });
        harness.step();
        collect_viewport_cmds(harness, saw);
        harness.event(egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed: false,
            modifiers: egui::Modifiers::default(),
        });
        harness.step();
        collect_viewport_cmds(harness, saw);
    }

    /// 收集最后一帧根视口的全部命令（逐帧调用做累积断言）。
    fn collect_viewport_cmds(
        harness: &egui_kittest::Harness<MinoApp>,
        saw: &mut impl FnMut(&egui::ViewportCommand),
    ) {
        if let Some(vp) = harness
            .output()
            .viewport_output
            .get(&egui::ViewportId::ROOT)
        {
            for cmd in &vp.commands {
                saw(cmd);
            }
        }
    }
    /// 标签栏中部空白点（`>_` 按钮右侧 40px，命中 `tab_bar_drag` 背景）。
    ///
    /// 曾以"＋右侧 60px"为锚点；项目按钮插入 ＋ 与 `>_` 之间后该点落在按钮上，
    /// 改为以 `>_` 为基准（其右侧到齿轮之间为连续空白）。
    fn titlebar_gap_pos(harness: &mut egui_kittest::Harness<MinoApp>) -> egui::Pos2 {
        use kittest::Queryable as _;
        let ssh = harness.get_by_label(">_");
        egui::pos2(ssh.rect().right() + 40.0, ssh.rect().center().y)
    }

    /// 本地会话默认工作目录为 home（Finder 启动 cwd=/ 时终端应落在 ~）。
    #[test]
    fn 本地会话默认home目录() {
        let opts = local_session_options();
        assert_eq!(
            opts.working_directory,
            std::env::var("HOME").ok().map(PathBuf::from),
            "本地会话应默认在 home 目录"
        );
        // 必须注入 TERM：GUI 启动继承 TERM=dumb 会致删除回显异常/回车不执行（回归测试）。
        assert_eq!(
            opts.env.get("TERM").map(String::as_str),
            Some("xterm-256color"),
            "本地会话必须注入 TERM=xterm-256color（避免继承 TERM=dumb）"
        );
        // 必须注入 COLORTERM：omp 只认该变量判 24-bit（`getColorMode` 实证），
        // 缺了它 agent 全程降级 256 色。
        assert_eq!(
            opts.env.get("COLORTERM").map(String::as_str),
            Some("truecolor"),
            "本地会话必须注入 COLORTERM=truecolor（omp 24-bit 开关）"
        );
    }

    /// 标题栏拖拽区双击应切换 zoom（回归：曾用 Sense::drag，双击永不触发）。
    ///
    /// 在标签栏空白处连击两次，断言根视口输出出现 `Maximized(true)`。
    /// `ViewportInfo::maximized` 在 eframe 注释里写明 macOS 运行时读取会
    /// 死锁、测试桩里恒为 None——取反后必为 `Maximized(true)`，不断言
    /// false 分支（恢复逻辑与标准行为一致，由同一行代码覆盖）。
    #[test]
    fn 标题栏空白处双击切换zoom() {
        let mut harness = egui_kittest::Harness::builder()
            .with_step_dt(1.0 / 60.0)
            .build_eframe(|cc| MinoApp::new(cc));
        harness.run_steps(6);
        let dbl_pos = titlebar_gap_pos(&mut harness);
        let mut saw_maximized = false;
        for _ in 0..2 {
            click_titlebar_gap(&mut harness, dbl_pos, &mut |cmd| {
                if matches!(cmd, egui::ViewportCommand::Maximized(true)) {
                    saw_maximized = true;
                }
            });
        }
        assert!(
            saw_maximized,
            "标签栏空白处双击应发出 Maximized(true)，点击点={dbl_pos:?}"
        );
    }

    /// 标题栏拖动仍应移动窗口（双击修复不能破坏 StartDrag）。
    ///
    /// 同一位置按下后移动指针（超过点击距离）再抬起：拖动过程中应发出
    /// StartDrag、且全程不发出 Maximized。
    #[test]
    fn 标题栏拖动仍移动窗口() {
        let mut harness = egui_kittest::Harness::builder()
            .with_step_dt(1.0 / 60.0)
            .build_eframe(|cc| MinoApp::new(cc));
        harness.run_steps(6);
        let start = titlebar_gap_pos(&mut harness);
        let mut saw_drag = false;
        let mut saw_maximized = false;
        let mut collect = |cmd: &egui::ViewportCommand| match cmd {
            egui::ViewportCommand::StartDrag => saw_drag = true,
            egui::ViewportCommand::Maximized(_) => saw_maximized = true,
            _ => {}
        };
        harness.event(egui::Event::PointerMoved(start));
        harness.step();
        collect_viewport_cmds(&harness, &mut collect);
        harness.event(egui::Event::PointerButton {
            pos: start,
            button: egui::PointerButton::Primary,
            pressed: true,
            modifiers: egui::Modifiers::default(),
        });
        harness.step();
        collect_viewport_cmds(&harness, &mut collect);
        // 按住拖出一段距离（远超 max_click_dist=6）：进入 drag 状态。
        let dragged = egui::pos2(start.x + 40.0, start.y + 30.0);
        harness.event(egui::Event::PointerMoved(dragged));
        harness.step();
        collect_viewport_cmds(&harness, &mut collect);
        harness.event(egui::Event::PointerButton {
            pos: dragged,
            button: egui::PointerButton::Primary,
            pressed: false,
            modifiers: egui::Modifiers::default(),
        });
        harness.step();
        collect_viewport_cmds(&harness, &mut collect);

        assert!(saw_drag, "拖动标签栏空白处应发出 StartDrag");
        assert!(!saw_maximized, "单纯拖动不应触发 zoom");
    }
}

#[cfg(test)]
mod dblclick_probe {
    /// 最小复现：kittest 两次 click 是否触发 double_clicked。
    #[test]
    fn 双击检测探针() {
        use kittest::Queryable;

        let mut harness = egui_kittest::Harness::builder()
            .with_step_dt(1.0 / 60.0)
            .build_ui(|ui| {
                let btn = ui.button("目标");
                if btn.double_clicked() {
                    ui.label("双击了");
                }
            });
        harness.step();
        {
            let b = harness.get_by_label("目标");
            b.click();
        }
        harness.step();
        {
            let b = harness.get_by_label("目标");
            b.click();
        }
        harness.step();
        assert!(
            harness.root().query_all_by_label("双击了").next().is_some(),
            "双击未触发"
        );
    }
}

#[cfg(test)]
mod settings_tests {
    use super::*;

    /// 设置标题区的品牌图标即使被指针悬浮，也不应绘制光晕图元。
    #[test]
    fn 设置图标悬浮保持静态() {
        fn circle_shape_count(draw: fn(&mut egui::Ui, f32) -> egui::Rect) -> usize {
            let ctx = egui::Context::default();
            let mut first_output = ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(100.0, 100.0),
                    )),
                    events: vec![egui::Event::PointerMoved(egui::pos2(20.0, 20.0))],
                    ..Default::default()
                },
                |ui| {
                    draw(ui, 40.0);
                },
            );
            first_output.textures_delta.clear();
            let mut output = ctx.run_ui(egui::RawInput::default(), |ui| {
                draw(ui, 40.0);
            });
            let count = output
                .shapes
                .iter()
                .filter(|clipped| matches!(&clipped.shape, egui::Shape::Circle(_)))
                .count();
            output.textures_delta.clear();
            count
        }

        assert_eq!(
            circle_shape_count(draw_logo_mark_static),
            0,
            "设置图标悬浮时不应绘制光晕圆形图元"
        );
        assert_eq!(
            circle_shape_count(draw_logo_mark),
            10,
            "普通品牌图标仍应保留既有悬浮光晕"
        );
    }

    /// 设置弹窗默认关闭，tabs 不含设置 tab（设置改弹窗后无 Tab::Settings 概念）。
    #[test]
    fn 设置弹窗默认关闭() {
        let mut harness = egui_kittest::Harness::new_eframe(|cc| MinoApp::new(cc));
        harness.run_steps(6);
        let app = harness.state();
        assert!(!app.show_settings, "启动时设置弹窗应默认关闭");
        assert_eq!(
            app.active_tab, 0,
            "默认激活第一个 tab（本地终端），不打扰用户"
        );
        // 启动时"主机管理"分组不应渲染。
        use kittest::Queryable;
        assert!(
            harness
                .root()
                .query_all_by_label("主机管理")
                .next()
                .is_none(),
            "设置弹窗未打开时不应渲染主机管理"
        );
    }

    /// 性能 HUD 默认显示在底部状态栏右侧。
    #[test]
    fn 性能_hud默认显示在状态栏右侧() {
        use kittest::Queryable;

        let mut harness = egui_kittest::Harness::new_eframe(|cc| MinoApp::new(cc));
        harness.run_steps(6);

        assert!(harness.state().show_perf_hud, "性能 HUD 启动时应默认展示");
        assert!(
            harness
                .ctx
                .memory(|memory| memory.area_rect(egui::Id::new("perf_hud")))
                .is_none(),
            "性能 HUD 不应再创建独立悬浮区域"
        );

        let hud = harness
            .root()
            .query_all_by_label_contains("帧")
            .next()
            .expect("状态栏中应显示性能 HUD 文本");
        let viewport = harness.ctx.viewport_rect();
        assert!(
            hud.rect().right() > viewport.right() - 300.0,
            "性能 HUD 应位于状态栏右侧，实际 right={} viewport right={}",
            hud.rect().right(),
            viewport.right()
        );
        assert!(
            hud.rect().bottom() > viewport.bottom() - 40.0,
            "性能 HUD 应位于底部状态栏，实际 bottom={} viewport bottom={}",
            hud.rect().bottom(),
            viewport.bottom()
        );
        // 与左侧会话标题同处一行（垂直中心对齐）。启动目录为 home，
        // 标题显示为 `~`（末级目录名的 home 缩写）。
        let title = harness
            .root()
            .query_all_by_label("~")
            .find(|node| node.rect().bottom() > viewport.bottom() - 40.0)
            .expect("状态栏中应显示会话标题");
        assert!(
            (hud.rect().center().y - title.rect().center().y).abs() < 8.0,
            "性能 HUD 应与会话标题同处一行，hud 中心 y={} title 中心 y={}",
            hud.rect().center().y,
            title.rect().center().y
        );
    }

    #[test]
    fn 测试构造器隔离配置并关闭自动更新() {
        let mut harness = egui_kittest::Harness::new_eframe(|cc| MinoApp::new(cc));
        harness.run_steps(2);
        let app = harness.state();
        assert!(matches!(app.update_state, UpdateState::Idle));
        assert_eq!(
            app.config_path.parent(),
            Some(std::env::temp_dir().as_path())
        );
        assert!(app
            .config_path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("mino-test-config-default-")));
    }

    /// 回归：从设置中手动检查更新时，设置窗口应继续保持打开。
    #[test]
    fn 检查更新不关闭设置弹窗() {
        use kittest::Queryable;

        let mut harness = egui_kittest::Harness::new_eframe(|cc| MinoApp::new(cc));
        harness.run_steps(6);
        harness.state_mut().show_settings = true;
        harness.run_steps(3);

        // “关于”卡片在默认高度下可能位于滚动区裁剪边缘，
        // 用无障碍点击精确触发按钮行为，不受测试视口大小影响。
        harness.get_by_label("检查更新").click_accesskit();
        harness.step();

        assert!(
            harness.state().show_settings,
            "点击检查更新后设置弹窗不应被关闭"
        );
        assert!(matches!(
            harness.state().update_state,
            UpdateState::Checking
        ));
    }

    /// 回归：设置弹窗覆盖终端时，滚轮只能滚动设置内容，
    /// 不能同时改变背后终端的 scrollback 偏移。
    #[test]
    fn 设置滚轮不传递到终端() {
        use alacritty_terminal::grid::{Dimensions, Scroll};
        use alacritty_terminal::index::Line;
        use alacritty_terminal::vte::ansi::Color as AColor;
        use kittest::Queryable;

        let mut harness = egui_kittest::Harness::new_eframe(|cc| MinoApp::new(cc));
        harness.run_steps(6);

        // 用足够多的主机条目撑高设置内容，确保设置 ScrollArea 可滚动。
        harness.state_mut().config.hosts = (0..12)
            .map(|index| HostProfile {
                name: format!("测试主机 {index}"),
                host: "127.0.0.1".into(),
                port: 22,
                user: "root".into(),
                auth: Auth::Password(String::new()),
            })
            .collect();
        harness.state_mut().show_settings = true;
        harness.run_steps(3);

        // 人工构造 scrollback 并先向上滚动；若滚轮穿透，向下滚时该值会变小。
        {
            let term = harness.state().tabs[harness.state().active_tab]
                .terminal
                .session()
                .term();
            let mut guard = term.lock();
            let lines = guard.grid().screen_lines();
            guard
                .grid_mut()
                .scroll_up::<AColor>(&(Line::from(0)..Line::from(lines)), 12);
            guard.grid_mut().scroll_display(Scroll::Delta(6));
        }
        let terminal_offset_before = {
            let term = harness.state().tabs[harness.state().active_tab]
                .terminal
                .session()
                .term();
            let guard = term.lock();
            guard.grid().display_offset()
        };
        let first_host = harness.get_by_label("测试主机 0");
        let first_host_y_before = first_host.rect().top();
        let pointer = first_host.rect().center();

        harness.event(egui::Event::PointerMoved(pointer));
        harness.step();
        harness.event(egui::Event::MouseWheel {
            unit: egui::MouseWheelUnit::Line,
            delta: egui::vec2(0.0, -4.0),
            modifiers: egui::Modifiers::NONE,
            phase: egui::TouchPhase::Move,
        });
        harness.run_steps(3);

        let terminal_offset_after = {
            let term = harness.state().tabs[harness.state().active_tab]
                .terminal
                .session()
                .term();
            let guard = term.lock();
            guard.grid().display_offset()
        };
        let first_host_y_after = harness.get_by_label("测试主机 0").rect().top();

        assert!(
            first_host_y_after < first_host_y_before,
            "滚轮应使设置内容向上移动：y={first_host_y_before} -> {first_host_y_after}"
        );
        assert_eq!(
            terminal_offset_after, terminal_offset_before,
            "设置中的滚轮事件不应传递到背后终端"
        );
    }

    /// ⌘, 快捷键切换设置弹窗（macOS 标准"应用偏好设置"）。
    #[test]
    fn 快捷键打开设置() {
        use kittest::Queryable;
        let mut harness = egui_kittest::Harness::new_eframe(|cc| MinoApp::new(cc));
        harness.run_steps(6);
        assert!(!harness.state().show_settings, "默认关闭");

        harness.event(egui::Event::Key {
            key: egui::Key::Comma,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::COMMAND,
        });
        for _ in 0..3 {
            harness.step();
        }
        // 打开后渲染"主机管理"分组。
        assert!(harness.state().show_settings, "⌘, 应打开设置弹窗");
        harness.get_by_label("主机管理");

        // 再按 ⌘, 关闭。
        harness.event(egui::Event::Key {
            key: egui::Key::Comma,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::COMMAND,
        });
        for _ in 0..3 {
            harness.step();
        }
        assert!(!harness.state().show_settings, "⌘, 应再次关闭设置弹窗");
    }

    #[test]
    fn 转义键关闭设置与连接弹窗() {
        let mut harness = egui_kittest::Harness::new_eframe(|cc| MinoApp::new(cc));
        harness.run_steps(6);

        harness.event(egui::Event::Key {
            key: egui::Key::Comma,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::COMMAND,
        });
        harness.run_steps(3);
        assert!(harness.state().show_settings);

        // 从设置打开新建连接后，Esc 关闭前台对话框并恢复设置窗口。
        harness.event(egui::Event::Key {
            key: egui::Key::N,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::COMMAND,
        });
        harness.run_steps(3);
        assert!(harness.state().show_new_conn);
        assert!(!harness.state().show_settings);

        harness.event(egui::Event::Key {
            key: egui::Key::Escape,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        });
        harness.run_steps(3);
        assert!(!harness.state().show_new_conn, "Esc 应关闭新建连接对话框");
        assert!(harness.state().show_settings, "Esc 后应恢复设置弹窗");

        harness.event(egui::Event::Key {
            key: egui::Key::Escape,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        });
        harness.run_steps(3);
        assert!(!harness.state().show_settings, "Esc 应关闭设置弹窗");

        harness.event(egui::Event::Key {
            key: egui::Key::N,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::COMMAND,
        });
        harness.run_steps(3);
        assert!(harness.state().show_new_conn);

        harness.event(egui::Event::Key {
            key: egui::Key::Escape,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        });
        harness.run_steps(3);
        assert!(!harness.state().show_new_conn, "Esc 应关闭新建连接对话框");
    }

    /// 标签栏齿轮按钮存在且在标签栏最右侧（> 200 px）。
    /// 齿轮纯图标无文字 label（`on_hover_text` 不被 kittest 识别为 label），
    /// 通过 `by_role(Button)` 查找；点击验证 ⌘, 等价路径。
    #[test]
    fn 齿轮存在并能打开设置() {
        use kittest::Queryable;
        let mut harness = egui_kittest::Harness::new_eframe(|cc| MinoApp::new(cc));
        harness.run_steps(6);
        // 齿轮是标签栏最右侧的 Button（视口宽 800，齿轮 right > 700）。
        let gear = harness
            .root()
            .query_all_by_role(accesskit::Role::Button)
            .max_by(|a, b| a.rect().right().partial_cmp(&b.rect().right()).unwrap())
            .expect("应找到按钮");
        assert!(
            gear.rect().right() > 700.0,
            "齿轮按钮应在标签栏最右侧（> 700），实际 right={}",
            gear.rect().right()
        );
        gear.click();
        for _ in 0..3 {
            harness.step();
        }
        assert!(harness.state().show_settings, "齿轮点击应打开设置弹窗");
        harness.get_by_label("主机管理");
    }

    /// 回归：关闭新建连接对话框后再次打开，不能带出上一次填写的内容。
    #[test]
    fn 新建连接每次打开都重置表单() {
        use kittest::Queryable;

        let config_path = test_config_path("new-conn-reset");
        let mut harness = egui_kittest::Harness::new_eframe(|cc| {
            MinoApp::new_with_config(cc, config_path.clone())
        });
        harness.run_steps(6);

        harness.event(egui::Event::Key {
            key: egui::Key::Comma,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::COMMAND,
        });
        harness.run_steps(3);
        harness.get_by_label("新建连接").click();
        harness.run_steps(3);

        // 显式点击名称输入框，写入一段应被下一次打开清除的内容。
        let name_input = harness
            .root()
            .query_all_by_role(accesskit::Role::TextInput)
            .next()
            .expect("找不到名称输入框");
        name_input.click();
        harness.run_steps(2);
        harness.event(egui::Event::Text("上一次连接".into()));
        harness.run_steps(3);
        assert_eq!(harness.state().form.name, "上一次连接");

        // 弹窗已经打开时重复按 ⌘N 不应重置用户正在填写的表单。
        harness.event(egui::Event::Key {
            key: egui::Key::N,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::COMMAND,
        });
        harness.run_steps(3);
        assert_eq!(harness.state().form.name, "上一次连接");

        harness.get_by_label("取消").click();
        harness.run_steps(3);
        assert!(!harness.state().show_new_conn, "取消后对话框应关闭");

        harness.get_by_label("新建连接").click();
        harness.run_steps(3);
        assert!(harness.state().show_new_conn, "第二次点击应重新打开对话框");
        assert_eq!(harness.state().form.name, "", "名称不应残留");
        assert_eq!(harness.state().form.host, "", "主机不应残留");
        assert_eq!(harness.state().form.password, "", "密码不应残留");
        assert_eq!(harness.state().form.user, "root", "默认用户名应恢复");
        assert_eq!(harness.state().form.port, "22", "默认端口应恢复");

        std::fs::remove_file(&config_path).ok();
    }

    /// 回归：主机行的身份列和认证列不应受名称长度影响而漂移。
    /// 认证列右对齐固定槽位，故断言右缘对齐（而非左缘）。
    #[test]
    fn 设置主机卡片列对齐() {
        use kittest::Queryable;

        let config_path = test_config_path("host-card-align");
        let config = HostConfig {
            theme: String::new(),
            hosts: vec![
                HostProfile {
                    name: "短名".into(),
                    host: "10.0.0.1".into(),
                    port: 22,
                    user: "root".into(),
                    auth: Auth::Password("x".into()),
                },
                HostProfile {
                    name: "一个很长的主机显示名称".into(),
                    host: "192.168.31.233".into(),
                    port: 22022,
                    user: "ubuntu".into(),
                    auth: Auth::Key {
                        path: PathBuf::from("~/.ssh/id_ed25519"),
                        passphrase: None,
                    },
                },
            ],
            projects: Vec::new(),
        };
        config.save(&config_path).expect("写入测试配置失败");

        let mut harness = egui_kittest::Harness::new_eframe(|cc| {
            MinoApp::new_with_config(cc, config_path.clone())
        });
        harness.run_steps(6);
        harness.event(egui::Event::Key {
            key: egui::Key::Comma,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::COMMAND,
        });
        harness.run_steps(3);

        let short_name = harness.get_by_label("短名").rect();
        let long_name = harness.get_by_label("一个很长的主机显示名称").rect();
        let short_addr = harness.get_by_label("root@10.0.0.1:22").rect();
        let long_addr = harness.get_by_label("ubuntu@192.168.31.233:22022").rect();
        let password = harness.get_by_label("PASSWORD").rect();
        let key = harness.get_by_label("SSH KEY").rect();

        assert!((short_name.left() - long_name.left()).abs() < 1.0);
        assert!((short_name.left() - short_addr.left()).abs() < 1.0);
        assert!((long_name.left() - long_addr.left()).abs() < 1.0);
        assert!((password.right() - key.right()).abs() < 1.0);

        std::fs::remove_file(&config_path).ok();
    }

    /// 标签栏 ">_" 快捷按钮：点击弹出已保存主机列表，单击主机行直接发起
    /// 连接（隔离配置路径，不碰用户真实 hosts.toml）。
    #[test]
    fn ssh快捷按钮连接主机() {
        use kittest::Queryable;

        let config_path = test_config_path("ssh-quick");
        let config = HostConfig {
            theme: String::new(),
            hosts: vec![HostProfile {
                name: "快捷主机".into(),
                host: "127.0.0.1".into(),
                // 端口 9（discard）必然拒绝连接：只验证"发起连接"，
                // 不依赖测试 sshd。
                port: 9,
                user: "root".into(),
                auth: Auth::Password("x".into()),
            }],
            projects: Vec::new(),
        };
        config.save(&config_path).expect("写入测试配置失败");

        let mut harness = egui_kittest::Harness::new_eframe(|cc| {
            MinoApp::new_with_config(cc, config_path.clone())
        });
        harness.run_steps(6);

        // 点击 ">_" → 弹出主机菜单（主机名可见）。
        harness.get_by_label(">_").click();
        harness.run_steps(6);
        harness.get_by_label("快捷主机");

        // 单击主机行 → 直接发起连接（pending_label 记录目标主机）。
        harness.get_by_label("快捷主机").click();
        harness.run_steps(6);
        assert_eq!(
            harness.state().pending_label,
            "快捷主机",
            "单击主机行应直接发起连接"
        );
        // 弹出菜单应已关闭（user@host 行不再可见）。
        assert!(
            harness
                .root()
                .query_all_by_label("root@127.0.0.1")
                .next()
                .is_none(),
            "点击主机行后快捷菜单应关闭"
        );

        std::fs::remove_file(&config_path).ok();
    }

    /// 无已保存主机时，" >_" 快捷菜单应提示并引导新建连接。
    #[test]
    fn ssh快捷按钮无主机提示() {
        use kittest::Queryable;

        let config_path = test_config_path("ssh-quick-empty");
        let mut harness = egui_kittest::Harness::new_eframe(|cc| {
            MinoApp::new_with_config(cc, config_path.clone())
        });
        harness.run_steps(6);

        harness.get_by_label(">_").click();
        harness.run_steps(6);
        harness.get_by_label("暂无已保存主机");
        harness.get_by_label("新建连接");

        std::fs::remove_file(&config_path).ok();
    }

    /// 快捷菜单行：短名称与长名称左缘对齐（不因 add_sized 居中），
    /// 名称与 user@host 同一列。
    #[test]
    fn ssh快捷菜单行左对齐() {
        use kittest::Queryable;

        let config_path = test_config_path("ssh-quick-align");
        let config = HostConfig {
            theme: String::new(),
            hosts: vec![
                HostProfile {
                    name: "短名".into(),
                    host: "10.0.0.1".into(),
                    port: 9,
                    user: "root".into(),
                    auth: Auth::Password("x".into()),
                },
                HostProfile {
                    name: "很长的主机名称对齐".into(),
                    host: "192.168.31.233".into(),
                    port: 9,
                    user: "ubuntu".into(),
                    auth: Auth::Password("x".into()),
                },
            ],
            projects: Vec::new(),
        };
        config.save(&config_path).expect("写入测试配置失败");

        let mut harness = egui_kittest::Harness::new_eframe(|cc| {
            MinoApp::new_with_config(cc, config_path.clone())
        });
        harness.run_steps(6);
        harness.get_by_label(">_").click();
        harness.run_steps(6);

        let short = harness.get_by_label("短名").rect();
        let long = harness.get_by_label("很长的主机名称对齐").rect();
        let short_addr = harness.get_by_label("root@10.0.0.1").rect();
        let long_addr = harness.get_by_label("ubuntu@192.168.31.233").rect();

        assert!(
            (short.left() - long.left()).abs() < 1.0,
            "短名称与长名称应左对齐（曾被 add_sized 居中），短={:.1} 长={:.1}",
            short.left(),
            long.left()
        );
        assert!(
            (short.left() - short_addr.left()).abs() < 1.0,
            "名称与地址应同一列左对齐，名称={:.1} 地址={:.1}",
            short.left(),
            short_addr.left()
        );
        assert!(
            (long.left() - long_addr.left()).abs() < 1.0,
            "长名称与地址应同一列左对齐"
        );
        // 短名称若被居中，右缘会靠近/越过两列内容的水平中线。
        let menu_right = long.right().max(long_addr.right());
        let menu_center = (short.left() + menu_right) * 0.5;
        assert!(
            short.right() < menu_center,
            "短名称右缘应在内容中线左侧（居中时会越过中线），right={:.1} center={:.1}",
            short.right(),
            menu_center
        );

        std::fs::remove_file(&config_path).ok();
    }
}

#[cfg(test)]
mod project_tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn project_base(tag: &str) -> PathBuf {
        let base =
            std::env::temp_dir().join(format!("mino-proj-test-{}-{}", tag, std::process::id()));
        std::fs::remove_dir_all(&base).ok();
        base
    }

    /// 点击标签栏项目按钮（`>_` 左侧最近的 Button；标签栏顺序 ＋ → 项目 → `>_`）。
    fn open_project_menu(harness: &mut egui_kittest::Harness<MinoApp>) {
        use kittest::Queryable;
        let ssh_left = harness.get_by_label(">_").rect().left();
        harness
            .root()
            .query_all_by_role(accesskit::Role::Button)
            .filter(|n| n.rect().right() < ssh_left)
            .max_by(|a, b| a.rect().right().partial_cmp(&b.rect().right()).unwrap())
            .expect("项目按钮应在 >_ 左侧")
            .click();
        harness.run_steps(6);
    }

    /// 快捷菜单单击好路径行打开新本地标签；坏路径行只 toast、不建 tab。
    #[test]
    fn 项目菜单单击打开新标签() {
        use kittest::Queryable;
        let base = project_base("menu");
        let dir_a = base.join("alpha");
        std::fs::create_dir_all(&dir_a).unwrap();

        let config_path = test_config_path("projects-menu");
        let config = HostConfig {
            theme: String::new(),
            hosts: Vec::new(),
            projects: vec![
                ProjectProfile {
                    name: "项目甲".into(),
                    path: dir_a.clone(),
                    command: String::new(),
                },
                ProjectProfile {
                    name: "坏路径".into(),
                    path: base.join("gone"),
                    command: String::new(),
                },
            ],
        };
        config.save(&config_path).expect("写入测试配置失败");

        let mut harness = egui_kittest::Harness::new_eframe(|cc| {
            MinoApp::new_with_config(cc, config_path.clone())
        });
        harness.run_steps(6);
        let tabs_before = harness.state().tabs.len();

        open_project_menu(&mut harness);
        harness.get_by_label("项目甲");

        harness.get_by_label("项目甲").click();
        harness.run_steps(6);
        assert_eq!(
            harness.state().tabs.len(),
            tabs_before + 1,
            "单击项目行应打开新标签"
        );
        let active = harness.state().active_tab;
        assert!(
            !harness.state().tabs[active].terminal.session().is_remote(),
            "项目打开的应是本地标签"
        );

        open_project_menu(&mut harness);
        harness.get_by_label("坏路径").click();
        harness.run_steps(6);
        assert_eq!(
            harness.state().tabs.len(),
            tabs_before + 1,
            "坏路径项目不应新建标签"
        );
        let toast = harness.state().toast.as_ref().expect("应有错误提示");
        assert!(toast.is_error, "坏路径应为错误提示");
        assert!(
            toast.message.contains("项目目录不存在"),
            "错误提示应说明目录不存在：{}",
            toast.message
        );

        std::fs::remove_file(&config_path).ok();
        std::fs::remove_dir_all(&base).ok();
    }

    /// ⌘O 打开面板 → 输入过滤词只剩一项 → 回车打开新标签并关闭面板。
    #[test]
    fn 面板搜索过滤与回车打开() {
        use kittest::Queryable;
        let base = project_base("panel");
        let dir_a = base.join("alpha");
        let dir_b = base.join("beta");
        std::fs::create_dir_all(&dir_a).unwrap();
        std::fs::create_dir_all(&dir_b).unwrap();

        let config_path = test_config_path("projects-panel");
        let config = HostConfig {
            theme: String::new(),
            hosts: Vec::new(),
            projects: vec![
                ProjectProfile {
                    name: "项目甲".into(),
                    path: dir_a.clone(),
                    command: String::new(),
                },
                ProjectProfile {
                    name: "项目乙".into(),
                    path: dir_b.clone(),
                    command: String::new(),
                },
            ],
        };
        config.save(&config_path).expect("写入测试配置失败");

        let mut harness = egui_kittest::Harness::new_eframe(|cc| {
            MinoApp::new_with_config(cc, config_path.clone())
        });
        harness.run_steps(6);
        let tabs_before = harness.state().tabs.len();

        harness.event(egui::Event::Key {
            key: egui::Key::O,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::COMMAND,
        });
        harness.run_steps(6);
        assert!(harness.state().show_projects, "⌘O 应打开项目面板");
        harness.get_by_label("打开项目");

        harness.event(egui::Event::Text("甲".into()));
        harness.run_steps(6);
        harness.get_by_label("项目甲");
        assert!(
            harness.root().query_all_by_label("项目乙").next().is_none(),
            "过滤后项目乙不应再可见"
        );

        harness.event(egui::Event::Key {
            key: egui::Key::Enter,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        });
        harness.run_steps(6);
        assert!(!harness.state().show_projects, "回车打开后面板应关闭");
        assert_eq!(
            harness.state().tabs.len(),
            tabs_before + 1,
            "回车应打开选中项目的新标签"
        );

        std::fs::remove_file(&config_path).ok();
        std::fs::remove_dir_all(&base).ok();
    }

    /// ⌘D 收藏当前目录并落盘；再按一次去重，数量不变。
    #[test]
    fn 收藏当前目录并落盘() {
        let config_path = test_config_path("projects-bookmark");
        let mut harness = egui_kittest::Harness::new_eframe(|cc| {
            MinoApp::new_with_config(cc, config_path.clone())
        });
        harness.run_steps(6);

        harness.event(egui::Event::Key {
            key: egui::Key::D,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::COMMAND,
        });
        harness.run_steps(6);
        assert_eq!(
            harness.state().config.projects.len(),
            1,
            "⌘D 应收藏当前目录"
        );
        let home_canon = std::fs::canonicalize(std::env::var("HOME").expect("测试环境应有 HOME"))
            .expect("规范化 HOME 失败");
        assert_eq!(
            harness.state().config.projects[0].path,
            home_canon,
            "新建终端的当前目录应为 HOME"
        );
        let content = std::fs::read_to_string(&config_path).expect("配置应已落盘");
        assert!(
            content.contains(home_canon.to_string_lossy().as_ref()),
            "落盘 toml 应含收藏路径：{content}"
        );

        harness.event(egui::Event::Key {
            key: egui::Key::D,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::COMMAND,
        });
        harness.run_steps(6);
        assert_eq!(
            harness.state().config.projects.len(),
            1,
            "重复收藏同一目录不应新增"
        );

        std::fs::remove_file(&config_path).ok();
    }
    /// 回归（用户报告“cd 后收藏的还是启动目录”）：粘贴 `cd` 让输入跟踪
    /// `invalidate` 后，收藏必须用 shell 真正所在的目录（内核 cwd，与标题
    /// 同源），不能停在跟踪旧值。仅 macOS：Linux 无内核 cwd 接口。
    #[cfg(target_os = "macos")]
    #[test]
    fn 粘贴cd后收藏真实目录() {
        use std::time::{Duration, Instant};
        let base = project_base("bookmark-stale");
        let target = base.join("real");
        std::fs::create_dir_all(&target).unwrap();
        let config_path = test_config_path("projects-bookmark-stale");
        let mut harness = egui_kittest::Harness::new_eframe(|cc| {
            MinoApp::new_with_config(cc, config_path.clone())
        });
        harness.run_steps(6);
        harness.event(egui::Event::Text("printf __MINO_BOOKMARK_READY__".into()));
        harness.event(egui::Event::Key {
            key: egui::Key::Enter,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::default(),
        });
        let deadline = Instant::now() + Duration::from_secs(8);
        let mut ready = false;
        while Instant::now() < deadline {
            harness.step();
            let text = crate::views::terminal_view::tests_grid_text(
                harness.state().tabs[harness.state().active_tab]
                    .terminal
                    .session(),
            );
            if text.contains("__MINO_BOOKMARK_READY__") {
                ready = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(60));
        }
        assert!(ready, "zsh 未就绪");
        harness.event(egui::Event::Paste(format!("cd {}", target.display())));
        harness.event(egui::Event::Key {
            key: egui::Key::Enter,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::default(),
        });
        harness.run_steps(6);
        let expected = std::fs::canonicalize(&target).expect("规范化测试目录失败");
        let expected_text = expected.to_string_lossy().into_owned();
        let deadline = Instant::now() + Duration::from_secs(8);
        loop {
            harness.step();
            let dir = harness.state().tabs[harness.state().active_tab]
                .terminal
                .effective_local_directory();
            if dir.as_deref() == Some(expected_text.as_str()) {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "内核 cwd 未跟随到目标目录，当前：{dir:?}（期望 {expected_text}）"
            );
            std::thread::sleep(Duration::from_millis(60));
        }
        harness.state_mut().bookmark_current_directory();
        assert_eq!(harness.state().config.projects.len(), 1, "应收藏一项");
        assert_eq!(
            harness.state().config.projects[0].path,
            expected,
            "粘贴 cd 后收藏的应是真实目录"
        );
        std::fs::remove_file(&config_path).ok();
        std::fs::remove_dir_all(&base).ok();
    }

    /// 项目启动命令在打开后自动执行（终端输出出现标记）。
    #[test]
    fn 启动命令自动执行() {
        use kittest::Queryable;
        let base = project_base("cmd");
        let dir_a = base.join("alpha");
        std::fs::create_dir_all(&dir_a).unwrap();

        let config_path = test_config_path("projects-cmd");
        let config = HostConfig {
            theme: String::new(),
            hosts: Vec::new(),
            projects: vec![ProjectProfile {
                name: "命令项目".into(),
                path: dir_a.clone(),
                command: "echo MINO_PROJ_MARK".into(),
            }],
        };
        config.save(&config_path).expect("写入测试配置失败");

        let mut harness = egui_kittest::Harness::new_eframe(|cc| {
            MinoApp::new_with_config(cc, config_path.clone())
        });
        harness.run_steps(6);

        open_project_menu(&mut harness);
        harness.get_by_label("命令项目").click();
        let deadline = Instant::now() + Duration::from_secs(8);
        let mut found = false;
        while Instant::now() < deadline {
            harness.step();
            let text = crate::views::terminal_view::tests_grid_text(
                harness.state().tabs[harness.state().active_tab]
                    .terminal
                    .session(),
            );
            if text.contains("MINO_PROJ_MARK") {
                found = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(60));
        }
        assert!(found, "启动命令未在终端输出中出现");

        std::fs::remove_file(&config_path).ok();
        std::fs::remove_dir_all(&base).ok();
    }
}

#[cfg(test)]
mod project_manage_tests {
    use super::*;
    use kittest::NodeT as _;
    #[test]
    fn 设置项目管理新增并落盘() {
        use kittest::Queryable;
        let config_path = test_config_path("projects-manage");
        let mut harness = egui_kittest::Harness::new_eframe(|cc| {
            MinoApp::new_with_config(cc, config_path.clone())
        });
        harness.run_steps(6);

        harness.event(egui::Event::Key {
            key: egui::Key::Comma,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::COMMAND,
        });
        harness.run_steps(6);
        harness.get_by_label("项目管理");

        macro_rules! find_button {
            ($label:expr) => {
                harness
                    .root()
                    .query_all_by_role(accesskit::Role::Button)
                    .find(|n| n.accesskit_node().label() == Some($label.to_string()))
                    .unwrap_or_else(|| panic!("找不到按钮：{}", $label))
            };
        }
        find_button!("新增项目").click();
        harness.run_steps(6);
        harness.get_by_label("使用当前终端目录");

        // 表单输入框顺序：名称、路径、启动命令。
        let inputs: Vec<_> = harness
            .root()
            .query_all_by_role(accesskit::Role::TextInput)
            .collect();
        assert!(inputs.len() >= 3, "新增表单应有名称/路径/命令输入框");
        inputs[0].click();
        harness.run_steps(2);
        harness.event(egui::Event::Text("管理项目".into()));
        harness.run_steps(2);

        find_button!("使用当前终端目录").scroll_to_me();
        harness.run_steps(3);
        find_button!("使用当前终端目录").click();
        harness.run_steps(3);
        find_button!("保存").click();
        harness.run_steps(6);

        assert_eq!(
            harness.state().config.projects.len(),
            1,
            "保存后应新增一个项目"
        );
        let home_canon = std::fs::canonicalize(std::env::var("HOME").expect("测试环境应有 HOME"))
            .expect("规范化 HOME 失败");
        assert_eq!(harness.state().config.projects[0].name, "管理项目");
        assert_eq!(harness.state().config.projects[0].path, home_canon);
        let content = std::fs::read_to_string(&config_path).expect("配置应已落盘");
        assert!(
            content.contains("管理项目"),
            "落盘 toml 应含新项目：{content}"
        );

        std::fs::remove_file(&config_path).ok();
    }
}
