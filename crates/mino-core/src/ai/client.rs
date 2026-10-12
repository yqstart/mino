//! OpenAI 兼容传输层：同步 HTTP + SSE 流式解析。
//!
//! 全部为同步阻塞调用（ureq），由调用方放到后台线程执行；取消通过
//! `AtomicBool` 传入，请求前与每个 SSE 行之间都会检查。
//!
//! 安全约定：错误信息里只出现 HTTP 状态码、base_url 摘要与响应体片段，
//! **不包含 api_key**，也不把它写进日志。

use std::io::{BufRead, BufReader, Read};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::config::{AiError, ApiMode, Connection};

/// 建连超时（DNS + TCP + TLS）。
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// 整体读超时：流式回答要允许模型长时间思考，但不能无限挂住后台线程。
const READ_TIMEOUT: Duration = Duration::from_secs(300);

/// 每次请求的服务端错误预览上限。
///
/// 网关返回的 HTML 错误页动辄几十 KB，截断后再进 UI/日志，避免把整页
/// 塞进界面；512 字节足以包含 JSON 错误体的 message 字段。
const ERROR_BODY_PREVIEW_BYTES: usize = 512;

/// 单条 SSE 行的长度上限。
///
/// 正常事件只有几百字节；超长行意味着对端在发非 SSE 内容（如 HTML 错误页），
/// 直接截断丢弃，避免一次读入数 MB。
const MAX_SSE_LINE_BYTES: usize = 1024 * 1024;

/// 非流式响应体读取上限（正常回答远小于此值）。
const MAX_COMPLETE_BODY_BYTES: u64 = 16 * 1024 * 1024;

/// 一条对话消息。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatMessage {
    /// `system` / `user` / `assistant`。
    pub role: &'static str,
    /// 消息正文。
    pub content: String,
}

impl ChatMessage {
    /// 系统消息（提示词）。
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: "system",
            content: content.into(),
        }
    }

    /// 用户消息。
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: "user",
            content: content.into(),
        }
    }

    /// 助手消息（多轮对话时回填历史）。
    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: "assistant",
            content: content.into(),
        }
    }
}

/// 流式请求的请求体。
#[derive(Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: &'a [ChatMessage],
    stream: bool,
}

/// 非流式请求的请求体（`stream` 显式置 false，避免个别网关默认开启流式）。
#[derive(Serialize)]
struct CompleteRequest<'a> {
    model: &'a str,
    messages: &'a [ChatMessage],
    stream: bool,
}

/// 从零开始拼接一段完整回答（流式接口）。
///
/// `on_delta` 收到每一段增量文本；返回值是拼接后的完整回答。
pub fn stream_chat(
    conn: &Connection,
    model: &str,
    messages: &[ChatMessage],
    cancel: &AtomicBool,
    on_delta: &mut dyn FnMut(&str),
) -> Result<String, AiError> {
    let body = serde_json::to_string(&ChatRequest {
        model,
        messages,
        stream: true,
    })
    .map_err(|e| AiError::Parse(format!("序列化请求体失败：{e}")))?;
    let mode = conn.api_mode;
    let path = match mode {
        ApiMode::ChatCompletions => "/chat/completions",
        ApiMode::Responses => "/responses",
    };
    let mut response = post_json(conn, path, &body, cancel, true)?;
    let mut reader = BufReader::with_capacity(8 * 1024, response.body_mut().as_reader());
    let mut text = String::new();
    let mut seen_text = false;
    // `event:` 行与随后的 `data:` 行成对出现，跨行维护事件类型。
    let mut event_kind: Option<String> = None;
    while let Some(line) = read_line_limited(&mut reader)? {
        if cancel.load(Ordering::Relaxed) {
            return Err(AiError::Cancelled);
        }
        if let Some(event) = merge_sse_line(parse_sse_line(&line), &mut event_kind) {
            dispatch_sse(mode, &event, &mut text, &mut seen_text, &mut *on_delta)?;
        }
    }
    Ok(text)
}

/// 非流式请求：一次拿到完整回答（命令生成/命令修复用）。
///
/// chat 模式取 `choices[0].message.content`；responses 模式优先取
/// `output_text` 便利字段，缺失时回退到 `output[].content[].text`。
pub fn complete(
    conn: &Connection,
    model: &str,
    messages: &[ChatMessage],
) -> Result<String, AiError> {
    complete_with_cancel(conn, model, messages, None)
}

/// 可取消的非流式请求（`super` 内部使用：高层入口带取消标记时走这里）。
///
/// 非流式调用没有可切分的中间点，取消只能在「请求前后」生效：
/// 等待期间线程仍会阻塞到超时或响应返回（这也是命令生成/修复走非流式、
/// 由调用方放到后台线程的原因）。`None` 表示不检查取消。
pub(super) fn complete_with_cancel(
    conn: &Connection,
    model: &str,
    messages: &[ChatMessage],
    cancel: Option<&AtomicBool>,
) -> Result<String, AiError> {
    let body = serde_json::to_string(&CompleteRequest {
        model,
        messages,
        stream: false,
    })
    .map_err(|e| AiError::Parse(format!("序列化请求体失败：{e}")))?;
    let path = match conn.api_mode {
        ApiMode::ChatCompletions => "/chat/completions",
        ApiMode::Responses => "/responses",
    };
    // 非流式请求没有中途检查点，用一个恒为 false 的标记传给传输层；
    // 真正的取消只在请求前后各检查一次（见函数文档）。
    let no_cancel = AtomicBool::new(false);
    let cancel = cancel.unwrap_or(&no_cancel);
    let mut response = post_json(conn, path, &body, cancel, false)?;
    if cancel.load(Ordering::Relaxed) {
        return Err(AiError::Cancelled);
    }
    let raw = response
        .body_mut()
        .with_config()
        .limit(MAX_COMPLETE_BODY_BYTES)
        .lossy_utf8(true)
        .read_to_string()
        .map_err(|e| AiError::Network(format!("读取响应失败：{e}")))?;
    let value: serde_json::Value = serde_json::from_str(&raw)
        .map_err(|e| AiError::Parse(format!("响应不是合法 JSON：{e}")))?;
    let text = if conn.api_mode == ApiMode::Responses {
        response_text(&value)
    } else {
        value
            .get("choices")
            .and_then(|c| c.get(0))
            .and_then(|c| c.get("message"))
            .and_then(|m| m.get("content"))
            .and_then(|c| c.as_str())
            .map(str::to_string)
    };
    match text {
        Some(text) => Ok(text),
        // 结构不符合预期时附上片段，便于用户判断是不是网关在返回别的东西
        // （例如未登录时返回 HTML）。片段可能很长，截断到 200 字符。
        None => Err(AiError::Parse(format!(
            "响应缺少回答内容：{}",
            preview_of(&value)
        ))),
    }
}

/// 非 2xx 响应体里取一段可读文本（截断上限内）。
fn preview_of(value: &serde_json::Value) -> String {
    let text = serde_json::to_string(value).unwrap_or_default();
    text.chars().take(200).collect()
}

/// 从 responses 响应里取回答文本。
///
/// 完整响应对象既可能直接是事件数据（`{"output_text": ...}`），也可能
/// 嵌在 `response` 字段下（`response.completed` 事件），两种都支持。
fn response_text(value: &serde_json::Value) -> Option<String> {
    let body = value
        .get("response")
        .filter(|v| v.is_object())
        .unwrap_or(value);
    if let Some(text) = body.get("output_text").and_then(|t| t.as_str()) {
        return Some(text.to_string());
    }
    let mut out = String::new();
    for item in body.get("output")?.as_array()? {
        let Some(parts) = item.get("content").and_then(|c| c.as_array()) else {
            continue;
        };
        for part in parts {
            if let Some(text) = part.get("text").and_then(|t| t.as_str()) {
                out.push_str(text);
            }
        }
    }
    Some(out)
}

/// 发一个 JSON POST 请求；非 2xx 时把响应体前 512 字节拼进中文错误。
///
/// `streaming` 决定 `Accept`：流式要 `text/event-stream`，非流式声明
/// JSON——个别网关会按 `Accept` 选择响应形态，写错会拿到不可解析的内容。
fn post_json(
    conn: &Connection,
    path: &str,
    body: &str,
    cancel: &AtomicBool,
    streaming: bool,
) -> Result<ureq::http::Response<ureq::Body>, AiError> {
    if cancel.load(Ordering::Relaxed) {
        return Err(AiError::Cancelled);
    }
    let url = format!("{}{path}", conn.base_url);
    let agent = ureq::Agent::config_builder()
        .timeout_connect(Some(CONNECT_TIMEOUT))
        // 全局超时同时覆盖建连与响应体读取：它包含连接阶段，也兜住
        // 「连接后被中间设备挂住」这种按阶段超时抓不住的等待。
        .timeout_global(Some(READ_TIMEOUT))
        // 手动处理 4xx/5xx，否则 ureq 默认把状态码转成 Err，拿不到响应体。
        .http_status_as_error(false)
        .build()
        .new_agent();

    let mut request = agent
        .post(&url)
        .header("Content-Type", "application/json")
        .header(
            "Accept",
            if streaming {
                "text/event-stream"
            } else {
                "application/json"
            },
        )
        .header("Authorization", format!("Bearer {}", conn.api_key));
    for (name, value) in &conn.headers {
        // 头名/头值先转成 http 类型再设置：`header()` 遇到非法值会 panic，
        // 而这里的输入来自用户配置文件，不能让一个笔误崩掉整个助手。
        if let (Ok(name), Ok(value)) = (
            name.parse::<ureq::http::HeaderName>(),
            value.parse::<ureq::http::HeaderValue>(),
        ) {
            request = request.header(name, value);
        }
    }
    let response = request
        .send(body)
        .map_err(|e| AiError::Network(format!("请求 {url} 失败：{e}")))?;
    let status = response.status();
    if !status.is_success() {
        let body = read_error_preview(response);
        return Err(AiError::Http {
            status: status.as_u16(),
            body,
        });
    }
    Ok(response)
}

/// 读取错误响应体的前若干字节（丢失解码器按 UTF-8 宽松处理）。
fn read_error_preview(response: ureq::http::Response<ureq::Body>) -> String {
    let (_, mut body) = response.into_parts();
    let mut buf = vec![0u8; ERROR_BODY_PREVIEW_BYTES];
    let read = body.as_reader().read(&mut buf).unwrap_or(0);
    buf.truncate(read);
    String::from_utf8_lossy(&buf).trim().to_string()
}

/// 一条 SSE 行解析后的语义。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SseEvent {
    /// 指定了事件类型（`event:` 行 + 后续 `data:` 行）。
    Typed {
        /// 事件类型（如 `response.output_text.delta`）。
        kind: String,
        /// 事件数据（`data:` 行的原文）。
        data: String,
    },
    /// 仅数据行（OpenAI chat/completions 的常见形态）。
    Data(String),
    /// `data: [DONE]`：流结束。
    Done,
    /// 注释行（`: keep-alive` 之类）。
    Comment,
}

/// 单个 SSE 行的解析结果（先按字段解析，再合并成 [`SseEvent`]）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SseLine {
    /// `comment` 行。
    Comment,
    /// `data:` 行（已去掉字段名与前导空格）。
    Data(String),
    /// `data: [DONE]`。
    Done,
    /// `event:` 行：事件类型，随后的 data 行属于它。
    Event(String),
    /// `id:` / `retry:` / 未知字段 / 空行：与回答内容无关。
    Ignored,
}

/// 解析单个 SSE 行；空行、未知字段、裸字段名都安静跳过。
///
/// 必须先按字段切分再判定 `data`，否则 JSON 里的冒号（`data: {"a":1}`）
/// 会被 `event:`/`data:` 的字段匹配吃掉。
pub fn parse_sse_line(line: &str) -> SseLine {
    let trimmed = line.trim_end_matches(['\r', '\n']);
    if trimmed.is_empty() {
        // 事件之间的空行分隔符。
        return SseLine::Ignored;
    }
    if trimmed.starts_with(':') {
        // 注释行（`: keep-alive`）。
        return SseLine::Comment;
    }
    let Some((field, value)) = trimmed.split_once(':') else {
        // 裸字段名（无冒号、无值）没有内容可读。
        return SseLine::Ignored;
    };
    let value = value.strip_prefix(' ').unwrap_or(value);
    match field {
        "data" => {
            let data = value.trim();
            if data.is_empty() {
                SseLine::Ignored
            } else if data == "[DONE]" {
                SseLine::Done
            } else {
                SseLine::Data(data.to_string())
            }
        }
        "event" => {
            let kind = value.trim();
            if kind.is_empty() {
                SseLine::Ignored
            } else {
                SseLine::Event(kind.to_string())
            }
        }
        // 注释行（`: keep-alive`）与 id/retry 字段都不影响回答内容。
        _ => SseLine::Ignored,
    }
}

/// 把「事件类型 + 数据」合并成 [`SseEvent`]。
///
/// `event_kind` 是调用方维护的状态：`event:` 行设置它，`data:` 行消费它。
/// 这样 `data` 先于 `event` 出现（顺序异常的服务端）时也不会丢内容。
fn merge_sse_line(line: SseLine, event_kind: &mut Option<String>) -> Option<SseEvent> {
    match line {
        SseLine::Comment => Some(SseEvent::Comment),
        SseLine::Ignored => None,
        SseLine::Done => Some(SseEvent::Done),
        SseLine::Event(kind) => {
            *event_kind = Some(kind);
            None
        }
        SseLine::Data(data) => Some(match event_kind.take() {
            Some(kind) => SseEvent::Typed { kind, data },
            None => SseEvent::Data(data),
        }),
    }
}

/// 按接口模式分发一条 SSE 事件。
fn dispatch_sse(
    mode: ApiMode,
    event: &SseEvent,
    text: &mut String,
    seen_text: &mut bool,
    on_delta: &mut dyn FnMut(&str),
) -> Result<(), AiError> {
    match mode {
        ApiMode::ChatCompletions => dispatch_chat(event, text, seen_text, on_delta),
        ApiMode::Responses => dispatch_responses(event, text, seen_text, on_delta),
    }
}

/// chat/completions 的事件分发。
fn dispatch_chat(
    event: &SseEvent,
    text: &mut String,
    seen_text: &mut bool,
    on_delta: &mut dyn FnMut(&str),
) -> Result<(), AiError> {
    let data = match event {
        SseEvent::Data(data) => data,
        SseEvent::Typed { data, .. } => data,
        // `[DONE]` 与注释行不含数据，安静跳过。
        SseEvent::Done | SseEvent::Comment => return Ok(()),
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(data) else {
        // 非 JSON 数据行安静跳过（有的网关会插入 keep-alive 文本）。
        return Ok(());
    };
    if let Some(message) = error_message(&value) {
        return Err(AiError::Parse(format!("服务端返回错误：{message}")));
    }
    if let Some(delta) = chat_delta(&value) {
        *seen_text = true;
        text.push_str(&delta);
        on_delta(&delta);
    }
    Ok(())
}

/// responses 的事件分发。
fn dispatch_responses(
    event: &SseEvent,
    text: &mut String,
    seen_text: &mut bool,
    on_delta: &mut dyn FnMut(&str),
) -> Result<(), AiError> {
    match event {
        SseEvent::Typed { kind, data } => {
            let value = serde_json::from_str::<serde_json::Value>(data.trim()).ok();
            if let Some(message) = value
                .as_ref()
                .and_then(responses_error_message)
                .or_else(|| match kind.as_str() {
                    // 错误事件的数据里没有 message 字段时，把原文带出去。
                    "error" | "response.failed" | "response.incomplete" => {
                        Some(data.trim().to_string())
                    }
                    _ => None,
                })
            {
                return Err(AiError::Parse(format!("服务端返回错误：{message}")));
            }
            match kind.as_str() {
                "response.output_text.delta" => {
                    let delta = value
                        .as_ref()
                        .and_then(|v| v.get("delta"))
                        .and_then(|v| v.as_str())
                        .unwrap_or_default();
                    if !delta.is_empty() {
                        *seen_text = true;
                        text.push_str(delta);
                        on_delta(delta);
                    }
                    Ok(())
                }
                "response.completed" | "response.done" => {
                    if !*seen_text {
                        // 流里没有 delta（例如被中间层合并），从完成事件里补全文。
                        if let Some(full) = value.as_ref().and_then(response_text) {
                            if !full.is_empty() {
                                *seen_text = true;
                                text.push_str(&full);
                                on_delta(&full);
                            }
                        }
                    }
                    Ok(())
                }
                // `response.output_text.done` 里是完整文本，delta 已经拼过；
                // 其它事件（created/in_progress/output_item.*）安静跳过。
                _ => Ok(()),
            }
        }
        // 没有 event: 前缀时按 chat 的 data 行规则处理（兼容只发 data 的网关）。
        SseEvent::Data(_) => dispatch_chat(event, text, seen_text, on_delta),
        SseEvent::Done | SseEvent::Comment => Ok(()),
    }
}

/// 从 JSON 里取错误描述；`{"error": {...}}` 与 `{"error": "..."}` 都支持。
fn error_message(value: &serde_json::Value) -> Option<String> {
    let error = value.get("error")?;
    if error.is_null() {
        return None;
    }
    Some(
        error
            .get("message")
            .and_then(|m| m.as_str())
            .map(str::to_string)
            .unwrap_or_else(|| preview_of(error)),
    )
}

/// Responses API 的错误描述：既认嵌套 `error.message`，也认顶层 `message`
/// （`response.failed` 事件把 message 放在顶层）。
fn responses_error_message(value: &serde_json::Value) -> Option<String> {
    error_message(value).or_else(|| {
        value
            .get("message")
            .and_then(|m| m.as_str())
            .map(str::to_string)
            .filter(|s| !s.trim().is_empty())
    })
}

/// 取 chat/completions 增量文本（`choices[0].delta.content`）。
fn chat_delta(value: &serde_json::Value) -> Option<String> {
    value
        .get("choices")?
        .get(0)?
        .get("delta")?
        .get("content")?
        .as_str()
        .map(str::to_string)
}

/// 读一行（按 `\n` 分界，行内保留原始字节，由调用方 trim）。
///
/// 用 `read_until` 而不是 `read_line`：非 UTF-8 的响应体（如网关返回的
/// HTML 错误页）不应该让整个流失败。
///
/// 超长行（超过 [`MAX_SSE_LINE_BYTES`]）整体丢弃：`take` 限制单次读取量，
/// 既不会把数 MB 的内容读进内存，也不会因为「读一半再跳过剩余」而吃掉
/// 紧跟其后的下一条事件。
fn read_line_limited<R: BufRead>(reader: &mut R) -> Result<Option<String>, AiError> {
    let mut buf: Vec<u8> = Vec::new();
    let read = reader
        .take(MAX_SSE_LINE_BYTES as u64)
        .read_until(b'\n', &mut buf)
        .map_err(|e| AiError::Network(format!("读取流式响应失败：{e}")))?;
    if read == 0 {
        return Ok(None);
    }
    if read >= MAX_SSE_LINE_BYTES && buf.last() != Some(&b'\n') {
        // 超长行：剩余部分读到下一个换行为止再丢弃。这里同样用 `take`
        // 限制单次读取量，避免一个不换行的服务端把内存吃满。
        loop {
            let mut discard: Vec<u8> = Vec::new();
            let n = reader
                .take(MAX_SSE_LINE_BYTES as u64)
                .read_until(b'\n', &mut discard)
                .map_err(|e| AiError::Network(format!("读取流式响应失败：{e}")))?;
            if n == 0 || discard.last() == Some(&b'\n') {
                break;
            }
        }
        return Ok(Some(String::new()));
    }
    Ok(Some(String::from_utf8_lossy(&buf).into_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// 用内存里的 SSE 字节流跑一遍流式解析（与网络路径共用同一段实现）。
    fn run_stream(mode: ApiMode, raw: &str) -> Result<(String, Vec<String>), AiError> {
        let mut reader = BufReader::new(Cursor::new(raw.as_bytes().to_vec()));
        let mut text = String::new();
        let mut seen_text = false;
        let mut deltas: Vec<String> = Vec::new();
        let mut event_kind: Option<String> = None;
        while let Some(line) = read_line_limited(&mut reader)? {
            if let Some(event) = merge_sse_line(parse_sse_line(&line), &mut event_kind) {
                dispatch_sse(mode, &event, &mut text, &mut seen_text, &mut |d: &str| {
                    deltas.push(d.to_string())
                })?;
            }
        }
        Ok((text, deltas))
    }

    #[test]
    fn sse行解析覆盖各类行() {
        assert_eq!(
            parse_sse_line("data: {\"a\":1}"),
            SseLine::Data("{\"a\":1}".into())
        );
        // 无空格的写法同样成立。
        assert_eq!(
            parse_sse_line("data:{\"a\":1}"),
            SseLine::Data("{\"a\":1}".into())
        );
        assert_eq!(parse_sse_line("data: [DONE]"), SseLine::Done);
        assert_eq!(
            parse_sse_line("event: response.completed\r\n"),
            SseLine::Event("response.completed".into())
        );
        assert_eq!(parse_sse_line(": keep-alive"), SseLine::Comment);
        assert_eq!(parse_sse_line(""), SseLine::Ignored);
        assert_eq!(parse_sse_line("\r"), SseLine::Ignored);
        assert_eq!(parse_sse_line("retry: 1000"), SseLine::Ignored);
        assert_eq!(parse_sse_line("id: 42"), SseLine::Ignored);
        assert_eq!(parse_sse_line("data:"), SseLine::Ignored);
    }

    #[test]
    fn 事件类型与数据行合并() {
        let mut kind = None;
        assert_eq!(
            merge_sse_line(parse_sse_line("event: x.y"), &mut kind),
            None
        );
        assert_eq!(kind.as_deref(), Some("x.y"));
        assert_eq!(
            merge_sse_line(parse_sse_line("data: {\"a\":1}"), &mut kind),
            Some(SseEvent::Typed {
                kind: "x.y".into(),
                data: "{\"a\":1}".into(),
            })
        );
        // 事件类型只作用于一行的数据；下一行没有 event: 时退回 Data。
        assert_eq!(kind, None);
        assert_eq!(
            merge_sse_line(parse_sse_line("data: {\"b\":2}"), &mut kind),
            Some(SseEvent::Data("{\"b\":2}".into()))
        );
        // data 里含冒号的 JSON：必须整体作为数据，不能被字段匹配吃掉。
        assert_eq!(
            merge_sse_line(
                parse_sse_line("data: {\"choices\":[{\"delta\":{\"content\":\"a:b\"}}]}"),
                &mut kind
            ),
            Some(SseEvent::Data(
                "{\"choices\":[{\"delta\":{\"content\":\"a:b\"}}]}".into()
            ))
        );
    }

    #[test]
    fn 解析chat流并跳过非json行() {
        let raw = "data: {\"choices\":[{\"delta\":{\"role\":\"assistant\"}}]}\n\n\
                   data: 这里是网关插入的噪声\n\n\
                   data: {\"choices\":[{\"delta\":{\"content\":\"好\"}}]}\n\
                   data: {\"choices\":[{\"delta\":{\"content\":\"的\"}}]}\n\n\
                   data: [DONE]\n\n";
        let (text, deltas) = run_stream(ApiMode::ChatCompletions, raw).unwrap();
        assert_eq!(text, "好的");
        assert_eq!(deltas, vec!["好".to_string(), "的".to_string()]);
    }

    #[test]
    fn 解析responses流并跳过未知事件() {
        let raw = "event: response.created\n\
                   data: {\"type\":\"response.created\"}\n\n\
                   event: response.output_item.added\n\
                   data: {\"type\":\"response.output_item.added\"}\n\n\
                   event: response.output_text.delta\n\
                   data: {\"type\":\"response.output_text.delta\",\"delta\":\"你\"}\n\n\
                   event: response.output_text.delta\n\
                   data: {\"type\":\"response.output_text.delta\",\"delta\":\"好\"}\n\n\
                   event: response.completed\n\
                   data: {\"type\":\"response.completed\",\"response\":{\"output_text\":\"你好\"}}\n\n";
        let (text, deltas) = run_stream(ApiMode::Responses, raw).unwrap();
        assert_eq!(text, "你好");
        assert_eq!(deltas, vec!["你".to_string(), "好".to_string()]);

        // 没有 delta（被中间层合并）时用完成事件里的完整文本兜底。
        let raw = "event: response.completed\n\
                   data: {\"type\":\"response.completed\",\"response\":{\"output_text\":\"整段回答\"}}\n\n";
        let (text, deltas) = run_stream(ApiMode::Responses, raw).unwrap();
        assert_eq!(text, "整段回答");
        assert_eq!(deltas, vec!["整段回答".to_string()]);

        // 只有 struct 化的 output 数组时也能取出文本。
        let raw = "event: response.completed\n\
                   data: {\"type\":\"response.completed\",\"response\":{\"output\":[{\"content\":[{\"type\":\"output_text\",\"text\":\"A\"},{\"type\":\"output_text\",\"text\":\"B\"}]}]}}\n\n";
        let (text, _) = run_stream(ApiMode::Responses, raw).unwrap();
        assert_eq!(text, "AB");
    }

    #[test]
    fn 错误事件转成错误() {
        // chat 模式：数据行里带 error 对象。
        let raw = "data: {\"error\":{\"message\":\"Invalid API key\",\"type\":\"invalid_request_error\"}}\n\n";
        let err = run_stream(ApiMode::ChatCompletions, raw).unwrap_err();
        assert!(matches!(err, AiError::Parse(_)), "得到 {err:?}");
        assert!(err.to_string().contains("Invalid API key"));

        // responses 模式：event: error 独立事件。
        let raw = "event: error\n\
                   data: {\"type\":\"error\",\"code\":\"invalid_api_key\",\"message\":\"bad key\"}\n\n";
        let err = run_stream(ApiMode::Responses, raw).unwrap_err();
        assert!(matches!(err, AiError::Parse(_)), "得到 {err:?}");
        assert!(err.to_string().contains("bad key"));

        // 已知 delta 之后再报错，同样返回错误（不能把半截回答当成功）。
        let raw = "event: response.output_text.delta\n\
                   data: {\"delta\":\"半截\"}\n\n\
                   event: error\n\
                   data: {\"message\":\"upstream reset\"}\n\n";
        let err = run_stream(ApiMode::Responses, raw).unwrap_err();
        assert!(err.to_string().contains("upstream reset"));
    }

    #[test]
    fn 流结束返回已拼接文本() {
        let raw = "data: {\"choices\":[{\"delta\":{\"content\":\"无\"}}]}\n\
                   data: {\"choices\":[{\"delta\":{\"content\":\"DONE标记\"}}]}\n";
        let (text, _) = run_stream(ApiMode::ChatCompletions, raw).unwrap();
        assert_eq!(text, "无DONE标记");
    }

    #[test]
    fn 错误预览截断且不越界() {
        let long = "x".repeat(4096);
        let response = ureq::http::Response::builder()
            .status(500)
            .body(ureq::Body::builder().data(long.clone()))
            .unwrap();
        let preview = read_error_preview(response);
        assert_eq!(preview.len(), ERROR_BODY_PREVIEW_BYTES);
        assert!(long.starts_with(&preview));
    }

    #[test]
    fn 超长行被截断而不是吃满内存() {
        let mut raw = String::from("event: response.output_text.delta\ndata: ");
        raw.push_str(&"x".repeat(MAX_SSE_LINE_BYTES + 1024));
        raw.push('\n');
        raw.push_str("event: response.output_text.delta\ndata: {\"delta\":\"ok\"}\n");
        let (text, _) = run_stream(ApiMode::Responses, &raw).unwrap();
        assert_eq!(text, "ok", "超长行应被丢弃，后续事件仍正常");
    }
}
