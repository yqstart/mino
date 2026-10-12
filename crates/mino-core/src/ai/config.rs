//! 助手配置（`assistant.toml`）与连接解析。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// 默认的 OpenAI 兼容 API 根地址。
pub const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";

/// 未显式配置模型名时使用的占位值。
///
/// 留空而不是写死某个厂商的模型名：写了假模型名，用户点开设置会以为
/// 已经配好，实际每次请求都失败。
pub const DEFAULT_MODEL: &str = "";

/// 助手配置与连接解析的错误。
///
/// 消息一律为中文，且**不包含 api_key**：这些字符串会进 UI 与日志，
/// 密钥泄露到日志等于明文落盘。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AiError {
    /// 配置文件读写失败。
    #[error("助手配置读写失败：{0}")]
    Config(String),
    /// 配置内容非法（如 auth_type 取值未知、必填项缺失）。
    #[error("助手配置无效：{0}")]
    ConfigInvalid(String),
    /// Codex 登录态不受支持（ChatGPT 账号登录，没有可直接使用的 API key）。
    #[error("Codex 登录态不受支持：{0}")]
    CodexUnsupported(String),
    /// 网络层失败（DNS、连接、超时、传输中断）。
    #[error("网络请求失败：{0}")]
    Network(String),
    /// 服务端返回非 2xx。
    #[error("服务端返回 HTTP {status}：{body}")]
    Http {
        /// HTTP 状态码。
        status: u16,
        /// 响应体前若干字节（截断，避免把整页 HTML 塞进 UI）。
        body: String,
    },
    /// 响应解析失败（JSON 结构不符合预期、缺少必要字段）。
    #[error("响应解析失败：{0}")]
    Parse(String),
    /// 调用被取消（用户关闭面板/切换会话）。
    #[error("请求已取消")]
    Cancelled,
}

/// OpenAI 兼容接口的调用模式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ApiMode {
    /// `POST /chat/completions`（绝大多数兼容服务，默认）。
    #[default]
    ChatCompletions,
    /// `POST /responses`（OpenAI Responses API，Codex 走这条）。
    Responses,
}

impl ApiMode {
    /// 配置里的字符串值（设置面板回显用）。
    pub fn as_str(self) -> &'static str {
        match self {
            ApiMode::ChatCompletions => "chat_completions",
            ApiMode::Responses => "responses",
        }
    }

    /// 解析配置里的字符串；未知取值回落到 chat/completions。
    pub fn from_config(value: &str) -> Self {
        match value.trim() {
            "responses" => ApiMode::Responses,
            // "chat_completions" 与未知取值（含空串）都按默认模式处理：
            // 一个拼错的模式名不应该让助手整个不可用。
            _ => ApiMode::ChatCompletions,
        }
    }
}

/// `~/.config/mino/assistant.toml` 的模型。
///
/// 所有字段都带 `#[serde(default)]`：老配置文件缺字段时按默认值处理，
/// 不因为新增字段导致启动读取失败（与 `config::HostConfig` 同一约定）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssistantConfig {
    /// 是否启用助手功能。
    #[serde(default)]
    pub enabled: bool,
    /// API key（`auth_type = "api_key"` 时使用；文件必须 0600）。
    #[serde(default)]
    pub api_key: String,
    /// Simple 模型：修命令、`#` 生成命令等轻量任务用。
    #[serde(default)]
    pub model: String,
    /// 聊天用的 Deep 模型；留空则回退 `model`。
    #[serde(default)]
    pub chat_model: String,
    /// OpenAI 兼容 API 根地址（如 `https://api.openai.com/v1`）。
    #[serde(default = "default_base_url")]
    pub base_url: String,
    /// 接口模式：`"chat_completions"`（默认）或 `"responses"`。
    #[serde(default = "default_api_mode")]
    pub api_mode: String,
    /// 追加到每个请求的额外请求头，形如 `"X-A: 1"`。
    #[serde(default)]
    pub custom_headers: Vec<String>,
    /// 认证方式：`"api_key"`（默认）或 `"codex"`。
    #[serde(default = "default_auth_type")]
    pub auth_type: String,
}

/// serde 默认：老配置缺 `base_url`/`api_mode`/`auth_type` 时回落到默认值
/// （而不是空串，空串会让连接解析看起来「配了但用不了」）。
fn default_base_url() -> String {
    DEFAULT_BASE_URL.to_string()
}

fn default_api_mode() -> String {
    ApiMode::ChatCompletions.as_str().to_string()
}

fn default_auth_type() -> String {
    "api_key".to_string()
}

impl Default for AssistantConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            api_key: String::new(),
            model: DEFAULT_MODEL.to_string(),
            chat_model: String::new(),
            base_url: default_base_url(),
            api_mode: default_api_mode(),
            custom_headers: Vec::new(),
            auth_type: default_auth_type(),
        }
    }
}

impl AssistantConfig {
    /// 加载配置文件；文件不存在时返回全默认值之外的 `Config` 错误。
    ///
    /// 与 `HostConfig::load` 的差别：助手配置缺失应当能区分「没配」和
    /// 「读了但解析失败」，因此这里把 IO 错误原样上报给调用方判断。
    pub fn load(path: &Path) -> Result<AssistantConfig, AiError> {
        let content = std::fs::read_to_string(path)
            .map_err(|e| AiError::Config(format!("读取 {} 失败：{e}", path.display())))?;
        toml::from_str(&content)
            .map_err(|e| AiError::Config(format!("解析 {} 失败：{e}", path.display())))
    }

    /// 保存配置到文件（unix 下强制 0600：文件含明文 api_key）。
    ///
    /// 原子写入：先写带 pid 的临时文件再 rename 覆盖，进程被杀/磁盘满
    /// 时不会把 assistant.toml 截断或清空（与 `HostConfig::save` 同一模式）。
    /// 创建临时文件时即用 0600，避免先按 umask（常见 0644）建文件再
    /// chmod 的短暂可读窗口；失败路径清理临时文件，不留明文密钥副本。
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        let content = toml::to_string_pretty(self)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        if let Some(dir) = path.parent() {
            if !dir.as_os_str().is_empty() {
                std::fs::create_dir_all(dir)?;
            }
        }
        let tmp = path.with_extension(format!("toml.tmp-{}", std::process::id()));
        let result = (|| {
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
            use std::io::Write;
            file.write_all(content.as_bytes())?;
            file.sync_all()?;
            drop(file);
            std::fs::rename(&tmp, path)?;
            // 目录条目落盘：断电时保证 rename 本身持久（写入频率极低）。
            #[cfg(unix)]
            if let Some(dir) = path.parent() {
                if let Ok(d) = std::fs::File::open(dir) {
                    let _ = d.sync_all();
                }
            }
            Ok(())
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        result
    }

    /// 聊天实际使用的模型名：`chat_model` 为空时回退 `model`。
    pub fn effective_chat_model(&self) -> &str {
        let chat = self.chat_model.trim();
        if chat.is_empty() {
            self.model.trim()
        } else {
            chat
        }
    }

    /// 解析出可直接发请求的连接参数。
    ///
    /// `codex_home` 仅在 `auth_type = "codex"` 时使用（调用方传
    /// `~/.codex`；测试传临时目录）。函数只读文件，不写任何用户数据。
    pub fn resolve_connection(&self, codex_home: &Path) -> Result<Connection, AiError> {
        let api_mode = ApiMode::from_config(&self.api_mode);
        let base_url = normalize_base_url(&self.base_url);
        // 自定义请求头解析失败（缺冒号）时跳过该条，不让一个笔误
        // 导致整个助手不可用。
        let headers = parse_custom_headers(&self.custom_headers);
        match self.auth_type.trim() {
            "" | "api_key" => {
                let api_key = self.api_key.trim().to_string();
                if api_key.is_empty() {
                    return Err(AiError::ConfigInvalid(
                        "未填写 API key；请在助手设置里填入密钥，或把认证方式改为 codex。".into(),
                    ));
                }
                Ok(Connection {
                    api_mode,
                    base_url,
                    api_key,
                    headers,
                    default_model: String::new(),
                })
            }
            "codex" => resolve_codex_connection(codex_home, headers),
            other => Err(AiError::ConfigInvalid(format!(
                "未知的认证方式：{other}（可用值：api_key、codex）"
            ))),
        }
    }
}

/// 解析完成、可直接发起请求的连接参数。
///
/// 由 [`AssistantConfig::resolve_connection`] 产出，网络层只认这个结构，
/// 不再回头看配置文件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Connection {
    /// 调用模式（chat/completions 或 responses）。
    pub api_mode: ApiMode,
    /// 规范化后的 API 根地址（已去掉末尾斜杠）。
    pub base_url: String,
    /// 明文 API key；绝不出现在错误信息或日志里。
    pub api_key: String,
    /// 追加到每个请求的自定义请求头。
    pub headers: Vec<(String, String)>,
    /// 连接自带的默认模型名；仅 codex 模式会带上（来自 Codex 配置的
    /// 顶层 `model`），API key 模式为空串。
    pub default_model: String,
}

/// 默认配置文件路径：`~/.config/mino/assistant.toml`。
pub fn default_assistant_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home)
        .join(".config")
        .join("mino")
        .join("assistant.toml")
}

/// 去掉末尾斜杠，避免拼出 `//chat/completions` 这类地址。
fn normalize_base_url(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return DEFAULT_BASE_URL.to_string();
    }
    trimmed.trim_end_matches('/').to_string()
}

/// 解析 `"X-A: 1"` 形式的请求头；首个冒号前为名字，其余为值。
fn parse_custom_headers(raw: &[String]) -> Vec<(String, String)> {
    let mut out = Vec::with_capacity(raw.len());
    for entry in raw {
        let Some((name, value)) = entry.split_once(':') else {
            continue;
        };
        let name = name.trim();
        if name.is_empty() {
            continue;
        }
        out.push((name.to_string(), value.trim().to_string()));
    }
    out
}

/// Codex CLI 的配置目录（`config.toml` + `auth.json`）。
struct CodexSetup {
    /// 顶层 `model`（可能为空，由调用方决定用什么模型名）。
    model: String,
    /// `[model_providers.<id>].base_url`。
    base_url: String,
    /// `auth.json` 里的 `OPENAI_API_KEY`。
    api_key: String,
}

/// 解析 Codex 登录态：读配置目录下的 `config.toml` 与 `auth.json`。
fn resolve_codex_connection(
    codex_home: &Path,
    headers: Vec<(String, String)>,
) -> Result<Connection, AiError> {
    let setup = read_codex_setup(codex_home)?;
    Ok(Connection {
        // Codex CLI 走 Responses API，与 auth_type 一起切换，避免出现
        // 「Codex 密钥 + chat/completions 端点」这种半配置状态。
        api_mode: ApiMode::Responses,
        base_url: normalize_base_url(&setup.base_url),
        api_key: setup.api_key,
        headers,
        default_model: setup.model,
    })
}

/// 读取 Codex 配置；`auth.json` 只有 ChatGPT 登录态（无 API key）时
/// 返回 [`AiError::CodexUnsupported`]。
fn read_codex_setup(codex_home: &Path) -> Result<CodexSetup, AiError> {
    let config_path = codex_home.join("config.toml");
    let config_text = std::fs::read_to_string(&config_path).map_err(|e| {
        AiError::Config(format!(
            "读取 Codex 配置 {} 失败：{e}；请确认已安装并登录 codex CLI。",
            config_path.display()
        ))
    })?;
    let config: toml::Value = toml::from_str(&config_text)
        .map_err(|e| AiError::Config(format!("解析 {} 失败：{e}", config_path.display())))?;
    let provider_id = config
        .get("model_provider")
        .and_then(|v| v.as_str())
        .unwrap_or("openai")
        .to_string();
    let provider = config
        .get("model_providers")
        .and_then(|v| v.get(&provider_id))
        .ok_or_else(|| {
            AiError::ConfigInvalid(format!(
                "Codex 配置缺少 [model_providers.{provider_id}] 段（请检查 {}）",
                config_path.display()
            ))
        })?;
    let base_url = provider.get("base_url").and_then(|v| v.as_str());
    let wire_api = provider.get("wire_api").and_then(|v| v.as_str());
    // Codex 的内置 openai provider 通常不写 base_url（默认官方端点）。
    let base_url = match base_url {
        Some(url) => url.to_string(),
        None if provider_id == "openai" => DEFAULT_BASE_URL.to_string(),
        None => {
            return Err(AiError::ConfigInvalid(format!(
                "Codex 配置里的 [model_providers.{provider_id}] 未设置 base_url"
            )));
        }
    };
    if let Some(api) = wire_api {
        if api != "responses" {
            log::debug!("Codex provider {provider_id} 的 wire_api={api}，本客户端固定走 responses");
        }
    }

    let model = config
        .get("model")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    // `config.toml` 是明文 TOML（无凭据）；密钥在 auth.json 或环境变量里。
    let auth_path = codex_home.join("auth.json");
    let auth: serde_json::Value = match std::fs::read_to_string(&auth_path) {
        Ok(raw) => serde_json::from_str(&raw)
            .map_err(|e| AiError::Config(format!("解析 {} 失败：{e}", auth_path.display())))?,
        Err(e) => {
            return Err(AiError::Config(format!(
                "读取 Codex 登录信息 {} 失败：{e}；请先运行 codex login。",
                auth_path.display()
            )));
        }
    };
    let file_key = auth
        .get("OPENAI_API_KEY")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty());
    if file_key.is_none() && auth.get("tokens").is_some() {
        // ChatGPT 账号登录（tokens 里是 OAuth 令牌）。直接用令牌打
        // OpenAI 接口需要额外的账号头与刷新流程，本客户端只做客户端层，
        // 不做 OAuth 刷新；与其发一个必然失败的请求，不如明确报错。
        return Err(AiError::CodexUnsupported(
            "auth.json 里只有 ChatGPT 登录令牌（无 OPENAI_API_KEY），\
             本客户端暂不支持令牌登录。请在 Codex 中改用 API key 登录，\
             或在助手设置里选择「API key」并直接填写密钥。"
                .into(),
        ));
    }
    // 环境变量优先：容器/CI 里常见用环境变量覆盖，配置文件里的是登录时写入的旧值。
    let api_key = std::env::var("OPENAI_API_KEY")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| file_key.map(str::to_string))
        .ok_or_else(|| {
            AiError::Config(format!(
                "{} 里没有 OPENAI_API_KEY，且环境变量 OPENAI_API_KEY 为空",
                auth_path.display()
            ))
        })?;
    Ok(CodexSetup {
        model,
        base_url,
        api_key,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 独立临时目录；测试之间互不干扰，也绝不触碰真实用户目录。
    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("mino-ai-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn sample() -> AssistantConfig {
        AssistantConfig {
            enabled: true,
            api_key: "sk-test-123".into(),
            model: "gpt-4o-mini".into(),
            chat_model: "gpt-4o".into(),
            base_url: "https://api.example.com/v1/".into(),
            api_mode: "responses".into(),
            custom_headers: vec!["X-A: 1".into(), "X-B:2".into(), "坏头".into()],
            auth_type: "api_key".into(),
        }
    }

    #[test]
    fn 配置往返与默认值() {
        let dir = temp_dir("config-roundtrip");
        let path = dir.join("assistant.toml");
        let config = sample();
        config.save(&path).unwrap();
        let loaded = AssistantConfig::load(&path).unwrap();
        assert_eq!(loaded, config);

        // 缺字段的旧文件：全部回落到默认值，不报错。
        let legacy = dir.join("legacy.toml");
        std::fs::write(&legacy, "model = \"gpt-4o-mini\"\n").unwrap();
        let loaded = AssistantConfig::load(&legacy).unwrap();
        assert!(!loaded.enabled, "缺 enabled 应为 false");
        assert!(loaded.api_key.is_empty(), "缺 api_key 应为空串");
        assert_eq!(
            loaded.base_url, DEFAULT_BASE_URL,
            "缺 base_url 应为官方端点"
        );
        assert_eq!(loaded.auth_type, "api_key", "缺 auth_type 默认 API key");
        assert_eq!(loaded.api_mode, "chat_completions", "缺 api_mode 默认 chat");
        assert_eq!(
            loaded.effective_chat_model(),
            "gpt-4o-mini",
            "chat_model 空回退 model"
        );
        assert!(loaded.custom_headers.is_empty());
    }

    #[test]
    fn 空配置文件走默认值() {
        let dir = temp_dir("config-empty");
        let path = dir.join("assistant.toml");
        std::fs::write(&path, "").unwrap();
        let loaded = AssistantConfig::load(&path).unwrap();
        assert_eq!(loaded, AssistantConfig::default());
        assert_eq!(loaded.base_url, DEFAULT_BASE_URL);
        assert_eq!(loaded.effective_chat_model(), DEFAULT_MODEL);
    }

    #[cfg(unix)]
    #[test]
    fn 保存强制0600且失败清理临时文件() {
        use std::os::unix::fs::PermissionsExt;
        let dir = temp_dir("config-perm");
        let path = dir.join("assistant.toml");
        sample().save(&path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "含明文 api_key 的配置必须以 0600 落盘");

        // 失败路径：目标被同名目录占用时 rename 失败，临时文件须清理干净。
        let blocked = dir.join("blocked");
        std::fs::create_dir_all(blocked.join("assistant.toml")).unwrap();
        assert!(sample().save(&blocked.join("assistant.toml")).is_err());
        let leftovers: Vec<_> = std::fs::read_dir(&blocked)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp-"))
            .collect();
        assert!(leftovers.is_empty(), "失败后不应残留含明文密钥的临时文件");
    }

    #[test]
    fn api_key模式解析连接() {
        let dir = temp_dir("conn-apikey");
        let mut config = sample();
        config.base_url = "https://api.example.com/v1/".into();
        let conn = config.resolve_connection(&dir).unwrap();
        assert_eq!(
            conn.base_url, "https://api.example.com/v1",
            "末尾斜杠应被去掉"
        );
        assert_eq!(conn.api_key, "sk-test-123");
        assert_eq!(conn.api_mode, ApiMode::Responses);
        // 缺冒号的自定义头被跳过，其余保留并去掉值两侧空白。
        assert_eq!(
            conn.headers,
            vec![
                ("X-A".to_string(), "1".to_string()),
                ("X-B".to_string(), "2".to_string()),
            ]
        );

        // 空密钥明确报错，不发出必然 401 的请求。
        let mut empty = config.clone();
        empty.api_key = "  ".into();
        assert!(matches!(
            empty.resolve_connection(&dir),
            Err(AiError::ConfigInvalid(_))
        ));
        // 未知认证方式同样报错。
        let mut unknown = config.clone();
        unknown.auth_type = "copilot".into();
        assert!(matches!(
            unknown.resolve_connection(&dir),
            Err(AiError::ConfigInvalid(_))
        ));
        // 空 base_url 回落到官方端点。
        let mut default_url = config;
        default_url.base_url = "  ".into();
        assert_eq!(
            default_url.resolve_connection(&dir).unwrap().base_url,
            DEFAULT_BASE_URL
        );
    }

    #[test]
    fn codex模式读取临时目录配置() {
        let dir = temp_dir("conn-codex");
        let home = dir.join("codex");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(
            home.join("config.toml"),
            "model = \"gpt-5\"\nmodel_provider = \"corp\"\n\n\
             [model_providers.corp]\nname = \"corp\"\nbase_url = \"https://codex.example.com/v1\"\n\
             wire_api = \"responses\"\n",
        )
        .unwrap();
        std::fs::write(
            home.join("auth.json"),
            r#"{"OPENAI_API_KEY":"sk-codex-key","tokens":null}"#,
        )
        .unwrap();

        let config = AssistantConfig {
            auth_type: "codex".into(),
            ..Default::default()
        };
        let conn = config.resolve_connection(&home).unwrap();
        assert_eq!(conn.base_url, "https://codex.example.com/v1");
        assert_eq!(conn.api_key, "sk-codex-key");
        assert_eq!(conn.api_mode, ApiMode::Responses, "codex 固定走 responses");
        assert_eq!(conn.default_model, "gpt-5", "应带上 Codex 配置里的模型名");

        // 只有 ChatGPT 登录令牌：明确报「不支持」，而不是发一个必然失败的请求。
        std::fs::write(
            home.join("auth.json"),
            r#"{"tokens":{"access_token":"oauth-tok","id_token":"id"}}"#,
        )
        .unwrap();
        let err = config.resolve_connection(&home).unwrap_err();
        assert!(matches!(err, AiError::CodexUnsupported(_)), "得到 {err:?}");
        assert!(
            err.to_string().contains("不支持"),
            "错误信息应为中文说明：{err}"
        );
        // 错误信息里不能出现密钥内容。
        assert!(!err.to_string().contains("oauth-tok"));

        // 配置目录不存在时给出可操作的中文提示。
        let missing = dir.join("no-such-codex");
        let err = config.resolve_connection(&missing).unwrap_err();
        assert!(matches!(err, AiError::Config(_)));
        assert!(err.to_string().contains("codex"), "应提示 codex：{err}");
    }

    #[test]
    fn codex缺provider段报错且密钥不进错误信息() {
        let dir = temp_dir("conn-codex-missing-provider");
        let home = dir.join("codex");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(home.join("config.toml"), "model = \"gpt-5\"\n").unwrap();
        std::fs::write(
            home.join("auth.json"),
            r#"{"OPENAI_API_KEY":"sk-should-not-leak"}"#,
        )
        .unwrap();
        let config = AssistantConfig {
            auth_type: "codex".into(),
            ..Default::default()
        };
        // model_provider 缺省为 openai，配置里没有该段 → 报错。
        let err = config.resolve_connection(&home).unwrap_err();
        assert!(matches!(err, AiError::ConfigInvalid(_)), "得到 {err:?}");
        assert!(
            !err.to_string().contains("sk-should-not-leak"),
            "错误信息不得含密钥"
        );
    }

    #[test]
    fn 默认路径在mino配置目录下() {
        let path = default_assistant_path();
        assert!(
            path.ends_with("mino/assistant.toml"),
            "得到 {}",
            path.display()
        );
    }

    #[test]
    fn 缺失配置文件报错而不是静默默认() {
        let dir = temp_dir("config-missing");
        let err = AssistantConfig::load(&dir.join("nope.toml")).unwrap_err();
        assert!(matches!(err, AiError::Config(_)));
    }
}
