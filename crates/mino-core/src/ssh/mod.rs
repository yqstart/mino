//! SSH 远程会话：远程终端 + SFTP（基于 russh / russh-sftp）。
//!
//! 远程终端与本地终端共用 `Term` 状态机：后台 tokio 任务读 SSH channel，
//! 数据经 vte 解析器喂入 `Term`，写操作走命令队列（UI 线程非阻塞）。

pub(crate) mod known_hosts;
pub mod sftp;

use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, LazyLock,
};
use std::time::Duration;

use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::{self, Term};
use alacritty_terminal::vte::ansi::Processor;
use russh::client;
use russh::keys::{decode_secret_key, HashAlg, PrivateKeyWithHashAlg};
use russh::{Channel, ChannelMsg};
use tokio::sync::{
    mpsc::{self, UnboundedReceiver},
    Notify,
};

use crate::config::Auth;
use crate::terminal::{
    feed_program_output, notify_wakeup, EventHandler, Listener, Session, SessionEvent, Shared,
    TermSize,
};

use known_hosts::{default_known_hosts_path, HostKeyVerifier};

/// 进程级共享 tokio runtime（远程终端与 SFTP 后台任务共用）。
///
/// 此前每个远程连接与每个 SFTP 连接各建一个 2-worker runtime：N 个远程
/// 标签 = 2N 个 runtime + 2N 个常驻 worker 线程，线程数、内存与上下文
/// 切换开销随标签数线性增长。共享 runtime 只建一次（worker 数 = CPU 数，
/// 上限 8），所有连接任务经 `spawn` 提交。
///
/// 初始化失败以 `Err` 返回而不是 panic：调用方需要像以前一样把失败回传
/// 给 UI（不能让窗口永久停在"正在连接…"）。`LazyLock` 持有结果本身，
/// 失败后不会每次访问都重新尝试创建。
pub(crate) static SHARED_RUNTIME: LazyLock<Result<tokio::runtime::Runtime, String>> =
    LazyLock::new(|| {
        let workers = std::thread::available_parallelism()
            .map(|n| n.get().clamp(2, 8))
            .unwrap_or(4);
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(workers)
            .thread_name("mino-async")
            .enable_all()
            .build()
            .map_err(|e| e.to_string())
    });

/// 在一个只做阻塞等待的轻量 runtime 上等一个共享 runtime 任务结束。
///
/// 共享 runtime 的 worker 里不能 `block_on`（会占住 worker 甚至自锁），
/// 而调用方历史上拿到的是一个可被丢弃的线程句柄；等待线程自身不需要
/// reactor，`current_thread` runtime 只用来 `block_on` 那个 JoinHandle。
pub(crate) fn wait_for_task(task: tokio::task::JoinHandle<()>) {
    match tokio::runtime::Builder::new_current_thread().build() {
        Ok(runtime) => {
            let _ = runtime.block_on(task);
        }
        Err(e) => log::warn!("创建等待任务的 runtime 失败，后台任务改由进程退出回收：{e}"),
    }
}

/// 远程会话后台命令。
enum SessionCmd {
    /// 写入字节。
    Write(Vec<u8>),
    /// 调整终端尺寸。
    Resize(u16, u16),
    /// 关闭。
    Shutdown,
}

/// 连接结果（异步任务 → UI 线程）。
pub enum ConnectResult {
    /// 连接成功，返回会话。
    Connected(Session),
    /// 连接失败，返回错误信息。
    Failed(String),
}

/// 取消尚未完成的 SSH 连接尝试。
///
/// 连接失败或用户发起下一次连接时，必须取消旧的 TCP/认证 future；否则
/// 丢弃 receiver 只会丢掉结果，后台线程仍可能继续占用连接和 runtime，
/// 直到超时才退出。
#[derive(Clone)]
pub struct ConnectCancel {
    cancelled: Arc<AtomicBool>,
    notify: Arc<Notify>,
}

impl ConnectCancel {
    /// 取消连接尝试；重复调用安全。
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
        self.notify.notify_one();
    }

    fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }
}

/// 统一的 SSH 客户端配置：30 秒无数据发 keepalive，3 次无响应断开
/// （空闲连接被中间设备静默断开后能及时发现，避免会话假死）。
///
/// 注意：russh 0.62 的 `client::Config` 没有连接超时字段（TCP 阶段依赖 OS
/// 默认超时，可达 75s+），TCP 连接/握手与认证阶段的限时由调用处
/// `tokio::time::timeout` 显式包裹（见 `CONNECT_TIMEOUT`/`AUTH_TIMEOUT`）。
fn ssh_config() -> client::Config {
    client::Config {
        keepalive_interval: Some(std::time::Duration::from_secs(30)),
        keepalive_max: 3,
        ..Default::default()
    }
}

/// TCP 连接 + SSH 握手 + KEX 阶段超时：不可达主机（黑洞防火墙/路由丢弃）
/// 下 OS TCP 超时过长，UI 会无限期"正在连接…"且无取消入口。
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

/// 认证阶段超时：服务器接受 TCP 后静默时，认证不应无限挂起
/// （密码/私钥均为自动认证，无需等待人工输入）。
const AUTH_TIMEOUT: Duration = Duration::from_secs(30);

/// 连接并校验服务器密钥（TOFU），失败时返回带原因的错误。
/// TCP 连接与握手阶段限时 `CONNECT_TIMEOUT`，超时返回明确错误。
pub(crate) async fn connect_verified(
    config: Arc<client::Config>,
    profile: &crate::config::HostProfile,
) -> Result<client::Handle<HostKeyVerifier>, String> {
    let (verifier, verifier_error) = HostKeyVerifier::new(
        profile.host.clone(),
        profile.port,
        default_known_hosts_path(),
    );
    match tokio::time::timeout(
        CONNECT_TIMEOUT,
        client::connect(config, (profile.host.as_str(), profile.port), verifier),
    )
    .await
    {
        Ok(Ok(handle)) => Ok(handle),
        Ok(Err(e)) => {
            // 密钥校验失败时给出明确原因（指纹不匹配 + 修复指引）；
            // 其他错误（TCP 拒绝等）沿用 russh 原文。
            let detail = verifier_error.lock().unwrap().take();
            Err(detail.unwrap_or_else(|| e.to_string()))
        }
        Err(_) => Err(format!(
            "连接超时（{} 秒无响应）",
            CONNECT_TIMEOUT.as_secs()
        )),
    }
}

/// 发起远程会话连接（非阻塞，立即返回）。
///
/// 连接在后台线程完成；结果通过返回的 receiver 接收。
/// 后台线程的 tokio runtime 存活到会话关闭（remote_loop 结束时）。
pub fn connect_remote(
    profile: &crate::config::HostProfile,
    cols: u16,
    rows: u16,
    on_event: EventHandler,
) -> (
    std::thread::JoinHandle<()>,
    UnboundedReceiver<ConnectResult>,
) {
    let (thread, rx, _cancel) = connect_remote_with_cancel(profile, cols, rows, on_event);
    (thread, rx)
}

/// 发起可取消的远程会话连接。
///
/// 返回的取消句柄只影响 TCP、认证和 shell 建立阶段；连接成功后，终端
/// 会话由 `Session` 自己的关闭逻辑管理。应用在替换/销毁 pending receiver
/// 前应调用 `ConnectCancel::cancel()`。
pub fn connect_remote_with_cancel(
    profile: &crate::config::HostProfile,
    cols: u16,
    rows: u16,
    on_event: EventHandler,
) -> (
    std::thread::JoinHandle<()>,
    UnboundedReceiver<ConnectResult>,
    ConnectCancel,
) {
    let (tx, rx) = mpsc::unbounded_channel();
    let cancel = ConnectCancel {
        cancelled: Arc::new(AtomicBool::new(false)),
        notify: Arc::new(Notify::new()),
    };
    let thread_cancel = cancel.clone();
    let profile = profile.clone();
    // 共享 runtime 上提交连接任务：不再为每个连接建 2-worker runtime + 线程。
    // 外层线程只等待任务结束（调用方历史上拿到线程句柄，仅持有不 join）。
    let handle = std::thread::spawn(move || {
        // runtime 不可用（资源耗尽等极端情况）也必须回传失败事件，
        // 不能 panic 在后台线程让 UI 永久停在"正在连接…"。
        let runtime = match SHARED_RUNTIME.as_ref() {
            Ok(runtime) => runtime,
            Err(e) => {
                let _ = tx.send(ConnectResult::Failed(format!("初始化连接运行时失败：{e}")));
                return;
            }
        };
        wait_for_task(runtime.spawn(connect_and_serve_remote(
            profile,
            cols,
            rows,
            on_event,
            thread_cancel,
            tx,
        )));
    });
    (handle, rx, cancel)
}

/// 远程建连与会话服务（共享 runtime 上的任务体）。
///
/// 建连阶段每一步都经 `cancellable_connect` 响应取消；成功后 `remote_loop`
/// 接管 channel，会话关闭（`session_done`）即任务结束，等待线程随之退出。
async fn connect_and_serve_remote(
    profile: crate::config::HostProfile,
    cols: u16,
    rows: u16,
    on_event: EventHandler,
    thread_cancel: ConnectCancel,
    tx: mpsc::UnboundedSender<ConnectResult>,
) {
    {
        let session_done = Arc::new(tokio::sync::Notify::new());
        let session_done_loop = session_done.clone();
        {
            // ============ 1. TCP 连接与认证（含主机密钥 TOFU 校验） ============
            let config = Arc::new(ssh_config());
            let mut handle =
                match cancellable_connect(&thread_cancel, connect_verified(config, &profile)).await
                {
                    Ok(h) => h,
                    Err(e) if e == CONNECT_CANCELLED => return,
                    Err(e) => {
                        let _ = tx.send(ConnectResult::Failed(format!(
                            "连接 {}:{} 失败：{e}",
                            profile.host, profile.port
                        )));
                        return;
                    }
                };

            let authed = match cancellable_connect(
                &thread_cancel,
                authenticate(&mut handle, &profile),
            )
            .await
            {
                Ok(ok) => ok,
                Err(e) if e == CONNECT_CANCELLED => return,
                Err(e) => {
                    let _ = tx.send(ConnectResult::Failed(e));
                    return;
                }
            };
            if !authed {
                let _ = tx.send(ConnectResult::Failed(
                    "认证失败：用户名或密码/密钥错误".into(),
                ));
                return;
            }

            // ============ 2. 打开 shell channel ============
            let channel =
                match cancellable_connect(&thread_cancel, handle.channel_open_session()).await {
                    Ok(c) => c,
                    Err(e) if e == CONNECT_CANCELLED => return,
                    Err(e) => {
                        let _ = tx.send(ConnectResult::Failed(format!("打开会话失败：{e}")));
                        return;
                    }
                };
            if let Err(e) = cancellable_connect(
                &thread_cancel,
                channel.request_pty(true, "xterm-256color", cols as u32, rows as u32, 0, 0, &[]),
            )
            .await
            {
                if e == CONNECT_CANCELLED {
                    return;
                }
                let _ = tx.send(ConnectResult::Failed(format!("申请 PTY 失败：{e}")));
                return;
            }
            // COLORTERM=truecolor：omp 的颜色档位只认该变量（`getColorMode`
            // 实证，`TERM=xterm-256color` 只判 256 色）；sshd 默认不透传该
            // 变量，`AcceptEnv` 未放行时 set_env 失败也不阻塞建连（want_reply
            // 取 false，失败静默忽略）。
            let _ = cancellable_connect(
                &thread_cancel,
                channel.set_env(false, "COLORTERM", "truecolor"),
            )
            .await;
            if let Err(e) = cancellable_connect(&thread_cancel, channel.request_shell(true)).await {
                if e == CONNECT_CANCELLED {
                    return;
                }
                let _ = tx.send(ConnectResult::Failed(format!("启动 shell 失败：{e}")));
                return;
            }

            if thread_cancel.is_cancelled() {
                return;
            }

            // ============ 3. 创建终端状态机 ============
            let shared = Arc::new(Shared::default());
            let term: Arc<FairMutex<Term<Listener>>> = Arc::new(FairMutex::new(Term::new(
                term::Config {
                    kitty_keyboard: true,
                    ..term::Config::default()
                },
                &TermSize {
                    rows: rows as usize,
                    cols: cols as usize,
                },
                Listener {
                    shared: shared.clone(),
                    on_event: on_event.clone(),
                },
            )));

            // ============ 4. 命令队列（UI → 后台） ============
            let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<SessionCmd>();

            // ============ 5. 后台读循环（持有 handle 保持连接） ============
            let remote_term = term.clone();
            let remote_shared = shared.clone();
            let remote_on_event = on_event.clone();
            let session_cancel = Arc::new(tokio::sync::Notify::new());
            let session_cancel_loop = session_cancel.clone();
            tokio::spawn(async move {
                remote_loop(
                    channel,
                    cmd_rx,
                    remote_term,
                    remote_shared,
                    remote_on_event,
                    handle,
                    session_cancel_loop,
                )
                .await;
                session_done_loop.notify_one();
            });

            // ============ 6. 组装会话 ============
            let writer_tx = cmd_tx.clone();
            let resizer_tx = cmd_tx.clone();
            let shuttor_tx = cmd_tx.clone();
            let writer = crate::terminal::Writer::new(move |bytes: &[u8]| {
                let _ = writer_tx.send(SessionCmd::Write(bytes.to_vec()));
            });
            let resizer = crate::terminal::Resizer::new(move |cols: u16, rows: u16| {
                let _ = resizer_tx.send(SessionCmd::Resize(cols, rows));
            });
            let shuttor_cancel = session_cancel.clone();
            let shuttor = crate::terminal::Shuttor::new(move || {
                // 关闭时先唤醒可能正在等待 SSH channel 流控窗口的写操作，
                // 再发送 Shutdown；否则命令循环可能卡在 channel.data().await，
                // 永远处理不到队列里的关闭命令。
                shuttor_cancel.notify_one();
                let _ = shuttor_tx.send(SessionCmd::Shutdown);
            });

            let _ = tx.send(ConnectResult::Connected(Session::new(
                term, shared, writer, resizer, shuttor, true, None,
            )));

            // ============ 7. 等待会话关闭：任务随 remote_loop 结束而结束 ============
            session_done.notified().await;
        }
    }
}

const CONNECT_CANCELLED: &str = "连接已取消";

/// 让连接建立阶段的任意异步操作响应取消句柄。
async fn cancellable_connect<T, E, F>(cancel: &ConnectCancel, future: F) -> Result<T, String>
where
    E: std::fmt::Display,
    F: std::future::Future<Output = Result<T, E>>,
{
    if cancel.is_cancelled() {
        return Err(CONNECT_CANCELLED.to_string());
    }
    let notified = cancel.notify.notified();
    tokio::pin!(notified);
    if cancel.is_cancelled() {
        return Err(CONNECT_CANCELLED.to_string());
    }
    tokio::select! {
        biased;
        result = future => result.map_err(|error| error.to_string()),
        _ = &mut notified => Err(CONNECT_CANCELLED.to_string()),
    }
}

/// 远程终端后台循环：读 channel 数据喂解析器，消费命令队列。
async fn remote_loop(
    mut channel: Channel<russh::client::Msg>,
    mut cmd_rx: UnboundedReceiver<SessionCmd>,
    term: Arc<FairMutex<Term<Listener>>>,
    shared: Arc<Shared>,
    on_event: EventHandler,
    _handle: client::Handle<HostKeyVerifier>,
    cancel: Arc<tokio::sync::Notify>,
) {
    log::info!("remote_loop 启动");
    let mut parser: alacritty_terminal::vte::ansi::Processor = Processor::new();

    loop {
        tokio::select! {
            // 尺寸/输入命令优先于继续消费输出。npm 等动态进度渲染会根据
            // PTY 宽度生成回车刷新行；窗口变窄后若先处理多段旧宽度输出，
            // 这些行会在本地终端中换行成醒目的背景块。
            biased;
            // 关闭句柄时无论命令队列是否还能入队，都要退出读循环并释放
            // channel/SSH handle，避免远程连接线程泄漏。
            _ = cancel.notified() => {
                break;
            }
            // 命令队列：UI 线程写入/缩放。
            cmd = cmd_rx.recv() => {
                match cmd {
                    Some(SessionCmd::Write(bytes)) => {
                        log::debug!("远程写入 {} 字节", bytes.len());
                        let result = tokio::select! {
                            biased;
                            result = channel.data_bytes(bytes) => Some(result),
                            _ = cancel.notified() => None,
                        };
                        match result {
                            Some(Err(e)) => {
                                log::warn!("写入远程终端失败：{e}");
                                break;
                            }
                            Some(Ok(())) => {}
                            None => break,
                        }
                    }
                    Some(SessionCmd::Resize(cols, rows)) => {
                        let result = tokio::select! {
                            biased;
                            result = channel.window_change(cols as u32, rows as u32, 0, 0) =>
                                Some(result),
                            _ = cancel.notified() => None,
                        };
                        match result {
                            Some(Err(e)) => log::debug!("调整远程终端尺寸失败：{e}"),
                            Some(Ok(())) => {}
                            None => break,
                        }
                    }
                    Some(SessionCmd::Shutdown) | None => {
                        break;
                    }
                }
            }
            // channel 数据：远程输出 → vte 解析 → Term。
            msg = channel.wait() => {
                match msg {
                    Some(ChannelMsg::Data { data }) => {
                        log::debug!("远程收到 {} 字节", data.len());
                        if feed_program_output(&term, &shared, &data) {
                            notify_wakeup(&shared, &on_event);
                        } else {
                            let mut guard = term.lock();
                            parser.advance(&mut *guard, &data);
                            drop(guard);
                            notify_wakeup(&shared, &on_event);
                        }
                    }
                    Some(ChannelMsg::ExtendedData { data, .. }) => {
                        // stderr 也喂入解析器（保持输出顺序完整）。
                        let mut guard = term.lock();
                        parser.advance(&mut *guard, &data);
                        drop(guard);
                        notify_wakeup(&shared, &on_event);
                    }
                    Some(ChannelMsg::Eof) | Some(ChannelMsg::Close) => {
                        break;
                    }
                    Some(_) => {}
                    None => {
                        break;
                    }
                }
            }
        }
    }

    *shared.exited.lock().unwrap() = true;
    (on_event)(&SessionEvent::ChildExit);
}

/// 展开私钥路径中的 `~`（配置里用户常写 `~/.ssh/id_ed25519`，
/// 而 `std::fs` 不展开波浪号，会导致加载失败）。
fn expand_tilde(path: &Path) -> PathBuf {
    let Some(s) = path.to_str() else {
        return path.to_path_buf();
    };
    let home = std::env::var("HOME").ok().map(PathBuf::from);
    if s == "~" {
        if let Some(h) = home {
            return h;
        }
    } else if let Some(rest) = s.strip_prefix("~/") {
        if let Some(h) = home {
            return h.join(rest);
        }
    }
    path.to_path_buf()
}

/// 加载私钥文件（支持口令，路径自动展开 `~`）。
fn load_private_key(
    path: &Path,
    passphrase: Option<&str>,
) -> Result<russh::keys::PrivateKey, String> {
    let expanded = expand_tilde(path);
    let content = std::fs::read_to_string(&expanded)
        .map_err(|e| format!("读取私钥 {} 失败：{e}", expanded.display()))?;
    decode_secret_key(&content, passphrase).map_err(|e| e.to_string())
}

/// 执行认证（密码或私钥），返回是否成功。
/// 整体限时 `AUTH_TIMEOUT`：服务器接受 TCP 后静默时认证不应无限挂起。
pub(crate) async fn authenticate(
    handle: &mut client::Handle<HostKeyVerifier>,
    profile: &crate::config::HostProfile,
) -> Result<bool, String> {
    tokio::time::timeout(AUTH_TIMEOUT, authenticate_inner(handle, profile))
        .await
        .map_err(|_| format!("认证超时（{} 秒无响应）", AUTH_TIMEOUT.as_secs()))?
}

/// `authenticate` 的实际认证逻辑（密码或私钥）。
async fn authenticate_inner(
    handle: &mut client::Handle<HostKeyVerifier>,
    profile: &crate::config::HostProfile,
) -> Result<bool, String> {
    let authed = match &profile.auth {
        Auth::Password(password) => handle
            .authenticate_password(&profile.user, password)
            .await
            .map_err(|e| format!("认证失败：{e}"))?
            .success(),
        Auth::Key { path, passphrase } => {
            let key = load_private_key(path, passphrase.as_deref())
                .map_err(|e| format!("加载私钥失败：{e}"))?;
            let key = PrivateKeyWithHashAlg::new(Arc::new(key), Some(HashAlg::Sha256));
            handle
                .authenticate_publickey(&profile.user, key)
                .await
                .map_err(|e| format!("公钥认证失败：{e}"))?
                .success()
        }
    };
    Ok(authed)
}
