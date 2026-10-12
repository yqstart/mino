//! AI 助手：OpenAI 兼容客户端、助手配置与内置提示词。
//!
//! 只做「客户端层」：配置解析与落盘、同步 HTTP/SSE 传输、命令生成与
//! 命令修复两个高层入口。UI（面板、快捷键、内联提示）由 `mino-app`
//! 负责，本模块不依赖 egui。
//!
//! 线程模型：网络请求是阻塞式的（ureq 同步 API），调用方负责放到后台
//! 线程执行；取消通过调用方持有的 [`std::sync::atomic::AtomicBool`] 传入，
//! 传输层在每个 SSE 行之间检查该标记。
//!
//! 安全约定：错误信息与日志里只出现 base_url、模型名和 HTTP 状态码，
//! 绝不会写入 api_key（`AiError` 的任何中文消息都不拼接密钥）。

mod client;
mod config;
mod prompt;

pub use client::{complete, stream_chat, ChatMessage};
pub use config::{
    default_assistant_path, AiError, ApiMode, AssistantConfig, Connection, DEFAULT_BASE_URL,
    DEFAULT_MODEL,
};
pub use prompt::{
    build_chat_messages, build_fix_messages, build_suggest_messages, clean_command_output,
    parse_fix_suggestion, suggest_command, suggest_fix, FixContext, FixSuggestion,
};
