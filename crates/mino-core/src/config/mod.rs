//! 工作区（本地目录 / SSH 主机）配置模型与持久化。

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// 认证方式。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum Auth {
    /// 密码认证。
    Password(String),
    /// 私钥认证（路径 + 可选口令）。
    Key {
        path: PathBuf,
        passphrase: Option<String>,
    },
}

/// serde 默认：老配置缺端口字段时回 22（与 `HostProfile::default` 同值）。
fn default_ssh_port() -> u16 {
    22
}

/// SSH 主机连接参数（连接层 `mino_core::ssh` 的输入，不再直接持久化）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HostProfile {
    /// 显示名称。
    pub name: String,
    /// 主机地址。
    pub host: String,
    /// 端口（默认 22）。
    pub port: u16,
    /// 用户名。
    pub user: String,
    /// 认证方式。
    pub auth: Auth,
}

impl Default for HostProfile {
    fn default() -> Self {
        Self {
            name: String::new(),
            host: String::new(),
            port: default_ssh_port(),
            user: String::new(),
            auth: Auth::Password(String::new()),
        }
    }
}

/// 本地项目收藏（旧模型：加载老配置时迁移为 `Workspace::Local`，不再写回）。
///
/// 打开即新建本地终端标签（工作目录为 `path`），`command` 非空时在
/// 会话建立后自动执行一条启动命令（视觉等价用户在首个提示符后粘贴回车）。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectProfile {
    /// 显示名称。
    pub name: String,
    /// 本地目录。
    pub path: PathBuf,
    /// 打开后自动执行的启动命令（可空）。
    #[serde(default)]
    pub command: String,
}

/// 统一「工作区」：本地目录或 SSH 主机（SSH 与项目收藏的融合形态）。
///
/// 侧栏 / 启动器对两类工作区一视同仁：打开本地工作区即新建本地终端
/// 标签（工作目录 `path`）；打开 SSH 工作区即建立远程会话。`command`
/// 非空时在会话建立后自动执行一条启动命令。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Workspace {
    /// 本地目录：打开即新建本地终端标签（工作目录 `path`），`command` 非空时自动执行。
    Local {
        /// 显示名称。
        name: String,
        /// 本地目录。
        path: PathBuf,
        /// 打开后自动执行的启动命令（可空）。
        #[serde(default)]
        command: String,
    },
    /// SSH 主机：可带远端起始目录与启动命令。
    Ssh {
        /// 显示名称。
        name: String,
        /// 主机地址。
        host: String,
        /// 端口（默认 22）。
        #[serde(default = "default_ssh_port")]
        port: u16,
        /// 用户名。
        user: String,
        /// 认证方式。
        auth: Auth,
        /// 登录后的远端起始目录（可空：由远端 shell 决定）。
        #[serde(default)]
        path: String,
        /// 登录后自动执行的启动命令（可空）。
        #[serde(default)]
        command: String,
    },
}

impl Workspace {
    /// 显示名称。
    pub fn name(&self) -> &str {
        match self {
            Self::Local { name, .. } | Self::Ssh { name, .. } => name,
        }
    }

    /// 是否为远程（SSH）工作区。
    pub fn is_remote(&self) -> bool {
        matches!(self, Self::Ssh { .. })
    }

    /// 打开后自动执行的启动命令（无则空串）。
    pub fn command(&self) -> &str {
        match self {
            Self::Local { command, .. } | Self::Ssh { command, .. } => command,
        }
    }

    /// 供 UI 次要文本展示的位置描述：本地为目录路径，
    /// 远程为 `user@host`（非默认端口补 `:port`）。
    pub fn detail(&self) -> String {
        match self {
            Self::Local { path, .. } => path.display().to_string(),
            Self::Ssh {
                host, port, user, ..
            } => {
                if *port == default_ssh_port() {
                    format!("{user}@{host}")
                } else {
                    format!("{user}@{host}:{port}")
                }
            }
        }
    }

    /// 转为 SSH 连接参数（本地工作区没有远端主机，返回 `None`）。
    pub fn to_host_profile(&self) -> Option<HostProfile> {
        match self {
            Self::Ssh {
                name,
                host,
                port,
                user,
                auth,
                ..
            } => Some(HostProfile {
                name: name.clone(),
                host: host.clone(),
                port: *port,
                user: user.clone(),
                auth: auth.clone(),
            }),
            Self::Local { .. } => None,
        }
    }
}

/// 默认终端字号（pt；`mino-app` 的 `TerminalView::DEFAULT_FONT_SIZE` 同源引用）。
///
/// Kaku 基准：高分辨率屏 17、低分辨率屏（设备像素比 ≤ 1）15——
/// 应用启动时按屏幕档位细化（见 `MinoApp::new_with_config_inner`）。
pub const DEFAULT_FONT_SIZE: f32 = 17.0;

/// serde 默认：老配置缺 `font_size` 字段时回默认（与主题字段同样的兼容约定）。
fn default_font_size() -> f32 {
    DEFAULT_FONT_SIZE
}

/// 全部工作区配置。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HostConfig {
    /// 外观偏好：`auto`（跟随系统，默认）/ `dark` / `light`。
    ///
    /// 旧配置的 `theme` 字段（已废弃的三套主题名：「深色」「深蓝」「霓虹」）
    /// 通过 alias 读入；未知值一律按 `auto` 处理（`theme::resolve_appearance`）。
    #[serde(default, alias = "theme")]
    pub appearance: String,
    /// 终端字号（pt；`mino-app` 的快捷键与外观滑杆读写此字段）。
    ///
    /// 老配置缺字段时 `default_font_size` 回默认（与主题字段同样的
    /// 兼容约定：缺字段不报错、启动按默认处理）。
    #[serde(default = "default_font_size")]
    pub font_size: f32,
    /// 统一工作区列表（本地目录 + SSH 主机，侧栏展示顺序）。
    #[serde(default)]
    pub workspaces: Vec<Workspace>,
    /// 旧字段：SSH 主机列表（只读兼容，加载时迁移进 `workspaces`，不再写回）。
    #[serde(default, skip_serializing)]
    pub hosts: Vec<HostProfile>,
    /// 旧字段：本地项目收藏（只读兼容，加载时迁移进 `workspaces`，不再写回）。
    #[serde(default, skip_serializing)]
    pub projects: Vec<ProjectProfile>,
}

impl Default for HostConfig {
    fn default() -> Self {
        Self {
            appearance: String::new(),
            font_size: DEFAULT_FONT_SIZE,
            workspaces: Vec::new(),
            hosts: Vec::new(),
            projects: Vec::new(),
        }
    }
}

impl HostConfig {
    /// 加载配置文件；不存在时返回空配置。
    ///
    /// 成功解析后执行老配置迁移（见 `merge_legacy`）。
    pub fn load(path: &std::path::Path) -> std::io::Result<HostConfig> {
        let content = std::fs::read_to_string(path)?;
        let mut config: HostConfig = toml::from_str(&content)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        config.merge_legacy();
        Ok(config)
    }

    /// 老配置迁移：`workspaces` 为空时把只读兼容字段 `hosts` / `projects`
    /// 按原文件顺序（先主机后项目，逐条转换）并入统一工作区。
    ///
    /// 仅在 `workspaces` 为空时执行：新配置已写 `workspaces`，此时老字段
    /// 只作升级读入用（保存时也不再写回），避免已迁移配置被重复追加。
    fn merge_legacy(&mut self) {
        if !self.workspaces.is_empty() {
            return;
        }
        let migrated: Vec<Workspace> = self
            .hosts
            .iter()
            .map(|host| Workspace::Ssh {
                name: host.name.clone(),
                host: host.host.clone(),
                port: host.port,
                user: host.user.clone(),
                auth: host.auth.clone(),
                path: String::new(),
                command: String::new(),
            })
            .chain(self.projects.iter().map(|project| Workspace::Local {
                name: project.name.clone(),
                path: project.path.clone(),
                command: project.command.clone(),
            }))
            .collect();
        self.workspaces = migrated;
    }

    /// 保存配置到文件（unix 下强制 0600：配置含明文密码/私钥口令，
    /// 默认 umask 022 会产生 0644，本机其他用户可读）。
    ///
    /// 原子写入：先写临时文件再 rename 覆盖——进程被杀/磁盘满时不会把
    /// hosts.toml 截断或清空（曾因直接 write 出现主机列表丢失）。
    /// 临时文件带 pid（并发保存互不覆盖，known_hosts 同模式），错误路径
    /// 清理临时文件；rename 前 fsync、后同步目录条目（写入频率极低）。
    pub fn save(&self, path: &std::path::Path) -> std::io::Result<()> {
        let content = toml::to_string_pretty(self)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension(format!("toml.tmp-{}", std::process::id()));
        let result = (|| {
            #[cfg(unix)]
            let mut file = {
                use std::os::unix::fs::OpenOptionsExt;
                // 创建时即 0600：避免先按 umask 建文件（常见 0644）再
                // chmod 的短暂可读窗口（文件内容为明文密码/私钥口令）。
                std::fs::OpenOptions::new()
                    .write(true)
                    .create(true)
                    .truncate(true)
                    .mode(0o600)
                    .open(&tmp)?
            };
            #[cfg(not(unix))]
            let mut file = std::fs::File::create(&tmp)?;
            use std::io::Write;
            file.write_all(content.as_bytes())?;
            file.sync_all()?;
            drop(file);
            std::fs::rename(&tmp, path)?;
            // 目录条目落盘：断电时保证 rename 本身持久（写入频率极低，
            // 成本可忽略；失败不视为保存失败——文件已原子替换完成）。
            #[cfg(unix)]
            if let Some(dir) = path.parent() {
                if let Ok(d) = std::fs::File::open(dir) {
                    let _ = d.sync_all();
                }
            }
            Ok(())
        })();
        if result.is_err() {
            // 失败清理临时文件，不留明文配置副本。
            let _ = std::fs::remove_file(&tmp);
        }
        result
    }
}

/// 默认配置文件路径：`~/.config/mino/hosts.toml`。
pub fn default_config_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home)
        .join(".config")
        .join("mino")
        .join("hosts.toml")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn local_workspace() -> Workspace {
        Workspace::Local {
            name: "演示项目".into(),
            path: PathBuf::from("/tmp/mino-demo"),
            command: "echo hi".into(),
        }
    }

    fn ssh_auth() -> Auth {
        Auth::Key {
            path: PathBuf::from("~/.ssh/id_ed25519"),
            passphrase: None,
        }
    }

    fn ssh_workspace(port: u16) -> Workspace {
        Workspace::Ssh {
            name: "测试服务器".into(),
            host: "example.com".into(),
            port,
            user: "root".into(),
            auth: ssh_auth(),
            path: "/srv/app".into(),
            command: String::new(),
        }
    }

    fn sample() -> HostConfig {
        HostConfig {
            appearance: "light".into(),
            font_size: 15.0,
            workspaces: vec![local_workspace(), ssh_workspace(default_ssh_port())],
            hosts: Vec::new(),
            projects: Vec::new(),
        }
    }

    #[test]
    fn 配置序列化与反序列化() {
        let config = sample();
        let toml_str = toml::to_string_pretty(&config).unwrap();
        assert!(
            toml_str.contains("appearance"),
            "外观偏好必须随配置落盘（曾丢失）：{toml_str}"
        );
        let parsed: HostConfig = toml::from_str(&toml_str).unwrap();
        assert_eq!(parsed, config);
    }

    #[test]
    fn 老配置缺外观字段时默认空串() {
        // 升级前已存在的 hosts.toml 没有外观字段：不能解析失败，
        // 启动按 auto 跟随系统处理。
        let parsed: HostConfig = toml::from_str("").unwrap();
        assert_eq!(parsed.appearance, "");
    }

    #[test]
    fn 旧主题字段按别名读入() {
        // 旧配置存的是三套已废弃主题名（深色/深蓝/霓虹）：字段名 `theme`
        // 通过 serde alias 读入 `appearance`，未知值由上层按 auto 处理。
        let legacy: HostConfig = toml::from_str("theme = \"深蓝\"\n").unwrap();
        assert_eq!(legacy.appearance, "深蓝");
    }

    #[test]
    fn 字号随配置落盘且老配置回默认() {
        // 新配置的字号必须序列化（否则重启回到默认——曾有同类丢失）。
        let toml_str = toml::to_string_pretty(&sample()).unwrap();
        assert!(
            toml_str.contains("font_size"),
            "终端字号必须随配置落盘：{toml_str}"
        );
        // 升级前已存在的 hosts.toml 没有 font_size 字段：不报错、回默认。
        let legacy: HostConfig = toml::from_str("theme = \"深蓝\"\n").unwrap();
        assert_eq!(legacy.font_size, DEFAULT_FONT_SIZE);
        // 显式 0/负数等脏值不由解析层兜底（clamp 是应用层的职责），
        // 但至少不能解析失败。
        let parsed: HostConfig = toml::from_str("font_size = 0\n").unwrap();
        assert_eq!(parsed.font_size, 0.0);
    }

    #[test]
    fn 工作区序列化格式直观() {
        let config = HostConfig {
            appearance: "light".into(),
            font_size: 15.0,
            workspaces: vec![
                Workspace::Local {
                    name: "mino".into(),
                    path: PathBuf::from("/Users/me/mino"),
                    command: String::new(),
                },
                Workspace::Ssh {
                    name: "prod".into(),
                    host: "1.2.3.4".into(),
                    port: default_ssh_port(),
                    user: "root".into(),
                    auth: Auth::Key {
                        path: PathBuf::from("~/.ssh/id_ed25519"),
                        passphrase: Some(String::new()),
                    },
                    path: "/srv/app".into(),
                    command: String::new(),
                },
            ],
            hosts: Vec::new(),
            projects: Vec::new(),
        };
        let toml_str = toml::to_string_pretty(&config).unwrap();
        for expected in [
            "[[workspaces]]",
            "kind = \"local\"",
            "kind = \"ssh\"",
            "name = \"mino\"",
            "path = \"/Users/me/mino\"",
            "command = \"\"",
            "host = \"1.2.3.4\"",
            "port = 22",
            "user = \"root\"",
            "[workspaces.auth.Key]",
            "path = \"~/.ssh/id_ed25519\"",
            "passphrase = \"\"",
        ] {
            assert!(
                toml_str.contains(expected),
                "序列化应含 `{expected}`：\n{toml_str}"
            );
        }
        // 只读兼容的老字段不得再落盘。
        assert!(
            !toml_str.contains("[[hosts]]") && !toml_str.contains("[[projects]]"),
            "旧 hosts/projects 字段不得写回：\n{toml_str}"
        );
        assert_eq!(toml::from_str::<HostConfig>(&toml_str).unwrap(), config);
    }

    #[test]
    fn 工作区缺省字段回默认() {
        // SSH 工作区缺 port/path/command（老配置手写或将来新增字段）：
        // 端口回 22，路径/命令回空串。
        let text = "\
[[workspaces]]
kind = \"ssh\"
name = \"prod\"
host = \"1.2.3.4\"
user = \"root\"

[workspaces.auth]
Password = \"secret\"
";
        let parsed: HostConfig = toml::from_str(text).unwrap();
        let workspace = &parsed.workspaces[0];
        assert!(workspace.is_remote());
        assert_eq!(workspace.name(), "prod");
        assert_eq!(workspace.command(), "");
        assert_eq!(workspace.detail(), "root@1.2.3.4");
        let profile = workspace.to_host_profile().unwrap();
        assert_eq!(profile.port, default_ssh_port());
        assert_eq!(profile.auth, Auth::Password("secret".into()));
        // 缺字段的本地工作区：command 回空串。
        let local: HostConfig =
            toml::from_str("[[workspaces]]\nkind = \"local\"\nname = \"mino\"\npath = \"/tmp\"\n")
                .unwrap();
        assert_eq!(local.workspaces[0].command(), "");
        assert!(!local.workspaces[0].is_remote());
    }

    #[test]
    fn 工作区方法与次要文本() {
        let local = local_workspace();
        assert_eq!(local.name(), "演示项目");
        assert!(!local.is_remote());
        assert_eq!(local.command(), "echo hi");
        assert_eq!(local.detail(), "/tmp/mino-demo");
        assert!(local.to_host_profile().is_none());

        let ssh = ssh_workspace(default_ssh_port());
        assert_eq!(ssh.name(), "测试服务器");
        assert!(ssh.is_remote());
        assert_eq!(ssh.command(), "");
        // 默认端口 22 时略去（UI 次要文本）。
        assert_eq!(ssh.detail(), "root@example.com");
        let profile = ssh.to_host_profile().unwrap();
        assert_eq!(profile.name, "测试服务器");
        assert_eq!(profile.host, "example.com");
        assert_eq!(profile.port, default_ssh_port());
        assert_eq!(profile.user, "root");
        assert_eq!(profile.auth, ssh_auth());

        // 非默认端口补 `:port`。
        let custom = ssh_workspace(2222);
        assert_eq!(custom.detail(), "root@example.com:2222");
        assert_eq!(custom.to_host_profile().unwrap().port, 2222);
    }

    #[test]
    fn 老配置的主机与项目迁移为工作区() {
        let dir = std::env::temp_dir().join(format!("mino-config-migrate-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("hosts.toml");
        // 升级前的 hosts.toml：独立的主机列表 + 项目收藏，没有 workspaces。
        std::fs::write(
            &path,
            "\
appearance = \"dark\"
font_size = 15.0

[[hosts]]
name = \"prod\"
host = \"1.2.3.4\"
port = 2222
user = \"root\"

[hosts.auth]
Password = \"secret\"

[[projects]]
name = \"mino\"
path = \"/Users/me/mino\"
command = \"cargo run\"
",
        )
        .unwrap();

        let config = HostConfig::load(&path).unwrap();
        assert_eq!(config.appearance, "dark");
        assert_eq!(config.font_size, 15.0);
        assert_eq!(
            config.workspaces.len(),
            2,
            "主机与项目都要迁移为工作区：{:?}",
            config.workspaces
        );
        // 顺序与原文件一致：先 hosts，后 projects。
        match &config.workspaces[0] {
            Workspace::Ssh {
                name,
                host,
                port,
                user,
                auth,
                path,
                command,
            } => {
                assert_eq!(name, "prod");
                assert_eq!(host, "1.2.3.4");
                assert_eq!(*port, 2222);
                assert_eq!(user, "root");
                assert_eq!(*auth, Auth::Password("secret".into()));
                assert_eq!(path, "", "迁移不带远端起始目录");
                assert_eq!(command, "", "迁移不带启动命令");
            }
            other => panic!("第一条应为 SSH 工作区：{other:?}"),
        }
        match &config.workspaces[1] {
            Workspace::Local {
                name,
                path,
                command,
            } => {
                assert_eq!(name, "mino");
                assert_eq!(path, &PathBuf::from("/Users/me/mino"));
                assert_eq!(command, "cargo run");
            }
            other => panic!("第二条应为本地工作区：{other:?}"),
        }

        // save → load 稳定：工作区保留，旧字段不再写回。
        config.save(&path).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("[[workspaces]]"), "工作区必须落盘：\n{text}");
        assert!(
            !text.contains("[[hosts]]") && !text.contains("[[projects]]"),
            "旧 hosts/projects 字段不得写回：\n{text}"
        );
        let reloaded = HostConfig::load(&path).unwrap();
        assert_eq!(reloaded.workspaces, config.workspaces);
        assert_eq!(reloaded.appearance, "dark");
        assert_eq!(reloaded.font_size, 15.0);
        assert_eq!(
            toml::to_string_pretty(&reloaded).unwrap(),
            text,
            "再次往返应稳定"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn 已有工作区配置不再迁移老字段() {
        // 新配置同时留着老字段（手改残留）时：workspaces 非空即不迁移，
        // 避免重复追加；老字段也不会写回。
        let text = "\
[[workspaces]]
kind = \"local\"
name = \"mino\"
path = \"/tmp/mino\"
command = \"\"

[[hosts]]
name = \"prod\"
host = \"1.2.3.4\"
port = 22
user = \"root\"
auth = { Password = \"secret\" }
";
        let parsed: HostConfig = toml::from_str(text).unwrap();
        assert_eq!(parsed.workspaces.len(), 1);
        assert_eq!(parsed.hosts.len(), 1);
        let dumped = toml::to_string_pretty(&parsed).unwrap();
        assert!(!dumped.contains("[[hosts]]"), "老字段不得写回：\n{dumped}");
    }

    #[test]
    fn 新配置缺工作区字段时默认空() {
        let parsed: HostConfig = toml::from_str("").unwrap();
        assert!(parsed.workspaces.is_empty());
        assert!(parsed.hosts.is_empty());
        assert!(parsed.projects.is_empty());
        let minimal: HostConfig = toml::from_str("theme = \"深蓝\"\n").unwrap();
        assert!(minimal.workspaces.is_empty());
        assert_eq!(minimal.appearance, "深蓝");
    }

    #[cfg(unix)]
    #[test]
    fn 保存强制0600且失败清理临时文件() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("mino-config-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        let path = dir.join("hosts.toml");
        sample().save(&path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "明文密码配置必须以 0600 落盘");

        // 失败路径：目标被同名目录占用时 rename 失败，临时文件须被清理，
        // 不留明文配置副本。
        let blocked = dir.join("blocked.toml");
        std::fs::create_dir_all(&blocked).unwrap();
        assert!(sample().save(&blocked).is_err());
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.file_name().to_string_lossy().contains(".tmp-"))
            .collect();
        assert!(leftovers.is_empty(), "临时文件残留：{leftovers:?}");

        std::fs::remove_dir_all(&dir).ok();
    }
}
