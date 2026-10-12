//! Mino 应用主体：布局、连接管理与状态。
//!
//! 视觉参照 Tabby 终端（tabby-core theme.vars.scss）：分层深色背景、终端绿与琥珀强调、
//! 扁平控件、标签页底部指示条与扫光动效（动效细节见 `crate::anim`）。

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::{Duration, Instant};

use eframe::egui;
use mino_core::config::{HostConfig, HostProfile, Workspace};
use mino_core::ssh::sftp::{connect_sftp_with_handler, SftpEvent, SftpHandle};
use mino_core::ssh::{connect_remote_with_cancel, ConnectCancel, ConnectResult};
use mino_core::terminal::{Session, SessionEvent, SessionOptions};
use mino_core::updater::{check_for_update, UpdateInfo};
use tokio::sync::mpsc::{Receiver, UnboundedReceiver};

use crate::anim;
use crate::dialog;
use crate::views::pane_tree::{self, PaneNode, SplitAxis};
use crate::views::sftp_view::SftpView;
use crate::views::terminal_view::TerminalView;

/// 应用对外展示名称。
pub const PRODUCT_NAME: &str = "Mino";

// ==================== Kaku 窗口样板常量 ====================
/// 标题条高度（Kaku/WezTerm 集成标题栏：交通灯条，与终端同底色）。
const TITLEBAR_HEIGHT: f32 = 28.0;
/// 底部标签栏高度（Kaku/WezTerm 非 fancy 标签栏 ≈ 字号 × 1.6 + 内边距）。
const TAB_BAR_HEIGHT: f32 = 30.0;
/// 标签标题字号。
const TAB_TITLE_SIZE: f32 = 12.5;
/// 标签左右内边距（左给标题、右给关闭位）。
const TAB_PADDING_L: f32 = 12.0;
const TAB_PADDING_R: f32 = 4.0;
/// 标签关闭位（「×」出现时占用的宽度）。
const TAB_CLOSE_WIDTH: f32 = 20.0;
/// 标签宽度夹取范围（上限 ≈ Kaku `tab_max_width = 48` 列）。
const TAB_MIN_WIDTH: f32 = 84.0;
const TAB_MAX_WIDTH: f32 = 320.0;
/// 「＋」新建标签按钮宽度。
const NEW_TAB_WIDTH: f32 = 34.0;
/// 分屏窗格间隙（点）。Kaku `split_pane_gap = 2`（WezTerm cells 语义）的
/// 视觉等效值：窗格底色相同，间隙只提供呼吸空间。
const PANE_GAP: f32 = 10.0;

/// 工作区编辑表单状态（本地目录 / SSH 主机二选一；新增或编辑既有条目）。
struct WorkspaceEdit {
    /// 编辑既有工作区的下标；None = 新增。
    index: Option<usize>,
    /// 0 = 本地目录，1 = SSH 主机。
    kind: usize,
    name: String,
    // 本地目录
    path: String,
    command: String,
    // SSH
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

impl Default for WorkspaceEdit {
    fn default() -> Self {
        Self {
            index: None,
            kind: 1,
            name: String::new(),
            path: std::env::var("HOME").unwrap_or_else(|_| "~".into()),
            command: String::new(),
            host: String::new(),
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

impl WorkspaceEdit {
    /// 用既有工作区填充表单（编辑入口）。
    fn from_workspace(workspace: &Workspace) -> Self {
        let mut edit = WorkspaceEdit {
            name: workspace.name().to_string(),
            command: workspace.command().to_string(),
            ..WorkspaceEdit::default()
        };
        match workspace {
            Workspace::Local { path, .. } => {
                edit.kind = 0;
                edit.path = path.display().to_string();
            }
            Workspace::Ssh {
                host,
                port,
                user,
                auth,
                path,
                ..
            } => {
                edit.kind = 1;
                edit.host = host.clone();
                edit.port = port.to_string();
                edit.user = user.clone();
                edit.path = path.clone();
                match auth {
                    mino_core::config::Auth::Password(password) => {
                        edit.auth_kind = 0;
                        edit.password = password.clone();
                    }
                    mino_core::config::Auth::Key { path, passphrase } => {
                        edit.auth_kind = 1;
                        edit.key_path = path.display().to_string();
                        edit.passphrase = passphrase.clone().unwrap_or_default();
                    }
                }
            }
        }
        edit
    }

    /// 表单 → 工作区（含必填校验；错误信息为中文提示）。
    fn to_workspace(&self) -> Result<Workspace, String> {
        if self.kind == 0 {
            let path = self.path.trim();
            if path.is_empty() {
                return Err("请填写目录路径".into());
            }
            let path = PathBuf::from(path);
            let name = if self.name.trim().is_empty() {
                path.file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| path.to_string_lossy().into_owned())
            } else {
                self.name.trim().to_string()
            };
            Ok(Workspace::Local {
                name,
                path,
                command: self.command.trim().to_string(),
            })
        } else {
            let host = self.host.trim();
            if host.is_empty() {
                return Err("请填写主机地址".into());
            }
            let port_wrong =
                !self.port.trim().is_empty() && self.port.trim().parse::<u16>().is_err();
            let port = if port_wrong {
                return Err("端口必须是 1-65535 的数字".into());
            } else {
                self.port.trim().parse::<u16>().unwrap_or(22)
            };
            let user = if self.user.trim().is_empty() {
                "root".to_string()
            } else {
                self.user.trim().to_string()
            };
            let auth = if self.auth_kind == 0 {
                mino_core::config::Auth::Password(self.password.clone())
            } else {
                let key_path = self.key_path.trim();
                if key_path.is_empty() {
                    return Err("请填写私钥路径".into());
                }
                mino_core::config::Auth::Key {
                    path: PathBuf::from(key_path),
                    passphrase: (!self.passphrase.is_empty()).then(|| self.passphrase.clone()),
                }
            };
            let name = if self.name.trim().is_empty() {
                host.to_string()
            } else {
                self.name.trim().to_string()
            };
            Ok(Workspace::Ssh {
                name,
                host: host.to_string(),
                port,
                user,
                auth,
                path: self.path.trim().to_string(),
                command: self.command.trim().to_string(),
            })
        }
    }
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

/// 单个窗格（分屏后的一个终端视图；每个窗格一个独立会话）。
pub struct Pane {
    /// 窗格稳定身份（布局树叶子 id；与标签共用同一 id 分配器）。
    id: u64,
    view: TerminalView,
}

/// 会话来源：分屏时新窗格继承同源会话（本地 → 本地、SSH → 同主机重连），
/// 快照恢复也按它重建。
#[derive(Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
enum SessionOrigin {
    /// 本地 shell（分屏的新窗格以活动窗格当前目录启动）。
    Local,
    /// SSH 会话（分屏重连同主机）。
    Remote(HostProfile),
}

/// 以 0600 原子写一份 TOML（快照等含凭据的文件；与 `HostConfig::save`
/// 同一套安全约定：临时文件 + rename + 失败清理）。
fn save_private_toml<T: serde::Serialize>(
    value: &T,
    path: &std::path::Path,
) -> std::io::Result<()> {
    let content = toml::to_string_pretty(value)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension(format!("toml.tmp-{}", std::process::id()));
    let result = (|| {
        use std::io::Write;
        #[cfg(unix)]
        let mut file = {
            use std::os::unix::fs::OpenOptionsExt;
            std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&tmp)?
        };
        #[cfg(not(unix))]
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(content.as_bytes())?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&tmp, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// Codex 配置目录（`~/.codex`；`auth_type = "codex"` 时读它）。
fn codex_home_path() -> std::path::PathBuf {
    std::env::var("HOME")
        .map(|home| PathBuf::from(home).join(".codex"))
        .unwrap_or_else(|_| PathBuf::from(".codex"))
}

/// AI 助手简单模型名（`model`；空则回退 `chat_model`；都空则未配置）。
fn ai_simple_model(config: &mino_core::ai::AssistantConfig) -> Option<String> {
    let model = if !config.model.is_empty() {
        &config.model
    } else {
        &config.chat_model
    };
    (!model.is_empty()).then(|| model.clone())
}

/// 当前目录的 git 分支（不在仓库/无 git 时 None；供提示词上下文用）。
fn git_branch_for(cwd: &str) -> Option<String> {
    let output = std::process::Command::new("git")
        .args(["-C", cwd, "rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let branch = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!branch.is_empty()).then_some(branch)
}

/// toast 用的一行摘要（截到 72 个字符，换行折叠为空格）。
fn truncate_for_toast(text: &str) -> String {
    let mut out: String = text.chars().take(72).collect();
    if text.chars().count() > 72 {
        out.push('…');
    }
    out.replace('\n', " ")
}

/// 失败命令修复的后台作业（阻塞网络调用；返回 None 表示跳过/失败）。
fn ai_fix_worker(
    config: &mino_core::ai::AssistantConfig,
    command: &str,
    exit_code: i32,
    cwd: &str,
    tail: &str,
    codex_home: &std::path::Path,
) -> Option<mino_core::ai::FixSuggestion> {
    let conn = config.resolve_connection(codex_home).ok()?;
    let model = ai_simple_model(config)?;
    let branch = git_branch_for(cwd);
    let context = mino_core::ai::FixContext {
        failed_command: command,
        exit_code,
        cwd,
        git_branch: branch.as_deref(),
        tail_output: tail,
    };
    let cancel = std::sync::atomic::AtomicBool::new(false);
    mino_core::ai::suggest_fix(&conn, &model, &context, &cancel).ok()
}

/// `#` 命令生成的后台作业（错误已转中文文案）。
fn ai_generate_worker(
    config: &mino_core::ai::AssistantConfig,
    query: &str,
    cwd: &str,
    tail: &str,
    codex_home: &std::path::Path,
) -> Result<String, String> {
    let conn = config
        .resolve_connection(codex_home)
        .map_err(|e| format!("{e}"))?;
    let model =
        ai_simple_model(config).ok_or_else(|| "未配置模型（设置 → AI 助手）".to_string())?;
    let branch = git_branch_for(cwd);
    let cancel = std::sync::atomic::AtomicBool::new(false);
    mino_core::ai::suggest_command(&conn, &model, query, cwd, &branch, tail, &cancel)
        .map_err(|e| format!("{e}"))
}

/// AI 失败修复的结果（挂到具体窗格；⌘⇧E 回填）。
struct AiFixOutcome {
    tab_id: u64,
    pane_id: u64,
    suggestion: mino_core::ai::FixSuggestion,
}

/// AI `#` 命令生成的结果（挂到具体窗格；成功即回填提示符）。
struct AiGenerateOutcome {
    tab_id: u64,
    pane_id: u64,
    result: Result<String, String>,
}

/// Launcher 覆盖层的两种形态（决定每帧是否同步数据源）。
#[derive(Clone, Copy, PartialEq, Eq)]
enum LauncherMode {
    /// 标签导航器（⌘⇧O）。
    Tabs,
    /// 命令面板（⌘⇧P）。
    Palette,
}

/// 已关闭标签的重开信息（Kaku `Cmd+Shift+T`）。
struct ClosedTab {
    title: String,
    origin: SessionOrigin,
    /// 本地标签关闭时的当前目录（重开用它恢复工作目录）。
    dir: Option<PathBuf>,
}

/// 重开栈上限（超出丢弃最旧；Kaku 同款"最近若干次"语义）。
const CLOSED_TAB_STACK: usize = 12;

/// 窗口快照（Kaku「Restore Previous Window」：重开上次的全部标签）。
///
/// 快照里含 SSH 认证信息（与 hosts.toml 同级的明文口令/私钥口令），因此
/// 落盘必须 0600；损坏/缺失一律按「没有快照」处理，不阻塞启动。
#[derive(Default, serde::Serialize, serde::Deserialize)]
struct WindowSnapshot {
    #[serde(default)]
    tabs: Vec<SnapshotTab>,
}

/// 单个快照标签：本地记目录，远程记主机档案与远端目录。
#[derive(serde::Serialize, serde::Deserialize)]
struct SnapshotTab {
    origin: SessionOrigin,
    #[serde(default)]
    dir: Option<PathBuf>,
}

/// 分屏挂载目标：新会话要挂进哪个标签的哪个窗格、以哪个方向拆分。
#[derive(Clone, Copy)]
struct SplitTarget {
    tab_id: u64,
    pane_id: u64,
    axis: SplitAxis,
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

/// 单个终端标签页（本地或 SSH；可含多个分屏窗格）。
pub struct TerminalTab {
    /// 标签稳定身份，不随 Vec 中的插入、删除或移动变化。
    id: u64,
    /// 兜底标题（远程标签 = 主机名；目录未知时也用它）。
    label: String,
    /// 分屏窗格集合（至少一个）。
    panes: Vec<Pane>,
    /// 布局树；叶子 id 与 `panes[].id` 一一对应。
    layout: PaneNode,
    /// 活动窗格 id（键盘输入、标题、SFTP 定位都以它为上下文）。
    active_pane: u64,
    /// 放大显示的窗格（Kaku `Cmd+Shift+Enter` = TogglePaneZoomState）。
    zoomed: Option<u64>,
    /// 最近一帧各窗格的屏幕矩形（渲染时写入；窗格导航/比例调整读它，
    /// 保证几何判断与真实渲染同源）。
    pane_rects: Vec<(u64, egui::Rect)>,
    /// 用户双击重命名后的标签标题（None = 跟随目录/主机）。
    title_override: Option<String>,
    /// 会话来源（分屏继承、快照恢复用）。
    origin: SessionOrigin,
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

impl TerminalTab {
    fn new(
        id: u64,
        label: String,
        view: TerminalView,
        pane_id: u64,
        origin: SessionOrigin,
    ) -> Self {
        Self {
            id,
            label,
            panes: vec![Pane { id: pane_id, view }],
            layout: pane_tree::leaf(pane_id),
            active_pane: pane_id,
            zoomed: None,
            pane_rects: Vec::new(),
            title_override: None,
            origin,
            sftp: None,
            sftp_open: false,
            locate_pending: None,
        }
    }

    /// 活动窗格视图。
    fn active_view(&self) -> &TerminalView {
        self.view(self.active_pane).unwrap_or(&self.panes[0].view)
    }

    /// 活动窗格视图（可变）。
    fn active_view_mut(&mut self) -> &mut TerminalView {
        let id = self.active_pane;
        let index = self
            .panes
            .iter()
            .position(|pane| pane.id == id)
            .unwrap_or(0);
        &mut self.panes[index].view
    }

    /// 按窗格 id 取视图。
    fn view(&self, pane_id: u64) -> Option<&TerminalView> {
        self.panes
            .iter()
            .find(|pane| pane.id == pane_id)
            .map(|pane| &pane.view)
    }

    /// 在 `near` 窗格旁按 `axis` 加入一个新窗格并聚焦（分屏挂载）。
    ///
    /// 找不到 `near`（布局与集合不同步）时拒绝：宁可不分屏也不留下
    /// 树里没有的孤儿窗格。
    fn add_pane(&mut self, pane_id: u64, view: TerminalView, near: u64, axis: SplitAxis) -> bool {
        if !pane_tree::split_leaf(&mut self.layout, near, pane_id, axis) {
            return false;
        }
        self.panes.push(Pane { id: pane_id, view });
        self.active_pane = pane_id;
        self.zoomed = None;
        true
    }

    /// 标签标题：`(显示文本, 全路径悬浮提示)`。
    ///
    /// - 用户重命名过：用自定义标题；
    /// - 多窗格：各窗格目录名以 `∙` 去重连接（Kaku 多窗格标签同款），
    ///   悬浮提示给出各自全路径；
    /// - 本地单窗格：当前目录的**末级文件夹名**（与 zsh `%c` 一致，home
    ///   显示 `~`）+ 全路径提示。目录以 shell 子进程的内核 cwd 为准
    ///   （`Session::child_current_dir`）；**不**采用 shell 上报的窗口标题
    ///   （oh-my-zsh 的标题是截断过的 `%15<..<%~%<<`，拿不到完整路径）。
    /// - 远程标签：主机名（远端目录由 sshd 决定、本地跟踪器不适用）。
    fn title(&self) -> (String, Option<String>) {
        if let Some(custom) = &self.title_override {
            return (custom.clone(), None);
        }
        let dirs: Vec<(String, String)> = self
            .panes
            .iter()
            .filter_map(|pane| {
                pane.view
                    .effective_local_directory()
                    .map(|full| (dir_display_name(&full), full))
            })
            .collect();
        if dirs.len() >= 2 {
            let mut seen: Vec<String> = Vec::new();
            for (name, _) in &dirs {
                if !seen.contains(name) {
                    seen.push(name.clone());
                }
            }
            let tooltip = dirs
                .iter()
                .map(|(_, full)| full.clone())
                .collect::<Vec<_>>()
                .join("\n");
            return (seen.join("∙"), Some(tooltip));
        }
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
        let full = self.active_view().effective_local_directory()?;
        Some((dir_display_name(&full), full))
    }
}

/// 绘制一行工作区条目（头像 + 名称 + 类型/地址 + 选中/悬停底），返回行点击响应。
///
/// 三处共用（快捷菜单、⌘O 面板、设置卡片）保证观感与命中行为一致；
/// 行交互用显式 `ui.interact(row_rect, 稳定 Id, Sense::click())` 注册在内容
/// 之后（egui 响应链：后注册者在上层，参考 SFTP 行的既有做法）。
fn paint_workspace_row(
    ui: &mut egui::Ui,
    workspace: &Workspace,
    selected: bool,
    row_id: egui::Id,
    row_height: f32,
    avatar: f32,
) -> egui::Response {
    let theme = crate::theme::current_theme();
    let width = ui.available_width();
    let (row_rect, _) = ui.allocate_exact_size(egui::vec2(width, row_height), egui::Sense::hover());
    let highlight = row_rect.shrink2(egui::vec2(4.0, 1.0));
    let hovered = ui.rect_contains_pointer(highlight);
    if selected {
        ui.painter().rect_filled(
            highlight,
            crate::theme::tokens::RADIUS_ITEM,
            theme.accent_soft,
        );
    } else if hovered {
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
    inner.add_space(10.0);
    let (avatar_rect, _) =
        inner.allocate_exact_size(egui::vec2(avatar, avatar), egui::Sense::hover());
    let initial = workspace.name().chars().next().unwrap_or('?');
    dialog::paint_avatar(inner.painter(), avatar_rect, initial, theme, selected);
    // 类型徽标（本地 / SSH）：小圆角片，颜色区分两类工作区。
    let kind_color = if workspace.is_remote() {
        theme.accent2
    } else {
        theme.accent
    };
    let kind_label = if workspace.is_remote() {
        "SSH"
    } else {
        "本地"
    };
    inner.add_space(8.0);
    let (badge_rect, _) = inner.allocate_exact_size(egui::vec2(34.0, 16.0), egui::Sense::hover());
    inner.painter().rect_filled(
        badge_rect,
        3.0,
        egui::Color32::from_rgba_unmultiplied(kind_color.r(), kind_color.g(), kind_color.b(), 40),
    );
    let mut badge = inner.new_child(
        egui::UiBuilder::new()
            .id_salt(row_id.with("badge"))
            .max_rect(badge_rect)
            .layout(egui::Layout::centered_and_justified(
                egui::Direction::TopDown,
            )),
    );
    badge.label(egui::RichText::new(kind_label).size(9.5).color(kind_color));
    inner.add_space(8.0);
    let text_width = (row_rect.width() - 10.0 * 2.0 - avatar - 34.0 - 8.0 * 2.0).max(80.0);
    let text_rect = inner
        .allocate_exact_size(egui::vec2(text_width, 30.0), egui::Sense::hover())
        .0;
    let mut text = inner.new_child(
        egui::UiBuilder::new()
            .id_salt(row_id.with("text"))
            .max_rect(text_rect)
            .layout(egui::Layout::top_down(egui::Align::Min)),
    );
    text.spacing_mut().item_spacing.y = 1.0;
    text.add_space(2.0);
    text.add(
        egui::Label::new(
            egui::RichText::new(workspace.name())
                .size(12.5)
                .color(theme.text_primary),
        )
        .truncate(),
    );
    text.add(
        egui::Label::new(
            egui::RichText::new(workspace.detail())
                .size(10.5)
                .color(theme.text_secondary),
        )
        .truncate(),
    );
    ui.interact(row_rect, row_id.with("click"), egui::Sense::click())
        .on_hover_cursor(egui::CursorIcon::PointingHand)
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
    /// 当前等待中的连接若为分屏用途，目标窗格信息（None = 建新标签）。
    pending_split: Option<SplitTarget>,
    /// 当前等待中的连接主机档案（分屏/新标签挂载时写入会话来源）。
    pending_profile: Option<HostProfile>,
    /// 标签与连接身份分配器。
    next_id: u64,
    /// 设置弹窗是否打开（`⌘,` 或齿轮按钮切换；Esc/× 关闭）。
    show_settings: bool,
    /// 项目打开面板是否打开（`⌘O` 切换；Esc/回车关闭）。
    show_workspaces: bool,
    /// 项目搜索过滤词（快捷菜单与 ⌘O 面板共用）。
    workspace_filter: String,
    /// 过滤后项目列表的选中下标（⌘O 面板上下键导航用）。
    workspace_selected: usize,
    /// 最近一帧的 egui::Context（`new_local_tab` 等非 UI 闭包内构造时使用）。
    last_ctx: egui::Context,
    config: HostConfig,
    config_path: PathBuf,
    /// 原配置无法读取且备份也失败时，禁止用空配置覆盖原文件。
    config_write_blocked: bool,
    /// 新建连接弹窗关闭后是否恢复此前被其遮住的设置窗口。
    settings_before_workspace_edit: bool,
    /// 工作区编辑表单（`None` = 对话框关闭；新增/编辑共用同一表单）。
    workspace_edit: Option<WorkspaceEdit>,
    /// AI 聊天面板（⌘L，Kaku 同款；输入门禁在渲染处统一处理）。
    ai_panel: crate::views::ai_panel::AiPanel,
    /// Launcher 覆盖层：标签导航（⌘⇧O）或命令面板（⌘⇧P），互斥。
    launcher: Option<(LauncherMode, crate::views::launcher::Launcher)>,
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
    /// 终端自管 GPU 渲染资源（`None` = 无 wgpu 后端，走 egui Shape 路径）。
    terminal_gpu: Option<std::sync::Arc<crate::views::terminal_gpu::TerminalGpu>>,
    /// 中文 fallback 字体后台加载器（生产路径由 `main` 注入；测试为空，
    /// 中文由需要它的测试自己 `setup_fonts` + `wait_ready`）。
    cjk_fonts: Option<crate::CjkFontLoader>,
    /// 后台创建中的本地终端会话（PTY fork + shell 启动不再堵住首帧）。
    pending_local: Option<PendingLocalSpawn>,
    /// 已关闭标签的重开栈（Kaku `Cmd+Shift+T`；上限 `CLOSED_TAB_STACK`）。
    closed_tabs: Vec<ClosedTab>,
    /// 正在重命名的标签 `(标签 id, 编辑缓冲)`（双击标签标题进入，Kaku 同款）。
    renaming_tab: Option<(u64, String)>,
    /// `~/.zshrc` 路径（测试用隔离路径；shell 集成的受管块写入目标）。
    zshrc_path: PathBuf,
    /// 进行中的 AI 失败修复请求（单飞：新失败覆盖旧请求的结果展示）。
    ai_fix_rx: Option<std::sync::mpsc::Receiver<Option<AiFixOutcome>>>,
    /// 进行中的 `#` 命令生成请求。
    ai_generate_rx: Option<std::sync::mpsc::Receiver<Option<AiGenerateOutcome>>>,
    /// 最近一次失败修复建议（⌘⇧E 应用到对应窗格；应用后清空）。
    ai_fix_suggestion: Option<AiFixOutcome>,
    /// Shell 集成插件后台下载的结果通道。
    vendor_rx: Option<
        std::sync::mpsc::Receiver<Vec<Result<mino_core::shell_integration::Plugin, String>>>,
    >,
    /// 插件下载进行中（按钮状态与重复点击防护）。
    vendor_running: bool,
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
    /// 分屏挂载目标；None = 作为新标签挂载。
    target: Option<SplitTarget>,
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
            // 终端身份（Kaku 用 TERM_PROGRAM=Kaku 同款）：shell 集成脚本按
            // `TERM_PROGRAM == "Mino"` / `MINO_SHELL_INTEGRATION=1` 守卫，
            // 只在 Mino 会话里加载 zsh 集成与 Smart Tab，其它终端不受影响。
            ("TERM_PROGRAM".to_string(), "Mino".to_string()),
            ("MINO_SHELL_INTEGRATION".to_string(), "1".to_string()),
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
    // 与终端内容内边距一致（Kaku 的 40 设备像素），悬浮于终端区域右上角。
    let pad = crate::views::terminal_view::padding_points(ui.ctx().pixels_per_point());
    let btn_rect = egui::Rect::from_min_size(
        egui::pos2(area.right() - pad - btn_size.x, area.top() + pad),
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

/// 标签栏「工作区」按钮（SSH 与项目收藏的融合入口）。
///
/// 22×22 纯矢量图标（三个堆叠的圆角方块 = 集合/工作区），风格与齿轮一致：
/// 次要色描边、hover 白 8% 圆角底、无 unicode 依赖（缺字形会变方块）。
/// 点击弹出统一工作区菜单（`Popup::menu` 自行管理开关状态，Id 需稳定）。
fn workspace_quick_button(ui: &mut egui::Ui) -> egui::Response {
    let theme = crate::theme::current_theme();
    let btn_size = 22.0;
    let btn = egui::Button::new("")
        .fill(egui::Color32::TRANSPARENT)
        .stroke(egui::Stroke::NONE)
        .min_size(egui::vec2(btn_size, btn_size));
    let response = ui
        .add(btn)
        .on_hover_cursor(egui::CursorIcon::PointingHand)
        .on_hover_text("工作区：本地目录与 SSH 主机（⌘O）");
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
        for (offset_y, size) in [
            (4.0_f32, egui::vec2(12.0, 4.0)),
            (0.5, egui::vec2(8.5, 3.5)),
            (-3.0, egui::vec2(5.0, 3.0)),
        ] {
            let block = egui::Rect::from_center_size(center + egui::vec2(0.0, offset_y), size);
            ui.painter().rect_stroke(
                block,
                1.5,
                egui::Stroke::new(1.3, icon_color),
                egui::StrokeKind::Inside,
            );
        }
    }
    response
}

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

        // shell 集成的受管块目标：生产写用户 `~/.zshrc`；测试写到隔离配置
        // 同目录（测试绝不能改用户的真实 shell 配置——与主机配置隔离同理）。
        let zshrc_path = {
            #[cfg(test)]
            {
                config_path.with_extension("zshrc")
            }
            #[cfg(not(test))]
            {
                let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
                PathBuf::from(home).join(".zshrc")
            }
        };

        // 外观恢复：`auto` 跟随系统、`dark`/`light` 强制；旧配置的主题名
        // （「深色/深蓝/霓虹」）解析为 auto（`resolve_appearance` 语义）。
        // 字号恢复：老默认 13 视为“未设置”，提升到 Kaku 基准（高分屏 17 /
        // 低分屏 15——Kaku `get_font_size`）；用户显式调过的其它值保留并钳制。
        let mut config = config;
        let system_dark = ctx.system_theme() != Some(egui::Theme::Light);
        crate::theme::set_theme(
            &ctx,
            crate::theme::resolve_appearance(config.appearance.trim(), system_dark),
        );
        if (config.font_size - 13.0).abs() < 0.01 {
            config.font_size = if ctx.pixels_per_point() <= 1.0 {
                15.0
            } else {
                17.0
            };
        }
        config.font_size = config
            .font_size
            .clamp(TerminalView::MIN_FONT_SIZE, TerminalView::MAX_FONT_SIZE);

        let mut app = Self {
            tabs: Vec::new(),
            active_tab: 0,
            pending_tab: None,
            pending_connection_id: None,
            pending_split: None,
            pending_profile: None,
            next_id: 1,
            show_settings: false,
            show_workspaces: false,
            workspace_filter: String::new(),
            workspace_selected: 0,
            last_ctx: cc.egui_ctx.clone(),
            config,
            config_path,
            config_write_blocked,
            settings_before_workspace_edit: false,
            workspace_edit: None,
            ai_panel: crate::views::ai_panel::AiPanel::new(),
            launcher: None,
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
            terminal_gpu,
            cjk_fonts: None,
            pending_local: None,
            closed_tabs: Vec::new(),
            renaming_tab: None,
            zshrc_path: zshrc_path.clone(),
            ai_fix_rx: None,
            ai_generate_rx: None,
            ai_fix_suggestion: None,
            vendor_rx: None,
            vendor_running: false,
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
        // 首次运行：安装 Shell 集成（Kaku 的 first-run 体验：受管块幂等可
        // 卸载，插件后台补下载；已安装则零动作、不再打扰）。
        #[cfg(not(test))]
        {
            if !mino_core::shell_integration::is_installed(&app.zshrc_path) {
                match mino_core::shell_integration::install(
                    &app.zshrc_path,
                    app.config_path
                        .parent()
                        .unwrap_or_else(|| std::path::Path::new(".")),
                ) {
                    Ok(_) => {
                        app.show_toast("已安装 Shell 集成（zsh）：重启终端后生效", false);
                        app.start_vendor_bootstrap(&ctx);
                    }
                    Err(e) => {
                        log::warn!("Shell 集成安装失败：{e}");
                    }
                }
            }
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

    /// 打开工作区（本地目录 → 本地标签；SSH → 远程连接；名称即标签标题）。
    ///
    /// 本地目录不存在（被删/移动/外接盘拔出）时只 toast，不建标签。
    fn open_workspace(&mut self, ctx: &egui::Context, workspace: &Workspace) {
        match workspace {
            Workspace::Local { path, command, .. } => {
                if !path.is_dir() {
                    self.show_toast(format!("目录不存在：{}", path.display()), true);
                    return;
                }
                self.new_local_tab_at(ctx, Some(path.clone()), command);
            }
            Workspace::Ssh { .. } => {
                if let Some(profile) = workspace.to_host_profile() {
                    self.start_connect_into(ctx, profile, None);
                }
            }
        }
    }

    /// 打开工作区编辑表单（新增：`kind` 预选 0 本地 / 1 SSH；已有连接流程
    /// 默认 SSH，保持「⌘N = 新建连接」的肌肉记忆）。
    fn open_workspace_editor(&mut self, kind: usize) {
        let edit = WorkspaceEdit {
            kind,
            ..WorkspaceEdit::default()
        };
        // 工作区编辑弹窗是前台模态：从设置里的入口打开时先收起设置。
        self.settings_before_workspace_edit = self.show_settings;
        self.show_settings = false;
        self.workspace_edit = Some(edit);
    }

    /// 带目录与启动命令的本地标签构造（`new_local_tab` 与 `open_project` 共用）。
    ///
    /// 生产构建在后台线程创建会话（PTY fork + shell 启动/oh-my-zsh 初始化
    /// 可达数百毫秒，同步做会推迟首帧、也让 ⌘T 明显卡顿），本帧只登记
    /// `pending_local`，就绪后由 `poll_local_spawn` 挂上标签。
    fn new_local_tab_at(&mut self, ctx: &egui::Context, dir: Option<PathBuf>, command: &str) {
        self.spawn_local_into(ctx, dir, command, None);
    }

    /// 启动一个本地会话并挂到 `target`（None = 新标签；Some = 目标窗格旁分屏）。
    fn spawn_local_into(
        &mut self,
        ctx: &egui::Context,
        dir: Option<PathBuf>,
        command: &str,
        target: Option<SplitTarget>,
    ) {
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
                Ok(session) => self.mount_local_session(ctx, session, &command, target),
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
            self.pending_local = Some(PendingLocalSpawn {
                rx,
                command,
                target,
            });
            // 让"正在启动终端…"占位立刻可见。
            ctx.request_repaint();
        }
    }

    /// 把已创建的本地会话挂上标签栏 / 分屏（异步与同步路径共用）。
    fn mount_local_session(
        &mut self,
        ctx: &egui::Context,
        session: Session,
        command: &str,
        target: Option<SplitTarget>,
    ) {
        let mut view = TerminalView::new(session);
        view.set_gpu(self.terminal_gpu.clone());
        // 新标签继承当前全局字号（启动恢复/快捷键/滑杆都只改 `config` +
        // 存量标签；新建标签必须跟上，否则新旧标签字不一样大）。
        view.set_font_size(self.config.font_size);
        // 启动命令只取首个非空行：多行粘贴会被 shell 逐行执行，
        // 配置里换行只可能是误粘贴，不应多行注入。
        if let Some(line) = command.lines().map(str::trim).find(|l| !l.is_empty()) {
            view.session().write(format!("{line}\n").as_bytes());
        }
        // 分屏目标：把新会话挂进已有标签的目标窗格旁。
        if let Some(target) = target {
            let pane_id = self.allocate_id();
            let mut view_slot = Some(view);
            let mounted = match self.tabs.iter_mut().find(|tab| tab.id == target.tab_id) {
                Some(tab) => {
                    let view = view_slot.take().expect("view 仅在挂载时被取走");
                    tab.add_pane(pane_id, view, target.pane_id, target.axis)
                }
                None => false,
            };
            if mounted {
                if let Some(tab) = self.tabs.iter().find(|tab| tab.id == target.tab_id) {
                    Self::request_pane_focus(tab, ctx, pane_id);
                }
                return;
            }
            // 目标标签已被关闭：退化为新标签，不丢会话。
            let view = view_slot.take().expect("未挂载时 view 必须仍在");
            let tab_id = self.allocate_id();
            self.tabs.push(Box::new(TerminalTab::new(
                tab_id,
                "本地终端".into(),
                view,
                pane_id,
                SessionOrigin::Local,
            )));
            self.active_tab = self.tabs.len() - 1;
            return;
        }
        let tab_id = self.allocate_id();
        let pane_id = self.allocate_id();
        let tab = Box::new(TerminalTab::new(
            tab_id,
            "本地终端".into(),
            view,
            pane_id,
            SessionOrigin::Local,
        ));
        self.tabs.push(tab);
        self.active_tab = self.tabs.len() - 1;
    }

    /// 处理后台创建中的本地终端：就绪即挂标签/分屏，失败即提示。
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
        let (command, target) = self
            .pending_local
            .take()
            .map(|pending| (pending.command, pending.target))
            .unwrap_or_default();
        match result {
            Ok(session) => {
                self.mount_local_session(ctx, session, &command, target);
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

    /// 收藏当前会话为工作区（⌘B 与空态入口共用）。
    ///
    /// - 本地标签：收藏当前目录（内核 cwd 优先，⌘B 那一刻的即时真值），
    ///   默认名取末级目录名，按规范路径去重；
    /// - SSH 标签：收藏其主机档案（含认证信息，落盘与 hosts.toml 同级
    ///   0600），按 user@host:port 去重。
    fn bookmark_current_workspace(&mut self) {
        let Some(tab) = self.tabs.get(self.active_tab) else {
            self.show_toast("没有可收藏的终端", true);
            return;
        };
        // 以会话来源判定（origin 是"这个标签是什么"的事实来源；远程标签的
        // session 必然远程，两者一致，origin 还覆盖快照/测试构造的场景）。
        let workspace = if matches!(tab.origin, SessionOrigin::Remote(_)) {
            let SessionOrigin::Remote(profile) = &tab.origin else {
                self.show_toast("远程会话信息缺失，无法收藏", true);
                return;
            };
            let duplicate = self
                .config
                .workspaces
                .iter()
                .any(|existing| match existing {
                    Workspace::Ssh {
                        host, port, user, ..
                    } => *host == profile.host && *port == profile.port && *user == profile.user,
                    _ => false,
                });
            if duplicate {
                self.show_toast("已收藏过该主机", true);
                return;
            }
            Workspace::Ssh {
                name: profile.name.clone(),
                host: profile.host.clone(),
                port: profile.port,
                user: profile.user.clone(),
                auth: profile.auth.clone(),
                path: String::new(),
                command: String::new(),
            }
        } else {
            let Some(cwd) = tab.active_view().fresh_local_directory() else {
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
            let duplicate = self
                .config
                .workspaces
                .iter()
                .any(|existing| match existing {
                    Workspace::Local { path, .. } => {
                        std::fs::canonicalize(path).is_ok_and(|existing| existing == canonical)
                    }
                    _ => false,
                });
            if duplicate {
                self.show_toast("已收藏过该目录", true);
                return;
            }
            let name = canonical
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| canonical.to_string_lossy().into_owned());
            Workspace::Local {
                name,
                path: canonical,
                command: String::new(),
            }
        };
        let name = workspace.name().to_string();
        self.config.workspaces.push(workspace);
        if !self.save_config() {
            self.config.workspaces.pop();
            return;
        }
        self.show_toast(format!("已收藏「{name}」到工作区"), false);
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
        // 记录重开信息（⌘⇧T）：本地记目录，远程记主机档案。
        {
            let tab = &self.tabs[index];
            let dir = (!tab.active_view().session().is_remote())
                .then(|| tab.active_view().fresh_local_directory())
                .flatten()
                .map(PathBuf::from);
            self.closed_tabs.push(ClosedTab {
                title: tab.title().0,
                origin: tab.origin.clone(),
                dir,
            });
            if self.closed_tabs.len() > CLOSED_TAB_STACK {
                self.closed_tabs.remove(0);
            }
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

    /// 关闭窗格；最后一个窗格关闭时整个标签一起关闭。
    ///
    /// 布局树的兄弟节点上提（`pane_tree::remove_leaf`）；关闭的是活动
    /// 窗格时焦点交给布局顺序的第一个窗格（Kaku 同款行为：不悬空焦点）。
    fn close_pane(&mut self, ctx: &egui::Context, tab_index: usize, pane_id: u64) {
        let Some(tab) = self.tabs.get_mut(tab_index) else {
            return;
        };
        if tab.panes.len() <= 1 {
            self.close_tab(tab_index);
            return;
        }
        if pane_tree::remove_leaf(&mut tab.layout, pane_id) != pane_tree::RemoveOutcome::NotFound {
            tab.panes.retain(|pane| pane.id != pane_id);
            if tab.zoomed == Some(pane_id) {
                tab.zoomed = None;
            }
            if tab.active_pane == pane_id {
                tab.active_pane = pane_tree::leaves(&tab.layout)
                    .first()
                    .copied()
                    .unwrap_or(tab.panes[0].id);
                let next = tab.active_pane;
                Self::request_pane_focus(tab, ctx, next);
            }
        }
    }

    /// 快照文件路径（与主机配置同目录：`…/snapshot.toml`；测试用隔离路径）。
    fn snapshot_path(&self) -> PathBuf {
        self.config_path.with_file_name("snapshot.toml")
    }

    /// 保存窗口快照（关闭/退出时调用；Kaku 的自动保存语义）。
    ///
    /// 原子写 + 0600（快照含 SSH 明文认证信息）。失败只记日志：快照是
    /// 便利功能，不能拦住退出。
    fn save_window_snapshot(&self) {
        let tabs: Vec<SnapshotTab> = self
            .tabs
            .iter()
            .map(|tab| {
                let dir = (!tab.active_view().session().is_remote())
                    .then(|| tab.active_view().fresh_local_directory())
                    .flatten()
                    .map(PathBuf::from);
                SnapshotTab {
                    origin: tab.origin.clone(),
                    dir,
                }
            })
            .collect();
        let snapshot = WindowSnapshot { tabs };
        let path = self.snapshot_path();
        if let Err(e) = save_private_toml(&snapshot, &path) {
            log::warn!("保存窗口快照失败：{e}");
        }
    }

    /// ⌘⌥⇧T：恢复上次窗口（Kaku Restore Previous Window）。
    ///
    /// 快照缺失/损坏与「没有快照」同义（Kaku 同款容错）；恢复是**追加**
    /// 标签而不是替换当前窗口（不销毁用户正在用的会话）。
    fn restore_window_snapshot(&mut self, ctx: &egui::Context) {
        let path = self.snapshot_path();
        let snapshot = match std::fs::read_to_string(&path)
            .ok()
            .and_then(|text| toml::from_str::<WindowSnapshot>(&text).ok())
        {
            Some(snapshot) if !snapshot.tabs.is_empty() => snapshot,
            _ => {
                self.show_toast("没有可恢复的窗口快照", false);
                return;
            }
        };
        let count = snapshot.tabs.len();
        for tab in snapshot.tabs {
            match tab.origin {
                SessionOrigin::Local => {
                    self.spawn_local_into(ctx, tab.dir, "", None);
                }
                SessionOrigin::Remote(profile) => {
                    self.start_connect_into(ctx, profile, None);
                }
            }
        }
        self.show_toast(format!("正在恢复上次窗口（{count} 个标签）…"), false);
    }

    /// ⌘⇧T：重开最近关闭的标签（Kaku Reopen closed tab）。
    ///
    /// 本地标签按关闭时目录重建；SSH 标签重连同主机（认证来自同一档案）。
    /// 栈空时给出轻提示而不是静默无操作。
    fn reopen_closed_tab(&mut self, ctx: &egui::Context) {
        let Some(closed) = self.closed_tabs.pop() else {
            self.show_toast("没有可重开的标签", false);
            return;
        };
        self.show_toast(format!("已重开标签：{}", closed.title), false);
        match closed.origin {
            SessionOrigin::Local => {
                self.spawn_local_into(ctx, closed.dir, "", None);
            }
            SessionOrigin::Remote(profile) => {
                self.start_connect_into(ctx, profile, None);
            }
        }
    }

    /// ⌘W 智能关闭（Kaku 语义）：多窗格 → 关窗格；多标签 → 关标签；
    /// 最后一个标签 → 最小化窗口。
    ///
    /// Kaku 在最后一种情况下隐藏应用（HideApplication）；Mino 没有 Dock
    /// 重新打开应用的通道（点击 Dock 图标恢复窗口依赖 macOS reopen 事件），
    /// 用最小化代替——同样「不退出应用」，且用户一定能从 Dock 找回窗口。
    fn smart_close(&mut self, ctx: &egui::Context) {
        let Some(tab) = self.tabs.get(self.active_tab) else {
            return;
        };
        if tab.panes.len() > 1 {
            let pane_id = tab.active_pane;
            self.close_pane(ctx, self.active_tab, pane_id);
            ctx.request_repaint();
        } else if self.tabs.len() > 1 {
            self.close_tab(self.active_tab);
            ctx.request_repaint();
        } else {
            ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
        }
    }

    /// 在活动窗格旁分屏（Kaku `Cmd+D` 左右并排 / `Cmd+Shift+D` 上下堆叠）。
    ///
    /// 新窗格继承标签的会话来源：本地 → 在活动窗格当前目录启动本地
    /// shell（继承字号等全局设置）；SSH → 重连同一主机（认证信息来自同一
    /// `HostProfile`），SFTP 仍按标签挂载、不重复建连。
    fn split_active_pane(&mut self, ctx: &egui::Context, axis: SplitAxis) {
        let Some(tab) = self.tabs.get(self.active_tab) else {
            return;
        };
        let target = SplitTarget {
            tab_id: tab.id,
            pane_id: tab.active_pane,
            axis,
        };
        match tab.origin.clone() {
            SessionOrigin::Local => {
                let dir = tab.active_view().fresh_local_directory().map(PathBuf::from);
                self.spawn_local_into(ctx, dir, "", Some(target));
            }
            SessionOrigin::Remote(profile) => {
                self.start_connect_into(ctx, profile, Some(target));
            }
        }
    }

    /// 按方向把焦点移到相邻窗格（Kaku `Cmd+Opt+方向键`）。
    fn focus_pane_neighbor(&mut self, ctx: &egui::Context, dx: i32, dy: i32) {
        let Some(tab) = self.tabs.get_mut(self.active_tab) else {
            return;
        };
        if tab.zoomed.is_some() || tab.panes.len() <= 1 {
            return;
        }
        let rects = tab.pane_rects.clone();
        if let Some(next) = pane_tree::neighbor(&rects, tab.active_pane, dx, dy) {
            tab.active_pane = next;
            Self::request_pane_focus(tab, ctx, next);
            ctx.request_repaint();
        }
    }

    /// 把 egui 键盘焦点移到指定窗格。
    ///
    /// 渲染期的「焦点跟随」（egui 焦点决定活动窗格）会覆盖任何程序化切换：
    /// 只改 `active_pane` 而不动 egui 焦点，下一帧焦点跟随就把活动窗格拉回
    /// 旧窗格（⌘⌥方向键"按了没反应"）。程序化切换必须同时请求焦点。
    fn request_pane_focus(tab: &TerminalTab, ctx: &egui::Context, pane_id: u64) {
        if let Some(view) = tab.view(pane_id) {
            ctx.memory_mut(|m| m.request_focus(view.focus_id()));
        }
    }

    /// 调整活动窗格所在分割的比例（Kaku `Cmd+Ctrl+方向键`，每次 5 列）。
    fn adjust_pane_size(&mut self, ctx: &egui::Context, axis: SplitAxis, delta: f32) {
        let Some(tab) = self.tabs.get_mut(self.active_tab) else {
            return;
        };
        if tab.zoomed.is_some() {
            return;
        }
        let rects = tab.pane_rects.clone();
        let extent = rects
            .iter()
            .find(|(id, _)| *id == tab.active_pane)
            .map(|(_, rect)| match axis {
                SplitAxis::Horizontal => rect.width(),
                SplitAxis::Vertical => rect.height(),
            })
            .filter(|value| *value > 1.0)
            .unwrap_or(1.0);
        // 5 列 ≈ 5 × cell 宽；按窗格当前尺寸换算成比例增量。
        let cell = tab
            .active_view()
            .cell_metrics()
            .map(|(w, _)| w)
            .unwrap_or(8.0);
        let delta_ratio = (5.0 * cell) / extent * delta;
        if pane_tree::adjust_ratio(&mut tab.layout, tab.active_pane, axis, delta_ratio) {
            ctx.request_repaint();
        }
    }

    /// 切换活动窗格最近一层的拆分方向（Kaku `Cmd+Shift+S`
    /// TogglePaneSplitDirection）：左右并排 ⇄ 上下堆叠，比例保持。
    fn toggle_split_direction(&mut self, ctx: &egui::Context) {
        let Some(tab) = self.tabs.get_mut(self.active_tab) else {
            return;
        };
        if tab.panes.len() <= 1 || tab.zoomed.is_some() {
            return;
        }
        if pane_tree::flip_axis(&mut tab.layout, tab.active_pane) {
            ctx.request_repaint();
        }
    }

    /// 放大/还原活动窗格（Kaku `Cmd+Shift+Enter` = TogglePaneZoomState）。
    fn toggle_pane_zoom(&mut self, ctx: &egui::Context) {
        let Some(tab) = self.tabs.get_mut(self.active_tab) else {
            return;
        };
        if tab.panes.len() <= 1 {
            return;
        }
        tab.zoomed = if tab.zoomed == Some(tab.active_pane) {
            None
        } else {
            Some(tab.active_pane)
        };
        ctx.request_repaint();
    }

    /// 渲染活动标签的全部分屏窗格。
    ///
    /// - 布局：`pane_tree::layout` 按窗口可用区分配（相邻窗格间留
    ///   `PANE_GAP`；Kaku/WezTerm 的 split gap 同构）；
    /// - 放大态：只渲染被放大的窗格、独占整个区域；
    /// - 输入：只有活动窗格消费键盘/鼠标（Kaku `swallow_mouse_click_on_pane_focus`
    ///   同款——点击非活动窗格先聚焦、点击内容不透传给程序）；
    /// - 焦点跟随：egui 焦点落在哪个窗格，活动窗格就切到哪个（点击/导航/新
    ///   分屏统一收敛到这一处）。
    fn render_tab_panes(&mut self, ui: &mut egui::Ui, input_enabled: bool) {
        let Some(tab) = self.tabs.get_mut(self.active_tab) else {
            return;
        };
        let area = ui.max_rect();
        let mut rects: Vec<(u64, egui::Rect)> = Vec::new();
        if let Some(zoom) = tab.zoomed {
            rects.push((zoom, area));
        } else {
            pane_tree::layout(&tab.layout, area, PANE_GAP, &mut rects);
        }
        let active = tab.active_pane;
        tab.pane_rects = rects.clone();
        for (pane_id, rect) in rects {
            let enabled = input_enabled && pane_id == active;
            let Some(pane) = tab.panes.iter_mut().find(|pane| pane.id == pane_id) else {
                continue;
            };
            let mut child = ui.new_child(
                egui::UiBuilder::new()
                    .max_rect(rect)
                    .id_salt(("pane", pane_id)),
            );
            child.set_clip_rect(rect);
            pane.view.show_with_input(&mut child, enabled);
        }
        // 焦点跟随：egui 焦点对应的窗格即活动窗格。
        if let Some(focused) = ui.ctx().memory(|m| m.focused()) {
            if let Some(pane) = tab
                .panes
                .iter()
                .find(|pane| pane.view.focus_id() == focused)
            {
                let new_active = pane.id;
                if new_active != tab.active_pane {
                    tab.active_pane = new_active;
                    ui.ctx().request_repaint();
                }
            }
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

    /// 切换外观偏好并持久化（`auto` / `dark` / `light`）。
    ///
    /// 保存失败时回退到原偏好并 toast 提示：内存态与落盘态必须一致，
    /// 否则本次看着切成功了、重启又回到原来的。
    fn apply_appearance_and_persist(&mut self, ctx: &egui::Context, preference: &str) {
        let previous = self.config.appearance.clone();
        self.config.appearance = preference.to_string();
        let system_dark = ctx.system_theme() != Some(egui::Theme::Light);
        crate::theme::set_theme(
            ctx,
            crate::theme::resolve_appearance(preference, system_dark),
        );
        if !self.save_config() {
            // 落盘失败：内存态回滚，保持与磁盘一致。
            self.config.appearance = previous;
            let rollback_dark = ctx.system_theme() != Some(egui::Theme::Light);
            crate::theme::set_theme(
                ctx,
                crate::theme::resolve_appearance(self.config.appearance.trim(), rollback_dark),
            );
            return;
        }
        let label = match preference {
            "dark" => "深色",
            "light" => "浅色",
            _ => "跟随系统",
        };
        self.show_toast(format!("外观：{label}"), false);
    }

    /// 设置终端字号并持久化（退出重进保持；保存失败回滚内存态）。
    ///
    /// 与 `apply_theme_and_persist` 同一约定：内存态与落盘态必须一致，否则
    /// 本次看着调成功了、重启又回到原来的。字号是全局的：存量**全部**标签
    /// 即时生效（不是只改活动标签——切标签字忽大忽小），新标签创建时继承
    /// （见 `mount_local_session`/`poll_connection`）。落盘只写 `font_size`
    /// 一个字段的变化，不碰主机/项目。`silent` 为 true 时不弹 toast（快捷键
    /// 连续缩放每步都弹会刷屏；滑杆拖动中同样静默，释放后再提示）。
    fn apply_font_size_and_persist(&mut self, ctx: &egui::Context, size: f32, silent: bool) {
        let size = size.clamp(TerminalView::MIN_FONT_SIZE, TerminalView::MAX_FONT_SIZE);
        if (size - self.config.font_size).abs() < f32::EPSILON
            && self.tabs.iter().all(|tab| {
                tab.panes
                    .iter()
                    .all(|pane| (pane.view.font_size() - size).abs() < f32::EPSILON)
            })
        {
            return;
        }
        let previous = self.config.font_size;
        self.config.font_size = size;
        for tab in &mut self.tabs {
            for pane in &mut tab.panes {
                pane.view.set_font_size(size);
            }
        }
        if !self.save_config() {
            // 落盘失败：内存态整体回滚，保持与磁盘一致。
            self.config.font_size = previous;
            for tab in &mut self.tabs {
                for pane in &mut tab.panes {
                    pane.view.set_font_size(previous);
                }
            }
            return;
        }
        ctx.request_repaint();
        if !silent {
            self.show_toast(format!("终端字号：{size:.0}pt"), false);
        }
    }

    /// 活动窗格的终端上下文（AI 面板的 system 上下文：目录 + 最近输出）。
    fn active_terminal_context(&self) -> String {
        let Some(tab) = self.tabs.get(self.active_tab) else {
            return String::new();
        };
        let view = tab.active_view();
        let cwd = view.current_directory().unwrap_or_else(|| "~".to_string());
        let rows = view.visible_rows_snapshot();
        format!(
            "当前终端：{}；工作目录：{}\n最近输出：\n{}",
            tab.title().0,
            cwd,
            rows.join("\n")
        )
    }

    /// 标签导航器条目（⌘⇧O）：标题 = 标签名，详情 = 全路径/主机，快捷键 = 序号。
    fn tab_navigator_items(&self) -> Vec<crate::views::launcher::LauncherItem> {
        self.tabs
            .iter()
            .enumerate()
            .map(|(i, tab)| {
                let (title, tooltip) = tab.title();
                crate::views::launcher::LauncherItem {
                    key: format!("tab:{i}"),
                    title,
                    detail: tooltip.unwrap_or_else(|| {
                        if tab.active_view().session().is_remote() {
                            "SSH 会话".to_string()
                        } else {
                            String::new()
                        }
                    }),
                    shortcut: if i < 9 {
                        format!("⌘{}", i + 1)
                    } else {
                        String::new()
                    },
                }
            })
            .collect()
    }

    /// 命令面板条目（⌘⇧P）：Mino 的常用动作（Kaku Command Palette 的本地化）。
    fn command_palette_items() -> Vec<crate::views::launcher::LauncherItem> {
        use crate::views::launcher::LauncherItem;
        let item = |key: &str, title: &str, shortcut: &str| LauncherItem {
            key: key.to_string(),
            title: title.to_string(),
            detail: String::new(),
            shortcut: shortcut.to_string(),
        };
        vec![
            item("tab.new", "新建本地终端标签", "⌘T"),
            item("tab.close", "关闭当前标签", "⌘⇧W"),
            item("tab.reopen", "重开上次关闭的标签", "⌘⇧T"),
            item("pane.split.right", "向右分屏", "⌘D"),
            item("pane.split.down", "向下分屏", "⌘⇧D"),
            item("pane.zoom", "放大 / 还原窗格", "⌘⇧↩"),
            item("pane.close", "关闭当前窗格", ""),
            item("search.open", "搜索滚动缓冲", "⌘F"),
            item("shell.clear", "清屏并清空回滚", "⌘K"),
            item("font.inc", "增大字号", "⌘+"),
            item("font.dec", "减小字号", "⌘-"),
            item("font.reset", "重置字号", "⌘0"),
            item("workspaces.open", "打开工作区面板", "⌘O"),
            item("workspace.bookmark", "收藏当前会话为工作区", "⌘B"),
            item("workspace.new", "新增工作区", "⌘N"),
            item("ai.chat", "AI 聊天面板", "⌘L"),
            item("ai.settings", "AI 助手设置", "⌘⇧A"),
            item("sftp.toggle", "切换 SFTP 面板", ""),
            item("appearance.dark", "外观：深色（Kaku Dark）", ""),
            item("appearance.light", "外观：浅色（Kaku Light）", ""),
            item("appearance.auto", "外观：跟随系统", ""),
            item("settings.open", "打开设置", "⌘,"),
            item("shell.integration", "安装 / 修复 Shell 集成", ""),
        ]
    }

    /// 执行命令面板动作。
    fn run_palette_action(&mut self, ctx: &egui::Context, action: &str) {
        match action {
            "tab.new" => self.new_local_tab(ctx),
            "tab.close" => {
                if !self.tabs.is_empty() {
                    self.close_tab(self.active_tab);
                }
            }
            "tab.reopen" => self.reopen_closed_tab(ctx),
            "pane.split.right" => self.split_active_pane(ctx, SplitAxis::Horizontal),
            "pane.split.down" => self.split_active_pane(ctx, SplitAxis::Vertical),
            "pane.zoom" => self.toggle_pane_zoom(ctx),
            "pane.close" => {
                let pane = self
                    .tabs
                    .get(self.active_tab)
                    .map(|tab| tab.active_pane)
                    .unwrap_or_default();
                self.close_pane(ctx, self.active_tab, pane);
            }
            "search.open" => {
                if let Some(tab) = self.tabs.get_mut(self.active_tab) {
                    tab.active_view_mut().toggle_search();
                }
            }
            "shell.clear" => {
                if let Some(tab) = self.tabs.get_mut(self.active_tab) {
                    tab.active_view_mut().clear_scrollback_and_screen();
                }
            }
            "font.inc" => {
                let next = self.config.font_size + TerminalView::FONT_SIZE_STEP;
                self.apply_font_size_and_persist(ctx, next, false);
            }
            "font.dec" => {
                let next = self.config.font_size - TerminalView::FONT_SIZE_STEP;
                self.apply_font_size_and_persist(ctx, next, false);
            }
            "font.reset" => {
                self.apply_font_size_and_persist(ctx, TerminalView::DEFAULT_FONT_SIZE, false);
            }
            "workspaces.open" => self.toggle_workspaces(),
            "workspace.bookmark" => self.bookmark_current_workspace(),
            "workspace.new" => self.open_workspace_editor(1),
            "ai.chat" => {
                let context = self.active_terminal_context();
                self.ai_panel.set_context(context);
                self.ai_panel.open();
            }
            "ai.settings" => self.show_settings = true,
            "sftp.toggle" => {
                if let Some(tab) = self.tabs.get_mut(self.active_tab) {
                    if tab.sftp.is_some() {
                        tab.sftp_open = !tab.sftp_open;
                    }
                }
            }
            "appearance.dark" => self.apply_appearance_and_persist(ctx, "dark"),
            "appearance.light" => self.apply_appearance_and_persist(ctx, "light"),
            "appearance.auto" => self.apply_appearance_and_persist(ctx, "auto"),
            "settings.open" => self.show_settings = true,
            "shell.integration" => self.install_shell_integration(ctx),
            _ => {}
        }
    }

    /// 读取助手配置；未启用/未配置（无 key 且非 codex）时返回 None。
    ///
    /// 每次触发读一次文件（几 KB、频率极低），避免缓存与「设置里刚改完
    /// 不生效」的不一致。
    fn assistant_config(&self) -> Option<mino_core::ai::AssistantConfig> {
        let path = mino_core::ai::default_assistant_path();
        let config = mino_core::ai::AssistantConfig::load(&path).ok()?;
        if !config.enabled {
            return None;
        }
        if config.api_key.is_empty() && config.auth_type != "codex" {
            return None;
        }
        Some(config)
    }

    /// 每帧推进 AI 助手任务：失败命令 → 修复建议；`#` → 命令生成。
    ///
    /// - 触发源：窗格的 shell 集成标记（`133;D;<code>`，非 0 时）与
    ///   `# ` 拦截请求；两者都由窗格侧消费（`take_*` 语义天然去重）；
    /// - 网络调用在后台线程（ureq 阻塞），结果经通道回 UI；
    /// - 未配置/未启用助手时静默跳过（不打扰未启用的用户）。
    fn poll_ai_tasks(&mut self, ctx: &egui::Context) {
        // 1. 失败命令：取结果 → 起修复任务（单飞；未配置则只消费不请求）。
        if self.ai_fix_rx.is_none() {
            let config = self.assistant_config();
            let mut pending: Option<(u64, u64, String, i32, String, String)> = None;
            'tabs: for tab in &mut self.tabs {
                let tab_id = tab.id;
                for pane in &mut tab.panes {
                    let Some((exit_code, command)) = pane.view.take_command_result() else {
                        continue;
                    };
                    // 退出码 0 / 130（Ctrl+C）不触发；无命令行快照（裸回车等）也跳过。
                    if exit_code == 0 || exit_code == 130 {
                        continue;
                    }
                    let Some(command) = command.filter(|c| !c.is_empty()) else {
                        continue;
                    };
                    if config.is_some() {
                        let cwd = pane
                            .view
                            .current_directory()
                            .unwrap_or_else(|| "~".to_string());
                        let tail = pane.view.visible_rows_snapshot().join("\n");
                        pending = Some((tab_id, pane.id, command, exit_code, cwd, tail));
                    }
                    break 'tabs;
                }
            }
            if let (Some(config), Some((tab_id, pane_id, command, exit_code, cwd, tail))) =
                (config, pending)
            {
                let codex_home = codex_home_path();
                let (tx, rx) = std::sync::mpsc::channel();
                let notify = ctx.clone();
                std::thread::spawn(move || {
                    let result =
                        ai_fix_worker(&config, &command, exit_code, &cwd, &tail, &codex_home);
                    let _ = tx.send(result.map(|suggestion| AiFixOutcome {
                        tab_id,
                        pane_id,
                        suggestion,
                    }));
                    notify.request_repaint();
                });
                self.ai_fix_rx = Some(rx);
            }
        }
        // 2. `#` 生成：取请求 → 起生成任务（单飞）。
        if self.ai_generate_rx.is_none() {
            let config = self.assistant_config();
            let mut pending: Option<(u64, u64, String, String, String)> = None;
            'gen: for tab in &mut self.tabs {
                let tab_id = tab.id;
                for pane in &mut tab.panes {
                    let Some(query) = pane.view.take_ai_generate_request() else {
                        continue;
                    };
                    if config.is_some() {
                        let cwd = pane
                            .view
                            .current_directory()
                            .unwrap_or_else(|| "~".to_string());
                        let tail = pane.view.visible_rows_snapshot().join("\n");
                        pending = Some((tab_id, pane.id, query, cwd, tail));
                    } else {
                        self.show_toast("AI 助手未配置（设置 → AI 助手）", true);
                    }
                    break 'gen;
                }
            }
            if let (Some(config), Some((tab_id, pane_id, query, cwd, tail))) = (config, pending) {
                let codex_home = codex_home_path();
                let (tx, rx) = std::sync::mpsc::channel();
                let notify = ctx.clone();
                std::thread::spawn(move || {
                    let result = ai_generate_worker(&config, &query, &cwd, &tail, &codex_home);
                    let _ = tx.send(Some(AiGenerateOutcome {
                        tab_id,
                        pane_id,
                        result,
                    }));
                    notify.request_repaint();
                });
                self.ai_generate_rx = Some(rx);
            }
        }
        // 3. 收取修复结果。
        if let Some(rx) = &self.ai_fix_rx {
            match rx.try_recv() {
                Ok(result) => {
                    self.ai_fix_rx = None;
                    if let Some(outcome) = result {
                        let message = if !outcome.suggestion.command.is_empty() {
                            format!(
                                "AI 建议（⌘⇧E 填入提示符）：{}",
                                truncate_for_toast(&outcome.suggestion.command)
                            )
                        } else if !outcome.suggestion.explanation.is_empty() {
                            format!(
                                "AI：{}",
                                truncate_for_toast(&outcome.suggestion.explanation)
                            )
                        } else {
                            String::new()
                        };
                        if !message.is_empty() {
                            self.show_toast(message, false);
                        }
                        self.ai_fix_suggestion = Some(outcome);
                    }
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    self.ai_fix_rx = None;
                }
            }
        }
        // 4. 收取生成结果并回填提示符。
        if let Some(rx) = &self.ai_generate_rx {
            match rx.try_recv() {
                Ok(Some(outcome)) => {
                    self.ai_generate_rx = None;
                    match outcome.result {
                        Ok(command) => {
                            let applied = self
                                .tabs
                                .iter_mut()
                                .find(|tab| tab.id == outcome.tab_id)
                                .and_then(|tab| {
                                    tab.panes.iter_mut().find(|pane| pane.id == outcome.pane_id)
                                })
                                .map(|pane| pane.view.inject_command(&command))
                                .is_some();
                            if applied {
                                self.show_toast(
                                    format!("已生成（回车执行）：{}", truncate_for_toast(&command)),
                                    false,
                                );
                            }
                        }
                        Err(e) => self.show_toast(format!("AI 生成失败：{e}"), true),
                    }
                }
                Ok(None) => self.ai_generate_rx = None,
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    self.ai_generate_rx = None;
                }
            }
        }
    }

    /// ⌘⇧E：把最近的 AI 修复建议填到对应窗格的提示符（不自动执行）。
    fn apply_ai_suggestion(&mut self) {
        let Some(outcome) = self.ai_fix_suggestion.take() else {
            self.show_toast("暂无 AI 建议", false);
            return;
        };
        let command = outcome.suggestion.command.clone();
        if command.is_empty() {
            let explanation = outcome.suggestion.explanation.clone();
            if !explanation.is_empty() {
                self.show_toast(format!("AI：{}", truncate_for_toast(&explanation)), false);
            }
            return;
        }
        let applied = self
            .tabs
            .iter_mut()
            .find(|tab| tab.id == outcome.tab_id)
            .and_then(|tab| tab.panes.iter_mut().find(|pane| pane.id == outcome.pane_id))
            .map(|pane| {
                pane.view.inject_command(&command);
            })
            .is_some();
        if applied {
            self.show_toast("已填入建议命令（回车执行）", false);
        } else {
            self.show_toast("原窗格已关闭，建议未应用", true);
        }
    }

    /// Shell 集成的配置目录（宿主 `~/.config/mino`；测试为隔离路径）。
    fn shell_config_dir(&self) -> PathBuf {
        self.config_path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."))
    }

    /// 安装/修复 Shell 集成（受管块 + 脚本落盘），随后补下载插件。
    fn install_shell_integration(&mut self, ctx: &egui::Context) {
        let config_dir = self.shell_config_dir();
        match mino_core::shell_integration::install(&self.zshrc_path, &config_dir) {
            Ok(outcome) => {
                let message = if outcome.installed {
                    "Shell 集成已安装：重启终端后生效"
                } else {
                    "Shell 集成已是最新"
                };
                self.show_toast(message, false);
                // 插件缺失则后台补齐（联网；失败逐个提示）。
                if !mino_core::shell_integration::missing_plugins(&config_dir).is_empty() {
                    self.start_vendor_bootstrap(ctx);
                }
            }
            Err(e) => {
                log::warn!("Shell 集成安装失败：{e}");
                self.show_toast(format!("Shell 集成安装失败：{e}"), true);
            }
        }
    }

    /// 卸载 Shell 集成（移除受管块；脚本与插件保留，便于重装）。
    fn uninstall_shell_integration(&mut self) {
        match mino_core::shell_integration::uninstall(&self.zshrc_path) {
            Ok(true) => self.show_toast("Shell 集成已卸载", false),
            Ok(false) => self.show_toast("Shell 集成未安装", false),
            Err(e) => {
                log::warn!("Shell 集成卸载失败：{e}");
                self.show_toast(format!("卸载失败：{e}"), true);
            }
        }
    }

    /// 后台下载缺失的 Shell 插件（git clone；结果经 `vendor_rx` 回传）。
    fn start_vendor_bootstrap(&mut self, ctx: &egui::Context) {
        if self.vendor_running {
            return;
        }
        let config_dir = self.shell_config_dir();
        let (tx, rx) = std::sync::mpsc::channel();
        let notify_ctx = ctx.clone();
        std::thread::spawn(move || {
            let results = mino_core::shell_integration::bootstrap_vendor(&config_dir);
            let _ = tx.send(results);
            notify_ctx.request_repaint();
        });
        self.vendor_rx = Some(rx);
        self.vendor_running = true;
        self.show_toast("正在下载 Shell 插件…", false);
    }

    /// 每帧轮询插件下载结果。
    fn poll_vendor_bootstrap(&mut self) {
        let Some(rx) = &self.vendor_rx else {
            return;
        };
        match rx.try_recv() {
            Ok(results) => {
                self.vendor_rx = None;
                self.vendor_running = false;
                let failed: Vec<String> = results
                    .iter()
                    .filter_map(|result| result.as_ref().err().cloned())
                    .collect();
                if failed.is_empty() {
                    self.show_toast("Shell 插件已就绪", false);
                } else {
                    self.show_toast(
                        format!("部分插件下载失败（可稍后重试）：{}", failed.join("；")),
                        true,
                    );
                }
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {}
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                self.vendor_rx = None;
                self.vendor_running = false;
                self.show_toast("插件下载线程异常退出", true);
            }
        }
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
                    view.set_font_size(self.config.font_size);
                    let origin = self
                        .pending_profile
                        .take()
                        .map(SessionOrigin::Remote)
                        .unwrap_or(SessionOrigin::Local);
                    let label = self.pending_label.clone();
                    let split_target = self.pending_split.take();
                    if let Some(target) = split_target {
                        // 分屏挂载：不新建标签、不挂 SFTP（SFTP 属于标签）。
                        let pane_id = self.allocate_id();
                        let mut view_slot = Some(view);
                        let mounted = match self.tabs.iter_mut().find(|tab| tab.id == target.tab_id)
                        {
                            Some(tab) => {
                                let view = view_slot.take().expect("view 仅在挂载时被取走");
                                tab.add_pane(pane_id, view, target.pane_id, target.axis)
                            }
                            None => false,
                        };
                        if mounted {
                            if let Some(tab) = self.tabs.iter().find(|tab| tab.id == target.tab_id)
                            {
                                Self::request_pane_focus(tab, ctx, pane_id);
                            }
                        }
                        if !mounted {
                            // 目标标签已关闭：退化为新标签，不丢会话。
                            let view = view_slot.take().expect("未挂载时 view 必须仍在");
                            let tab_id = self.allocate_id();
                            self.tabs.push(Box::new(TerminalTab::new(
                                tab_id,
                                label.clone(),
                                view,
                                pane_id,
                                origin,
                            )));
                            self.active_tab = self.tabs.len() - 1;
                        }
                    } else {
                        let pane_id = self.allocate_id();
                        self.tabs.push(Box::new(TerminalTab::new(
                            connection_id,
                            label.clone(),
                            view,
                            pane_id,
                            origin,
                        )));
                        self.active_tab = self.tabs.len() - 1;
                        self.pending_tab = Some(connection_id);
                        self.mount_ready_sftp();
                    }
                    self.show_toast(format!("已连接到 {label}"), false);
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

    /// 发起远程连接：`target` 为 None 时挂新标签（并启动 SFTP），
    /// 为 Some 时在目标窗格旁分屏（不新建 SFTP——SFTP 按标签挂载）。
    fn start_connect_into(
        &mut self,
        ctx: &egui::Context,
        profile: HostProfile,
        target: Option<SplitTarget>,
    ) {
        if let Some(cancel) = self.pending_connect_cancel.take() {
            cancel.cancel();
        }
        // 标签展示新建连接时填写的名称，不把用户名和远程当前路径带进来。
        let label = profile.name.clone();
        let connection_id = self.allocate_id();
        self.pending_connection_id = Some(connection_id);
        if target.is_none() {
            if let Some(connection) = self.pending_sftp.take() {
                connection.handle.close();
            }
            if let Some(connection) = self.ready_sftp.take() {
                connection.handle.close();
            }
            self.pending_tab = None;
        }
        let terminal_ctx = ctx.clone();
        let on_event = Arc::new(move |_ev: &SessionEvent| {
            terminal_ctx.request_repaint();
        });
        let (_thread, rx, connect_cancel) = connect_remote_with_cancel(&profile, 80, 24, on_event);
        self.pending = Some(rx);
        self.pending_connect_cancel = Some(connect_cancel);
        self.pending_label = label.clone();
        self.pending_profile = Some(profile.clone());
        self.pending_split = target;

        if target.is_none() {
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
            tab.active_view_mut().set_remote_current_directory(&home);
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
        if tab.sftp.is_none() {
            return;
        }
        // 注意借用拆分：`active_view()` 借整个 tab，不能与 `tab.sftp.as_mut()`
        // 并存——先取终端侧信息/做终端侧变更，sftp 可变借用每次都短命。
        if tab.active_view().current_directory().is_none() {
            return;
        }
        // 本地会话直接读 shell 子进程的内核 cwd（source/别名/函数等场景下
        // 输入跟踪早已失效，`pwd` 探测还要往用户终端里注命令；内核值无
        // 注入、无延迟，直接导航）。
        if !tab.active_view().session().is_remote() {
            if let Some(dir) = tab.active_view().session().child_current_dir() {
                if let Some(sftp) = tab.sftp.as_mut() {
                    sftp.locate_terminal_directory(&dir.to_string_lossy());
                }
                ctx.request_repaint();
                return;
            }
        }
        if tab.active_view_mut().request_fresh_pwd() {
            // 探测已注入：等待终端输出（见 `poll_locate_pending`），
            // 这几帧内保持重绘，pwd 回显/输出到达后第一时间导航。
            tab.locate_pending = Some(LocatePending {
                started_at: ctx.input(|i| i.time),
            });
            ctx.request_repaint();
        } else {
            // 不适合探测（全屏应用/有未执行输入/已有探测在途）：直接用
            // 已知目录回退，行为与此前一致，不让用户觉得“点了没反应”。
            if let Some(path) = tab.active_view().current_directory() {
                if let Some(sftp) = tab.sftp.as_mut() {
                    sftp.locate_terminal_directory(&path);
                }
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
            if tab.sftp.is_none() {
                tab.locate_pending = None;
                tab.active_view_mut().cancel_fresh_pwd();
                continue;
            }
            let ready = tab.active_view().auto_pwd_ready();
            // 借用拆分同 `begin_locate_terminal`：先算终端侧结论、
            // sftp 可变借用保持短命。
            let timed_out = now - pending.started_at >= LOCATE_TIMEOUT_SECS;
            if ready || timed_out {
                let path = tab.active_view().current_directory();
                if timed_out {
                    tab.active_view_mut().cancel_fresh_pwd();
                }
                tab.locate_pending = None;
                if let Some(path) = path {
                    if let Some(sftp) = tab.sftp.as_mut() {
                        sftp.locate_terminal_directory(&path);
                    }
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
            // 逐窗格收取（粘贴可能发生在任一聚焦窗格；SFTP 按标签挂载）。
            for pane in &mut tab.panes {
                if let Some(local) = pane.view.take_pending_image() {
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
                if let Some(message) = pane.view.take_image_paste_error() {
                    paste_errors.push(message);
                }
                if let Some(message) = pane.view.take_clipboard_write_error() {
                    paste_errors.push(message);
                }
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
                tab.active_view().session().write(token.as_bytes());
                tab.active_view_mut().push_pasted_text(&token);
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
                                // ============ 工作区（SSH 与项目收藏的融合） ============
                                let workspace_count =
                                    format!("{} 个", self.config.workspaces.len());
                                Self::settings_card(
                                    ui,
                                    "工作区",
                                    Some(&workspace_count),
                                    |ui| {
                                        self.workspace_manager(ui);
                                    },
                                );

                                // ============ 外观 ============
                                // 全盘 Kaku 化：不再有可切换的多套主题，外观
                                // 只有「跟随系统 / 深色 / 浅色」三种偏好
                                // （Kaku Dark / Kaku Light 两套配色自动跟随）。
                                Self::settings_card(ui, "外观", None, |ui| {
                                    ui.horizontal(|ui| {
                                        ui.label(
                                            egui::RichText::new("主题")
                                                .size(12.0)
                                                .color(theme.text_muted),
                                        );
                                        ui.add_space(8.0);
                                        let current = self.config.appearance.trim().to_string();
                                        let label = |pref: &str| match pref {
                                            "dark" => "深色（Kaku Dark）",
                                            "light" => "浅色（Kaku Light）",
                                            _ => "跟随系统",
                                        };
                                        let selected_text =
                                            label(if current.is_empty() { "auto" } else { current.as_str() });
                                        egui::ComboBox::from_id_salt("settings_appearance_switcher")
                                            .selected_text(
                                                egui::RichText::new(selected_text)
                                                    .color(theme.text_primary),
                                            )
                                            .show_ui(ui, |ui| {
                                                for pref in ["auto", "dark", "light"] {
                                                    let selected =
                                                        current == pref || (pref == "auto" && current.is_empty());
                                                    if ui
                                                        .selectable_label(
                                                            selected,
                                                            egui::RichText::new(label(pref)).color(
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
                                                        self.apply_appearance_and_persist(
                                                            &theme_ctx, pref,
                                                        );
                                                    }
                                                }
                                            });
                                    });
                                    ui.add_space(4.0);
                                    ui.label(
                                        egui::RichText::new(
                                            "Kaku Dark / Kaku Light 两套配色，跟随 macOS 外观自动切换",
                                        )
                                        .size(10.0)
                                        .color(theme.text_muted),
                                    );
                                    ui.add_space(10.0);
                                    // 终端字号：滑杆 10-24pt（步进 0.5）+ −/＋
                                    // 微调 + 重置默认。拖动中静默应用（每步都
                                    // 落盘但不弹 toast），释放后提示一次；
                                    // 快捷键 ⌘+/- / ⌘0 见 `MinoApp::ui`。
                                    ui.horizontal(|ui| {
                                        ui.label(
                                            egui::RichText::new("终端字号")
                                                .size(12.0)
                                                .color(theme.text_muted),
                                        );
                                        ui.add_space(8.0);
                                        let mut size = self.config.font_size;
                                        let slider = egui::Slider::new(
                                            &mut size,
                                            TerminalView::MIN_FONT_SIZE
                                                ..=TerminalView::MAX_FONT_SIZE,
                                        )
                                        .step_by(0.5)
                                        .fixed_decimals(1)
                                        .suffix(" pt")
                                        .show_value(true);
                                        // 滑块配色单独覆盖（见
                                        // `theme::apply_slider_style` 的说明：全局
                                        // 控件底色调在弹窗卡片上看不清轨道）。
                                        let changed = ui
                                            .scope(|ui| {
                                                crate::theme::apply_slider_style(
                                                    ui.style_mut(),
                                                    theme,
                                                );
                                                ui.add(slider)
                                            })
                                            .inner
                                            .changed();
                                        if changed {
                                            let font_ctx = ui.ctx().clone();
                                            // 拖动中每步都即时生效+落盘（静默，
                                            // 不弹 toast），松手后提示一次。
                                            self.apply_font_size_and_persist(&font_ctx, size, true);
                                        }
                                    });
                                    ui.add_space(4.0);
                                    ui.horizontal(|ui| {
                                        ui.spacing_mut().item_spacing.x = 8.0;
                                        if ui
                                            .small_button(
                                                egui::RichText::new("−")
                                                    .size(13.0)
                                                    .color(theme.text_primary),
                                            )
                                            .on_hover_text("缩小（⌘-）")
                                            .clicked()
                                        {
                                            let font_ctx = ui.ctx().clone();
                                            let next = self.config.font_size
                                                - TerminalView::FONT_SIZE_STEP;
                                            self.apply_font_size_and_persist(
                                                &font_ctx, next, false,
                                            );
                                        }
                                        if ui
                                            .small_button(
                                                egui::RichText::new("＋")
                                                    .size(13.0)
                                                    .color(theme.text_primary),
                                            )
                                            .on_hover_text("放大（⌘+）")
                                            .clicked()
                                        {
                                            let font_ctx = ui.ctx().clone();
                                            let next = self.config.font_size
                                                + TerminalView::FONT_SIZE_STEP;
                                            self.apply_font_size_and_persist(
                                                &font_ctx, next, false,
                                            );
                                        }
                                        if ui
                                            .small_button(
                                                egui::RichText::new("重置")
                                                    .size(11.5)
                                                    .color(theme.text_secondary),
                                            )
                                            .on_hover_text("恢复默认字号（⌘0）")
                                            .clicked()
                                        {
                                            let font_ctx = ui.ctx().clone();
                                            self.apply_font_size_and_persist(
                                                &font_ctx,
                                                TerminalView::DEFAULT_FONT_SIZE,
                                                false,
                                            );
                                        }
                                        ui.label(
                                            egui::RichText::new("⌘+ / ⌘- 缩放 · ⌘0 重置")
                                                .monospace()
                                                .size(10.0)
                                                .color(theme.text_muted),
                                        );
                                    });
                                });
                                ui.add_space(10.0);

                                // ============ AI 助手 ============
                                // 对照 Kaku 的 ai 配置：OpenAI 兼容连接 +
                                // 模型名；`# 描述` 生成命令与失败命令修复都
                                // 用它（⌘L 聊天面板见 AiPanel）。
                                Self::settings_card(ui, "AI 助手", None, |ui| {
                                    let path = mino_core::ai::default_assistant_path();
                                    let mut config =
                                        mino_core::ai::AssistantConfig::load(&path)
                                            .unwrap_or_default();
                                    let mut changed = false;
                                    ui.horizontal(|ui| {
                                        changed |= ui
                                            .checkbox(&mut config.enabled, "启用")
                                            .changed();
                                        ui.add_space(8.0);
                                        ui.label(
                                            egui::RichText::new(if config.enabled {
                                                "命令生成（# 描述）与失败修复已开启"
                                            } else {
                                                "未启用"
                                            })
                                            .size(11.0)
                                            .color(theme.text_muted),
                                        );
                                    });
                                    ui.add_space(4.0);
                                    egui::Grid::new("ai_assistant_grid")
                                        .num_columns(2)
                                        .spacing([8.0, 6.0])
                                        .show(ui, |ui| {
                                            let mut field = |ui: &mut egui::Ui,
                                                             label: &str,
                                                             value: &mut String,
                                                             hint: &str,
                                                             password: bool| {
                                                ui.label(
                                                    egui::RichText::new(label)
                                                        .size(11.5)
                                                        .color(theme.text_muted),
                                                );
                                                let edit = egui::TextEdit::singleline(value)
                                                    .desired_width(260.0)
                                                    .hint_text(hint)
                                                    .password(password);
                                                let resp = ui.add(edit);
                                                ui.end_row();
                                                if resp.changed() {
                                                    changed = true;
                                                }
                                            };
                                            field(ui, "模型", &mut config.model, "如 gpt-4o-mini", false);
                                            field(ui, "聊天模型", &mut config.chat_model, "留空回退模型", false);
                                            field(ui, "Base URL", &mut config.base_url, mino_core::ai::DEFAULT_BASE_URL, false);
                                            field(ui, "API Key", &mut config.api_key, "sk-…", true);
                                            ui.label(
                                                egui::RichText::new("认证")
                                                    .size(11.5)
                                                    .color(theme.text_muted),
                                            );
                                            egui::ComboBox::from_id_salt("ai_auth_type")
                                                .selected_text(if config.auth_type == "codex" {
                                                    "codex（跟随 Codex 登录）"
                                                } else {
                                                    "api_key"
                                                })
                                                .show_ui(ui, |ui| {
                                                    for option in ["api_key", "codex"] {
                                                        if ui
                                                            .selectable_label(
                                                                config.auth_type == option,
                                                                option,
                                                            )
                                                            .clicked()
                                                            && config.auth_type != option
                                                        {
                                                            config.auth_type = option.to_string();
                                                            changed = true;
                                                        }
                                                    }
                                                });
                                            ui.end_row();
                                        });
                                    if changed {
                                        let save_path = mino_core::ai::default_assistant_path();
                                        match config.save(&save_path) {
                                            Ok(()) => {
                                                // 打开中的 AI 面板立刻采用新配置。
                                                self.ai_panel.reload_config();
                                            }
                                            Err(e) => {
                                                log::warn!("保存 AI 助手配置失败：{e}");
                                                self.show_toast(
                                                    format!("保存 AI 配置失败：{e}"),
                                                    true,
                                                );
                                            }
                                        }
                                    }
                                    ui.add_space(4.0);
                                    ui.label(
                                        egui::RichText::new(format!(
                                            "配置存储：{}（0600）",
                                            mino_core::ai::default_assistant_path().display()
                                        ))
                                        .size(10.0)
                                        .color(theme.text_muted),
                                    );
                                });
                                ui.add_space(10.0);

                                // ============ Shell 集成 ============
                                // Kaku 的 shell suite（z / syntax-highlighting /
                                // autosuggestions / Smart Tab）：受管块安装进
                                // `~/.zshrc`，插件按需下载；一键可卸载。
                                Self::settings_card(ui, "Shell 集成", None, |ui| {
                                    let config_dir = self.shell_config_dir();
                                    let installed = mino_core::shell_integration::is_installed(
                                        &self.zshrc_path,
                                    );
                                    let plugin_total =
                                        mino_core::shell_integration::Plugin::ALL.len();
                                    let missing = mino_core::shell_integration::missing_plugins(
                                        &config_dir,
                                    )
                                    .len();
                                    ui.horizontal(|ui| {
                                        ui.spacing_mut().item_spacing.x = 8.0;
                                        status_dot(
                                            ui,
                                            if installed {
                                                theme.success
                                            } else {
                                                theme.text_muted
                                            },
                                            false,
                                        );
                                        ui.label(
                                            egui::RichText::new(if installed {
                                                "已安装"
                                            } else {
                                                "未安装"
                                            })
                                            .size(12.0)
                                            .color(theme.text_primary),
                                        );
                                        ui.label(
                                            egui::RichText::new(format!(
                                                "· 插件 {}/{}",
                                                plugin_total.saturating_sub(missing),
                                                plugin_total
                                            ))
                                            .size(11.0)
                                            .color(theme.text_muted),
                                        );
                                        if self.vendor_running {
                                            loading_hint(ui, "插件下载中…");
                                        }
                                    });
                                    ui.add_space(6.0);
                                    ui.horizontal(|ui| {
                                        ui.spacing_mut().item_spacing.x = 8.0;
                                        if ui.small_button("安装 / 修复").clicked() {
                                            let card_ctx = ui.ctx().clone();
                                            self.install_shell_integration(&card_ctx);
                                        }
                                        if ui.small_button("卸载").clicked() {
                                            self.uninstall_shell_integration();
                                        }
                                        if ui
                                            .add_enabled(
                                                !self.vendor_running && missing > 0,
                                                egui::Button::new("下载插件").small(),
                                            )
                                            .clicked()
                                        {
                                            let card_ctx = ui.ctx().clone();
                                            self.start_vendor_bootstrap(&card_ctx);
                                        }
                                    });
                                    ui.add_space(4.0);
                                    ui.label(
                                        egui::RichText::new(
                                            "向 ~/.zshrc 写入受管片段（可一键卸载）：z 目录跳转 / 语法高亮 / 自动补全 / Smart Tab。",
                                        )
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

    // ===== workspace_quick_menu（替换旧 host_quick_menu） =====
    /// 标签栏「工作区」快捷菜单：单击条目直接打开（本地建标签 / SSH 连接）。
    ///
    /// 行样式与 ⌘O 面板共用 `paint_workspace_row`；空态给「新增工作区」入口。
    fn workspace_quick_menu(&mut self, ui: &mut egui::Ui) {
        let theme = crate::theme::current_theme();
        let ctx = ui.ctx().clone();
        const MENU_W: f32 = 300.0;
        const ROW_H: f32 = 44.0;
        const AVATAR: f32 = 22.0;
        ui.set_min_width(MENU_W);
        ui.set_max_width(MENU_W);
        ui.add_space(2.0);
        if self.config.workspaces.is_empty() {
            ui.add_space(4.0);
            ui.label(
                egui::RichText::new("暂无工作区")
                    .size(11.5)
                    .color(theme.text_muted),
            );
            ui.add_space(6.0);
            if ui
                .add(egui::Button::new(
                    egui::RichText::new("新增工作区…")
                        .size(11.5)
                        .color(theme.text_primary),
                ))
                .clicked()
            {
                ui.close();
                self.open_workspace_editor(1);
            }
            ui.add_space(4.0);
            return;
        }
        let mut open: Option<Workspace> = None;
        egui::ScrollArea::vertical()
            .id_salt("workspace_quick_scroll")
            .max_height(320.0)
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 2.0;
                for (i, workspace) in self.config.workspaces.iter().enumerate() {
                    let row_id = egui::Id::new(("workspace_quick_row", i));
                    let resp = paint_workspace_row(ui, workspace, false, row_id, ROW_H, AVATAR);
                    if resp.clicked() {
                        open = Some(workspace.clone());
                    }
                }
            });
        ui.add_space(4.0);
        if ui
            .add(egui::Button::new(
                egui::RichText::new("管理工作区…")
                    .size(11.0)
                    .color(theme.text_secondary),
            ))
            .clicked()
        {
            ui.close();
            self.show_settings = true;
        }
        if let Some(workspace) = open {
            ui.close();
            self.open_workspace(&ctx, &workspace);
        }
    }

    // ===== filtered_workspace_indices（替换 filtered_project_indices） =====
    /// 按搜索词过滤工作区（名称/详情子串，大小写不敏感），返回原下标。
    ///
    /// 快捷菜单与 ⌘O 面板共用，保证两处过滤语义一致。
    fn filtered_workspace_indices(&self) -> Vec<usize> {
        let keyword = self.workspace_filter.trim().to_lowercase();
        if keyword.is_empty() {
            return (0..self.config.workspaces.len()).collect();
        }
        self.config
            .workspaces
            .iter()
            .enumerate()
            .filter(|(_, workspace)| {
                workspace.name().to_lowercase().contains(&keyword)
                    || workspace.detail().to_lowercase().contains(&keyword)
            })
            .map(|(i, _)| i)
            .collect()
    }

    /// 切换工作区面板（⌘O；工作区编辑模态时不响应，防弹窗叠加）。
    fn toggle_workspaces(&mut self) {
        if self.workspace_edit.is_some() {
            return;
        }
        self.show_workspaces = !self.show_workspaces;
        if self.show_workspaces {
            self.workspace_filter.clear();
            self.workspace_selected = 0;
        }
    }

    // ===== workspaces_panel（替换 projects_panel） =====
    /// 工作区打开面板（⌘O）：搜索 + 列表（本地目录与 SSH 主机同列）。
    ///
    /// 键盘 ↑↓ 选择、回车/单击打开；SSH 条目发起连接，本地条目新建标签。
    fn workspaces_panel(&mut self, ctx: &egui::Context) {
        let theme = crate::theme::current_theme();
        // ==================== 键盘导航（每帧先处理） ====================
        let matched = self.filtered_workspace_indices();
        if !matched.is_empty() {
            if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowDown)) {
                self.workspace_selected = (self.workspace_selected + 1) % matched.len();
            }
            if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowUp)) {
                self.workspace_selected =
                    (self.workspace_selected + matched.len() - 1) % matched.len();
            }
        }
        let mut open_idx: Option<usize> = None;
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Enter))
            && !matched.is_empty()
        {
            open_idx = Some(matched[self.workspace_selected.min(matched.len() - 1)]);
        }
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape)) {
            self.show_workspaces = false;
        }
        if let Some(idx) = open_idx {
            let workspace = self.config.workspaces[idx].clone();
            self.show_workspaces = false;
            self.workspace_filter.clear();
            self.workspace_selected = 0;
            self.open_workspace(ctx, &workspace);
            return;
        }
        if !self.show_workspaces {
            return;
        }

        let mut close_requested = false;
        let mut open_clicked: Option<Workspace> = None;
        let mut create_local = false;
        let mut create_ssh = false;
        egui::Window::new("workspaces_panel")
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
                    "workspaces_header_title",
                    "工作区",
                    "本地目录 · SSH 主机",
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
                            .id_salt("workspaces_header_esc")
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
                        let search_id = egui::Id::new("workspaces_panel_search");
                        let search_resp = dialog::form_input(
                            ui,
                            search_id,
                            &mut self.workspace_filter,
                            "搜索工作区（名称 / 路径 / 主机）",
                            ui.available_width(),
                            false,
                            false,
                        );
                        if search_resp.changed() {
                            self.workspace_selected = 0;
                        }
                        ui.memory_mut(|m| m.request_focus(search_id));
                        ui.add_space(8.0);

                        let matched = self.filtered_workspace_indices();
                        if self.config.workspaces.is_empty() {
                            ui.vertical_centered(|ui| {
                                ui.add_space(12.0);
                                ui.label(
                                    egui::RichText::new("暂无工作区")
                                        .size(12.0)
                                        .color(theme.text_muted),
                                );
                                ui.add_space(4.0);
                                ui.label(
                                    egui::RichText::new("⌘B 收藏当前会话 · 或新增一个")
                                        .monospace()
                                        .size(10.5)
                                        .color(theme.text_secondary),
                                );
                                ui.add_space(10.0);
                                ui.horizontal(|ui| {
                                    ui.spacing_mut().item_spacing.x = 8.0;
                                    if ui
                                        .add(dialog::primary_button(theme, "新增本地目录"))
                                        .clicked()
                                    {
                                        create_local = true;
                                    }
                                    if ui
                                        .add(dialog::secondary_button(theme, "新增 SSH 主机"))
                                        .clicked()
                                    {
                                        create_ssh = true;
                                    }
                                });
                                ui.add_space(12.0);
                            });
                            return;
                        }
                        if matched.is_empty() {
                            ui.vertical_centered(|ui| {
                                ui.add_space(12.0);
                                ui.label(
                                    egui::RichText::new("无匹配工作区")
                                        .size(12.0)
                                        .color(theme.text_muted),
                                );
                                ui.add_space(12.0);
                            });
                            return;
                        }
                        egui::ScrollArea::vertical()
                            .id_salt("workspaces_panel_scroll")
                            .max_height(280.0)
                            .show(ui, |ui| {
                                ui.spacing_mut().item_spacing.y = 2.0;
                                const ROW_H: f32 = 44.0;
                                const AVATAR: f32 = 22.0;
                                for (pos, idx) in matched.iter().enumerate() {
                                    let workspace = self.config.workspaces[*idx].clone();
                                    let row_id = egui::Id::new(("workspaces_panel_row", *idx));
                                    let resp = paint_workspace_row(
                                        ui,
                                        &workspace,
                                        pos == self.workspace_selected,
                                        row_id,
                                        ROW_H,
                                        AVATAR,
                                    );
                                    if resp.clicked() {
                                        open_clicked = Some(workspace);
                                    }
                                }
                            });
                    });
                ui.add_space(10.0);
            });
        if close_requested {
            self.show_workspaces = false;
        }
        if create_local {
            self.show_workspaces = false;
            self.open_workspace_editor(0);
        }
        if create_ssh {
            self.show_workspaces = false;
            self.open_workspace_editor(1);
        }
        if let Some(workspace) = open_clicked {
            self.show_workspaces = false;
            self.workspace_filter.clear();
            self.workspace_selected = 0;
            self.open_workspace(ctx, &workspace);
        }
    }

    // ===== workspace_manager（设置卡片：主机管理 + 项目管理 融合为一张卡） =====
    /// 渲染设置里的工作区管理卡片（本地目录与 SSH 主机统一列表）。
    ///
    /// 行样式与快捷菜单/⌘O 面板共用 `paint_workspace_row`；行右侧
    /// ↑ ↓ 改 删四个紧凑按钮（改进入表单、删直接生效，与旧主机行一致）；
    /// 底部两个新增入口（本地目录 / SSH 主机）与展开的编辑表单。
    fn workspace_manager(&mut self, ui: &mut egui::Ui) {
        let theme = crate::theme::current_theme();
        let mut move_up: Option<usize> = None;
        let mut move_down: Option<usize> = None;
        let mut edit_index: Option<usize> = None;
        let mut delete_index: Option<usize> = None;
        let mut open_index: Option<usize> = None;
        let mut start_local_form = false;
        let mut start_ssh_form = false;

        if self.config.workspaces.is_empty() && self.workspace_edit.is_none() {
            ui.label(
                egui::RichText::new("暂无工作区：收藏当前会话（⌘B）或新增一个")
                    .size(11.5)
                    .color(theme.text_muted),
            );
            ui.add_space(6.0);
        }

        let count = self.config.workspaces.len();
        if count > 0 {
            let mut sorted: Vec<usize> = (0..count).collect();
            sorted.sort_by_key(|&i| i); // 保持原顺序（可直接切片遍历）
            egui::ScrollArea::vertical()
                .id_salt("workspace_manager_scroll")
                .max_height(200.0)
                .show(ui, |ui| {
                    ui.spacing_mut().item_spacing.y = 2.0;
                    for (pos, &idx) in sorted.iter().enumerate() {
                        let workspace = self.config.workspaces[idx].clone();
                        let row_id = egui::Id::new(("workspace_manager_row", idx));
                        let edit_mode = self
                            .workspace_edit
                            .as_ref()
                            .is_some_and(|e| e.index == Some(idx));
                        let resp =
                            paint_workspace_row(ui, &workspace, edit_mode, row_id, 44.0, 22.0);
                        if resp.clicked() {
                            open_index = Some(idx);
                        }
                        // 操作按钮叠在行右缘（后注册在上层，命中优先）。
                        let row_rect = resp.rect;
                        let buttons: [(&str, &str); 4] =
                            [("↑", "上移"), ("↓", "下移"), ("改", "编辑"), ("删", "删除")];
                        let mut x = row_rect.right() - 6.0;
                        for (label, hover) in buttons {
                            let rect = egui::Rect::from_center_size(
                                egui::pos2(x - 12.0, row_rect.center().y),
                                egui::vec2(24.0, 24.0),
                            );
                            x -= 26.0;
                            let btn = egui::Button::new(
                                egui::RichText::new(label)
                                    .size(11.0)
                                    .color(theme.text_secondary),
                            )
                            .fill(egui::Color32::TRANSPARENT)
                            .stroke(egui::Stroke::NONE)
                            .corner_radius(crate::theme::tokens::RADIUS_ITEM)
                            .min_size(egui::vec2(24.0, 24.0));
                            if ui.put(rect, btn).on_hover_text(hover).clicked() {
                                match label {
                                    "↑" => move_up = Some(pos),
                                    "↓" => move_down = Some(pos),
                                    "改" => edit_index = Some(idx),
                                    _ => delete_index = Some(idx),
                                }
                            }
                        }
                    }
                });
        }

        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            if ui
                .add(dialog::secondary_button(theme, "新增本地目录"))
                .clicked()
            {
                start_local_form = true;
            }
            if ui
                .add(dialog::secondary_button(theme, "新增 SSH 主机"))
                .clicked()
            {
                start_ssh_form = true;
            }
        });

        // ---- 应用动作（借用结束后统一做） ----
        if let Some(pos) = move_up {
            if pos > 0 {
                self.config.workspaces.swap(pos, pos - 1);
                self.save_config();
            }
        }
        if let Some(pos) = move_down {
            if pos + 1 < self.config.workspaces.len() {
                self.config.workspaces.swap(pos, pos + 1);
                self.save_config();
            }
        }
        if let Some(pos) = delete_index {
            let removed = self.config.workspaces.remove(pos);
            if !self.save_config() {
                self.config.workspaces.insert(pos, removed);
            } else if self
                .workspace_edit
                .as_ref()
                .is_some_and(|e| e.index == Some(pos))
            {
                self.workspace_edit = None;
            }
        }
        if let Some(idx) = edit_index {
            let mut edit = WorkspaceEdit::from_workspace(&self.config.workspaces[idx]);
            edit.index = Some(idx);
            self.workspace_edit = Some(edit);
        }
        if start_local_form {
            self.open_workspace_editor(0);
        }
        if start_ssh_form {
            self.open_workspace_editor(1);
        }
        if let Some(idx) = open_index {
            let workspace = self.config.workspaces[idx].clone();
            let ctx = ui.ctx().clone();
            // 打开即离开设置：用户意图是"去这个工作区"，设置窗口不该继续
            // 盖着新标签（与旧版双击主机行连接即关设置一致）。
            self.show_settings = false;
            self.open_workspace(&ctx, &workspace);
        }
    }

    /// 工作区编辑对话框（居中模态；⌘N / 快捷菜单 / 设置管理共用同一表单）。
    ///
    /// 类型分段（本地目录 / SSH 主机）切换字段组；保存时校验并落盘，
    /// 失败保留表单内容（用户改了半截不该被清空）。
    fn workspace_edit_dialog(&mut self, ctx: &egui::Context) {
        let theme = crate::theme::current_theme();
        // 「使用当前终端目录」的数据源：活动本地会话的即时目录（无本地标签则禁用）。
        let current_terminal_dir = self
            .tabs
            .get(self.active_tab)
            .and_then(|tab| tab.active_view().fresh_local_directory());
        let Some(mut edit) = self.workspace_edit.take() else {
            return;
        };
        let field_width = 320.0;
        let mut open = true;
        let mut close = false;
        let mut save = false;
        egui::Window::new("workspace_edit_dialog")
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
                    "workspace_edit_header",
                    if edit.index.is_some() {
                        "编辑工作区"
                    } else {
                        "新增工作区"
                    },
                    "本地目录与 SSH 主机统一管理",
                );
                header.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.add_space(12.0);
                    if dialog::close_icon_button(ui, "关闭（Esc）") {
                        close = true;
                    }
                });

                egui::Frame::new()
                    .inner_margin(egui::Margin::symmetric(18, 14))
                    .show(ui, |ui| {
                        ui.set_min_width(field_width);
                        // 类型分段（本地 / SSH）。
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = 6.0;
                            for (kind, label) in [(0usize, "本地目录"), (1, "SSH 主机")] {
                                let selected = edit.kind == kind;
                                let btn = egui::Button::new(
                                    egui::RichText::new(label).size(12.0).color(if selected {
                                        theme.accent
                                    } else {
                                        theme.text_secondary
                                    }),
                                )
                                .fill(if selected {
                                    theme.accent_soft
                                } else {
                                    egui::Color32::TRANSPARENT
                                })
                                .stroke(egui::Stroke::NONE)
                                .corner_radius(crate::theme::tokens::RADIUS_ITEM)
                                .min_size(egui::vec2(88.0, 26.0));
                                if ui.add(btn).clicked() {
                                    edit.kind = kind;
                                }
                            }
                        });
                        ui.add_space(12.0);
                        dialog::section_title(
                            ui,
                            if edit.kind == 0 {
                                "目录"
                            } else {
                                "连接身份"
                            },
                        );
                        ui.add_space(8.0);
                        let name_id = egui::Id::new("workspace_edit_name");
                        if !edit.name_focused {
                            ui.memory_mut(|m| m.request_focus(name_id));
                            edit.name_focused = true;
                        }
                        dialog::field_label(ui, "名称");
                        ui.add_space(4.0);
                        dialog::form_input(
                            ui,
                            name_id,
                            &mut edit.name,
                            "名称（可选）",
                            field_width,
                            false,
                            false,
                        );
                        ui.add_space(8.0);
                        if edit.kind == 0 {
                            dialog::field_label(ui, "目录");
                            ui.add_space(4.0);
                            ui.horizontal(|ui| {
                                ui.spacing_mut().item_spacing.x = 8.0;
                                dialog::form_input(
                                    ui,
                                    egui::Id::new("workspace_edit_path"),
                                    &mut edit.path,
                                    "本地目录路径",
                                    176.0,
                                    false,
                                    false,
                                );
                                if ui
                                    .add(dialog::field_action_button(theme, "浏览…"))
                                    .clicked()
                                {
                                    if let Some(dir) = rfd::FileDialog::new()
                                        .set_directory(&edit.path)
                                        .pick_folder()
                                    {
                                        edit.path = dir.display().to_string();
                                    }
                                }
                                // 一键取当前终端目录（旧项目表单的同款入口）。
                                if ui
                                    .add(dialog::field_action_button(theme, "使用当前终端目录"))
                                    .clicked()
                                {
                                    if let Some(dir) = current_terminal_dir.as_deref() {
                                        edit.path = dir.to_string();
                                    }
                                }
                            });
                            ui.add_space(10.0);
                            dialog::field_label(ui, "启动命令（可选）");
                            ui.add_space(4.0);
                            dialog::form_input(
                                ui,
                                egui::Id::new("workspace_edit_command"),
                                &mut edit.command,
                                "打开后自动执行一行命令",
                                field_width,
                                false,
                                false,
                            );
                        } else {
                            dialog::field_label(ui, "用户名");
                            ui.add_space(4.0);
                            dialog::form_input(
                                ui,
                                egui::Id::new("workspace_edit_user"),
                                &mut edit.user,
                                "用户名，例如 root",
                                field_width,
                                false,
                                false,
                            );
                            ui.add_space(10.0);
                            dialog::field_label(ui, "主机与端口");
                            ui.add_space(4.0);
                            ui.horizontal(|ui| {
                                ui.spacing_mut().item_spacing.x = 8.0;
                                let port_invalid = !edit.port.trim().is_empty()
                                    && edit.port.trim().parse::<u16>().is_err();
                                dialog::form_input(
                                    ui,
                                    egui::Id::new("workspace_edit_host"),
                                    &mut edit.host,
                                    "主机名或 IP",
                                    230.0,
                                    false,
                                    false,
                                );
                                dialog::form_input(
                                    ui,
                                    egui::Id::new("workspace_edit_port"),
                                    &mut edit.port,
                                    "22",
                                    82.0,
                                    false,
                                    port_invalid,
                                );
                            });
                            ui.add_space(10.0);
                            dialog::field_label(ui, "认证");
                            ui.add_space(4.0);
                            dialog::auth_segmented(ui, &mut edit.auth_kind);
                            ui.add_space(6.0);
                            if edit.auth_kind == 0 {
                                dialog::form_input(
                                    ui,
                                    egui::Id::new("workspace_edit_password"),
                                    &mut edit.password,
                                    "密码",
                                    field_width,
                                    true,
                                    false,
                                );
                            } else {
                                dialog::form_input(
                                    ui,
                                    egui::Id::new("workspace_edit_key"),
                                    &mut edit.key_path,
                                    "私钥路径（如 ~/.ssh/id_ed25519）",
                                    field_width,
                                    false,
                                    false,
                                );
                                ui.add_space(6.0);
                                dialog::form_input(
                                    ui,
                                    egui::Id::new("workspace_edit_passphrase"),
                                    &mut edit.passphrase,
                                    "私钥口令（可选）",
                                    field_width,
                                    true,
                                    false,
                                );
                            }
                            ui.add_space(10.0);
                            dialog::field_label(ui, "远端目录与启动命令（可选）");
                            ui.add_space(4.0);
                            dialog::form_input(
                                ui,
                                egui::Id::new("workspace_edit_rpath"),
                                &mut edit.path,
                                "远端起始目录（可空）",
                                field_width,
                                false,
                                false,
                            );
                            ui.add_space(6.0);
                            dialog::form_input(
                                ui,
                                egui::Id::new("workspace_edit_rcommand"),
                                &mut edit.command,
                                "连接后自动执行一行命令",
                                field_width,
                                false,
                                false,
                            );
                        }
                        ui.add_space(14.0);
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.spacing_mut().item_spacing.x = 8.0;
                            if ui.add(dialog::primary_button(theme, "保存")).clicked() {
                                save = true;
                            }
                            if ui.add(dialog::secondary_button(theme, "取消")).clicked() {
                                close = true;
                            }
                        });
                    });
            });
        if close || !open {
            // 用户主动关闭：丢弃表单（下次打开回到默认值），并从设置打开的
            // 场景里把设置窗口还回来。
            self.close_workspace_editor();
            return;
        }
        if save {
            // 校验/保存失败时不丢用户输入：先算结果，失败则原样放回表单。
            match edit.to_workspace() {
                Ok(workspace) => {
                    if let Err(message) = self.commit_workspace_value(edit.index, workspace) {
                        self.show_toast(message, true);
                        self.workspace_edit = Some(edit);
                    } else {
                        self.close_workspace_editor();
                    }
                }
                Err(message) => {
                    self.show_toast(message, true);
                    self.workspace_edit = Some(edit);
                }
            }
            return;
        }
        self.workspace_edit = Some(edit);
    }

    /// 关闭工作区编辑对话框（取消 / 关闭按钮 / 保存成功后统一走这里）。
    ///
    /// 从设置里打开的编辑器（`settings_before_workspace_edit`）关闭后把
    /// 设置弹窗还回来——Esc 与取消按钮行为一致，不能让用户"弹窗一关全没了"。
    fn close_workspace_editor(&mut self) {
        self.workspace_edit = None;
        if self.settings_before_workspace_edit {
            self.show_settings = true;
        }
        self.settings_before_workspace_edit = false;
    }

    /// 保存工作区（`index` 为 Some 时覆盖既有条目，否则追加）。
    fn commit_workspace_value(
        &mut self,
        index: Option<usize>,
        workspace: Workspace,
    ) -> Result<(), String> {
        match index {
            Some(index) if index < self.config.workspaces.len() => {
                let previous = std::mem::replace(&mut self.config.workspaces[index], workspace);
                if !self.save_config() {
                    self.config.workspaces[index] = previous;
                    return Err("保存工作区失败".into());
                }
            }
            _ => {
                self.config.workspaces.push(workspace);
                if !self.save_config() {
                    self.config.workspaces.pop();
                    return Err("保存工作区失败".into());
                }
            }
        }
        Ok(())
    }

    /// 渲染标题条（顶部拖拽区，Kaku/WezTerm 集成标题栏风格）。
    ///
    /// 整条为窗口拖拽区（原生红绿灯悬浮在其上、不参与 egui 布局）：
    /// - 拖动（左键）→ `StartDrag` 交系统接管窗口移动；
    /// - 双击 → 切换窗口 zoom（macOS 标题栏标准行为）。
    ///
    /// Sense 必须带 CLICK（click_and_drag）：纯 drag 在按下瞬间即判为
    /// drag_started，第二次按下时指针几乎不动，`double_clicked` 永假。
    fn title_bar(&mut self, ui: &mut egui::Ui) {
        let drag_rect = ui.max_rect();
        let drag_resp = ui.interact(
            drag_rect,
            egui::Id::new("titlebar_drag"),
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
    }

    /// 渲染底部标签栏（Kaku 风：方角标签、激活面块、右端承载入口与状态）。
    ///
    /// 与 Kaku `tab_bar_at_bottom = true` 同构：标签栏在最底部、与终端同
    /// 底色；激活标签用 surface_active（`bg_elevated`）凸起、非激活灰字
    /// （`text_muted`），hover 用 surface（`bg_panel`）提亮文字转主色；
    /// 无圆角、无描边（WezTerm 非 fancy 标签）。
    /// 与 Kaku 的差异（有意保留）：单标签时本栏仍显示——Mino 没有 macOS
    /// 菜单栏，入口按钮（工作区/设置）与会话状态都在这里承载；「×」在
    /// hover/选中时可见（Kaku 同款 hover 提示），否则透明但仍可被辅助功能
    /// 定位（既有测试以 × 数量判定标签数）。
    fn tab_bar(&mut self, ui: &mut egui::Ui) {
        let theme = crate::theme::current_theme();
        ui.set_min_height(TAB_BAR_HEIGHT);
        ui.horizontal_centered(|ui| {
            ui.spacing_mut().item_spacing.x = 0.0;
            let mut switch_to: Option<usize> = None;
            let mut close_idx: Option<usize> = None;
            // 重命名的提交/取消在循环外统一应用（循环借 `self.tabs`，
            // 循环内不能对同一字段取可变借用）。
            let mut rename_commit: Option<(u64, String)> = None;
            let mut rename_cancel = false;
            for (i, tab) in self.tabs.iter().enumerate() {
                // 本地标签显示当前目录的末级名，全路径作悬浮提示。
                let (title, title_tooltip) = tab.title();
                let selected = i == self.active_tab;
                // 标签宽度 = 标题实际宽度 + 内边距 + 关闭位，夹到 [MIN, MAX]。
                let text_font = egui::FontId::proportional(TAB_TITLE_SIZE);
                let text_w = ui
                    .fonts_mut(|f| {
                        f.layout_no_wrap(title.clone(), text_font.clone(), egui::Color32::WHITE)
                    })
                    .size()
                    .x;
                let width = (text_w + TAB_PADDING_L + TAB_PADDING_R + TAB_CLOSE_WIDTH)
                    .clamp(TAB_MIN_WIDTH, TAB_MAX_WIDTH);
                let (rect, resp) =
                    ui.allocate_exact_size(egui::vec2(width, TAB_BAR_HEIGHT), egui::Sense::click());
                let hovered = resp.hovered();
                // 激活底必须画在内容之前（painter 顺序：先画的在下层）。
                let bg = if selected {
                    theme.bg_elevated
                } else if hovered {
                    theme.bg_panel
                } else {
                    egui::Color32::TRANSPARENT
                };
                ui.painter().rect_filled(rect, 0.0, bg);
                // 注意力点（Bell 或失败命令；Kaku `bell_tab_indicator` 同款）：
                // 非激活标签在关闭位画橙点（该状态 × 本就透明不可见）。
                let has_attention = tab.panes.iter().any(|pane| pane.view.attention());
                if has_attention && !selected && !hovered {
                    ui.painter().circle_filled(
                        egui::pos2(
                            rect.right() - TAB_PADDING_R - TAB_CLOSE_WIDTH / 2.0,
                            rect.center().y,
                        ),
                        3.5,
                        theme.attention,
                    );
                }

                // 标题（真 Label：可被辅助功能/kittest 找到、可 hover）。
                let text_color = if selected || hovered {
                    theme.text_primary
                } else {
                    theme.text_muted
                };
                let text_area = egui::Rect::from_min_max(
                    egui::pos2(rect.left() + TAB_PADDING_L, rect.top()),
                    egui::pos2(
                        rect.right() - TAB_PADDING_R - TAB_CLOSE_WIDTH,
                        rect.bottom(),
                    ),
                );
                let mut child = ui.new_child(
                    egui::UiBuilder::new()
                        .max_rect(text_area)
                        .layout(egui::Layout::left_to_right(egui::Align::Center)),
                );
                // 双击标题进入重命名（Kaku：双击标签标题改名）。编辑态下
                // 用 TextEdit 取代标题；Enter/失焦提交，Esc 取消。
                let renaming = self
                    .renaming_tab
                    .as_ref()
                    .is_some_and(|(id, _)| *id == tab.id);
                if renaming {
                    let buffer = &mut self.renaming_tab.as_mut().expect("重命名态已判定存在").1;
                    let edit = child.add(
                        egui::TextEdit::singleline(buffer)
                            .id(egui::Id::new(("tab_rename", tab.id)))
                            .desired_width(f32::INFINITY),
                    );
                    child.memory_mut(|m| {
                        if !m.has_focus(edit.id) {
                            m.request_focus(edit.id);
                        }
                    });
                    let committed =
                        edit.lost_focus() || child.input(|i| i.key_pressed(egui::Key::Enter));
                    let cancelled = child.input(|i| i.key_pressed(egui::Key::Escape));
                    if cancelled {
                        rename_cancel = true;
                    } else if committed {
                        rename_commit = Some((tab.id, buffer.clone()));
                    }
                } else {
                    let label = child.add(
                        egui::Label::new(
                            egui::RichText::new(&title)
                                .size(TAB_TITLE_SIZE)
                                .color(text_color),
                        )
                        .truncate()
                        // Label 默认 `Sense::hover`，不感知点击；双击重命名
                        // 需要它可交互（单击仍要切换标签，见下）。
                        .sense(egui::Sense::click()),
                    );
                    // 悬浮展示终端当前目录的全路径。
                    let label = match &title_tooltip {
                        Some(full) => label.on_hover_text(full),
                        None => label,
                    };
                    // 单击标签（标题区域）切换；双击进入重命名。
                    if label.clicked() {
                        switch_to = Some(i);
                    }
                    if label.double_clicked() {
                        self.renaming_tab = Some((tab.id, title.clone()));
                    }
                    // 点击标签空白处也切换（关闭按钮在更上层、命中优先级更高）。
                    if resp.clicked() {
                        switch_to = Some(i);
                    }
                }
                // 关闭「×」：Kaku 同款 hover/选中时可见；隐藏时保持透明但
                // 仍在辅助功能树里（测试以 × 数量判定标签数）。
                let close_alpha = if selected || hovered { 200 } else { 0 };
                let close_rect = egui::Rect::from_center_size(
                    egui::pos2(
                        rect.right() - TAB_PADDING_R - TAB_CLOSE_WIDTH / 2.0,
                        rect.center().y,
                    ),
                    egui::vec2(TAB_CLOSE_WIDTH, TAB_BAR_HEIGHT),
                );
                let close_btn = egui::Button::new(
                    egui::RichText::new("×")
                        .size(13.0)
                        .color(theme.text_muted.gamma_multiply(close_alpha as f32 / 255.0)),
                )
                .fill(egui::Color32::TRANSPARENT)
                .stroke(egui::Stroke::NONE)
                .corner_radius(0.0)
                .min_size(egui::vec2(TAB_CLOSE_WIDTH, TAB_BAR_HEIGHT));
                if ui
                    .put(close_rect, close_btn)
                    .on_hover_text("关闭标签页")
                    .clicked()
                {
                    close_idx = Some(i);
                }
            }

            // 新建本地终端（＋，Kaku `show_new_tab_button_in_tab_bar`）。
            let (plus_rect, _) = ui.allocate_exact_size(
                egui::vec2(NEW_TAB_WIDTH, TAB_BAR_HEIGHT),
                egui::Sense::hover(),
            );
            let plus_hovered = ui.rect_contains_pointer(plus_rect);
            if plus_hovered {
                ui.painter().rect_filled(plus_rect, 0.0, theme.bg_panel);
            }
            let plus_btn =
                egui::Button::new(egui::RichText::new("＋").size(13.0).color(if plus_hovered {
                    theme.text_primary
                } else {
                    theme.text_muted
                }))
                .fill(egui::Color32::TRANSPARENT)
                .stroke(egui::Stroke::NONE)
                .corner_radius(0.0)
                .min_size(egui::vec2(NEW_TAB_WIDTH, TAB_BAR_HEIGHT));
            if ui
                .put(plus_rect, plus_btn)
                .on_hover_text("新建本地终端（⌘T）")
                .clicked()
            {
                self.new_local_tab(ui.ctx());
            }

            // 顶到右侧：齿轮（设置）→ 状态指示 → 快速 SSH → 项目收藏。
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                ui.add_space(6.0);
                if settings_gear_button(ui) && self.workspace_edit.is_none() {
                    self.show_settings = true;
                }
                self.session_status(ui);
                // 工作区（SSH 与项目收藏的融合入口）：点击弹出统一列表，
                // 单击条目直接打开（本地建标签 / SSH 连接）。push_id 固定
                // 按钮 Id——Popup::menu 的开关状态按 Id 记忆，自动 Id 帧间
                // 漂移会让菜单闪断。
                let ws_btn_resp = ui.push_id("workspace_quick", workspace_quick_button).inner;
                egui::Popup::menu(&ws_btn_resp).show(|ui| self.workspace_quick_menu(ui));
            });

            if rename_cancel {
                self.renaming_tab = None;
            }
            if let Some((id, text)) = rename_commit {
                self.renaming_tab = None;
                if let Some(target) = self.tabs.iter_mut().find(|tab| tab.id == id) {
                    let trimmed = text.trim().to_string();
                    target.title_override = (!trimmed.is_empty()).then_some(trimmed);
                }
            }
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
        // 工作区编辑是前台模态窗口，不能让快捷键把设置窗口叠到它上面。
        if self.workspace_edit.is_some() {
            return;
        }
        self.show_settings = !self.show_settings;
    }

    /// 会话/SFTP 状态指示（底部标签栏右端；原独立状态栏的替代）。
    ///
    /// Kaku 没有状态栏——连接状态/传输错误以 toast 与标签状态点表达；
    /// Mino 保留这一小块**持久**指示（toast 一闪而过易忽略，SFTP 连接
    /// 错误需要持续可见），只显示必要信息、无独立背景。
    fn session_status(&mut self, ui: &mut egui::Ui) {
        let theme = crate::theme::current_theme();
        if let Some(tab) = self.tabs.get(self.active_tab) {
            let session = tab.active_view().session();
            if session.has_exited() {
                status_dot(ui, theme.danger, false);
                ui.colored_label(theme.danger, "会话已退出");
            }
            // SFTP 主机名从标签页的 SFTP 视图读取（每个标签绑定自己的连接，
            // 多远程标签共存时不会显示成最后一次连接的主机名）。
            if let Some(sftp) = &tab.sftp {
                status_dot(ui, theme.accent2, false);
                ui.colored_label(theme.accent2, format!("SFTP · {}", sftp.host_name()));
            }
        }
        if self.pending_sftp.is_some() {
            loading_hint(ui, "SFTP 连接中…");
        }
        if self.pending_local.is_some() {
            // ⌘T 新建终端时当前标签仍在显示：给出明确等待反馈。
            loading_hint(ui, "终端启动中…");
        }
        if let Some(e) = &self.sftp_error {
            status_dot(ui, theme.danger, false);
            // 截断显示，避免长错误把标签栏撑满；hover 看全文。
            ui.add(
                egui::Label::new(egui::RichText::new(e).size(11.0).color(theme.danger)).truncate(),
            )
            .on_hover_text(format!("{e}\n\n重新连接主机可再次尝试"));
        }
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
        + label_width("新建标签")
        + label_width("·")
        + key_width("⌘D")
        + label_width("分屏")
        + label_width("·")
        + key_width("⌘O")
        + label_width("工作区")
        + label_width("·")
        + key_width("⌘F")
        + label_width("搜索")
        + ITEM_SPACING * 10.0;
    // `ui.horizontal` 会占满父布局宽度且默认从左侧排布；根据当前 UI 的
    // 真实中心坐标补前导空间，避免嵌套布局的 available_width 造成偏移。
    let leading_space = (ui.max_rect().center().x - ui.cursor().left() - row_width * 0.5).max(0.0);
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = ITEM_SPACING;
        ui.add_space(leading_space);
        shortcut_key(ui, theme, "⌘T");
        ui.label(
            egui::RichText::new("新建标签")
                .size(11.5)
                .color(theme.text_muted),
        );
        ui.label(egui::RichText::new("·").size(11.5).color(theme.text_muted));
        shortcut_key(ui, theme, "⌘D");
        ui.label(
            egui::RichText::new("分屏")
                .size(11.5)
                .color(theme.text_muted),
        );
        ui.label(egui::RichText::new("·").size(11.5).color(theme.text_muted));
        shortcut_key(ui, theme, "⌘O");
        ui.label(
            egui::RichText::new("工作区")
                .size(11.5)
                .color(theme.text_muted),
        );
        ui.label(egui::RichText::new("·").size(11.5).color(theme.text_muted));
        shortcut_key(ui, theme, "⌘F");
        ui.label(
            egui::RichText::new("搜索")
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
    /// 退出时保存窗口快照（Kaku：关闭/隐藏窗口时自动保存布局）。
    fn on_exit(&mut self) {
        self.save_window_snapshot();
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        // 缓存 ctx 供非 UI 回调使用（如复制 tab 时构造 Session）。
        self.last_ctx = ctx.clone();

        // ==================== 快捷键 ====================
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::N))
            && self.workspace_edit.is_none()
        {
            self.open_workspace_editor(1);
        }
        // ⌘⌥⇧T：恢复上次窗口快照（Kaku Restore Previous Window）。
        if ctx.input_mut(|i| {
            i.consume_key(
                egui::Modifiers::COMMAND | egui::Modifiers::ALT | egui::Modifiers::SHIFT,
                egui::Key::T,
            )
        }) {
            self.restore_window_snapshot(&ctx);
        }
        // ⌘L：AI 聊天面板开关（Kaku Open AI Chat）。
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::L)) {
            if self.ai_panel.is_open() {
                self.ai_panel.close();
            } else {
                // 打开时带上终端上下文（目录 + 最近可见输出）。
                let context = self.active_terminal_context();
                self.ai_panel.set_context(context);
                self.ai_panel.open();
            }
        }
        // ⌘⇧A：AI 助手设置（Kaku Open AI settings：Mino 里就是设置弹窗的
        // AI 卡片；已配置与否都直接打开设置）。
        if ctx.input_mut(|i| {
            i.consume_key(
                egui::Modifiers::COMMAND | egui::Modifiers::SHIFT,
                egui::Key::A,
            )
        }) {
            self.show_settings = true;
        }
        // ⌘⇧O：标签导航器（Kaku Tab Navigator）。
        if ctx.input_mut(|i| {
            i.consume_key(
                egui::Modifiers::COMMAND | egui::Modifiers::SHIFT,
                egui::Key::O,
            )
        }) {
            let items = self.tab_navigator_items();
            self.launcher = Some((
                LauncherMode::Tabs,
                crate::views::launcher::Launcher::tab_navigator(items),
            ));
        }
        // ⌘⇧P：命令面板（Kaku Command Palette）。
        if ctx.input_mut(|i| {
            i.consume_key(
                egui::Modifiers::COMMAND | egui::Modifiers::SHIFT,
                egui::Key::P,
            )
        }) {
            let items = Self::command_palette_items();
            self.launcher = Some((
                LauncherMode::Palette,
                crate::views::launcher::Launcher::command_palette(items),
            ));
        }
        // ⌘⇧E：把最近的 AI 修复建议填入提示符（Kaku Apply suggestion）。
        if ctx.input_mut(|i| {
            i.consume_key(
                egui::Modifiers::COMMAND | egui::Modifiers::SHIFT,
                egui::Key::E,
            )
        }) {
            self.apply_ai_suggestion();
        }
        // ⌘⇧T：重开最近关闭的标签（Kaku Reopen closed tab；必须在 ⌘T
        // 之前消费——`consume_key` 是子集匹配，⌘T 会把 ⌘⇧T 一并吃掉）。
        if ctx.input_mut(|i| {
            i.consume_key(
                egui::Modifiers::COMMAND | egui::Modifiers::SHIFT,
                egui::Key::T,
            )
        }) {
            self.reopen_closed_tab(&ctx);
        }
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::T)) {
            self.new_local_tab(&ctx);
        }
        // ⌘, 切换设置弹窗（macOS 标准"应用偏好设置"快捷键）。
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::Comma)) {
            self.toggle_settings();
        }
        // ⌘O：切换项目打开面板（新建连接模态时不响应，见 `toggle_workspaces`）。
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::O)) {
            self.toggle_workspaces();
        }
        // ⌘B：收藏当前本地终端目录/远程主机为工作区（原 ⌘D；⌘D 让位给
        // Kaku 的分屏语义）。
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::B)) {
            self.bookmark_current_workspace();
        }
        // ⌘⇧W：关闭整个标签（含全部窗格；Kaku CloseCurrentTab）。
        // 必须在 ⌘W 之前消费（子集匹配：⌘W 会吃掉 ⌘⇧W）。
        if ctx.input_mut(|i| {
            i.consume_key(
                egui::Modifiers::COMMAND | egui::Modifiers::SHIFT,
                egui::Key::W,
            )
        }) && !self.tabs.is_empty()
        {
            self.close_tab(self.active_tab);
        }
        // ⌘W：智能关闭（Kaku 语义）——多窗格关窗格、多标签关标签、
        // 最后一个标签最小化窗口（见 `smart_close`）。
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::W))
            && !self.tabs.is_empty()
        {
            self.smart_close(&ctx);
        }
        // ⌘D / ⌘⇧D：分屏（Kaku：Cmd+D 左右并排、Cmd+Shift+D 上下堆叠）。
        // 必须先消费带 Shift 的组合：egui 的 `consume_key` 走
        // `matches_logically`（子集匹配、允许多余修饰），先匹配 ⌘D 会把
        // ⌘⇧D 一并吃掉。
        if ctx.input_mut(|i| {
            i.consume_key(
                egui::Modifiers::COMMAND | egui::Modifiers::SHIFT,
                egui::Key::D,
            )
        }) {
            self.split_active_pane(&ctx, SplitAxis::Vertical);
        }
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::D)) {
            self.split_active_pane(&ctx, SplitAxis::Horizontal);
        }
        // ⌘⇧[ / ⌘⇧]：上一个/下一个标签（Kaku ActivateTabRelative）。
        if ctx.input_mut(|i| {
            i.consume_key(
                egui::Modifiers::COMMAND | egui::Modifiers::SHIFT,
                egui::Key::OpenBracket,
            )
        }) && self.tabs.len() > 1
        {
            let count = self.tabs.len();
            self.active_tab = (self.active_tab + count - 1) % count;
        }
        if ctx.input_mut(|i| {
            i.consume_key(
                egui::Modifiers::COMMAND | egui::Modifiers::SHIFT,
                egui::Key::CloseBracket,
            )
        }) && self.tabs.len() > 1
        {
            self.active_tab = (self.active_tab + 1) % self.tabs.len();
        }
        // ⌘F：搜索浮层（Kaku 搜索，作用于当前窗格）。
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::F)) {
            if let Some(tab) = self.tabs.get_mut(self.active_tab) {
                tab.active_view_mut().toggle_search();
            }
        }
        // ⌘K：清屏并清空回滚（Kaku：Ctrl+L + ClearScrollback）。
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::K)) {
            if let Some(tab) = self.tabs.get_mut(self.active_tab) {
                tab.active_view_mut().clear_scrollback_and_screen();
            }
        }
        // ⌘M：最小化窗口（Kaku Minimize）。
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::M)) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
        }
        // ⌘⌃F：切换全屏（Kaku ToggleFullScreen；macOS 原生全屏空间）。
        if ctx.input_mut(|i| {
            i.consume_key(
                egui::Modifiers::COMMAND | egui::Modifiers::CTRL,
                egui::Key::F,
            )
        }) {
            let fullscreen = ctx.input(|i| i.viewport().fullscreen.unwrap_or(false));
            ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(!fullscreen));
        }
        // ⌘⇧S：切换拆分方向（Kaku TogglePaneSplitDirection）。
        if ctx.input_mut(|i| {
            i.consume_key(
                egui::Modifiers::COMMAND | egui::Modifiers::SHIFT,
                egui::Key::S,
            )
        }) {
            self.toggle_split_direction(&ctx);
        }
        // ⌘⇧Enter：放大/还原活动窗格（Kaku TogglePaneZoomState）。
        if ctx.input_mut(|i| {
            i.consume_key(
                egui::Modifiers::COMMAND | egui::Modifiers::SHIFT,
                egui::Key::Enter,
            )
        }) {
            self.toggle_pane_zoom(&ctx);
        }
        // ⌘⌥方向键：焦点在窗格间移动（必须排在普通 ⌘←/→ 之前消费：
        // 子集匹配下 ⌘← 的模式也会命中 ⌘⌥←）。
        for (key, dx, dy) in [
            (egui::Key::ArrowLeft, -1, 0),
            (egui::Key::ArrowRight, 1, 0),
            (egui::Key::ArrowUp, 0, -1),
            (egui::Key::ArrowDown, 0, 1),
        ] {
            if ctx
                .input_mut(|i| i.consume_key(egui::Modifiers::COMMAND | egui::Modifiers::ALT, key))
            {
                self.focus_pane_neighbor(&ctx, dx, dy);
            }
        }
        // ⌘⌃方向键：调整分割比例（Kaku AdjustPaneSize，每次 5 列）。
        for (key, axis, delta) in [
            (egui::Key::ArrowLeft, SplitAxis::Horizontal, -1.0),
            (egui::Key::ArrowRight, SplitAxis::Horizontal, 1.0),
            (egui::Key::ArrowUp, SplitAxis::Vertical, -1.0),
            (egui::Key::ArrowDown, SplitAxis::Vertical, 1.0),
        ] {
            if ctx
                .input_mut(|i| i.consume_key(egui::Modifiers::COMMAND | egui::Modifiers::CTRL, key))
            {
                self.adjust_pane_size(&ctx, axis, delta);
            }
        }
        // 终端字号：⌘+ 放大 / ⌘- 缩小 / ⌘0 重置（macOS 终端通用约定）。
        //
        // 必须在终端输入处理**之前**消费：`consume_key` 把按键从事件队列移除，
        // 终端的 `Event::Key`/`Text` 分支就看不到它——否则 `+`/`-` 会同时被
        // 写进 PTY（调一次字号、命令行多一个字符）。`matches_logically` 会忽略
        // 额外的 Shift：⌘Shift+=（美式键盘的 `+`）与 ⌘= 走同一分支，一次
        // `Plus` + 一次 `Equals` 即覆盖两种键位。⌘0 是数字行（`Num0`），与
        // ⌘1-9 的标签切换同修饰但键不同，无冲突。
        // 缩放静默落盘（`silent=true`）：连续按键每步都弹 toast 会刷屏；
        // 字号本身由 `apply_font_size_and_persist` 内钳制 10-24pt。
        if ctx.input_mut(|i| {
            i.consume_key(egui::Modifiers::COMMAND, egui::Key::Plus)
                || i.consume_key(egui::Modifiers::COMMAND, egui::Key::Equals)
        }) {
            let next = self.config.font_size + TerminalView::FONT_SIZE_STEP;
            self.apply_font_size_and_persist(&ctx, next, true);
        }
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::Minus)) {
            let next = self.config.font_size - TerminalView::FONT_SIZE_STEP;
            self.apply_font_size_and_persist(&ctx, next, true);
        }
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::Num0)) {
            self.apply_font_size_and_persist(&ctx, TerminalView::DEFAULT_FONT_SIZE, false);
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
        if (self.workspace_edit.is_some() || self.show_settings || self.show_workspaces)
            && ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape))
        {
            if self.workspace_edit.is_some() {
                self.close_workspace_editor();
            } else if self.show_settings {
                self.show_settings = false;
            } else if self.show_workspaces {
                self.show_workspaces = false;
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
        self.poll_local_spawn(&ctx);
        for tab in &mut self.tabs {
            for pane in &mut tab.panes {
                pane.view.drain_background_events();
            }
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
        self.poll_vendor_bootstrap();
        self.poll_ai_tasks(&ctx);

        // ==================== 标题条（顶部拖拽区） ====================
        // Kaku 的窗口样板（WezTerm `INTEGRATED_BUTTONS|RESIZE`）：标题条与
        // 终端同底色（深色 #15141b / 浅色 #FFFCF0），只有原生红绿灯与
        // 空白拖拽区（无标题文字，见 main.rs 的 viewport 设置）。
        // 外观跟随系统：auto 偏好下系统深浅色切换时在这里重套主题。
        let want_theme = crate::theme::resolve_appearance(
            self.config.appearance.trim(),
            ctx.system_theme() != Some(egui::Theme::Light),
        );
        if want_theme != crate::theme::current_theme_index() {
            crate::theme::set_theme(&ctx, want_theme);
        }
        let theme = crate::theme::current_theme();
        let titlebar_frame = egui::Frame::new().fill(theme.bg_app);
        egui::Panel::top("titlebar")
            .exact_size(TITLEBAR_HEIGHT)
            .frame(titlebar_frame)
            .show(ui, |ui| {
                self.title_bar(ui);
            });

        // ==================== 底部标签栏（Kaku `tab_bar_at_bottom = true`） ====================
        // Kaku 的标签栏在窗口最底部、与终端同底色；标签栏右侧承载 Mino 的
        // 入口按钮（工作区 / 设置）与会话状态指示（不再有独立状态栏）。
        let tab_frame = egui::Frame::new().fill(theme.bg_header);
        egui::Panel::bottom("tab_bar")
            .frame(tab_frame)
            .show(ui, |ui| {
                self.tab_bar(ui);
            });
        // 活动标签被查看后清除注意力点（Kaku：标签激活即消）。
        if let Some(tab) = self.tabs.get_mut(self.active_tab) {
            for pane in &mut tab.panes {
                pane.view.clear_attention();
            }
        }

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
            // 先取终端侧上下文，再对 sftp 取可变借用（借用拆分同定位逻辑）。
            let terminal_cwd = tab.active_view().current_directory();
            if let Some(sftp) = &mut tab.sftp {
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
        // 设置/工作区/AI 面板/Launcher 都是前台内容；终端仍渲染后台输出，
        // 但禁止它消费键盘、鼠标和滚轮事件，避免输入穿透。
        let terminal_input_enabled = !self.show_settings
            && !self.show_workspaces
            && !self.ai_panel.is_open()
            && self
                .launcher
                .as_ref()
                .is_none_or(|(_, launcher)| !launcher.is_open());
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
            } else {
                self.render_tab_panes(ui, terminal_input_enabled);
                // 右键菜单里的跨窗格动作（分屏/标签/字号）由这里统一执行
                //（`TerminalView` 只记录意图，聚焦同步已让目标窗格成为活动窗格）。
                let mctx = ctx.clone();
                let mut menu_actions: Vec<crate::views::terminal_view::MenuChoice> = Vec::new();
                if let Some(tab) = self.tabs.get_mut(self.active_tab) {
                    for pane in &mut tab.panes {
                        menu_actions.extend(pane.view.take_menu_choices());
                    }
                }
                for choice in menu_actions {
                    use crate::views::terminal_view::MenuChoice;
                    match choice {
                        MenuChoice::ResetFontSize => {
                            self.apply_font_size_and_persist(
                                &ctx,
                                TerminalView::DEFAULT_FONT_SIZE,
                                false,
                            );
                        }
                        MenuChoice::SplitRight => {
                            self.split_active_pane(&ctx, SplitAxis::Horizontal);
                        }
                        MenuChoice::SplitDown => {
                            self.split_active_pane(&ctx, SplitAxis::Vertical);
                        }
                        MenuChoice::ClosePane => {
                            let pane = self
                                .tabs
                                .get(self.active_tab)
                                .map(|tab| tab.active_pane)
                                .unwrap_or_default();
                            self.close_pane(&mctx, self.active_tab, pane);
                        }
                        MenuChoice::NewTab => self.new_local_tab(&ctx),
                        MenuChoice::CloseTab if !self.tabs.is_empty() => {
                            self.close_tab(self.active_tab);
                        }
                        // 本地动作已在 `TerminalView` 内执行，不会到这里。
                        _ => {}
                    }
                }
                // tabby 风格：远程标签页终端右上角悬浮 SFTP 开关按钮。
                if let Some(tab) = self.tabs.get_mut(self.active_tab) {
                    if tab.sftp.is_some() && sftp_floating_button(ui, tab.sftp_open) {
                        tab.sftp_open = !tab.sftp_open;
                    }
                }
            }
        });

        // ==================== 对话框与 Toast ====================
        if let Some(tab) = self.tabs.get_mut(self.active_tab) {
            if let Some(sftp) = &mut tab.sftp {
                sftp.show_dialog(&ctx);
            }
        }
        self.workspace_edit_dialog(&ctx);
        if self.show_settings {
            self.settings_panel(&ctx);
        }
        if self.show_workspaces {
            self.workspaces_panel(&ctx);
        }
        // ==================== AI 面板与 Launcher ====================
        // ⌘L 聊天面板：事件回填提示符（不自动执行）或引导去设置。
        for event in self.ai_panel.show(&ctx) {
            match event {
                crate::views::ai_panel::AiPanelEvent::InsertCommand(command) => {
                    if let Some(tab) = self.tabs.get_mut(self.active_tab) {
                        tab.active_view_mut().inject_command(&command);
                    }
                }
                crate::views::ai_panel::AiPanelEvent::OpenSettings => {
                    self.show_settings = true;
                }
            }
        }
        // Launcher（标签导航 / 命令面板）：选中即执行；打开期间每帧同步
        // 标签列表（标签增减时导航器数据不陈旧）。
        let mut launcher_choice: Option<String> = None;
        let mut launcher_close = false;
        // 先把标签导航器的新条目算出来（借用分离：set_items 期间不能同时
        // 借 self 的其余字段）。
        let navigator_items = self
            .launcher
            .as_ref()
            .filter(|(mode, _)| matches!(mode, LauncherMode::Tabs))
            .map(|_| self.tab_navigator_items());
        if let Some((mode, launcher)) = &mut self.launcher {
            if !launcher.is_open() {
                launcher_close = true;
            } else {
                // 标签导航器每帧刷新条目（标签增减/改名）；命令面板是静态表。
                if matches!(mode, LauncherMode::Tabs) {
                    if let Some(items) = navigator_items {
                        launcher.set_items(items);
                    }
                }
                launcher_choice = launcher.show(&ctx);
            }
        }
        if launcher_close {
            if let Some((_, launcher)) = &mut self.launcher {
                launcher.close();
            }
            self.launcher = None;
        }
        if let Some(key) = launcher_choice {
            self.launcher = None;
            if let Some(index) = key.strip_prefix("tab:") {
                if let Ok(index) = index.parse::<usize>() {
                    if index < self.tabs.len() {
                        self.active_tab = index;
                    }
                }
            } else {
                self.run_palette_action(&ctx, &key);
            }
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
    }
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
            target: None,
        });
        harness.run_steps(3);
        assert!(
            harness.state().pending_local.is_none(),
            "结果到达后 pending 应被消费"
        );
        assert_eq!(harness.state().tabs.len(), 2, "新会话应挂上第二个标签");
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

    /// 完整渲染工作区编辑对话框（SSH 分支字段 + 类型切换），不应崩溃。
    #[test]
    fn 工作区编辑对话框完整渲染() {
        use kittest::Queryable;

        let config_path = test_config_path("workspace-dialog-render");
        let mut harness = egui_kittest::Harness::new_eframe(|cc| {
            MinoApp::new_with_config(cc, config_path.clone())
        });
        harness.run_steps(6);
        harness.state_mut().open_workspace_editor(1);
        harness.run_steps(6);

        for label in ["名称", "用户名", "主机与端口", "认证"] {
            assert!(
                harness.query_all_by_label(label).next().is_some(),
                "{label} 字段缺失"
            );
        }
        // 切到本地目录分支：目录与启动命令字段出现。
        harness.get_by_label("本地目录").click();
        harness.run_steps(3);
        assert!(
            harness.query_all_by_label("目录").next().is_some(),
            "目录字段缺失"
        );
        assert!(
            harness
                .query_all_by_label("启动命令（可选）")
                .next()
                .is_some(),
            "启动命令字段缺失"
        );
        // Esc 关闭。
        harness.event(egui::Event::Key {
            key: egui::Key::Escape,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        });
        harness.run_steps(3);
        assert!(
            harness.state().workspace_edit.is_none(),
            "Esc 应关闭工作区编辑对话框"
        );
        std::fs::remove_file(&config_path).ok();
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
            let mut tab = TerminalTab::new(
                id,
                "测试主机".into(),
                TerminalView::new(session),
                1,
                SessionOrigin::Local,
            );
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
            let mut tab = TerminalTab::new(
                1,
                "定位测试".into(),
                TerminalView::new(session),
                1,
                SessionOrigin::Local,
            );
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
            tab.active_view_mut().show(ui);
        });
        harness.run_steps(6);
        // 借用已结束（harness 只在闭包内借 tab）；后续直接操作 tab。
        drop(harness);
        MinoApp::begin_locate_terminal(&mut tab, &ctx);
        // 本地会话有内核 cwd 可读时直接导航、不注入 `pwd`（macOS 新实现）；
        // 读不到时才回退到 `pwd` 探测（Linux CI 等无 libproc 实现的平台）。
        if tab.active_view().session().child_current_dir().is_some() {
            assert!(
                tab.locate_pending.is_none(),
                "内核 cwd 可读时应直接导航，不进入 pwd 探测等待"
            );
        } else {
            assert!(
                tab.locate_pending.is_some(),
                "空闲提示符下定位应进入 pwd 探测等待"
            );
            assert!(
                !tab.active_view().auto_pwd_ready(),
                "探测注入后应等待终端输出"
            );
        }

        // 场景二：有未执行输入 → 不注入，直接用已知目录回退。
        tab.locate_pending = None;
        tab.active_view_mut().cancel_fresh_pwd();
        tab.active_view_mut().push_workdir_text_for_test("echo hi");
        MinoApp::begin_locate_terminal(&mut tab, &ctx);
        assert!(
            tab.locate_pending.is_none(),
            "有未执行输入时不应进入探测等待"
        );
        assert!(tab.active_view().auto_pwd_ready());
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
        let term_arc = tab.active_view().session().term();
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

    /// 工作区表单默认值：SSH 分支默认 root/22/密码；本地分支默认 HOME。
    #[test]
    fn 工作区表单默认值() {
        let edit = WorkspaceEdit::default();
        assert_eq!(edit.kind, 1, "默认进入 SSH 分支（沿用新建连接的肌肉记忆）");
        assert_eq!(edit.user, "root");
        assert_eq!(edit.port, "22");
        assert!(edit.name.is_empty());
        assert!(edit.host.is_empty());
        assert_eq!(edit.port.trim().parse::<u16>().unwrap(), 22);
        assert_eq!(
            edit.path,
            std::env::var("HOME").unwrap_or_else(|_| "~".into()),
            "本地分支的路径默认 HOME"
        );
    }

    /// 完整应用：设置里的"新增 SSH 主机"打开工作区编辑器，字段齐备不崩溃。
    #[test]
    fn 点击新增工作区不崩溃() {
        use kittest::Queryable;

        let config_path = test_config_path("workspace-new-no-crash");
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
        for _ in 0..3 {
            harness.step();
        }

        harness.get_by_label("新增 SSH 主机").click();
        for _ in 0..8 {
            harness.step();
        }

        for label in ["名称", "用户名", "主机与端口", "认证"] {
            assert!(
                harness.query_all_by_label(label).next().is_some(),
                "{label} 字段缺失"
            );
        }
        std::fs::remove_file(&config_path).ok();
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
            workspaces: vec![Workspace::Ssh {
                name: profile.name.clone(),
                host: profile.host.clone(),
                port: profile.port,
                user: profile.user.clone(),
                auth: profile.auth.clone(),
                path: String::new(),
                command: String::new(),
            }],
            ..Default::default()
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

        // 设置里单击工作区行即打开（与快捷菜单/⌘O 面板一致的「单击即开」）。
        {
            let host_row = harness.get_by_label("链路测试");
            host_row.click();
        }
        harness.step();

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
            "单击工作区行连接成功后设置弹窗应自动关闭"
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
    /// 设置里「新增 SSH 主机」打开工作区编辑器（设置自动让位），
    /// 保存后两个弹窗都关闭、配置落盘。
    #[test]
    fn 新增工作区后关闭设置弹窗() {
        use kittest::{NodeT, Queryable};

        let config_path = test_config_path("workspace-new-close");
        let mut harness = egui_kittest::Harness::new_eframe(|cc| {
            MinoApp::new_with_config(cc, config_path.clone())
        });
        harness.run_steps(6);

        // ⌘, 打开设置弹窗 → 点"新增 SSH 主机"。
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
        harness.get_by_label("新增 SSH 主机").click();
        for _ in 0..3 {
            harness.step();
        }
        assert!(
            harness.state().workspace_edit.is_some(),
            "工作区编辑对话框应打开"
        );
        assert!(!harness.state().show_settings, "设置应让位给编辑器");

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

        // 点"保存" → 对话框关闭、配置落盘（保存不发起连接）。
        let save_btn = harness
            .root()
            .query_all_by_role(accesskit::Role::Button)
            .find(|n| n.accesskit_node().label() == Some("保存".to_string()))
            .expect("找不到保存按钮");
        save_btn.click();
        for _ in 0..3 {
            harness.step();
        }
        assert!(
            harness.state().workspace_edit.is_none(),
            "点保存后编辑器应关闭"
        );
        assert_eq!(harness.state().config.workspaces.len(), 1, "工作区应落盘");
        let Workspace::Ssh { host, .. } = &harness.state().config.workspaces[0] else {
            panic!("新增的应是 SSH 工作区");
        };
        assert_eq!(host, "127.0.0.1");
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
            workspaces: vec![Workspace::Ssh {
                name: profile.name.clone(),
                host: profile.host.clone(),
                port: profile.port,
                user: profile.user.clone(),
                auth: profile.auth.clone(),
                path: String::new(),
                command: String::new(),
            }],
            ..Default::default()
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
        // 设置里单击工作区行即打开（统一的「单击即开」交互）。
        {
            let host_row = harness.get_by_label("链路测试");
            host_row.click();
        }
        harness.step();

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

    /// 回归：设置弹窗里的字号滑块必须画得出轨道（egui 滑块的轨道固定取
    /// 全局 `widgets.inactive.bg_fill`＝面板色，而弹窗卡片底同为面板色，
    /// 轨道与卡片完全同色——用户反馈「轨道看不清」，肉眼只剩手柄的 1px
    /// 描边）。断言两点：未填充轨道明显亮于卡片背景、已填充段是主题 accent。
    #[test]
    fn 字号滑块轨道在卡片上可辨() {
        use kittest::Queryable;

        let _guard = THEME_TEST_LOCK.lock().unwrap();
        let config_path = test_config_path("slider-rail");
        let mut harness = egui_kittest::Harness::new_eframe(|cc| {
            MinoApp::new_with_config(cc, config_path.clone())
        });
        harness.run_steps(6);
        // ⌘, 打开设置弹窗；外观卡片在主机/项目卡片之后，先滚到滑块再取样。
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
        harness
            .root()
            .query_by_role(accesskit::Role::Slider)
            .expect("设置弹窗里应存在字号滑块")
            .scroll_to_me();
        for _ in 0..3 {
            harness.step();
        }
        let rect = harness
            .root()
            .query_by_role(accesskit::Role::Slider)
            .expect("设置弹窗里应存在字号滑块")
            .rect();
        let img = harness.render().expect("渲染失败");
        let theme = crate::theme::current_theme();
        // kittest 默认 pixels_per_point = 1：逻辑坐标可直接当像素索引。
        let px = |x: f32, y: f32| -> [u8; 4] {
            let p = img.get_pixel(x.round() as u32, y.round() as u32);
            [p[0], p[1], p[2], p[3]]
        };
        let channel = |p: [u8; 4]| p[0].max(p[1]).max(p[2]) as i32;
        // 卡片底：滑块 rect 顶缘附近（轨道上沿之上，无其它控件）。
        let card = px(rect.right() - 4.0, rect.top() + 4.0);
        // 未填充轨道：右端内侧（字号 14pt 靠范围左端，右侧必为未填充段）。
        let rail = px(rect.right() - 6.0, rect.center().y);
        assert!(
            channel(rail) - channel(card) >= 12,
            "滑块轨道与卡片背景对比不足：rail={rail:?} card={card:?}"
        );
        // 已填充段：左端内侧应是主题 accent（trailing_fill）。
        let filled = px(rect.left() + 6.0, rect.center().y);
        let near = |p: [u8; 4], c: egui::Color32| {
            let d = |a: u8, b: u8| (i32::from(a) - i32::from(b)).abs();
            d(p[0], c.r()) <= 24 && d(p[1], c.g()) <= 24 && d(p[2], c.b()) <= 24
        };
        assert!(
            near(filled, theme.accent),
            "滑块已填充段应为主题 accent：filled={filled:?} accent={:?}",
            (theme.accent.r(), theme.accent.g(), theme.accent.b())
        );
        std::fs::remove_file(&config_path).ok();
    }

    /// Kaku 深/浅两套外观切换并渲染截图（视觉验证用）。
    #[test]
    fn 深浅外观渲染截图() {
        use kittest::Queryable;

        let _guard = THEME_TEST_LOCK.lock().unwrap();
        let config_path = test_config_path("theme-shots");
        let mut harness = egui_kittest::Harness::new_eframe(|cc| {
            MinoApp::new_with_config(cc, config_path.clone())
        });
        harness.run_steps(6);
        // 外观下拉在设置弹窗里，先用 ⌘, 打开弹窗。
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

        for (theme_name, expect_name) in [
            ("深色（Kaku Dark）", "Kaku Dark"),
            ("浅色（Kaku Light）", "Kaku Light"),
        ] {
            // 项目管理卡片加入后设置内容变长，外观卡片可能被挤出可视区：
            // 先滚到下拉框再点，否则点击落在别的控件上、选项菜单弹不出。
            {
                // 设置里有多个下拉（外观 + AI 认证）：按当前值选外观那个。
                use kittest::NodeT as _;
                let combo = harness
                    .root()
                    .query_all_by_role(accesskit::Role::ComboBox)
                    .find(|node| {
                        let value = node.accesskit_node().value().unwrap_or_default();
                        value.contains("跟随")
                            || value.contains("Kaku")
                            || value.contains("深色")
                            || value.contains("浅色")
                    })
                    .expect("外观下拉不存在");
                combo.scroll_to_me();
            }
            for _ in 0..3 {
                harness.step();
            }
            {
                use kittest::NodeT as _;
                harness
                    .root()
                    .query_all_by_role(accesskit::Role::ComboBox)
                    .find(|node| {
                        let value = node.accesskit_node().value().unwrap_or_default();
                        value.contains("跟随")
                            || value.contains("Kaku")
                            || value.contains("深色")
                            || value.contains("浅色")
                    })
                    .expect("外观下拉不存在")
                    .click();
            }
            for _ in 0..3 {
                harness.step();
            }
            harness
                .get_by_role_and_label(accesskit::Role::Button, theme_name)
                .click();
            for _ in 0..3 {
                harness.step();
            }
            // 切换即落盘：hosts.toml 里应记录外观偏好（回归：曾只改内存，
            // 退出重进回到原来的）。
            let saved = HostConfig::load(&config_path).expect("外观切换后配置应可读");
            assert_ne!(saved.appearance, "", "外观偏好应持久化到配置");
            assert_eq!(crate::theme::current_theme().name, expect_name);
            let img = harness.render().expect("渲染失败");
            let out = format!("/tmp/mino_theme_{expect_name}.png");
            img.save(&out).expect("保存截图失败");
            eprintln!("已保存：{out}");
        }
        std::fs::remove_file(&config_path).ok();
    }

    /// 回归：切换外观偏好后重启应用，应恢复上次所选偏好而非跟随系统默认。
    #[test]
    fn 外观偏好重启后保持() {
        let _guard = THEME_TEST_LOCK.lock().unwrap();
        let config_path = test_config_path("theme-persist");
        HostConfig {
            hosts: Vec::new(),
            projects: Vec::new(),
            ..Default::default()
        }
        .save(&config_path)
        .expect("写入测试配置失败");

        // 首次启动 → 强制切到「浅色」（经持久化路径写回配置）。
        let mut harness = egui_kittest::Harness::new_eframe(|cc| {
            MinoApp::new_with_config(cc, config_path.clone())
        });
        harness.run_steps(6);
        let theme_ctx = harness.state().last_ctx.clone();
        harness
            .state_mut()
            .apply_appearance_and_persist(&theme_ctx, "light");
        harness.run_steps(3);
        assert_eq!(crate::theme::current_theme().name, "Kaku Light");
        drop(harness);

        // 第二次启动（模拟退出重进）→ 应仍是「浅色」，不受系统外观影响。
        let mut harness = egui_kittest::Harness::new_eframe(|cc| {
            MinoApp::new_with_config(cc, config_path.clone())
        });
        harness.run_steps(6);
        assert_eq!(
            crate::theme::current_theme().name,
            "Kaku Light",
            "重启后外观应保持上次选择，而非回到跟随系统"
        );

        std::fs::remove_file(&config_path).ok();
    }

    /// 字号调节：快捷键缩放与 ⌘0 重置（只改全局字号，不写字符进终端）。
    ///
    /// 快捷键在 `MinoApp::ui` 顶部消费（终端 `handle_input` 之前）：不断言
    /// PTY 内容（终端是否收到 `+` 由锁序与事件消费保证），断言三件事——
    /// 全标签即时生效、落盘可读、超界钳制。
    #[test]
    fn 快捷键缩放终端字号() {
        let config_path = test_config_path("font-zoom");
        let mut harness = egui_kittest::Harness::new_eframe(|cc| {
            MinoApp::new_with_config(cc, config_path.clone())
        });
        harness.run_steps(6);
        assert_eq!(harness.state().tabs.len(), 1, "测试构建应同步建好首个标签");
        let base = harness.state().config.font_size;

        // ⌘+ 放大 1pt（美式键盘 Shift 修饰被 matches_logically 忽略，
        // 与 ⌘= 同分支；这里直接发 Plus 覆盖主路径）。
        harness.event(egui::Event::Key {
            key: egui::Key::Plus,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::COMMAND,
        });
        harness.run_steps(3);
        assert_eq!(
            harness.state().config.font_size,
            base + TerminalView::FONT_SIZE_STEP,
            "⌘+ 应放大一档"
        );
        assert_eq!(
            harness.state().tabs[0].active_view().font_size(),
            base + TerminalView::FONT_SIZE_STEP,
            "存量标签应即时生效"
        );

        // ⌘- 缩小回原值。
        harness.event(egui::Event::Key {
            key: egui::Key::Minus,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::COMMAND,
        });
        harness.run_steps(3);
        assert!(
            (harness.state().config.font_size - base).abs() < f32::EPSILON,
            "⌘- 应缩小回原值，实际：{}",
            harness.state().config.font_size
        );

        // 非默认字号下 ⌘0 重置回 13。
        let font_ctx = harness.state().last_ctx.clone();
        harness
            .state_mut()
            .apply_font_size_and_persist(&font_ctx, 18.0, true);
        harness.run_steps(3);
        harness.event(egui::Event::Key {
            key: egui::Key::Num0,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::COMMAND,
        });
        harness.run_steps(3);
        assert_eq!(
            harness.state().config.font_size,
            TerminalView::DEFAULT_FONT_SIZE,
            "⌘0 应重置为默认字号"
        );

        // 超界钳制：999 → 24 上限。
        harness
            .state_mut()
            .apply_font_size_and_persist(&font_ctx, 999.0, true);
        assert_eq!(
            harness.state().config.font_size,
            TerminalView::MAX_FONT_SIZE,
            "超大字号应钳制到上限"
        );

        // 落盘可读：hosts.toml 里应记录最终字号。
        let saved = HostConfig::load(&config_path).expect("字号调整后配置应可读");
        assert_eq!(
            saved.font_size,
            TerminalView::MAX_FONT_SIZE,
            "字号选择应持久化到配置"
        );

        std::fs::remove_file(&config_path).ok();
    }

    /// 回归：调大字号后重启应用，应恢复上次字号而非默认 13。
    #[test]
    fn 字号调整重启后保持() {
        let config_path = test_config_path("font-persist");
        HostConfig {
            hosts: Vec::new(),
            projects: Vec::new(),
            ..Default::default()
        }
        .save(&config_path)
        .expect("写入测试配置失败");

        // 首次启动 → 经持久化路径调到 17pt。
        let mut harness = egui_kittest::Harness::new_eframe(|cc| {
            MinoApp::new_with_config(cc, config_path.clone())
        });
        harness.run_steps(6);
        let font_ctx = harness.state().last_ctx.clone();
        harness
            .state_mut()
            .apply_font_size_and_persist(&font_ctx, 17.0, true);
        harness.run_steps(3);
        assert_eq!(harness.state().tabs[0].active_view().font_size(), 17.0);
        drop(harness);

        // 第二次启动（模拟退出重进）→ 新标签应继承 17pt。
        let mut harness = egui_kittest::Harness::new_eframe(|cc| {
            MinoApp::new_with_config(cc, config_path.clone())
        });
        harness.run_steps(6);
        assert_eq!(
            harness.state().config.font_size,
            17.0,
            "重启后字号应保持上次选择，而非回到默认"
        );
        assert_eq!(
            harness.state().tabs[0].active_view().font_size(),
            17.0,
            "重启后新标签应继承已保存字号"
        );

        std::fs::remove_file(&config_path).ok();
    }

    /// 外观卡片的字号滑杆存在且可拖动（设置弹窗 → 外观）。
    #[test]
    fn 外观卡片字号滑杆可调() {
        use kittest::Queryable;

        let config_path = test_config_path("font-slider");
        let mut harness = egui_kittest::Harness::new_eframe(|cc| {
            MinoApp::new_with_config(cc, config_path.clone())
        });
        harness.run_steps(6);
        // 设置弹窗现在在 ⌘, 后面，先打开。
        harness.event(egui::Event::Key {
            key: egui::Key::Comma,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::COMMAND,
        });
        harness.run_steps(3);
        assert!(harness.state().show_settings, "⌘, 应打开设置弹窗");

        // 滑杆以数值后缀 " pt" 渲染（egui Slider 数值是可编辑文本）。
        let before = harness.state().config.font_size;
        harness.get_by_label("终端字号");
        // 滑杆拖动走 `changed()` 路径：直接调持久化入口模拟"拖到 16pt"，
        // 断言渲染链路（标签即时生效 + 落盘）；滑杆存在性由上一行保证。
        let font_ctx = harness.state().last_ctx.clone();
        harness
            .state_mut()
            .apply_font_size_and_persist(&font_ctx, 16.0, true);
        harness.run_steps(3);
        assert!(
            (harness.state().config.font_size - 16.0).abs() < f32::EPSILON && before != 16.0,
            "滑杆调整应改变全局字号，之前：{before}"
        );
        assert_eq!(
            harness.state().tabs[0].active_view().font_size(),
            16.0,
            "滑杆调整后存量标签应即时生效"
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

        // 再次 ⌘W（最后一个标签、单窗格）→ 不关标签，最小化窗口
        // （Kaku 的 Cmd+W 智能关闭语义；Mino 用最小化代替隐藏应用）。
        let mut saw_minimized = false;
        harness.event(egui::Event::Key {
            key: egui::Key::W,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::COMMAND,
        });
        for _ in 0..4 {
            harness.step();
            collect_viewport_cmds(&harness, &mut |cmd| {
                if matches!(cmd, egui::ViewportCommand::Minimized(true)) {
                    saw_minimized = true;
                }
            });
        }
        assert_eq!(
            harness.query_all_by_label("×").count(),
            1,
            "最后一个标签应保留（⌘W 最小化窗口而非关闭）"
        );
        assert!(saw_minimized, "⌘W 在唯一标签上应发出 Minimized(true)");
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
            1,
            SessionOrigin::Local,
        )));
        let show = tab.clone();
        let mut harness = egui_kittest::Harness::new_ui(move |ui| {
            show.borrow_mut().active_view_mut().show(ui);
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
                crate::views::terminal_view::tests_grid_text(tab.borrow().active_view().session());
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

    /// 标签栏渲染当前目录的末级名（不再是固定 "本地终端"），悬浮标题显示全路径。
    ///（状态栏已随 Kaku 化移除：状态指示并入标签栏右端。）
    #[test]
    fn 标签栏显示当前目录名() {
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
                harness.state().tabs[0].active_view().session(),
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
            if harness.query_all_by_label("proj-beta").count() == 1 {
                shown = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(60));
        }
        let tab_title = crate::views::terminal_view::tests_grid_text(
            harness.state().tabs[0].active_view().session(),
        );
        assert!(
            shown,
            "标签栏应显示目录名，实际 “proj-beta” 出现 {} 次；终端内容：\n{tab_title}",
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
        let tab = TerminalTab::new(1, "本地终端".into(), view, 1, SessionOrigin::Local);
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
        tab.active_view()
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
        // 关闭全部标签进入空状态（⌘W 在最后一个标签上会最小化窗口——
        // Kaku 语义，见 `smart_close`；这里直接驱动关闭动作）。
        harness.state_mut().close_tab(0);
        for _ in 0..4 {
            harness.step();
        }

        let title = harness.get_by_label(PRODUCT_NAME);
        assert!(
            title.rect().top() > 60.0,
            "空状态标题应避开顶部标题条与窗口上沿，实际 top={}",
            title.rect().top()
        );
        for chip in ["⌘T", "⌘D", "⌘O", "⌘F"] {
            assert!(
                harness.get_by_label(chip).rect().height() > 0.0,
                "空状态应展示 {chip} 键帽"
            );
        }
        let shortcut_left = harness.get_by_label("⌘T").rect().left();
        let shortcut_right = harness.get_by_label("搜索").rect().right();
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
        // 拖拽区在顶部标题条（Kaku 集成标题栏样式）：取整条的水平中点、
        // 竖直中心——避开左上角红绿灯预留区（约 80pt）且条上无其它控件。
        let width = harness
            .ctx
            .input(|i| i.raw.screen_rect.map(|r| r.width()).unwrap_or(800.0));
        egui::pos2(width * 0.5, 14.0)
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
    use super::workspace_test_support::open_workspace_menu;
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
        harness.state_mut().config.workspaces = (0..12)
            .map(|index| Workspace::Ssh {
                name: format!("测试主机 {index}"),
                host: "127.0.0.1".into(),
                port: 22,
                user: "root".into(),
                auth: mino_core::config::Auth::Password(String::new()),
                path: String::new(),
                command: String::new(),
            })
            .collect();
        harness.state_mut().show_settings = true;
        harness.run_steps(3);

        // 人工构造 scrollback 并先向上滚动；若滚轮穿透，向下滚时该值会变小。
        {
            let term = harness.state().tabs[harness.state().active_tab]
                .active_view()
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
                .active_view()
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
                .active_view()
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
        harness.get_by_label("工作区");

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
        assert!(harness.state().workspace_edit.is_some());
        assert!(!harness.state().show_settings);

        harness.event(egui::Event::Key {
            key: egui::Key::Escape,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        });
        harness.run_steps(3);
        assert!(
            !harness.state().workspace_edit.is_some(),
            "Esc 应关闭新建连接对话框"
        );
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
        assert!(harness.state().workspace_edit.is_some());

        harness.event(egui::Event::Key {
            key: egui::Key::Escape,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        });
        harness.run_steps(3);
        assert!(
            !harness.state().workspace_edit.is_some(),
            "Esc 应关闭新建连接对话框"
        );
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
        harness.get_by_label("工作区");
    }

    /// 工作区编辑每次打开都重置：关闭丢弃输入、重开回到默认值；
    /// 打开期间重复 ⌘N 不打断正在填写的表单。
    #[test]
    fn 工作区编辑每次打开都重置表单() {
        use kittest::Queryable;

        let config_path = test_config_path("workspace-edit-reset");
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
        harness.get_by_label("新增 SSH 主机").click();
        harness.run_steps(3);

        let name_input = harness
            .root()
            .query_all_by_role(accesskit::Role::TextInput)
            .next()
            .expect("找不到名称输入框");
        name_input.click();
        harness.run_steps(2);
        harness.event(egui::Event::Text("上一次连接".into()));
        harness.run_steps(3);
        assert_eq!(
            harness
                .state()
                .workspace_edit
                .as_ref()
                .map(|e| e.name.clone()),
            Some("上一次连接".to_string())
        );

        // 弹窗已经打开时重复按 ⌘N 不应重置用户正在填写的表单。
        harness.event(egui::Event::Key {
            key: egui::Key::N,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::COMMAND,
        });
        harness.run_steps(3);
        assert_eq!(
            harness
                .state()
                .workspace_edit
                .as_ref()
                .map(|e| e.name.clone()),
            Some("上一次连接".to_string())
        );

        harness.get_by_label("取消").click();
        harness.run_steps(3);
        assert!(
            harness.state().workspace_edit.is_none(),
            "取消后对话框应关闭"
        );

        harness.get_by_label("新增 SSH 主机").click();
        harness.run_steps(3);
        assert!(
            harness.state().workspace_edit.is_some(),
            "第二次点击应重新打开对话框"
        );
        let edit = harness
            .state()
            .workspace_edit
            .as_ref()
            .expect("工作区编辑表单应存在");
        assert_eq!(edit.name, "", "名称不应残留");
        assert_eq!(edit.host, "", "主机不应残留");
        assert_eq!(edit.password, "", "密码不应残留");
        assert_eq!(edit.user, "root", "默认用户名应恢复");
        assert_eq!(edit.port, "22", "默认端口应恢复");

        std::fs::remove_file(&config_path).ok();
    }

    /// 工作区列表行：短名与长名的名称/详情左缘对齐（行模板共用）。
    #[test]
    fn 工作区列表行对齐() {
        use kittest::Queryable;

        let config_path = test_config_path("workspace-card-align");
        let config = HostConfig {
            workspaces: vec![
                Workspace::Ssh {
                    name: "短名".into(),
                    host: "10.0.0.1".into(),
                    port: 22,
                    user: "root".into(),
                    auth: mino_core::config::Auth::Password("x".into()),
                    path: String::new(),
                    command: String::new(),
                },
                Workspace::Ssh {
                    name: "一个很长的工作区显示名称".into(),
                    host: "192.168.31.233".into(),
                    port: 22022,
                    user: "ubuntu".into(),
                    auth: mino_core::config::Auth::Key {
                        path: PathBuf::from("~/.ssh/id_ed25519"),
                        passphrase: None,
                    },
                    path: String::new(),
                    command: String::new(),
                },
            ],
            ..Default::default()
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
        let long_name = harness.get_by_label("一个很长的工作区显示名称").rect();
        let short_addr = harness.get_by_label("root@10.0.0.1").rect();
        let long_addr = harness.get_by_label("ubuntu@192.168.31.233:22022").rect();
        // 类型徽标两行同列。
        let badges: Vec<_> = harness.root().query_all_by_label("SSH").collect();
        assert!(badges.len() >= 2, "两行都应有 SSH 类型徽标");

        assert!(
            (short_name.left() - long_name.left()).abs() < 1.0,
            "短名与长名应左对齐：{} vs {}",
            short_name.left(),
            long_name.left()
        );
        assert!(
            (short_name.left() - short_addr.left()).abs() < 1.0,
            "名称与详情应同一列"
        );
        assert!(
            (long_name.left() - long_addr.left()).abs() < 1.0,
            "长名与详情应同一列"
        );

        std::fs::remove_file(&config_path).ok();
    }

    /// 标签栏「工作区」按钮：点击弹出统一列表，单击 SSH 行直接发起连接。
    #[test]
    fn 工作区菜单连接主机() {
        use kittest::Queryable;

        let config_path = test_config_path("workspace-quick");
        let config = HostConfig {
            workspaces: vec![Workspace::Ssh {
                name: "快捷主机".into(),
                host: "127.0.0.1".into(),
                // 端口 9（discard）必然拒绝连接：只验证"发起连接"，
                // 不依赖测试 sshd。
                port: 9,
                user: "root".into(),
                auth: mino_core::config::Auth::Password("x".into()),
                path: String::new(),
                command: String::new(),
            }],
            ..Default::default()
        };
        config.save(&config_path).expect("写入测试配置失败");

        let mut harness = egui_kittest::Harness::new_eframe(|cc| {
            MinoApp::new_with_config(cc, config_path.clone())
        });
        harness.run_steps(6);

        open_workspace_menu(&mut harness);
        harness.get_by_label("快捷主机");

        harness.get_by_label("快捷主机").click();
        harness.run_steps(6);
        assert_eq!(
            harness.state().pending_label,
            "快捷主机",
            "单击 SSH 工作区行应直接发起连接"
        );
        assert!(
            harness
                .root()
                .query_all_by_label("root@127.0.0.1")
                .next()
                .is_none(),
            "点击工作区行后快捷菜单应关闭"
        );

        std::fs::remove_file(&config_path).ok();
    }

    /// 无工作区时，「工作区」快捷菜单应提示并可新增。
    #[test]
    fn 工作区菜单无条目提示() {
        use kittest::Queryable;

        let config_path = test_config_path("workspace-quick-empty");
        let mut harness = egui_kittest::Harness::new_eframe(|cc| {
            MinoApp::new_with_config(cc, config_path.clone())
        });
        harness.run_steps(6);

        open_workspace_menu(&mut harness);
        harness.get_by_label("暂无工作区");
        harness.get_by_label("新增工作区…");

        std::fs::remove_file(&config_path).ok();
    }

    /// 工作区菜单行：短名与长名的名称/详情左缘对齐（行模板共用）。
    #[test]
    fn 工作区菜单行左对齐() {
        use kittest::Queryable;

        let config_path = test_config_path("workspace-quick-align");
        let config = HostConfig {
            workspaces: vec![
                Workspace::Ssh {
                    name: "短名".into(),
                    host: "10.0.0.1".into(),
                    port: 9,
                    user: "root".into(),
                    auth: mino_core::config::Auth::Password("x".into()),
                    path: String::new(),
                    command: String::new(),
                },
                Workspace::Ssh {
                    name: "很长的工作区名称对齐".into(),
                    host: "192.168.31.233".into(),
                    port: 9,
                    user: "ubuntu".into(),
                    auth: mino_core::config::Auth::Password("x".into()),
                    path: String::new(),
                    command: String::new(),
                },
            ],
            ..Default::default()
        };
        config.save(&config_path).expect("写入测试配置失败");

        let mut harness = egui_kittest::Harness::new_eframe(|cc| {
            MinoApp::new_with_config(cc, config_path.clone())
        });
        harness.run_steps(6);
        open_workspace_menu(&mut harness);

        let short = harness.get_by_label("短名").rect();
        let long = harness.get_by_label("很长的工作区名称对齐").rect();
        let short_addr = harness.get_by_label("root@10.0.0.1:9").rect();
        let long_addr = harness.get_by_label("ubuntu@192.168.31.233:9").rect();

        assert!(
            (short.left() - long.left()).abs() < 1.0,
            "短名与长名应左对齐：短={:.1} 长={:.1}",
            short.left(),
            long.left()
        );
        assert!(
            (short.left() - short_addr.left()).abs() < 1.0,
            "名称与地址应同一列左对齐"
        );
        assert!(
            (long.left() - long_addr.left()).abs() < 1.0,
            "长名与地址应同一列左对齐"
        );

        std::fs::remove_file(&config_path).ok();
    }
}

#[cfg(test)]
mod project_tests {
    use super::workspace_test_support::open_workspace_menu;
    use super::*;
    use std::time::{Duration, Instant};

    fn project_base(tag: &str) -> PathBuf {
        let base =
            std::env::temp_dir().join(format!("mino-proj-test-{}-{}", tag, std::process::id()));
        std::fs::remove_dir_all(&base).ok();
        base
    }

    /// 快捷菜单单击本地工作区打开新标签；坏路径只 toast、不建标签。
    #[test]
    fn 工作区菜单单击打开新标签() {
        use kittest::Queryable;
        let base = project_base("menu");
        let dir_a = base.join("alpha");
        std::fs::create_dir_all(&dir_a).unwrap();

        let config_path = test_config_path("workspaces-menu");
        let config = HostConfig {
            workspaces: vec![
                Workspace::Local {
                    name: "工作区甲".into(),
                    path: dir_a.clone(),
                    command: String::new(),
                },
                Workspace::Local {
                    name: "坏路径".into(),
                    path: base.join("gone"),
                    command: String::new(),
                },
            ],
            ..Default::default()
        };
        config.save(&config_path).expect("写入测试配置失败");

        let mut harness = egui_kittest::Harness::new_eframe(|cc| {
            MinoApp::new_with_config(cc, config_path.clone())
        });
        harness.run_steps(6);
        let tabs_before = harness.state().tabs.len();

        open_workspace_menu(&mut harness);
        harness.get_by_label("工作区甲");

        harness.get_by_label("工作区甲").click();
        harness.run_steps(6);
        assert_eq!(
            harness.state().tabs.len(),
            tabs_before + 1,
            "单击本地工作区行应打开新标签"
        );
        let active = harness.state().active_tab;
        assert!(
            !harness.state().tabs[active]
                .active_view()
                .session()
                .is_remote(),
            "本地工作区打开的应是本地标签"
        );

        open_workspace_menu(&mut harness);
        harness.get_by_label("坏路径").click();
        harness.run_steps(6);
        assert_eq!(
            harness.state().tabs.len(),
            tabs_before + 1,
            "坏路径不应新建标签"
        );
        let toast = harness.state().toast.as_ref().expect("应有错误提示");
        assert!(toast.is_error, "坏路径应为错误提示");
        assert!(
            toast.message.contains("目录不存在"),
            "错误提示应说明目录不存在：{}",
            toast.message
        );

        std::fs::remove_file(&config_path).ok();
        std::fs::remove_dir_all(&base).ok();
    }

    /// ⌘O 打开工作区面板 → 输入过滤词只剩一项 → 回车打开新标签并关闭面板。
    #[test]
    fn 面板搜索过滤与回车打开() {
        use kittest::Queryable;
        let base = project_base("panel");
        let dir_a = base.join("alpha");
        let dir_b = base.join("beta");
        std::fs::create_dir_all(&dir_a).unwrap();
        std::fs::create_dir_all(&dir_b).unwrap();

        let config_path = test_config_path("workspaces-panel");
        let config = HostConfig {
            workspaces: vec![
                Workspace::Local {
                    name: "工作区甲".into(),
                    path: dir_a.clone(),
                    command: String::new(),
                },
                Workspace::Local {
                    name: "工作区乙".into(),
                    path: dir_b.clone(),
                    command: String::new(),
                },
            ],
            ..Default::default()
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
        assert!(harness.state().show_workspaces, "⌘O 应打开工作区面板");
        harness.get_by_label("工作区");

        harness.event(egui::Event::Text("甲".into()));
        harness.run_steps(6);
        harness.get_by_label("工作区甲");
        assert!(
            harness
                .root()
                .query_all_by_label("工作区乙")
                .next()
                .is_none(),
            "过滤后工作区乙不应再可见"
        );

        harness.event(egui::Event::Key {
            key: egui::Key::Enter,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        });
        harness.run_steps(6);
        assert!(!harness.state().show_workspaces, "回车打开后面板应关闭");
        assert_eq!(
            harness.state().tabs.len(),
            tabs_before + 1,
            "回车应打开选中工作区的新标签"
        );

        std::fs::remove_file(&config_path).ok();
        std::fs::remove_dir_all(&base).ok();
    }

    /// ⌘B 收藏当前目录为工作区并落盘；再按一次去重，数量不变。
    #[test]
    fn 收藏当前目录为工作区() {
        let config_path = test_config_path("workspace-bookmark");
        let mut harness = egui_kittest::Harness::new_eframe(|cc| {
            MinoApp::new_with_config(cc, config_path.clone())
        });
        harness.run_steps(6);

        harness.event(egui::Event::Key {
            key: egui::Key::B,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::COMMAND,
        });
        harness.run_steps(6);
        assert_eq!(
            harness.state().config.workspaces.len(),
            1,
            "⌘B 应收藏当前目录为工作区"
        );
        let home_canon = std::fs::canonicalize(std::env::var("HOME").expect("测试环境应有 HOME"))
            .expect("规范化 HOME 失败");
        let Workspace::Local { path, .. } = &harness.state().config.workspaces[0] else {
            panic!("收藏当前目录应得到本地工作区");
        };
        assert_eq!(*path, home_canon, "新建终端的当前目录应为 HOME");
        let content = std::fs::read_to_string(&config_path).expect("配置应已落盘");
        assert!(
            content.contains(home_canon.to_string_lossy().as_ref()),
            "落盘 toml 应含收藏路径：{content}"
        );
        assert!(
            content.contains("kind = \"local\""),
            "落盘应使用统一工作区格式：{content}"
        );

        harness.event(egui::Event::Key {
            key: egui::Key::B,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::COMMAND,
        });
        harness.run_steps(6);
        assert_eq!(
            harness.state().config.workspaces.len(),
            1,
            "重复收藏同一目录不应新增"
        );

        std::fs::remove_file(&config_path).ok();
    }

    /// 粘贴 `cd` 致输入跟踪失效后，⌘B 收藏的仍是内核真实目录。
    ///
    /// 仅 macOS 有效（依赖 `PROC_PIDVNODEPATHINFO`）：目录取
    /// `fresh_local_directory`（内核 cwd 优先），不走纯跟踪值。
    #[test]
    #[cfg(target_os = "macos")]
    fn 粘贴cd后收藏真实目录() {
        let base = project_base("bookmark-fresh");
        let target = base.join("target-dir");
        std::fs::create_dir_all(&target).unwrap();
        let config_path = test_config_path("workspace-bookmark-fresh");
        let mut harness = egui_kittest::Harness::new_eframe(|cc| {
            MinoApp::new_with_config(cc, config_path.clone())
        });
        harness.run_steps(6);
        // 等 shell 就绪：打一条标记命令并等回显（目录名不含固定串，
        // 用命令本身做信号最可靠）。
        harness.event(egui::Event::Text("echo __MINO_BOOKMARK_READY__".into()));
        harness.event(egui::Event::Key {
            key: egui::Key::Enter,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::default(),
        });
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(8);
        let mut ready = false;
        while std::time::Instant::now() < deadline {
            harness.step();
            let text = crate::views::terminal_view::tests_grid_text(
                harness.state().tabs[0].active_view().session(),
            );
            if text.contains("__MINO_BOOKMARK_READY__")
                && text.matches("__MINO_BOOKMARK_READY__").count() >= 2
            {
                ready = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(60));
        }
        assert!(ready, "zsh 未就绪");
        // 粘贴 cd（跟踪器对粘贴文本不做解析 → 失效路径），等内核 cwd 跟上。
        harness.event(egui::Event::Paste(format!("cd {}", target.display())));
        harness.event(egui::Event::Key {
            key: egui::Key::Enter,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::default(),
        });
        let expected = std::fs::canonicalize(&target).expect("规范化测试目录失败");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(8);
        loop {
            harness.step();
            let dir = harness.state().tabs[0]
                .active_view()
                .effective_local_directory();
            if dir.as_deref() == Some(expected.to_string_lossy().as_ref()) {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "内核 cwd 未跟随到目标目录，当前：{dir:?}"
            );
            std::thread::sleep(std::time::Duration::from_millis(60));
        }
        harness.event(egui::Event::Key {
            key: egui::Key::B,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::COMMAND,
        });
        harness.run_steps(6);
        let Workspace::Local { path, .. } = &harness.state().config.workspaces[0] else {
            panic!("收藏当前目录应得到本地工作区");
        };
        assert_eq!(
            *path, expected,
            "⌘B 收藏的应是内核真实目录（不是失效的跟踪值）"
        );
        std::fs::remove_file(&config_path).ok();
        std::fs::remove_dir_all(&base).ok();
    }

    /// 本地工作区的启动命令在首个提示符后自动执行。
    #[test]
    fn 启动命令自动执行() {
        use kittest::Queryable;
        let base = project_base("command");
        std::fs::create_dir_all(&base).unwrap();

        let config_path = test_config_path("workspaces-command");
        let config = HostConfig {
            workspaces: vec![Workspace::Local {
                name: "命令工作区".into(),
                path: base.clone(),
                command: "echo MINO_PROJ_MARK".into(),
            }],
            ..Default::default()
        };
        config.save(&config_path).expect("写入测试配置失败");

        let mut harness = egui_kittest::Harness::new_eframe(|cc| {
            MinoApp::new_with_config(cc, config_path.clone())
        });
        harness.run_steps(6);

        open_workspace_menu(&mut harness);
        harness.get_by_label("命令工作区").click();
        let deadline = Instant::now() + Duration::from_secs(8);
        let mut found = false;
        while Instant::now() < deadline {
            harness.step();
            let text = crate::views::terminal_view::tests_grid_text(
                harness.state().tabs[harness.state().active_tab]
                    .active_view()
                    .session(),
            );
            if text.contains("MINO_PROJ_MARK") {
                found = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(60));
        }
        assert!(found, "启动命令应自动执行并出现在终端上");

        std::fs::remove_file(&config_path).ok();
        std::fs::remove_dir_all(&base).ok();
    }
}

#[cfg(test)]
mod project_manage_tests {
    use super::*;
    /// 设置里新增本地工作区并落盘（名称 + 使用当前终端目录）。
    #[test]
    fn 设置工作区新增并落盘() {
        use kittest::Queryable;
        let config_path = test_config_path("workspaces-manage");
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
        harness.get_by_label("工作区");

        macro_rules! find_button {
            ($label:expr) => {
                harness
                    .root()
                    .query_all_by_role(accesskit::Role::Button)
                    .find(|n| {
                        use kittest::NodeT as _;
                        n.accesskit_node().label() == Some($label.to_string())
                    })
                    .unwrap_or_else(|| panic!("找不到按钮：{}", $label))
            };
        }
        find_button!("新增本地目录").click();
        harness.run_steps(6);
        harness.get_by_label("使用当前终端目录");

        // 表单输入框顺序（本地分支）：名称、目录、启动命令。
        let inputs: Vec<_> = harness
            .root()
            .query_all_by_role(accesskit::Role::TextInput)
            .collect();
        assert!(inputs.len() >= 3, "新增表单应有名称/目录/命令输入框");
        inputs[0].click();
        harness.run_steps(2);
        harness.event(egui::Event::Text("管理工作区".into()));
        harness.run_steps(2);

        find_button!("使用当前终端目录").scroll_to_me();
        harness.run_steps(3);
        find_button!("使用当前终端目录").click();
        harness.run_steps(3);
        find_button!("保存").click();
        harness.run_steps(6);

        assert_eq!(
            harness.state().config.workspaces.len(),
            1,
            "保存后应新增一个工作区"
        );
        let home_canon = std::fs::canonicalize(std::env::var("HOME").expect("测试环境应有 HOME"))
            .expect("规范化 HOME 失败");
        let Workspace::Local { name, path, .. } = &harness.state().config.workspaces[0] else {
            panic!("新增的应是本地工作区");
        };
        assert_eq!(name, "管理工作区");
        assert_eq!(*path, home_canon);
        let content = std::fs::read_to_string(&config_path).expect("配置应已落盘");
        assert!(
            content.contains("管理工作区"),
            "落盘 toml 应含新工作区：{content}"
        );

        std::fs::remove_file(&config_path).ok();
    }

    /// 工作区编辑对话框的页脚：保存/取消右对齐（保存为主按钮在最右）。
    #[test]
    fn 工作区编辑页脚右对齐() {
        use kittest::Queryable;
        let config_path = test_config_path("workspace-footer");
        let mut harness = egui_kittest::Harness::new_eframe(|cc| {
            MinoApp::new_with_config(cc, config_path.clone())
        });
        harness.run_steps(6);
        harness.state_mut().open_workspace_editor(1);
        harness.run_steps(6);

        macro_rules! find_button {
            ($label:expr) => {
                harness
                    .root()
                    .query_all_by_role(accesskit::Role::Button)
                    .find(|n| {
                        use kittest::NodeT as _;
                        n.accesskit_node().label() == Some($label.to_string())
                    })
                    .unwrap_or_else(|| panic!("找不到按钮：{}", $label))
            };
        }
        let save = find_button!("保存").rect();
        let cancel = find_button!("取消").rect();
        assert!(
            save.right() > cancel.right(),
            "保存（主按钮）应在取消右侧：保存 right={:.1}，取消 right={:.1}",
            save.right(),
            cancel.right()
        );
        assert!(
            save.right() > 400.0,
            "页脚应右对齐到对话框右缘：保存 right={:.1}",
            save.right()
        );

        std::fs::remove_file(&config_path).ok();
    }
}

/// 测试支持：跨测试模块共用的小工具（避免各模块重复定义）。
#[cfg(test)]
mod workspace_test_support {
    use super::*;

    /// 点击标签栏「工作区」按钮（右端第二个按钮：最右是齿轮）。
    ///
    /// 两者都是空文字矢量按钮：按 `Role::Button` + 底部区域 + 右缘排序定位。
    pub(super) fn open_workspace_menu(harness: &mut egui_kittest::Harness<MinoApp>) {
        use kittest::Queryable;
        let mut buttons: Vec<_> = harness
            .root()
            .query_all_by_role(accesskit::Role::Button)
            .filter(|n| n.rect().top() > 560.0)
            .collect();
        buttons.sort_by(|a, b| {
            b.rect()
                .right()
                .partial_cmp(&a.rect().right())
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        assert!(buttons.len() >= 2, "标签栏应有齿轮与工作区按钮");
        buttons[1].click();
        harness.run_steps(6);
    }
}

/// 新增功能回归（分屏/搜索/快照/覆盖层/快捷键族）。
#[cfg(test)]
mod kaku_feature_tests {
    use super::*;

    fn key(
        harness: &mut egui_kittest::Harness<MinoApp>,
        key: egui::Key,
        modifiers: egui::Modifiers,
    ) {
        harness.event(egui::Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers,
        });
        harness.run_steps(4);
    }

    /// ⌘D 右分屏 / ⌘⇧D 下分屏：标签内出现两个窗格、布局树与集合同源；
    /// 关闭窗格回到单窗格；⌘⇧Enter 放大、⌘⇧S 翻转方向。
    #[test]
    fn 分屏拆分与关闭窗格() {
        let config_path = test_config_path("panes-split");
        let mut harness = egui_kittest::Harness::new_eframe(|cc| {
            MinoApp::new_with_config(cc, config_path.clone())
        });
        harness.run_steps(6);
        assert_eq!(harness.state().tabs[0].panes.len(), 1, "初始单窗格");

        // ⌘D：左右并排。
        key(&mut harness, egui::Key::D, egui::Modifiers::COMMAND);
        assert_eq!(
            harness.state().tabs[0].panes.len(),
            2,
            "⌘D 应分裂出第二窗格"
        );
        // 新窗格在右：布局矩形 x 更大。
        let rects = harness.state().tabs[0].pane_rects.clone();
        assert_eq!(rects.len(), 2);
        let right = rects
            .iter()
            .max_by(|a, b| a.1.left().partial_cmp(&b.1.left()).unwrap())
            .unwrap();
        assert_eq!(
            right.0,
            harness.state().tabs[0].active_pane,
            "新窗格（右）应获得焦点"
        );

        // ⌘⇧D：在活动窗格上再向下拆分 → 3 个窗格。
        key(
            &mut harness,
            egui::Key::D,
            egui::Modifiers::COMMAND | egui::Modifiers::SHIFT,
        );
        assert_eq!(
            harness.state().tabs[0].panes.len(),
            3,
            "⌘⇧D 应再分裂一个窗格"
        );

        // ⌘⇧Enter：放大；再按还原。
        key(
            &mut harness,
            egui::Key::Enter,
            egui::Modifiers::COMMAND | egui::Modifiers::SHIFT,
        );
        assert!(
            harness.state().tabs[0].zoomed.is_some(),
            "⌘⇧Enter 应放大活动窗格"
        );
        key(
            &mut harness,
            egui::Key::Enter,
            egui::Modifiers::COMMAND | egui::Modifiers::SHIFT,
        );
        assert!(harness.state().tabs[0].zoomed.is_none(), "再按应还原");

        // ⌘⇧S：翻转最近一层拆分方向（布局树仍与窗格集合一致）。
        let before = harness.state().tabs[0].layout.clone();
        key(
            &mut harness,
            egui::Key::S,
            egui::Modifiers::COMMAND | egui::Modifiers::SHIFT,
        );
        assert_ne!(harness.state().tabs[0].layout, before, "⌘⇧S 应翻转拆分方向");

        // ⌘⌥←：焦点移到相邻窗格。
        let active_before = harness.state().tabs[0].active_pane;
        key(
            &mut harness,
            egui::Key::ArrowLeft,
            egui::Modifiers::COMMAND | egui::Modifiers::ALT,
        );
        assert_ne!(
            harness.state().tabs[0].active_pane,
            active_before,
            "⌘⌥← 应移焦到左侧窗格"
        );

        // 关闭窗格：⌘W 关掉一个窗格而不是整个标签。
        key(&mut harness, egui::Key::W, egui::Modifiers::COMMAND);
        assert_eq!(harness.state().tabs[0].panes.len(), 2, "⌘W 应关窗格");
        assert_eq!(harness.state().tabs.len(), 1, "标签仍在");
        key(&mut harness, egui::Key::W, egui::Modifiers::COMMAND);
        assert_eq!(harness.state().tabs[0].panes.len(), 1, "再关一个");
        // 最后一个窗格上的 ⌘W → 最小化窗口，标签保留（Kaku 语义）。
        let mut minimized = false;
        harness.event(egui::Event::Key {
            key: egui::Key::W,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::COMMAND,
        });
        for _ in 0..4 {
            harness.step();
            if let Some(vp) = harness
                .output()
                .viewport_output
                .get(&egui::ViewportId::ROOT)
            {
                for cmd in &vp.commands {
                    if matches!(cmd, egui::ViewportCommand::Minimized(true)) {
                        minimized = true;
                    }
                }
            }
        }
        assert!(minimized, "唯一窗格唯一标签上的 ⌘W 应最小化窗口");
        assert_eq!(harness.state().tabs.len(), 1);

        std::fs::remove_file(&config_path).ok();
    }

    /// ⌘⇧W 关闭整个标签（含全部窗格）；⌘⇧T 重开最近关闭的标签。
    #[test]
    fn 快捷键关闭与重开标签() {
        let config_path = test_config_path("panes-close-reopen");
        let mut harness = egui_kittest::Harness::new_eframe(|cc| {
            MinoApp::new_with_config(cc, config_path.clone())
        });
        harness.run_steps(6);
        key(&mut harness, egui::Key::D, egui::Modifiers::COMMAND);
        assert_eq!(harness.state().tabs[0].panes.len(), 2);
        key(&mut harness, egui::Key::T, egui::Modifiers::COMMAND);
        assert_eq!(harness.state().tabs.len(), 2, "⌘T 新标签");

        // ⌘⇧W：关闭当前标签（不是窗格）。
        key(
            &mut harness,
            egui::Key::W,
            egui::Modifiers::COMMAND | egui::Modifiers::SHIFT,
        );
        assert_eq!(harness.state().tabs.len(), 1, "⌘⇧W 应关闭整个标签");

        // ⌘⇧T：重开。
        key(
            &mut harness,
            egui::Key::T,
            egui::Modifiers::COMMAND | egui::Modifiers::SHIFT,
        );
        assert_eq!(harness.state().tabs.len(), 2, "⌘⇧T 应重开标签");
        std::fs::remove_file(&config_path).ok();
    }

    /// ⌘K 清屏并清空回滚；⌘F 搜索浮层开关；⌘L AI 面板开关；⌘O 工作区面板。
    #[test]
    fn kaku快捷键族() {
        let config_path = test_config_path("kaku-keys");
        let mut harness = egui_kittest::Harness::new_eframe(|cc| {
            MinoApp::new_with_config(cc, config_path.clone())
        });
        harness.run_steps(6);

        // ⌘F：搜索打开/关闭。
        key(&mut harness, egui::Key::F, egui::Modifiers::COMMAND);
        assert!(
            harness.state().tabs[0].active_view().search_open(),
            "⌘F 应打开搜索浮层"
        );
        // 浮层里输入过滤词（键盘在浮层，不进 PTY）。
        harness.event(egui::Event::Text("xxx".into()));
        harness.run_steps(3);
        key(&mut harness, egui::Key::Escape, egui::Modifiers::NONE);
        assert!(
            !harness.state().tabs[0].active_view().search_open(),
            "Esc 应关闭搜索浮层"
        );

        // ⌘K：清屏（不崩溃；清空回滚后 offset 归零）。
        key(&mut harness, egui::Key::K, egui::Modifiers::COMMAND);

        // ⌘L：AI 面板打开/关闭。
        key(&mut harness, egui::Key::L, egui::Modifiers::COMMAND);
        assert!(harness.state().ai_panel.is_open(), "⌘L 应打开 AI 面板");
        key(&mut harness, egui::Key::L, egui::Modifiers::COMMAND);
        assert!(!harness.state().ai_panel.is_open(), "⌘L 再按应关闭");

        // ⌘O：工作区面板打开/关闭。
        key(&mut harness, egui::Key::O, egui::Modifiers::COMMAND);
        assert!(harness.state().show_workspaces, "⌘O 应打开工作区面板");
        key(&mut harness, egui::Key::Escape, egui::Modifiers::NONE);
        assert!(!harness.state().show_workspaces, "Esc 应关闭面板");

        std::fs::remove_file(&config_path).ok();
    }

    /// ⌘⇧O 标签导航器：列出标签、输入过滤、回车切换；⌘⇧P 命令面板：执行动作。
    #[test]
    fn 标签导航与命令面板() {
        use kittest::Queryable;
        let config_path = test_config_path("launcher-flow");
        let mut harness = egui_kittest::Harness::new_eframe(|cc| {
            MinoApp::new_with_config(cc, config_path.clone())
        });
        harness.run_steps(6);
        key(&mut harness, egui::Key::T, egui::Modifiers::COMMAND);
        assert_eq!(harness.state().tabs.len(), 2);
        harness.state_mut().active_tab = 0;

        // ⌘⇧O：标签导航。
        key(
            &mut harness,
            egui::Key::O,
            egui::Modifiers::COMMAND | egui::Modifiers::SHIFT,
        );
        assert!(harness.state().launcher.is_some(), "⌘⇧O 应打开标签导航");
        assert!(
            harness
                .root()
                .query_all_by_label("标签导航")
                .next()
                .is_some(),
            "导航器标题应可见"
        );
        // 方向键选择第 2 项并回车切换。
        key(&mut harness, egui::Key::ArrowDown, egui::Modifiers::NONE);
        key(&mut harness, egui::Key::Enter, egui::Modifiers::NONE);
        assert!(harness.state().launcher.is_none(), "确认后浮层应关闭");
        assert_eq!(harness.state().active_tab, 1, "回车应切换到第二个标签");

        // ⌘⇧P：命令面板执行"新建本地终端标签"。
        let tabs_before = harness.state().tabs.len();
        key(
            &mut harness,
            egui::Key::P,
            egui::Modifiers::COMMAND | egui::Modifiers::SHIFT,
        );
        assert!(harness.state().launcher.is_some(), "⌘⇧P 应打开命令面板");
        harness.event(egui::Event::Text("新建本地".into()));
        harness.run_steps(4);
        key(&mut harness, egui::Key::Enter, egui::Modifiers::NONE);
        assert_eq!(
            harness.state().tabs.len(),
            tabs_before + 1,
            "命令面板应执行新建标签"
        );
        std::fs::remove_file(&config_path).ok();
    }

    /// 窗口快照：保存后文件 0600 且含工作区/本地条目；恢复按条目重建标签。
    #[test]
    fn 窗口快照保存与恢复() {
        let config_path = test_config_path("window-snapshot");
        let mut harness = egui_kittest::Harness::new_eframe(|cc| {
            MinoApp::new_with_config(cc, config_path.clone())
        });
        harness.run_steps(6);
        key(&mut harness, egui::Key::T, egui::Modifiers::COMMAND);
        assert_eq!(harness.state().tabs.len(), 2);
        harness.state().save_window_snapshot();

        let snapshot = harness.state().snapshot_path();
        let content = std::fs::read_to_string(&snapshot).expect("快照应已写入");
        assert!(
            content.contains("kind = \"local\""),
            "快照应包含本地标签：{content}"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&snapshot).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "含凭据的快照必须 0600");
        }

        // 恢复到既有窗口（追加 2 个标签）。
        let ctx = harness.ctx.clone();
        harness.state_mut().restore_window_snapshot(&ctx);
        harness.run_steps(6);
        assert_eq!(harness.state().tabs.len(), 4, "恢复应重建两个标签");

        std::fs::remove_file(&config_path).ok();
        std::fs::remove_file(&snapshot).ok();
    }

    /// ⌘B 收藏 SSH 工作区：从远程标签的来源档案生成 Ssh 条目（去重）。
    #[test]
    fn 收藏远程会话为工作区() {
        let config_path = test_config_path("workspace-bookmark-ssh");
        let mut harness = egui_kittest::Harness::new_eframe(|cc| {
            MinoApp::new_with_config(cc, config_path.clone())
        });
        harness.run_steps(6);
        // 直接构造一个"远程标签"（不真连）：用本地会话 + origin 标记。
        {
            let state = harness.state_mut();
            let session = mino_core::terminal::Session::spawn_local(
                mino_core::terminal::SessionOptions::default(),
                80,
                24,
                std::sync::Arc::new(|_ev: &mino_core::terminal::SessionEvent| {}),
            )
            .expect("创建本地终端失败");
            let mut view = crate::views::terminal_view::TerminalView::new(session);
            view.set_font_size(state.config.font_size);
            let profile = HostProfile {
                name: "线上机".into(),
                host: "example.internal".into(),
                port: 2222,
                user: "deploy".into(),
                auth: mino_core::config::Auth::Password("secret".into()),
            };
            let id = state.allocate_id();
            let pane_id = state.allocate_id();
            state.tabs.push(Box::new(TerminalTab::new(
                id,
                "线上机".into(),
                view,
                pane_id,
                SessionOrigin::Remote(profile),
            )));
            state.active_tab = state.tabs.len() - 1;
        }
        harness.run_steps(2);
        harness.state_mut().bookmark_current_workspace();
        assert_eq!(harness.state().config.workspaces.len(), 1);
        let Workspace::Ssh {
            host, port, user, ..
        } = &harness.state().config.workspaces[0]
        else {
            panic!("远程收藏应得到 Ssh 工作区");
        };
        assert_eq!(
            (host.as_str(), *port, user.as_str()),
            ("example.internal", 2222, "deploy")
        );
        // 去重：再收藏一次数量不变。
        harness.state_mut().bookmark_current_workspace();
        assert_eq!(
            harness.state().config.workspaces.len(),
            1,
            "同主机不应重复收藏"
        );
        std::fs::remove_file(&config_path).ok();
    }

    /// ⌘⇧E：应用最近一次 AI 建议（无建议时给出提示而不是 panic）。
    #[test]
    fn 应用ai建议路径() {
        let config_path = test_config_path("ai-apply");
        let mut harness = egui_kittest::Harness::new_eframe(|cc| {
            MinoApp::new_with_config(cc, config_path.clone())
        });
        harness.run_steps(6);
        key(
            &mut harness,
            egui::Key::E,
            egui::Modifiers::COMMAND | egui::Modifiers::SHIFT,
        );
        let toast = harness.state().toast.as_ref().expect("无建议应有提示");
        assert!(toast.message.contains("暂无 AI 建议"), "{}", toast.message);
        std::fs::remove_file(&config_path).ok();
    }
}
