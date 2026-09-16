//! 主机配置模型与持久化。

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

/// 主机配置。
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
            port: 22,
            user: String::new(),
            auth: Auth::Password(String::new()),
        }
    }
}

/// 本地项目收藏（仅本地目录，不绑定远程主机）。
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

/// 默认终端字号（pt；`mino-app` 的 `TerminalView::DEFAULT_FONT_SIZE` 同源引用）。
pub const DEFAULT_FONT_SIZE: f32 = 13.0;

/// serde 默认：老配置缺 `font_size` 字段时回默认（与主题字段同样的兼容约定）。
fn default_font_size() -> f32 {
    DEFAULT_FONT_SIZE
}

/// 全部主机配置。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HostConfig {
    #[serde(default)]
    pub hosts: Vec<HostProfile>,
    /// 已选主题名称（`mino-app/src/theme.rs::THEMES[].name`）。
    ///
    /// 存名称而非下标：`THEMES` 顺序调整不影响已保存配置。
    /// 老配置文件缺该字段时 `#[serde(default)]` 给空串，启动按默认主题
    /// 处理（曾无此字段，切换皮肤退出重进永远回到第一套）。
    #[serde(default)]
    pub theme: String,
    /// 收藏的本地项目（老配置缺字段时默认空）。
    #[serde(default)]
    pub projects: Vec<ProjectProfile>,
    /// 终端字号（pt；`mino-app` 的快捷键与外观滑杆读写此字段）。
    ///
    /// 老配置缺字段时 `default_font_size` 回默认 13（与主题字段同样的
    /// 兼容约定：缺字段不报错、启动按默认处理）。
    #[serde(default = "default_font_size")]
    pub font_size: f32,
}

impl Default for HostConfig {
    fn default() -> Self {
        Self {
            hosts: Vec::new(),
            theme: String::new(),
            projects: Vec::new(),
            font_size: DEFAULT_FONT_SIZE,
        }
    }
}

impl HostConfig {
    /// 加载配置文件；不存在时返回空配置。
    pub fn load(path: &std::path::Path) -> std::io::Result<HostConfig> {
        let content = std::fs::read_to_string(path)?;
        toml::from_str(&content)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
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

    fn sample() -> HostConfig {
        HostConfig {
            theme: "深蓝".into(),
            font_size: 15.0,
            hosts: vec![HostProfile {
                name: "测试服务器".into(),
                host: "example.com".into(),
                port: 22,
                user: "root".into(),
                auth: Auth::Key {
                    path: PathBuf::from("~/.ssh/id_ed25519"),
                    passphrase: None,
                },
            }],
            projects: vec![ProjectProfile {
                name: "演示项目".into(),
                path: PathBuf::from("/tmp/mino-demo"),
                command: "echo hi".into(),
            }],
        }
    }

    #[test]
    fn 配置序列化与反序列化() {
        let config = sample();
        let toml_str = toml::to_string_pretty(&config).unwrap();
        assert!(
            toml_str.contains("theme"),
            "主题选择必须随配置落盘（曾丢失）：{toml_str}"
        );
        let parsed: HostConfig = toml::from_str(&toml_str).unwrap();
        assert_eq!(parsed, config);
    }

    #[test]
    fn 老配置缺主题字段时默认空串() {
        // 升级前已存在的 hosts.toml 没有 theme 字段：不能解析失败，
        // 启动按默认主题处理（此前无持久化，升级用户回到第一套）。
        let parsed: HostConfig = toml::from_str("").unwrap();
        assert_eq!(parsed.theme, "");
    }

    #[test]
    fn 字号随配置落盘且老配置回默认() {
        // 新配置的字号必须序列化（否则重启回到 13——主题曾有同类丢失）。
        let toml_str = toml::to_string_pretty(&sample()).unwrap();
        assert!(
            toml_str.contains("font_size"),
            "终端字号必须随配置落盘：{toml_str}"
        );
        // 升级前已存在的 hosts.toml 没有 font_size 字段：不报错、回默认 13。
        let legacy: HostConfig = toml::from_str("theme = \"深蓝\"\n").unwrap();
        assert_eq!(legacy.font_size, DEFAULT_FONT_SIZE);
        // 显式 0/负数等脏值不由解析层兜底（clamp 是应用层的职责），
        // 但至少不能解析失败。
        let parsed: HostConfig = toml::from_str("font_size = 0\n").unwrap();
        assert_eq!(parsed.font_size, 0.0);
    }

    #[test]
    fn 项目序列化与反序列化() {
        let config = sample();
        let toml_str = toml::to_string_pretty(&config).unwrap();
        assert!(
            toml_str.contains("command"),
            "启动命令必须随配置落盘：{toml_str}"
        );
        let parsed: HostConfig = toml::from_str(&toml_str).unwrap();
        assert_eq!(parsed, config);
        assert_eq!(parsed.projects[0].command, "echo hi");
    }

    #[test]
    fn 老配置缺projects字段时默认空() {
        let parsed: HostConfig = toml::from_str("").unwrap();
        assert!(parsed.projects.is_empty());
        let legacy = "[hosts]\nname = \"a\"\nhost = \"h\"\nport = 22\nuser = \"root\"\n[hosts.auth.Password]\n0 = \"x\"\n";
        let _ = legacy;
        let minimal: HostConfig = toml::from_str("theme = \"深蓝\"\n").unwrap();
        assert!(minimal.projects.is_empty());
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
