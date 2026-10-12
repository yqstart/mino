//! OSC 7 / OSC 133 标记嗅探（shell 集成经 PTY 字节流上报）。
//!
//! shell 集成脚本（`assets/shell-integration/mino.zsh`）在提示符与命令
//! 生命周期处输出标记；它们在到达 VT 解析器的同一条字节流上，本模块在
//! 读取路径上嗅探并转成会话事件：
//!
//! - `OSC 7`（`file://host/path`）→ 当前工作目录（标题/SFTP 定位的权威
//!   来源，远程会话尤其重要——此前远程目录只能靠 `cd` 文本跟踪）；
//! - `OSC 133;A/B` → 提示符就绪（shell 空闲待输入）；
//! - `OSC 133;C` → 命令开始执行；
//! - `OSC 133;D;<exit>` → 命令结束与退出码（AI 失败建议、标签注意力点）。
//!
//! 实现为逐字节状态机：Ground 态线性跳到下一个 ESC（普通文本零解析
//! 成本）；OSC 载荷在缓冲里累积（跨 read 分片的半截序列可靠补齐），
//! 超过上限（2KB）直接放弃回 Ground——畸形数据不会拖住读取路径。

/// shell 集成标记。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Marker {
    /// 当前目录（OSC 7）。
    Cwd(String),
    /// 提示符就绪（OSC 133;A/B）。
    PromptReady,
    /// 命令开始执行（OSC 133;C）。
    CommandStart,
    /// 命令结束（OSC 133;D;<exit_code>）。
    CommandFinished(i32),
}

/// OSC 载荷缓冲上限（畸形序列防护；正常标记 < 200 字节）。
const MAX_OSC_PAYLOAD: usize = 2048;

#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    Ground,
    Esc,
    Osc,
    OscEsc,
}

/// 增量嗅探器（跨 read 分片保持状态）。
pub struct OscSniffer {
    state: State,
    buf: Vec<u8>,
}

impl Default for OscSniffer {
    fn default() -> Self {
        Self::new()
    }
}

impl OscSniffer {
    pub fn new() -> Self {
        Self {
            state: State::Ground,
            buf: Vec::new(),
        }
    }

    /// 喂入一段字节；完整的标记追加到 `out`。
    pub fn feed(&mut self, bytes: &[u8], out: &mut Vec<Marker>) {
        let mut index = 0;
        while index < bytes.len() {
            match self.state {
                State::Ground => {
                    // 线性跳到下一个 ESC：普通文本（`cat` 大文件）零成本。
                    match bytes[index..].iter().position(|b| *b == 0x1b) {
                        Some(offset) => {
                            index += offset + 1;
                            self.state = State::Esc;
                        }
                        None => return,
                    }
                }
                State::Esc => {
                    let byte = bytes[index];
                    index += 1;
                    if byte == b']' {
                        self.state = State::Osc;
                        self.buf.clear();
                    } else if byte == 0x1b {
                        // ESC ESC：保持 Esc（下一字节仍可能是 `]`）。
                        self.state = State::Esc;
                    } else {
                        self.state = State::Ground;
                    }
                }
                State::Osc => {
                    let byte = bytes[index];
                    index += 1;
                    if byte == 0x07 {
                        // BEL 终结。
                        self.finish(out);
                    } else if byte == 0x1b {
                        self.state = State::OscEsc;
                    } else if self.buf.len() < MAX_OSC_PAYLOAD {
                        self.buf.push(byte);
                    } else {
                        // 超长载荷：放弃（回 Ground 重新找 ESC）。
                        self.state = State::Ground;
                        self.buf.clear();
                    }
                }
                State::OscEsc => {
                    let byte = bytes[index];
                    index += 1;
                    if byte == b'\\' {
                        // ST（ESC \）终结。
                        self.finish(out);
                    } else {
                        // 非 ST：序列作废（避免把后续内容吃进载荷）。
                        self.state = State::Ground;
                        self.buf.clear();
                    }
                }
            }
        }
    }

    /// 一个 OSC 序列结束：解析载荷并复位。
    fn finish(&mut self, out: &mut Vec<Marker>) {
        if let Some(marker) = parse_osc(&self.buf) {
            out.push(marker);
        }
        self.state = State::Ground;
        self.buf.clear();
    }
}

/// 解析 OSC 载荷（不含 `ESC ]` 与终结符）。
fn parse_osc(payload: &[u8]) -> Option<Marker> {
    let text = std::str::from_utf8(payload).ok()?;
    let (code, rest) = text.split_once(';')?;
    match code {
        "7" => parse_osc7(rest).map(Marker::Cwd),
        "133" => {
            let (kind, arg) = match rest.split_once(';') {
                Some((kind, arg)) => (kind, arg),
                None => (rest, ""),
            };
            match kind {
                "A" | "B" => Some(Marker::PromptReady),
                "C" => Some(Marker::CommandStart),
                "D" => Some(Marker::CommandFinished(
                    arg.trim().parse::<i32>().unwrap_or(0),
                )),
                // 其它 133 子类型（P;k=… 等）忽略。
                _ => None,
            }
        }
        _ => None,
    }
}

/// `OSC 7`：`file://host/path` → `/path`（带简易 %XX 解码）。
fn parse_osc7(rest: &str) -> Option<String> {
    let after_scheme = rest.strip_prefix("file://").unwrap_or(rest);
    // 跳过主机名：第一个 `/` 之后是路径（无主机时可能直接是 `/path`）。
    let index = after_scheme.find('/')?;
    let path = &after_scheme[index..];
    if path.is_empty() {
        return None;
    }
    Some(percent_decode(path))
}

/// 简易百分号解码（`%20` → 空格等；非法序列原样保留）。
fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            if let (Some(high), Some(low)) = (
                bytes.get(index + 1).copied().and_then(hex_value),
                bytes.get(index + 2).copied().and_then(hex_value),
            ) {
                out.push(high * 16 + low);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

// ==================== PTY 读取包装（unix） ====================

#[cfg(unix)]
pub(crate) mod pty {
    use std::io::{self, Read, Write};
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::sync::Arc;

    use alacritty_terminal::event::{OnResize, WindowSize};
    use alacritty_terminal::tty::{ChildEvent, EventedPty, EventedReadWrite, Pty};
    use parking_lot::Mutex;
    use polling::{Event, PollMode, Poller};

    use super::{Marker, OscSniffer};

    /// 标记回调（App 侧转成 `SessionEvent` 并入 pending 队列）。
    pub type MarkerHandler = Arc<dyn Fn(Marker) + Send + Sync>;

    /// 在读取路径上嗅探 OSC 标记的 PTY 包装。
    ///
    /// alacritty 的 `EventLoop` 通过 `reader().read(..)` 取字节；包装的
    /// reader 持有一份 **dup 的 ptmx fd**（与内层共享同一 file description，
    /// 读到同一条流；事件循环注册的仍是内层 fd，可读通知照常）。
    /// writer 同理走另一份 dup——写 PTY 与读相互独立，不引入竞争。
    pub(crate) struct SniffingPty {
        inner: Pty,
        reader: SniffReader,
        writer: SniffWriter,
    }

    impl SniffingPty {
        pub fn new(inner: Pty, on_marker: MarkerHandler) -> io::Result<Self> {
            let read_fd = unsafe { libc::dup(inner.file().as_raw_fd()) };
            if read_fd < 0 {
                return Err(io::Error::last_os_error());
            }
            let write_fd = unsafe { libc::dup(inner.file().as_raw_fd()) };
            if write_fd < 0 {
                unsafe { libc::close(read_fd) };
                return Err(io::Error::last_os_error());
            }
            Ok(Self {
                inner,
                reader: SniffReader {
                    file: unsafe { std::fs::File::from_raw_fd(read_fd) },
                    sniffer: Arc::new(Mutex::new(OscSniffer::new())),
                    on_marker: on_marker.clone(),
                },
                writer: SniffWriter {
                    file: unsafe { std::fs::File::from_raw_fd(write_fd) },
                },
            })
        }
    }

    /// 读句柄：读到字节后喂给嗅探器、把标记回调出去。
    pub(crate) struct SniffReader {
        file: std::fs::File,
        sniffer: Arc<Mutex<OscSniffer>>,
        on_marker: MarkerHandler,
    }

    impl Read for SniffReader {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let n = self.file.read(buf)?;
            if n > 0 {
                let mut markers = Vec::new();
                {
                    let mut sniffer = self.sniffer.lock();
                    sniffer.feed(&buf[..n], &mut markers);
                }
                for marker in markers {
                    (self.on_marker)(marker);
                }
            }
            Ok(n)
        }
    }

    /// 写句柄（不嗅探）。
    pub(crate) struct SniffWriter {
        file: std::fs::File,
    }

    impl Write for SniffWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.file.write(buf)
        }
        fn flush(&mut self) -> io::Result<()> {
            self.file.flush()
        }
    }

    impl EventedReadWrite for SniffingPty {
        type Reader = SniffReader;
        type Writer = SniffWriter;

        unsafe fn register(
            &mut self,
            poll: &Arc<Poller>,
            event: Event,
            mode: PollMode,
        ) -> io::Result<()> {
            unsafe { self.inner.register(poll, event, mode) }
        }

        fn reregister(
            &mut self,
            poll: &Arc<Poller>,
            event: Event,
            mode: PollMode,
        ) -> io::Result<()> {
            self.inner.reregister(poll, event, mode)
        }

        fn deregister(&mut self, poll: &Arc<Poller>) -> io::Result<()> {
            self.inner.deregister(poll)
        }

        fn reader(&mut self) -> &mut Self::Reader {
            &mut self.reader
        }

        fn writer(&mut self) -> &mut Self::Writer {
            &mut self.writer
        }
    }

    impl EventedPty for SniffingPty {
        fn next_child_event(&mut self) -> Option<ChildEvent> {
            self.inner.next_child_event()
        }
    }

    impl OnResize for SniffingPty {
        fn on_resize(&mut self, window_size: WindowSize) {
            self.inner.on_resize(window_size)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed_all(sniffer: &mut OscSniffer, chunks: &[&[u8]]) -> Vec<Marker> {
        let mut out = Vec::new();
        for chunk in chunks {
            sniffer.feed(chunk, &mut out);
        }
        out
    }

    #[test]
    fn 解析osc7与133标记() {
        let mut sniffer = OscSniffer::new();
        let mut out = Vec::new();
        sniffer.feed(b"hello \x1b]7;file://host/Users/me/proj\x07world", &mut out);
        sniffer.feed(b"\x1b]133;A\x07", &mut out);
        sniffer.feed(b"\x1b]133;B\x07", &mut out);
        sniffer.feed(b"\x1b]133;C;\x07", &mut out);
        sniffer.feed(b"\x1b]133;D;1\x07", &mut out);
        assert_eq!(
            out,
            vec![
                Marker::Cwd("/Users/me/proj".into()),
                Marker::PromptReady,
                Marker::PromptReady,
                Marker::CommandStart,
                Marker::CommandFinished(1),
            ]
        );
    }

    #[test]
    fn 跨分片与st终结() {
        let mut sniffer = OscSniffer::new();
        // 标记被切碎在分片边界上（含 ESC 与 `]` 分开）。
        let out = feed_all(
            &mut sniffer,
            &[b"\x1b", b"]1", b"33;D;", b"1", b"30\x1b", b"\\"],
        );
        assert_eq!(out, vec![Marker::CommandFinished(130)]);
        // ST 终结的 OSC 7 同样识别。
        let out = feed_all(&mut sniffer, &[b"\x1b]7;file://h/tmp/a%20b\x1b\\"]);
        assert_eq!(out, vec![Marker::Cwd("/tmp/a b".into())]);
    }

    #[test]
    fn 无关osc与畸形数据不影响() {
        let mut sniffer = OscSniffer::new();
        // SetUserVar / 标题 / 未知 133 子类型：全部安静忽略。
        let out = feed_all(
            &mut sniffer,
            &[
                b"\x1b]1337;SetUserVar=X\x07",
                b"\x1b]0;title\x07",
                b"\x1b]133;P;k=i\x07",
                b"\x1b]133;Q\x07",
            ],
        );
        assert!(out.is_empty());
        // 超长载荷被放弃，后续标记仍能解析。
        let mut long = Vec::new();
        long.push(0x1b);
        long.push(b']');
        long.extend(std::iter::repeat_n(b'x', MAX_OSC_PAYLOAD + 10));
        long.push(0x07);
        long.extend_from_slice(b"\x1b]133;A\x07");
        let out = feed_all(&mut sniffer, &[&long]);
        assert_eq!(out, vec![Marker::PromptReady]);
    }

    #[test]
    fn 普通控制序列不被误吞() {
        let mut sniffer = OscSniffer::new();
        // CSI 序列 + 普通文本：不产生标记、也不影响后续解析。
        let out = feed_all(
            &mut sniffer,
            &[
                b"\x1b[31mred\x1b[0m",
                b"\x1b]8;;https://example.com\x07link\x1b]8;;\x07",
                b"\x1b]133;D;0\x07",
            ],
        );
        assert_eq!(out, vec![Marker::CommandFinished(0)]);
    }
}
