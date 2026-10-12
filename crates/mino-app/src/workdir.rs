//! 终端工作目录跟踪。
//!
//! 终端本身仍由 shell 负责命令编辑和补全；这里根据已经写入 PTY 的字节，
//! 并结合终端中 `pwd` 的实际输出，尽力追踪终端当前目录，为 SFTP 面板
//! 提供快捷定位。

use std::path::{Path, PathBuf};

/// 终端工作目录跟踪器。
pub struct WorkdirTracker {
    text: String,
    valid: bool,
    cwd: PathBuf,
    /// 是否已经由终端输入显式改变过目录。
    ///
    /// 远程 SFTP 连接与 SSH 终端并行建立；SFTP 返回 home 时，只能覆盖
    /// 尚未被用户操作过的初始值，不能把用户已经输入的 `cd` 重置回 home。
    cwd_dirty: bool,
    /// 远程 home 尚未返回时暂存的 cd 命令，收到 home 后按输入顺序补算。
    pending_remote_cds: Vec<String>,
    /// 最近一次执行的 `pwd` 尚未从终端输出中得到结果。
    awaiting_pwd_output: bool,
    /// 定位到终端目录时自动发送的 `pwd` 尚未从终端输出中得到结果。
    /// 与用户手输 `pwd` 的 `awaiting_pwd_output` 区分开——两者共享同一份
    /// 基线/校正管线，但用户行为不能意外消费掉自动探测的状态。
    awaiting_auto_pwd_output: bool,
}

impl WorkdirTracker {
    pub fn new(cwd: PathBuf) -> Self {
        Self {
            text: String::new(),
            valid: true,
            cwd,
            cwd_dirty: false,
            pending_remote_cds: Vec::new(),
            awaiting_pwd_output: false,
            awaiting_auto_pwd_output: false,
        }
    }

    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    /// 是否正在等待最近一次 `pwd` 的终端输出。
    pub fn awaiting_pwd_output(&self) -> bool {
        self.awaiting_pwd_output
    }

    /// 当前输入行是否为空且跟踪有效（定位注入 `pwd` 的空闲前提）。
    pub(crate) fn input_is_idle(&self) -> bool {
        self.valid && self.text.is_empty()
    }

    /// 是否正在等待定位探测发送的 `pwd` 的终端输出。
    pub fn awaiting_auto_pwd_output(&self) -> bool {
        self.awaiting_auto_pwd_output
    }

    /// SFTP 定位前调用：进入自动 `pwd` 探测等待态。
    ///
    /// 返回 false 表示上一次自动探测的输出还没回来（pwd 正在执行中），
    /// 调用方不应再注入一条新的 `pwd`。
    pub fn begin_auto_pwd(&mut self) -> bool {
        if self.awaiting_auto_pwd_output {
            return false;
        }
        self.awaiting_auto_pwd_output = true;
        true
    }

    /// 取消未完成的自动 `pwd` 探测（定位超时 / SFTP 无终端上下文 / 会话关闭）。
    pub fn cancel_auto_pwd(&mut self) {
        self.awaiting_auto_pwd_output = false;
    }

    /// 追加可见文本（写入 PTY 后同步）。
    pub fn push_text(&mut self, text: &str) {
        if self.valid {
            self.text.push_str(text);
        }
    }

    /// 退格：删除末尾一个字符。
    pub fn backspace(&mut self) {
        if self.valid {
            self.text.pop();
        }
    }

    /// shell 集成上报的权威目录（OSC 7）：直接覆盖推测值。
    ///
    /// 输入跟踪在别名/函数/`cd -`/复合命令下会失效，OSC 7 是 shell 自己
    /// 在每个提示符处上报的真值（本地与远程会话通用）；置位 `cwd_dirty`
    /// 防止旧逻辑再把它重置回初始 home。
    pub fn set_directory(&mut self, path: PathBuf) {
        self.cwd = path;
        self.valid = true;
        self.cwd_dirty = true;
        self.text.clear();
    }

    /// 当前输入行（提示符后已输入、尚未回车的可见文本）。
    ///
    /// 供 AI 失败命令建议与 `#` 自然语言拦截取上下文；跟踪失效时内容
    /// 可能不完整（调用方自行判断 `valid` 语义）。
    pub fn current_input(&self) -> &str {
        &self.text
    }

    /// 设置初始目录，但不覆盖用户已经通过终端输入跟踪到的目录。
    pub fn set_cwd_if_unmodified(&mut self, cwd: PathBuf) {
        if !self.cwd_dirty {
            self.cwd = cwd.clone();
        }
        let pending = std::mem::take(&mut self.pending_remote_cds);
        for arg in pending {
            self.apply_remote_cd(&arg, Some(&cwd));
        }
    }

    /// 回车：执行当前命令，尝试解析 `cd` 后清空输入。
    pub fn execute(&mut self) {
        let is_pwd = self.valid && is_pwd_command(&self.text);
        if self.valid {
            if let Some(arg) = self.cd_argument() {
                self.apply_local_cd(&arg);
            }
        }
        self.text.clear();
        self.valid = true;
        self.awaiting_pwd_output = is_pwd;
    }

    /// 回车：在远程会话中按 POSIX 路径规则追踪常用的 `cd` 命令。
    ///
    /// 远程目录无法用本地文件系统 `canonicalize`，因此只做词法归一化；
    /// 未覆盖的 shell 函数、别名或 `cd -` 仍会保留上一次已知目录。
    pub fn execute_remote(&mut self, home: Option<&Path>) {
        let is_pwd = self.valid && is_pwd_command(&self.text);
        if self.valid {
            if let Some(arg) = self.cd_argument() {
                self.apply_remote_cd(&arg, home);
            }
        }
        self.text.clear();
        self.valid = true;
        self.awaiting_pwd_output = is_pwd;
    }

    /// 控制键/编辑序列后无法可靠追踪当前输入，暂停目录更新。
    pub fn invalidate(&mut self) {
        self.valid = false;
        self.awaiting_pwd_output = false;
    }

    /// 重置当前输入跟踪。
    pub fn reset(&mut self) {
        self.text.clear();
        self.valid = true;
        self.awaiting_pwd_output = false;
    }

    /// 终端输出一帧：自动 `pwd` 探测与用户手输 `pwd` 复用同一套基线/校正
    /// 管线。用户正常输入 `pwd` 时只占用手工态，定位探测的结果同样能把
    /// 推测值校正为 shell 真正所在的目录，两种等待态都要消费。
    pub fn awaiting_any_pwd_output(&self) -> bool {
        self.awaiting_pwd_output || self.awaiting_auto_pwd_output
    }

    /// 从终端前后两帧的可见行中读取 `pwd` 的实际结果。
    ///
    /// 只检查发生变化的行，避免把历史输出中的任意绝对路径误当成当前
    /// 目录。终端输入被粘贴、使用别名/函数，或跟踪器曾因编辑序列失效时，
    /// 这个结果可以把推测值校正为 shell 真正所在的目录。
    pub fn observe_local_output(&mut self, previous: &[String], current: &[String]) -> bool {
        if !self.awaiting_any_pwd_output() {
            return false;
        }
        let Some(path) = changed_pwd_path(previous, current) else {
            return false;
        };
        let Ok(canonical) = std::fs::canonicalize(path) else {
            return false;
        };
        self.cwd = canonical;
        self.cwd_dirty = true;
        self.awaiting_pwd_output = false;
        self.awaiting_auto_pwd_output = false;
        true
    }

    /// 从终端前后两帧的可见行中读取远程 `pwd` 的实际结果。
    pub fn observe_remote_output(&mut self, previous: &[String], current: &[String]) -> bool {
        if !self.awaiting_any_pwd_output() {
            return false;
        }
        let Some(path) = changed_pwd_path(previous, current) else {
            return false;
        };
        self.cwd = normalize_remote_path(Path::new(path));
        self.cwd_dirty = true;
        self.awaiting_pwd_output = false;
        self.awaiting_auto_pwd_output = false;
        true
    }

    /// 提取当前输入中的 `cd` 参数。
    fn cd_argument(&self) -> Option<String> {
        // 前缀判定要求 `cd` 后为空或空白开头——`cdfoo` 是另一个命令，
        // 不能误判为 `cd foo`。遇到 shell 控制语法时保守放弃更新，避免
        // 把 `cd /tmp && ls`、引号路径或变量展开误当成一个远程路径。
        let rest = self
            .text
            .trim()
            .strip_prefix("cd")
            .filter(|rest| rest.is_empty() || rest.starts_with(char::is_whitespace))?;
        let arg = rest.trim();
        if arg.chars().any(|c| {
            matches!(
                c,
                '&' | '|' | ';' | '#' | '>' | '<' | '"' | '\'' | '\\' | '$'
            )
        }) {
            return None;
        }
        Some(arg.to_string())
    }

    /// 应用本地 `cd` 参数。
    fn apply_local_cd(&mut self, arg: &str) {
        let home = std::env::var("HOME").map(PathBuf::from).ok();
        let target = if arg.is_empty() {
            home.clone().unwrap_or_else(|| self.cwd.clone())
        } else if arg == "~" {
            match home {
                Some(home) => home,
                None => self.cwd.clone(),
            }
        } else if let Some(rest) = arg.strip_prefix("~/") {
            match home {
                Some(home) => home.join(rest),
                None => self.cwd.join(arg),
            }
        } else if arg == "-" {
            // `cd -` 依赖 shell 保存的上一个目录，跟踪器无法可靠推断。
            return;
        } else {
            let path = Path::new(arg);
            if path.is_absolute() {
                path.to_path_buf()
            } else {
                self.cwd.join(path)
            }
        };
        if let Ok(canonical) = std::fs::canonicalize(target) {
            self.cwd = canonical;
            self.cwd_dirty = true;
        }
    }

    /// 应用远程 `cd` 参数，不访问本地文件系统。
    fn apply_remote_cd(&mut self, arg: &str, home: Option<&Path>) {
        if arg == "-" {
            return;
        }
        if home.is_none() {
            // 连接建立顺序不固定；在拿到真实 home 前保留完整 cd 序列，
            // 避免把相对路径错误地套在占位目录 "/" 上。
            self.pending_remote_cds.push(arg.to_string());
            return;
        }
        let home = home.expect("home 已在上方检查");
        let target = if arg.is_empty() || arg == "~" {
            home.to_path_buf()
        } else if let Some(rest) = arg.strip_prefix("~/") {
            home.join(rest)
        } else {
            let path = Path::new(arg);
            if path.is_absolute() {
                path.to_path_buf()
            } else {
                self.cwd.join(path)
            }
        };
        self.cwd = normalize_remote_path(&target);
        self.cwd_dirty = true;
    }
}

/// 判断当前输入是否是可以从下一次输出中读取结果的 pwd 命令。
fn is_pwd_command(text: &str) -> bool {
    let mut parts = text.split_whitespace();
    if parts.next() != Some("pwd") {
        return false;
    }
    parts.all(|part| matches!(part, "-P" | "-L" | "--physical" | "--logical"))
}

/// 从终端屏幕变化的行中提取绝对路径。
fn changed_pwd_path<'a>(previous: &[String], current: &'a [String]) -> Option<&'a str> {
    current
        .iter()
        .enumerate()
        .filter(|(index, line)| previous.get(*index) != Some(*line))
        .filter_map(|(_, line)| {
            let path = line.trim();
            (!path.is_empty()
                && path.starts_with('/')
                && !path.chars().any(|character| character.is_control()))
            .then_some(path)
        })
        .next_back()
}

/// 归一化远程 POSIX 路径，至少保证根目录不会被 `..` 越过。
fn normalize_remote_path(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::from("/");
    for component in path.components() {
        match component {
            std::path::Component::RootDir | std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                normalized.pop();
                if normalized.as_os_str().is_empty() {
                    normalized.push("/");
                }
            }
            std::path::Component::Normal(part) => normalized.push(part),
            std::path::Component::Prefix(_) => {}
        }
    }
    normalized
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cd解析更新工作目录() {
        let tmp = std::env::temp_dir();
        let base = tmp.join(format!("mino-cd-test-{}", std::process::id()));
        let sub = base.join("sub");
        std::fs::create_dir_all(&sub).ok();

        let mut tracker = WorkdirTracker::new(tmp.clone());
        tracker.push_text(&format!("cd {}", base.display()));
        tracker.execute();
        assert_eq!(tracker.cwd, std::fs::canonicalize(&base).unwrap());

        tracker.push_text("cd sub");
        tracker.execute();
        assert_eq!(tracker.cwd, std::fs::canonicalize(&sub).unwrap());

        let home = std::env::var("HOME").unwrap();
        tracker.push_text("cd");
        tracker.execute();
        assert_eq!(tracker.cwd, PathBuf::from(&home));

        tracker.push_text("cd ~");
        tracker.execute();
        assert_eq!(tracker.cwd, PathBuf::from(&home));

        tracker.push_text("cd /no/such/dir-xyz");
        tracker.execute();
        assert_eq!(tracker.cwd, PathBuf::from(&home));
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn cd前缀词不误判() {
        let tmp = std::env::temp_dir();
        let mut tracker = WorkdirTracker::new(tmp.clone());
        tracker.push_text("cdfoo /tmp");
        tracker.execute();
        assert_eq!(tracker.cwd, tmp, "cdfoo 不是 cd 命令，不应改变工作目录");

        let mut tracker = WorkdirTracker::new(tmp);
        tracker.push_text("cd   /tmp");
        tracker.execute();
        assert_eq!(
            tracker.cwd,
            std::fs::canonicalize("/tmp").unwrap(),
            "cd 带空白参数应生效"
        );
    }

    #[test]
    fn 远程复合cd命令不误改目录() {
        let mut tracker = WorkdirTracker::new(PathBuf::from("/srv/app"));
        tracker.push_text("cd /tmp && ls");
        tracker.execute_remote(Some(Path::new("/home/demo")));
        assert_eq!(tracker.cwd, PathBuf::from("/srv/app"));

        tracker.push_text("cd \"/tmp/work space\"");
        tracker.execute_remote(Some(Path::new("/home/demo")));
        assert_eq!(tracker.cwd, PathBuf::from("/srv/app"));
    }

    #[test]
    fn sftp初始目录不覆盖已跟踪的远程目录() {
        let mut tracker = WorkdirTracker::new(PathBuf::from("/"));
        tracker.push_text("cd /srv/project");
        tracker.execute_remote(None);

        tracker.set_cwd_if_unmodified(PathBuf::from("/root"));
        assert_eq!(tracker.cwd, PathBuf::from("/srv/project"));
    }

    #[test]
    fn 远程home未就绪时保留相对cd() {
        let mut tracker = WorkdirTracker::new(PathBuf::from("/"));
        tracker.push_text("cd workspace");
        tracker.execute_remote(None);

        tracker.set_cwd_if_unmodified(PathBuf::from("/root"));
        assert_eq!(tracker.cwd, PathBuf::from("/root/workspace"));
    }

    #[test]
    fn pwd输出可校正远程目录() {
        let mut tracker = WorkdirTracker::new(PathBuf::from("/root"));

        // 模拟此前粘贴命令导致跟踪器失效；回车后输入模型应恢复可用。
        tracker.invalidate();
        tracker.execute_remote(Some(Path::new("/root")));
        tracker.push_text("pwd");
        tracker.execute_remote(Some(Path::new("/root")));

        let previous = vec!["[root@host ~]#".to_string(), "旧输出".to_string()];
        let current = vec![
            "[root@host ~]# pwd".to_string(),
            "/home/tracsys/soft/front".to_string(),
            "[root@host front]#".to_string(),
        ];
        assert!(tracker.observe_remote_output(&previous, &current));
        assert_eq!(tracker.cwd, PathBuf::from("/home/tracsys/soft/front"));

        // 没有新的 pwd 请求时，屏幕里的其它绝对路径不能再次改目录。
        assert!(!tracker.observe_remote_output(&previous, &["/tmp/other".into()]));
        assert_eq!(tracker.cwd, PathBuf::from("/home/tracsys/soft/front"));
    }

    #[test]
    fn pwd输出可校正本地目录() {
        let base = std::env::temp_dir().join(format!("mino-pwd-test-{}", std::process::id()));
        std::fs::create_dir_all(&base).unwrap();
        let mut tracker = WorkdirTracker::new(PathBuf::from("/"));
        tracker.push_text("pwd -P");
        tracker.execute();

        let previous = vec!["$ pwd -P".to_string()];
        let current = vec!["$ pwd -P".to_string(), base.display().to_string()];
        assert!(tracker.observe_local_output(&previous, &current));
        assert_eq!(tracker.cwd, std::fs::canonicalize(&base).unwrap());
        std::fs::remove_dir_all(base).ok();
    }

    #[test]
    fn 自动pwd探测状态与手工pwd互不干扰() {
        let mut tracker = WorkdirTracker::new(PathBuf::from("/srv/stale"));
        // 定位前进入自动探测等待态；重复进入应被拒绝，避免注入两条 pwd。
        assert!(tracker.begin_auto_pwd());
        assert!(!tracker.begin_auto_pwd());
        assert!(tracker.awaiting_auto_pwd_output());
        assert!(tracker.awaiting_any_pwd_output());

        // 探测输出到达后，两种等待态一起消费，目录校正到真实值。
        let previous = vec!["$ pwd".to_string()];
        let current = vec!["$ pwd".to_string(), "/srv/real".to_string()];
        assert!(tracker.observe_remote_output(&previous, &current));
        assert_eq!(tracker.cwd, PathBuf::from("/srv/real"));
        assert!(!tracker.awaiting_auto_pwd_output());
        assert!(!tracker.awaiting_any_pwd_output());

        // 没有等待态时输出不能再改目录。
        assert!(!tracker.observe_remote_output(&previous, &["/tmp/other".into()]));
        assert_eq!(tracker.cwd, PathBuf::from("/srv/real"));

        // 超时取消后同样回到空闲，可发起下一次探测。
        assert!(tracker.begin_auto_pwd());
        tracker.cancel_auto_pwd();
        assert!(!tracker.awaiting_any_pwd_output());
        assert!(tracker.begin_auto_pwd());
    }

    #[test]
    fn 定位空闲判断只在有效空输入时通过() {
        let mut tracker = WorkdirTracker::new(PathBuf::from("/srv/app"));
        assert!(tracker.input_is_idle());

        tracker.push_text("cd /tmp");
        assert!(!tracker.input_is_idle());

        // Tab/粘贴等让跟踪失效后不能再认为空闲，避免往用户 IN-PROGRESS
        // 的命令行里注入 `pwd`。
        tracker.invalidate();
        assert!(!tracker.input_is_idle());

        tracker.reset();
        assert!(tracker.input_is_idle());
    }
}
