//! SFTP 客户端：后台线程驱动，UI 通过命令队列操作、事件流接收结果。

use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use russh_sftp::client::SftpSession;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc::{self, Receiver, Sender, UnboundedReceiver, UnboundedSender};
use tokio::sync::Notify;

use crate::config::HostProfile;
use crate::ssh::{connect_verified, ssh_config};

/// 传输临时文件序号。临时文件与目标文件位于同一目录，成功后用 rename
/// 原子替换目标，避免失败传输破坏已有文件。
static PARTIAL_COUNTER: AtomicU64 = AtomicU64::new(0);

fn partial_suffix() -> String {
    format!(
        "{}-{}",
        std::process::id(),
        PARTIAL_COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

fn partial_remote_path(remote: &str) -> String {
    if remote.is_empty() {
        format!(".mino-partial-{}", partial_suffix())
    } else {
        format!("{remote}.mino-partial-{}", partial_suffix())
    }
}

fn partial_local_path(local: &Path) -> PathBuf {
    let name = local
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("download");
    local
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(format!(".{name}.mino-partial-{}", partial_suffix()))
}

/// 返回远程路径所在的父目录，用于把异步操作结果绑定到发起操作时的目录。
fn remote_parent_path(path: &str) -> String {
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() || trimmed == "/" {
        return "/".to_string();
    }
    match trimmed.rsplit_once('/') {
        Some(("", _)) => "/".to_string(),
        Some((parent, _)) => parent.to_string(),
        None => ".".to_string(),
    }
}

/// SFTP 条目名必须是单个 POSIX 路径分量。
///
/// 正常服务器不会返回包含 `/`、`.` 或 `..` 的条目名，但列表内容来自远程
/// 端，不能直接把它拼入 UI 后续的删除、下载或进入目录路径。非法条目直接
/// 忽略，避免恶意/异常服务器借列表名把操作引到当前目录之外。
fn is_safe_entry_name(name: &str) -> bool {
    !name.is_empty() && name != "." && name != ".." && !name.contains('/') && !name.contains('\0')
}

/// 远程文件条目。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteEntry {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
    pub modified: Option<u64>,
    pub permissions: u32,
}

/// SFTP 后台命令（UI → 后台）。
#[derive(Debug)]
pub enum SftpCmd {
    List {
        path: String,
    },
    Upload {
        id: u64,
        local: PathBuf,
        remote: String,
    },
    Download {
        id: u64,
        remote: String,
        local: PathBuf,
    },
    Remove {
        path: String,
        is_dir: bool,
    },
    Rename {
        from: String,
        to: String,
    },
    Mkdir {
        path: String,
    },
    Shutdown,
}

/// SFTP 事件（后台 → UI）。
#[derive(Debug, Clone)]
pub enum SftpEvent {
    /// 连接就绪，同时返回 SFTP 会话的初始目录。
    Ready { home: String },
    /// 连接失败。
    Failed(String),
    /// 目录列表完成。
    Listed {
        path: String,
        entries: Vec<RemoteEntry>,
    },
    /// 传输进度。
    Progress {
        id: u64,
        label: String,
        done: u64,
        total: u64,
    },
    /// 操作完成；`refresh` 表示远程当前目录内容发生变化，需要重新列目录。
    Done {
        id: Option<u64>,
        label: String,
        refresh: bool,
        /// 产生目录变更的目录。UI 切换目录后，旧操作完成不能刷新新目录。
        path: Option<String>,
    },
    /// 操作失败。
    Error {
        id: Option<u64>,
        label: String,
        message: String,
        /// 目录列表失败时携带请求路径，避免旧请求的错误污染当前页面。
        path: Option<String>,
    },
    /// 连接关闭。
    Closed,
}

/// SFTP 后台事件到达时的唤醒回调。
///
/// 回调只负责通知 UI 尽快轮询，事件数据仍通过有界通道传输；这样核心层
/// 不需要依赖 egui，同时也避免面板必须先获得一帧重绘才能发现后台进度。
pub type SftpEventHandler = Arc<dyn Fn() + Send + Sync>;

/// UI 持有的 SFTP 操作句柄。
#[derive(Clone)]
pub struct SftpHandle {
    cmd_tx: UnboundedSender<SftpCmd>,
    next_transfer_id: Arc<AtomicU64>,
    /// 关闭标志：`close()` 立即置位，传输循环每块数据间检查，
    /// 不依赖 Shutdown 命令在队列中的顺序（排在传输后就会跑完整个传输）。
    shutdown: Arc<AtomicBool>,
    /// 唤醒正在等待网络 I/O 的操作，使关闭不必等待读写超时。
    cancel: Arc<Notify>,
}

impl SftpHandle {
    /// 从原始发送端构造（测试用），附带独立的取消标志。
    pub fn from_raw(cmd_tx: UnboundedSender<SftpCmd>) -> Self {
        Self {
            cmd_tx,
            next_transfer_id: Arc::new(AtomicU64::new(1)),
            shutdown: Arc::new(AtomicBool::new(false)),
            cancel: Arc::new(Notify::new()),
        }
    }

    /// 从原始发送端与取消标志构造（`connect_sftp` 内部使用）。
    fn with_shutdown(
        cmd_tx: UnboundedSender<SftpCmd>,
        shutdown: Arc<AtomicBool>,
        cancel: Arc<Notify>,
    ) -> Self {
        Self {
            cmd_tx,
            next_transfer_id: Arc::new(AtomicU64::new(1)),
            shutdown,
            cancel,
        }
    }
}

impl SftpHandle {
    /// 列出远程目录。
    pub fn list(&self, path: &str) {
        let _ = self.cmd_tx.send(SftpCmd::List {
            path: path.to_string(),
        });
    }

    /// 上传本地文件到远程。
    pub fn upload(&self, local: &Path, remote: &str) -> u64 {
        let id = self.next_transfer_id.fetch_add(1, Ordering::Relaxed);
        let _ = self.cmd_tx.send(SftpCmd::Upload {
            id,
            local: local.to_path_buf(),
            remote: remote.to_string(),
        });
        id
    }

    /// 下载远程文件到本地。
    pub fn download(&self, remote: &str, local: &Path) -> u64 {
        let id = self.next_transfer_id.fetch_add(1, Ordering::Relaxed);
        let _ = self.cmd_tx.send(SftpCmd::Download {
            id,
            remote: remote.to_string(),
            local: local.to_path_buf(),
        });
        id
    }

    /// 删除远程文件或目录。
    pub fn remove(&self, path: &str, is_dir: bool) {
        let _ = self.cmd_tx.send(SftpCmd::Remove {
            path: path.to_string(),
            is_dir,
        });
    }

    /// 重命名远程文件或目录。
    pub fn rename(&self, from: &str, to: &str) {
        let _ = self.cmd_tx.send(SftpCmd::Rename {
            from: from.to_string(),
            to: to.to_string(),
        });
    }

    /// 新建远程目录。
    pub fn mkdir(&self, path: &str) {
        let _ = self.cmd_tx.send(SftpCmd::Mkdir {
            path: path.to_string(),
        });
    }

    /// 关闭 SFTP 连接，立即中止进行中的传输。
    pub fn close(&self) {
        // 先置位取消标志：传输循环每块数据（64KB）间检查，正在进行的
        // 传输立刻中止并清理半成品，不等待队列中排在前面的命令执行完；
        // Shutdown 命令作为操作循环退出的兜底。
        self.shutdown.store(true, Ordering::SeqCst);
        // 先唤醒正在等待 connect/read/write/metadata 等网络 future 的任务；
        // 仅向命令队列发送 Shutdown 无法打断已经进入中的异步操作。
        self.cancel.notify_one();
        let _ = self.cmd_tx.send(SftpCmd::Shutdown);
    }
}

/// 发起 SFTP 连接（非阻塞）。
///
/// 返回（线程句柄, 操作句柄, 事件接收端）。
pub fn connect_sftp(
    profile: &HostProfile,
) -> (std::thread::JoinHandle<()>, SftpHandle, Receiver<SftpEvent>) {
    connect_sftp_with_handler(profile, Arc::new(|| {}))
}

/// 发起带 UI 唤醒回调的 SFTP 连接（非阻塞）。
///
/// `connect_sftp` 保留无回调入口供核心层调用和测试使用；应用层应使用本函数，
/// 否则在终端空闲、没有其它 egui 重绘源时，异步列表和传输进度可能迟迟不显示。
pub fn connect_sftp_with_handler(
    profile: &HostProfile,
    on_event: SftpEventHandler,
) -> (std::thread::JoinHandle<()>, SftpHandle, Receiver<SftpEvent>) {
    // 事件流有界，避免非活动标签在大文件传输时无限积压进度事件。
    let (ev_tx, ev_rx) = mpsc::channel::<SftpEvent>(128);
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<SftpCmd>();
    // 关闭标志：UI 调用 SftpHandle::close() 立即置位，后台传输循环
    // 每块数据间检查（见 upload_file/download_file），不等队列排空。
    let shutdown = Arc::new(AtomicBool::new(false));
    let thread_shutdown = shutdown.clone();
    let cancel = Arc::new(Notify::new());
    let thread_cancel = cancel.clone();
    let profile = profile.clone();
    let handle = std::thread::spawn(move || {
        // 共享 runtime 不可用时也必须把失败回传（否则面板永久停在"连接中…"）。
        let runtime = match crate::ssh::SHARED_RUNTIME.as_ref() {
            Ok(runtime) => runtime,
            Err(e) => {
                if ev_tx
                    .try_send(SftpEvent::Failed(format!("初始化 SFTP 运行时失败：{e}")))
                    .is_ok()
                {
                    on_event();
                }
                return;
            }
        };
        crate::ssh::wait_for_task(runtime.spawn(sftp_main(
            profile,
            cmd_rx,
            ev_tx,
            thread_shutdown,
            thread_cancel,
            on_event,
        )));
    });
    (
        handle,
        SftpHandle::with_shutdown(cmd_tx, shutdown, cancel),
        ev_rx,
    )
}

const SFTP_CANCELLED: &str = "SFTP 操作已取消";

/// 让一个 SFTP future 同时响应连接关闭。
///
/// 关闭标志与 Notify 配合使用：先检查原子标志，再创建通知 future，避免
/// `close()` 恰好发生在检查与注册等待之间时丢掉取消信号。`biased` 优先
/// 采用已经完成的 I/O 结果，避免“提交已完成但取消通知同时到达”时误报失败。
async fn cancellable<T, E, F>(
    shutdown: &AtomicBool,
    cancel: &Notify,
    future: F,
) -> Result<T, String>
where
    E: std::fmt::Display,
    F: Future<Output = Result<T, E>>,
{
    if shutdown.load(Ordering::SeqCst) {
        return Err(SFTP_CANCELLED.to_string());
    }
    let notified = cancel.notified();
    tokio::pin!(notified);
    if shutdown.load(Ordering::SeqCst) {
        return Err(SFTP_CANCELLED.to_string());
    }
    tokio::select! {
        biased;
        result = future => result.map_err(|error| error.to_string()),
        _ = &mut notified => Err(SFTP_CANCELLED.to_string()),
    }
}

fn is_sftp_cancelled(error: &str) -> bool {
    error == SFTP_CANCELLED
}

/// 将事件写入通道并唤醒 UI。返回 `false` 表示接收端已销毁。
async fn send_sftp_event(
    ev_tx: &Sender<SftpEvent>,
    on_event: &SftpEventHandler,
    cancel: &Notify,
    event: SftpEvent,
) -> bool {
    let notified = cancel.notified();
    tokio::pin!(notified);
    let sent = tokio::select! {
        biased;
        result = ev_tx.send(event) => result.is_ok(),
        _ = &mut notified => false,
    };
    if !sent {
        return false;
    }
    on_event();
    true
}

/// 尝试发送可丢弃的进度事件。队列满时保留已有事件并继续传输，
/// 只有接收端关闭才终止传输，避免后台线程在标签页销毁后继续工作。
fn try_send_progress(
    ev_tx: &Sender<SftpEvent>,
    on_event: &SftpEventHandler,
    event: SftpEvent,
) -> Result<(), String> {
    match ev_tx.try_send(event) {
        Ok(()) => {
            on_event();
            Ok(())
        }
        Err(mpsc::error::TrySendError::Full(_)) => Ok(()),
        Err(mpsc::error::TrySendError::Closed(_)) => Err("SFTP 事件通道已关闭".to_string()),
    }
}

/// 连接并运行 SFTP 操作循环（阻塞直到关闭）。
async fn sftp_main(
    profile: HostProfile,
    mut cmd_rx: UnboundedReceiver<SftpCmd>,
    ev_tx: Sender<SftpEvent>,
    shutdown: Arc<AtomicBool>,
    cancel: Arc<Notify>,
    on_event: SftpEventHandler,
) {
    // ==================== 1. 连接与认证（含主机密钥 TOFU 校验） ====================
    log::info!("sftp_main 启动：{}:{}", profile.host, profile.port);
    let config = Arc::new(ssh_config());
    let mut handle = match cancellable(&shutdown, &cancel, connect_verified(config, &profile)).await
    {
        Ok(h) => h,
        Err(e) if is_sftp_cancelled(&e) => return,
        Err(e) => {
            let _ = send_sftp_event(
                &ev_tx,
                &on_event,
                &cancel,
                SftpEvent::Failed(format!("连接 {}:{} 失败：{e}", profile.host, profile.port)),
            )
            .await;
            return;
        }
    };

    log::info!("TCP 连接成功，开始认证");
    let authed = match cancellable(
        &shutdown,
        &cancel,
        crate::ssh::authenticate(&mut handle, &profile),
    )
    .await
    {
        Ok(ok) => ok,
        Err(e) if is_sftp_cancelled(&e) => return,
        Err(e) => {
            let _ = send_sftp_event(&ev_tx, &on_event, &cancel, SftpEvent::Failed(e)).await;
            return;
        }
    };
    if !authed {
        let _ = send_sftp_event(
            &ev_tx,
            &on_event,
            &cancel,
            SftpEvent::Failed("认证失败：用户名或密码/密钥错误".into()),
        )
        .await;
        return;
    }

    // ==================== 2. 打开 SFTP subsystem ====================
    let channel = match cancellable(&shutdown, &cancel, handle.channel_open_session()).await {
        Ok(c) => c,
        Err(e) if is_sftp_cancelled(&e) => return,
        Err(e) => {
            let _ = send_sftp_event(
                &ev_tx,
                &on_event,
                &cancel,
                SftpEvent::Failed(format!("打开会话失败：{e}")),
            )
            .await;
            return;
        }
    };
    if let Err(e) = cancellable(&shutdown, &cancel, channel.request_subsystem(true, "sftp")).await {
        if is_sftp_cancelled(&e) {
            return;
        }
        let _ = send_sftp_event(
            &ev_tx,
            &on_event,
            &cancel,
            SftpEvent::Failed(format!("启动 SFTP 子系统失败：{e}")),
        )
        .await;
        return;
    }
    let sftp = match cancellable(&shutdown, &cancel, SftpSession::new(channel.into_stream())).await
    {
        Ok(s) => s,
        Err(e) if is_sftp_cancelled(&e) => return,
        Err(e) => {
            let _ = send_sftp_event(
                &ev_tx,
                &on_event,
                &cancel,
                SftpEvent::Failed(format!("初始化 SFTP 失败：{e}")),
            )
            .await;
            return;
        }
    };

    log::info!("SFTP 连接就绪");
    let home = match cancellable(&shutdown, &cancel, sftp.canonicalize(".")).await {
        Ok(home) => home,
        Err(e) if is_sftp_cancelled(&e) => return,
        Err(_) => "/".to_string(),
    };
    if !send_sftp_event(&ev_tx, &on_event, &cancel, SftpEvent::Ready { home }).await {
        // UI 已经放弃接收事件（例如标签页在连接握手期间被关闭），
        // 不再继续持有 SSH/SFTP 会话等待命令。
        return;
    }

    // ==================== 3. 操作循环 ====================
    // 命令循环同时轮询连接健康：服务器断开后 russh 底层会把连接标记为
    // 已关闭，此前的实现只在 recv 上等待，断连后命令持续失败但永远不
    // 退出、永远不发 Closed（UI 状态栏永远显示"已连接"）。每 2 秒轻量
    // 探测一次，断连即退出循环并发送 Closed。
    const LIVENESS_POLL: Duration = Duration::from_secs(2);
    loop {
        let cmd = tokio::select! {
            cmd = cmd_rx.recv() => cmd,
            _ = tokio::time::sleep(LIVENESS_POLL) => {
                if shutdown.load(Ordering::SeqCst) || handle.is_closed() {
                    log::warn!("SFTP 连接已关闭/断开，退出操作循环");
                    break;
                }
                continue;
            }
        };
        let Some(cmd) = cmd else {
            break;
        };
        if shutdown.load(Ordering::SeqCst) {
            break;
        }
        match cmd {
            SftpCmd::List { path } => {
                let result = cancellable(&shutdown, &cancel, list_dir(&sftp, &path)).await;
                match result {
                    Ok(entries) => {
                        if !send_sftp_event(
                            &ev_tx,
                            &on_event,
                            &cancel,
                            SftpEvent::Listed { path, entries },
                        )
                        .await
                        {
                            return;
                        }
                    }
                    Err(e) if is_sftp_cancelled(&e) => break,
                    Err(e) => {
                        if !send_sftp_event(
                            &ev_tx,
                            &on_event,
                            &cancel,
                            SftpEvent::Error {
                                id: None,
                                label: "列出目录".into(),
                                message: e,
                                path: Some(path),
                            },
                        )
                        .await
                        {
                            return;
                        }
                    }
                }
            }
            SftpCmd::Upload { id, local, remote } => {
                let label = format!(
                    "上传 {}",
                    local
                        .file_name()
                        .map(|s| s.to_string_lossy().into_owned())
                        .unwrap_or_else(|| local.display().to_string())
                );
                match upload_file(
                    &sftp, &local, &remote, id, &label, &ev_tx, &on_event, &shutdown, &cancel,
                )
                .await
                {
                    Ok(()) => {
                        if !send_sftp_event(
                            &ev_tx,
                            &on_event,
                            &cancel,
                            SftpEvent::Done {
                                id: Some(id),
                                label,
                                refresh: true,
                                path: Some(remote_parent_path(&remote)),
                            },
                        )
                        .await
                        {
                            return;
                        }
                    }
                    Err(e) if is_sftp_cancelled(&e) => break,
                    Err(e) => {
                        if !send_sftp_event(
                            &ev_tx,
                            &on_event,
                            &cancel,
                            SftpEvent::Error {
                                id: Some(id),
                                label,
                                message: e,
                                path: Some(remote_parent_path(&remote)),
                            },
                        )
                        .await
                        {
                            return;
                        }
                    }
                }
            }
            SftpCmd::Download { id, remote, local } => {
                let label = format!(
                    "下载 {}",
                    Path::new(&remote)
                        .file_name()
                        .map(|s| s.to_string_lossy().into_owned())
                        .unwrap_or_else(|| remote.clone())
                );
                match download_file(
                    &sftp, &remote, &local, id, &label, &ev_tx, &on_event, &shutdown, &cancel,
                )
                .await
                {
                    Ok(()) => {
                        if !send_sftp_event(
                            &ev_tx,
                            &on_event,
                            &cancel,
                            SftpEvent::Done {
                                id: Some(id),
                                label,
                                refresh: false,
                                path: Some(remote_parent_path(&remote)),
                            },
                        )
                        .await
                        {
                            return;
                        }
                    }
                    Err(e) if is_sftp_cancelled(&e) => break,
                    Err(e) => {
                        if !send_sftp_event(
                            &ev_tx,
                            &on_event,
                            &cancel,
                            SftpEvent::Error {
                                id: Some(id),
                                label,
                                message: e,
                                path: Some(remote_parent_path(&remote)),
                            },
                        )
                        .await
                        {
                            return;
                        }
                    }
                }
            }
            SftpCmd::Remove { path, is_dir } => {
                let label = format!(
                    "删除 {}",
                    Path::new(&path)
                        .file_name()
                        .map(|s| s.to_string_lossy().into_owned())
                        .unwrap_or_else(|| path.clone())
                );
                let result = remove_remote_path(&sftp, &path, is_dir, &shutdown, &cancel).await;
                let parent = remote_parent_path(&path);
                match result {
                    Ok(()) => {
                        if !send_sftp_event(
                            &ev_tx,
                            &on_event,
                            &cancel,
                            SftpEvent::Done {
                                id: None,
                                label,
                                refresh: true,
                                path: Some(parent.clone()),
                            },
                        )
                        .await
                        {
                            return;
                        }
                    }
                    Err(e) if is_sftp_cancelled(&e) => break,
                    Err(e) => {
                        if !send_sftp_event(
                            &ev_tx,
                            &on_event,
                            &cancel,
                            SftpEvent::Error {
                                id: None,
                                label,
                                message: e.to_string(),
                                path: Some(parent),
                            },
                        )
                        .await
                        {
                            return;
                        }
                    }
                }
            }
            SftpCmd::Rename { from, to } => {
                let label = format!("重命名 {}", from);
                let parent = remote_parent_path(&from);
                match cancellable(&shutdown, &cancel, sftp.rename(&from, &to)).await {
                    Ok(()) => {
                        if !send_sftp_event(
                            &ev_tx,
                            &on_event,
                            &cancel,
                            SftpEvent::Done {
                                id: None,
                                label,
                                refresh: true,
                                path: Some(parent.clone()),
                            },
                        )
                        .await
                        {
                            return;
                        }
                    }
                    Err(e) if is_sftp_cancelled(&e) => break,
                    Err(e) => {
                        if !send_sftp_event(
                            &ev_tx,
                            &on_event,
                            &cancel,
                            SftpEvent::Error {
                                id: None,
                                label,
                                message: e.to_string(),
                                path: Some(parent),
                            },
                        )
                        .await
                        {
                            return;
                        }
                    }
                }
            }
            SftpCmd::Mkdir { path } => {
                let label = format!("新建目录 {}", path);
                let parent = remote_parent_path(&path);
                match cancellable(&shutdown, &cancel, sftp.create_dir(&path)).await {
                    Ok(()) => {
                        if !send_sftp_event(
                            &ev_tx,
                            &on_event,
                            &cancel,
                            SftpEvent::Done {
                                id: None,
                                label,
                                refresh: true,
                                path: Some(parent.clone()),
                            },
                        )
                        .await
                        {
                            return;
                        }
                    }
                    Err(e) if is_sftp_cancelled(&e) => break,
                    Err(e) => {
                        if !send_sftp_event(
                            &ev_tx,
                            &on_event,
                            &cancel,
                            SftpEvent::Error {
                                id: None,
                                label,
                                message: e.to_string(),
                                path: Some(parent),
                            },
                        )
                        .await
                        {
                            return;
                        }
                    }
                }
            }
            SftpCmd::Shutdown => {
                shutdown.store(true, Ordering::SeqCst);
                break;
            }
        }
    }
    // 关闭取消了前一个操作时，取消通知可能已经被消费；此时不能再等待
    // 有界事件队列腾出空间，否则关闭连接会被最后一个 Closed 事件卡住。
    if shutdown.load(Ordering::SeqCst) {
        if ev_tx.try_send(SftpEvent::Closed).is_ok() {
            on_event();
        }
    } else {
        let _ = send_sftp_event(&ev_tx, &on_event, &cancel, SftpEvent::Closed).await;
    }
}

/// 列出远程目录。
async fn list_dir(sftp: &SftpSession, path: &str) -> Result<Vec<RemoteEntry>, String> {
    let mut entries = Vec::new();
    for entry in sftp.read_dir(path).await.map_err(|e| e.to_string())? {
        let meta = entry.metadata();
        let name = entry.file_name();
        if !is_safe_entry_name(&name) {
            log::warn!("忽略异常 SFTP 条目名：{name:?}（目录 {path}）");
            continue;
        }
        entries.push(RemoteEntry {
            name,
            is_dir: meta.is_dir(),
            size: meta.len(),
            modified: meta.mtime.map(u64::from),
            permissions: meta.permissions.map(u64::from).unwrap_or(0) as u32,
        });
    }
    // 目录优先，按名称排序。
    entries.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    Ok(entries)
}

/// 判断远程路径是否词法上指向根目录或当前目录。
fn is_remote_root_path(path: &str) -> bool {
    let mut components = Vec::new();
    for component in path.split('/') {
        match component {
            "" | "." => {}
            ".." => {
                // 继续向上越过当前路径的顶层时，目标可能已经是根目录；
                // 保守拒绝这类路径，避免递归删除范围超出用户选择的条目。
                if components.pop().is_none() {
                    return true;
                }
            }
            _ => components.push(component),
        }
    }
    components.is_empty()
}

/// 删除远程文件或目录。
///
/// SFTP 的 `rmdir` 只能删除空目录，因此目录需要先深度优先删除其内容，
/// 再删除目录本身。目录项类型直接使用 SFTP 返回的 `file_type`：符号链接
/// 不会被当成目录递归跟随，只会删除链接自身，避免误删链接目标。
async fn remove_remote_path(
    sftp: &SftpSession,
    path: &str,
    is_dir: bool,
    shutdown: &AtomicBool,
    cancel: &Notify,
) -> Result<(), String> {
    if is_dir && is_remote_root_path(path) {
        return Err("拒绝删除远程根目录".to_string());
    }

    enum Work {
        Entry { path: String, is_dir: bool },
        RemoveDir(String),
    }

    let mut work = vec![Work::Entry {
        path: path.to_string(),
        is_dir,
    }];
    while let Some(item) = work.pop() {
        if shutdown.load(Ordering::SeqCst) {
            return Err(SFTP_CANCELLED.to_string());
        }
        match item {
            Work::Entry { path, is_dir: true } => {
                let entries = cancellable(shutdown, cancel, sftp.read_dir(&path)).await?;
                let mut children = Vec::new();
                for entry in entries {
                    let name = entry.file_name();
                    if !is_safe_entry_name(&name) {
                        return Err(format!("无法安全删除目录：发现异常远程条目名 {name:?}"));
                    }
                    children.push(Work::Entry {
                        path: entry.path(),
                        is_dir: entry.file_type().is_dir(),
                    });
                }
                // 先压入删除目录动作，再逆序压入子项，形成后序遍历。
                work.push(Work::RemoveDir(path));
                work.extend(children.into_iter().rev());
            }
            Work::Entry {
                path,
                is_dir: false,
            } => {
                cancellable(shutdown, cancel, sftp.remove_file(&path)).await?;
            }
            Work::RemoveDir(path) => {
                cancellable(shutdown, cancel, sftp.remove_dir(&path)).await?;
            }
        }
    }
    Ok(())
}

/// 尽力清理远程半成品。关闭连接时远端请求可能也在收尾，清理不能无限期
/// 阻塞后台线程，否则标签页关闭会一直等待连接析构。
async fn cleanup_remote_partial(sftp: &SftpSession, path: &str) {
    let _ = tokio::time::timeout(Duration::from_secs(5), sftp.remove_file(path)).await;
}

/// 上传本地文件到远程（带进度）。
/// `shutdown` 置位或 `cancel` 被通知时中止，并清理远程半成品。
#[allow(clippy::too_many_arguments)]
async fn upload_file(
    sftp: &SftpSession,
    local: &Path,
    remote: &str,
    id: u64,
    label: &str,
    ev_tx: &Sender<SftpEvent>,
    on_event: &SftpEventHandler,
    shutdown: &AtomicBool,
    cancel: &Notify,
) -> Result<(), String> {
    let partial = partial_remote_path(remote);
    let result = cancellable(shutdown, cancel, async {
        if shutdown.load(Ordering::SeqCst) {
            return Err(SFTP_CANCELLED.to_string());
        }
        let mut local_file = tokio::fs::File::open(local)
            .await
            .map_err(|e| e.to_string())?;
        let total = local_file
            .metadata()
            .await
            .map_err(|e| e.to_string())?
            .len();
        if shutdown.load(Ordering::SeqCst) {
            return Err(SFTP_CANCELLED.to_string());
        }
        let mut remote_file = sftp.create(&partial).await.map_err(|e| e.to_string())?;

        let mut done = 0u64;
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            let n = local_file.read(&mut buf).await.map_err(|e| e.to_string())?;
            if n == 0 {
                break;
            }
            remote_file
                .write_all(&buf[..n])
                .await
                .map_err(|e| e.to_string())?;
            done += n as u64;
            // 进度是可丢弃的最新状态；有界队列满时不阻塞传输线程。
            try_send_progress(
                ev_tx,
                on_event,
                SftpEvent::Progress {
                    id,
                    label: label.to_string(),
                    done,
                    total,
                },
            )?;
            // 关闭标签页/连接时立即中止：外层错误路径会删除远程半成品。
            if shutdown.load(Ordering::SeqCst) {
                return Err(SFTP_CANCELLED.to_string());
            }
        }
        if shutdown.load(Ordering::SeqCst) {
            return Err(SFTP_CANCELLED.to_string());
        }
        remote_file.close().await.map_err(|e| e.to_string())
    })
    .await;
    if let Err(error) = result {
        cleanup_remote_partial(sftp, &partial).await;
        return Err(error);
    }
    if shutdown.load(Ordering::SeqCst) {
        cleanup_remote_partial(sftp, &partial).await;
        return Err(SFTP_CANCELLED.to_string());
    }
    match cancellable(shutdown, cancel, sftp.rename(&partial, remote)).await {
        Ok(()) => {}
        Err(error) => {
            cleanup_remote_partial(sftp, &partial).await;
            return Err(error);
        }
    }
    Ok(())
}

/// 下载远程文件到本地（带进度）。
/// `shutdown` 置位或 `cancel` 被通知时中止，并清理本地半成品。
#[allow(clippy::too_many_arguments)]
async fn download_file(
    sftp: &SftpSession,
    remote: &str,
    local: &Path,
    id: u64,
    label: &str,
    ev_tx: &Sender<SftpEvent>,
    on_event: &SftpEventHandler,
    shutdown: &AtomicBool,
    cancel: &Notify,
) -> Result<(), String> {
    let partial = partial_local_path(local);
    let result = cancellable(shutdown, cancel, async {
        if shutdown.load(Ordering::SeqCst) {
            return Err(SFTP_CANCELLED.to_string());
        }
        let meta = sftp.metadata(remote).await.map_err(|e| e.to_string())?;
        let total = meta.len();
        if shutdown.load(Ordering::SeqCst) {
            return Err(SFTP_CANCELLED.to_string());
        }
        let mut remote_file = sftp.open(remote).await.map_err(|e| e.to_string())?;
        let mut local_file = tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&partial)
            .await
            .map_err(|e| e.to_string())?;

        let mut done = 0u64;
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            let n = remote_file
                .read(&mut buf)
                .await
                .map_err(|e| e.to_string())?;
            if n == 0 {
                break;
            }
            local_file
                .write_all(&buf[..n])
                .await
                .map_err(|e| e.to_string())?;
            done += n as u64;
            try_send_progress(
                ev_tx,
                on_event,
                SftpEvent::Progress {
                    id,
                    label: label.to_string(),
                    done,
                    total,
                },
            )?;
            // 关闭标签页/连接时立即中止：外层错误路径会删除本地半成品。
            if shutdown.load(Ordering::SeqCst) {
                return Err(SFTP_CANCELLED.to_string());
            }
        }
        local_file.flush().await.map_err(|e| e.to_string())?;
        local_file.sync_all().await.map_err(|e| e.to_string())?;
        if shutdown.load(Ordering::SeqCst) {
            return Err(SFTP_CANCELLED.to_string());
        }
        Ok(())
    })
    .await;
    if let Err(error) = result {
        let _ = tokio::fs::remove_file(&partial).await;
        return Err(error);
    }
    if shutdown.load(Ordering::SeqCst) {
        let _ = tokio::fs::remove_file(&partial).await;
        return Err(SFTP_CANCELLED.to_string());
    }
    if let Err(error) = tokio::fs::rename(&partial, local).await {
        let _ = tokio::fs::remove_file(&partial).await;
        return Err(error.to_string());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 关闭句柄置位取消标志并发送关闭命令() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let handle = SftpHandle::from_raw(tx);
        handle.close();
        // 取消标志立即置位：后台传输循环不等命令队列排空即可中止。
        assert!(handle.shutdown.load(Ordering::SeqCst));
        // Shutdown 命令作为操作循环退出的兜底。
        assert!(matches!(rx.try_recv(), Ok(SftpCmd::Shutdown)));
    }

    #[test]
    fn 传输临时文件与目标同目录() {
        let local = Path::new("/tmp/result.txt");
        let partial = partial_local_path(local);
        assert_eq!(partial.parent(), local.parent());
        assert!(partial
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with(".result.txt.mino-partial-")));

        let remote = partial_remote_path("/srv/result.txt");
        assert!(remote.starts_with("/srv/result.txt.mino-partial-"));
    }

    #[test]
    fn 传输句柄分配稳定唯一标识() {
        let (cmd_tx, mut cmd_rx) = tokio::sync::mpsc::unbounded_channel();
        let handle = SftpHandle::from_raw(cmd_tx);
        let first = handle.upload(Path::new("/tmp/a.txt"), "/srv/a.txt");
        let second = handle.upload(Path::new("/tmp/a.txt"), "/srv/a.txt");
        assert_ne!(first, second);
        assert!(matches!(
            cmd_rx.try_recv(),
            Ok(SftpCmd::Upload { id, .. }) if id == first
        ));
        assert!(matches!(
            cmd_rx.try_recv(),
            Ok(SftpCmd::Upload { id, .. }) if id == second
        ));
    }

    #[test]
    fn 远程条目名限制为单个路径分量() {
        assert!(is_safe_entry_name("普通文件.txt"));
        assert!(is_safe_entry_name("带\\反斜杠的名字"));
        assert!(!is_safe_entry_name(""));
        assert!(!is_safe_entry_name("."));
        assert!(!is_safe_entry_name(".."));
        assert!(!is_safe_entry_name("../outside"));
        assert!(!is_safe_entry_name("sub/file"));
        assert!(!is_safe_entry_name("bad\0name"));
    }

    #[test]
    fn 远程操作父目录路径正确归一() {
        assert_eq!(remote_parent_path("/"), "/");
        assert_eq!(remote_parent_path("/file"), "/");
        assert_eq!(remote_parent_path("/home/file"), "/home");
        assert_eq!(remote_parent_path("/home/file/"), "/home");
        assert_eq!(remote_parent_path("relative"), ".");
    }

    #[test]
    fn 递归删除拒绝根目录路径() {
        assert!(is_remote_root_path(""));
        assert!(is_remote_root_path("."));
        assert!(is_remote_root_path("/"));
        assert!(is_remote_root_path("//"));
        assert!(is_remote_root_path("/home/.."));
        assert!(is_remote_root_path("work/../.."));
        assert!(!is_remote_root_path("/home/user"));
        assert!(!is_remote_root_path("/home/../user"));
    }
}
