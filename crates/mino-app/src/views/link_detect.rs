//! 链接识别：从终端一行的可见文本里扫出可点击片段（URL / 裸域名 / 文件路径 / IPv4:port）。
//!
//! 手写线性扫描器，不引入正则：终端每次重绘要扫几十行，手写状态机没有正则自动机的
//! 构建开销，单行 ≤ 1000 字符也就是几百次常数级判定。
//!
//! 判定原则是「宁可漏报，不误报」——点错的代价是真的去开浏览器或文件管理器，
//! 比少点亮几个链接糟得多，所以：
//! - 裸域名要求末级标签命中白名单，且**大小写敏感**（`System.Net` 是 C# 命名空间，
//!   不是域名；`foo.bar`、`main.rs` 这类高冲突标签也不在白名单里）；
//! - 候选紧接着 `(` 按方法调用排除（`df.info()`）；
//! - 没有 `/`、`./`、`../`、`~/` 前缀的 `a/b`、`2024/01/02` 不算路径；
//! - 端点只落在 ASCII 可见字符边界：中文/emoji 与其标点（`。，」`）天然截断候选，
//!   `https://a.com。` 得到的是 `https://a.com`。
//!
//! 与 OSC8 的分工：OSC8 超链接的目标字符串由渲染层直接携带，本模块只处理**裸文本**。
//! `detect_links` 是纯函数（不读环境、不碰 UI）；依赖环境的规整（`~/` 展开）放在
//! 点击时的 `normalize_target` 里。
//!
//! 接线方式：渲染时 `detect_links(line)` 拿片段，`LinkSpan::url` 已可直接打开；
//! 点击时若要知道这份文本在哪个会话/工作目录里解析，再对
//! `&line[span.start..span.end]` 调一次 `normalize_target(raw, cwd)`（幂等）。

/// 行内可点击片段（字节区间 `[start, end)` + 解析后的目标）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkSpan {
    /// 起始字节（含），必定落在字符边界。
    pub start: usize,
    /// 结束字节（不含），必定落在字符边界。
    pub end: usize,
    /// 目标字符串：裸域名/`IPv4:端口` 已补 `https://`，其余与命中的原文一致。
    /// `~/` 的展开要读环境，留给点击时的 `normalize_target` 处理。
    pub url: String,
}

/// 带 scheme 的 URL 前缀（大小写不敏感匹配）。
const SCHEMES: &[&str] = &["https://", "http://", "ftp://", "file://", "mailto:"];

/// 裸域名白名单末级标签。
///
/// 只收日常真会点到、且不易与代码标识符撞车的标签：`.rs`、`.bar`、`.md` 这类
/// 冲突高的不收，`main.rs`、`README.md` 之类的文件名就不会被当成域名点亮。
/// 匹配大小写敏感（见 `is_known_tld`）。
const TLDS: &[&str] = &[
    // 通用
    "com", "net", "org", "info", "biz", "pro", "name", "mobi", "int", "edu", "gov", "mil",
    // 国家/地区
    "cn", "us", "uk", "de", "jp", "fr", "ru", "in", "it", "nl", "pl", "ca", "au", "ch", "se", "no",
    "dk", "fi", "es", "pt", "gr", "cz", "kr", "tw", "hk", "sg", "my", "id", "ph", "th", "vn", "br",
    "mx", "ar", "za", "ie", "nz", "at", "be", "il", "tr", "ua", "ae", "sa", "co",
    // 技术向与常用新顶级域
    "io", "dev", "app", "ai", "sh", "tv", "cc", "xyz", "cloud", "host", "tech", "software", "codes",
    "systems", "network", "zone", "digital", "design", "page", "site", "website", "online",
    "store", "space", "link", "email", "press", "wiki", "news", "blog", "media", "live", "life",
    "world", "today", "group", "work", "art", "ink", "love", "club", "vip", "icu", "top", "fun",
    "fit", "asia", "travel", "jobs", "tel", "aero", "coop", "museum",
];

/// 在一行（单行、无换行）文本里识别可点击片段，按 `start` 升序且互不重叠。
pub fn detect_links(text: &str) -> Vec<LinkSpan> {
    let bytes = text.as_bytes();
    let mut spans = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        match scan_at(text, i) {
            // 命中后直接跳到片段末尾：既不重叠，也不会在 URL 内部再扫一遍。
            Some(end) => {
                spans.push(LinkSpan {
                    start: i,
                    end,
                    url: to_target(&text[i..end]),
                });
                i = end;
            }
            None => i += next_char_len(text, i),
        }
    }
    spans
}

/// 把命中的原始文本规整成可打开的目标（供 `open::that` 类调用）。
///
/// - 带 scheme（`https://`、`mailto:`、`ssh://`…）与文件路径（`/`、`./`、`../` 开头）原样返回
///   （路径保留 `:行[:列]` 后缀，交给调用方决定怎么用）；
/// - 裸域名 / `IPv4:端口` 补 `https://`；
/// - 只有未传 `cwd` 时才用环境变量 `HOME` 展开 `~/`：传了 `cwd` 说明调用方清楚这份文本
///   在哪个上下文里解析（本地某个工作目录或远端会话），拿本地 HOME 展开会指到完全
///   错误的目录，所以原样返回交给调用方。
pub fn normalize_target(raw: &str, cwd: Option<&str>) -> String {
    if cwd.is_none() && raw.starts_with("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return format!("{}{}", home.to_string_lossy(), &raw[1..]);
        }
    }
    to_target(raw)
}

/// 在 `start`（字符边界）处尝试各类候选，命中则返回终点（不含）。
///
/// 顺序即优先级：带 scheme 的 URL → 文件路径 → 裸域名 → `IPv4:端口`。
/// 每条规则都做词首判定，所以 `a/b` 只会在 `a` 处失败，不会从 `/` 处误判成路径。
fn scan_at(text: &str, start: usize) -> Option<usize> {
    let bytes = text.as_bytes();
    if let Some(end) = scan_scheme_url(text, start) {
        return Some(end);
    }
    if let Some(end) = scan_path(text, start) {
        return Some(end);
    }
    if bytes[start].is_ascii_alphanumeric() {
        return scan_bare_domain(text, start).or_else(|| scan_ipv4_port(text, start));
    }
    None
}

/// 带 scheme 的 URL：正文吃到终止符为止，再剥尾随标点。
fn scan_scheme_url(text: &str, start: usize) -> Option<usize> {
    let bytes = text.as_bytes();
    if starts_mid_token(bytes, start) {
        return None;
    }
    let scheme_len = SCHEMES
        .iter()
        .find(|scheme| starts_with_ignore_case(&bytes[start..], scheme.as_bytes()))?
        .len();
    let body_start = start + scheme_len;
    let end = strip_trailing(text, start, scan_to_terminator(bytes, body_start));
    // `http://`、`mailto:` 这种光杆没有目标，不当链接。
    (end > body_start).then_some(end)
}

/// 文件路径：以 `/`、`./`、`../`、`~/` 开头、不含空白，允许 `:行[:列]` 后缀
/// （`/Users/a/b.rs:12:3`——编辑器跳转靠它，原样保留交给调用方解析）。
fn scan_path(text: &str, start: usize) -> Option<usize> {
    let bytes = text.as_bytes();
    if starts_mid_token(bytes, start) {
        return None;
    }
    let prefix_len = path_prefix_len(&bytes[start..])?;
    let end = strip_trailing(text, start, scan_to_terminator(bytes, start));
    // 光有前缀（`/`、`./`）或只有斜杠（`//`）没有可打开的东西，不算路径。
    let body = text.get(start + prefix_len..end)?;
    if body.is_empty() || body.bytes().all(|b| b == b'/') {
        return None;
    }
    Some(end)
}

/// 裸域名：`[A-Za-z0-9-]+(\.[A-Za-z0-9-]+)+`，末级标签命中白名单或整体以 `www.` 起头，
/// 后面可跟 `:端口` 与 `/路径?查询#片段`。
fn scan_bare_domain(text: &str, start: usize) -> Option<usize> {
    let bytes = text.as_bytes();
    if starts_mid_token(bytes, start) {
        return None;
    }
    let mut i = start;
    let mut labels = 0usize;
    let mut last_label = start..start;
    loop {
        let label_start = i;
        while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'-') {
            i += 1;
        }
        if i == label_start {
            return None;
        }
        labels += 1;
        last_label = label_start..i;
        // 点后面还得跟着下一个标签的字符才算分隔符：句末的 `.` 不属于域名。
        if bytes.get(i) != Some(&b'.')
            || !matches!(bytes.get(i + 1), Some(b) if b.is_ascii_alphanumeric() || *b == b'-')
        {
            break;
        }
        i += 1;
    }
    if labels < 2 {
        return None;
    }
    // `df.info()`：紧随其后的 `(` 说明是方法调用，一律不点亮。
    if bytes.get(i) == Some(&b'(') {
        return None;
    }
    let host_end = i;
    let www = starts_with_ignore_case(&bytes[start..host_end], b"www.");
    if !www && !is_known_tld(&text[last_label]) {
        return None;
    }
    Some(scan_host_tail(text, host_end))
}

/// 主机后面可选的部分：`:端口` 与 `/路径?查询#片段`。
fn scan_host_tail(text: &str, host_end: usize) -> usize {
    let bytes = text.as_bytes();
    let mut end = host_end;
    if bytes.get(end) == Some(&b':') {
        if let Some(port_len) = scan_port(text, end + 1) {
            end += 1 + port_len;
        }
    }
    if bytes.get(end) == Some(&b'/') {
        end = strip_trailing(text, host_end, scan_to_terminator(bytes, end));
    }
    end
}

/// 端口长度（字节数）：至少一位数字、≤ 65535，且后面不粘标识符字符。
///
/// 粘着字符（`example.com:8080x`）说明整段不是完整 token，宁可不认这个端口。
fn scan_port(text: &str, from: usize) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut i = from;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    if i == from || text[from..i].parse::<u32>().ok()? > 65535 {
        return None;
    }
    if matches!(bytes.get(i), Some(b) if b.is_ascii_alphanumeric()) {
        return None;
    }
    Some(i - from)
}

/// `192.168.1.1:8080`：四段 0-255 的数字加端口。
///
/// 裸 IP 不点亮（版本号 `1.2.3.4` 这类四段数字太像），必须带端口；
/// 出现在 URL/路径里的 IP 已被前面的候选整段吃掉，不会重复命中。
fn scan_ipv4_port(text: &str, start: usize) -> Option<usize> {
    let bytes = text.as_bytes();
    if starts_mid_token(bytes, start) {
        return None;
    }
    let mut i = start;
    for segment in 0..4 {
        let seg_start = i;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
        let digits = i - seg_start;
        if digits == 0 || digits > 3 || text[seg_start..i].parse::<u16>().ok()? > 255 {
            return None;
        }
        if segment < 3 {
            if bytes.get(i) != Some(&b'.') {
                return None;
            }
            i += 1;
        }
    }
    if bytes.get(i) != Some(&b':') {
        return None;
    }
    let port_len = scan_port(text, i + 1)?;
    Some(i + 1 + port_len)
}

/// 规整成可直接打开的目标（不读环境，`detect_links` 与 `normalize_target` 共用）。
fn to_target(raw: &str) -> String {
    if raw.is_empty() || has_scheme(raw) || path_prefix_len(raw.as_bytes()).is_some() {
        return raw.to_string();
    }
    if scan_bare_domain(raw, 0).is_some() || scan_ipv4_port(raw, 0).is_some() {
        return format!("https://{raw}");
    }
    raw.to_string()
}

/// 是否已带 scheme（`https://`、`mailto:`、`ssh://`…）：带了本身就是可打开目标。
///
/// 单独的 `host:端口`（`example.com:8080`）不算 scheme，否则会被当成「原样」
/// 而漏掉该补的 `https://` 前缀。
fn has_scheme(raw: &str) -> bool {
    let Some(colon) = raw.find(':') else {
        return false;
    };
    let scheme = &raw[..colon];
    let mut chars = scheme.chars();
    if !matches!(chars.next(), Some(c) if c.is_ascii_alphabetic())
        || !chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
    {
        return false;
    }
    raw.as_bytes()[colon + 1..].starts_with(b"//") || scheme.eq_ignore_ascii_case("mailto")
}

/// 路径前缀长度（`../` 与 `./` 顺序不能换：先长后短）。
fn path_prefix_len(bytes: &[u8]) -> Option<usize> {
    if bytes.starts_with(b"../") {
        Some(3)
    } else if bytes.starts_with(b"./") || bytes.starts_with(b"~/") {
        Some(2)
    } else if bytes.starts_with(b"/") {
        Some(1)
    } else {
        None
    }
}

/// 末级标签是否在白名单里。
///
/// **大小写敏感**：`System.Net` 是命名空间而不是域名，`Net` 不该命中 `net`；
/// 真域名里全小写是绝对主流，剩下的大写域名宁可漏报。
fn is_known_tld(label: &str) -> bool {
    TLDS.contains(&label)
}

/// 候选起点前一个字节是不是「词的一部分」，是则不起候选。
///
/// `a/b` 的 `/`、`foo.com` 里的 `.`、`--flag=/x` 里的 `-` 都靠这条挡住，
/// 也就是说候选必须是 token 的开头，不会把长标识符的尾巴当成链接。
fn starts_mid_token(bytes: &[u8], start: usize) -> bool {
    matches!(
        bytes.get(start.wrapping_sub(1)),
        Some(b) if b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-' | b'~' | b'/')
    )
}

/// 目标终止字节：空白、引号/尖括号/反引号、控制符（含 CSI 的 ESC）、以及全部非 ASCII
/// 字节（中文、emoji 与其标点）。非 ASCII 一律截断，端点因此天然落在字符边界上。
fn is_target_terminator(byte: u8) -> bool {
    byte < 0x21 || byte == 0x7f || byte >= 0x80 || matches!(byte, b'"' | b'\'' | b'`' | b'<' | b'>')
}

/// 从 `from` 吃到第一个终止符。
fn scan_to_terminator(bytes: &[u8], from: usize) -> usize {
    let mut end = from;
    while end < bytes.len() && !is_target_terminator(bytes[end]) {
        end += 1;
    }
    end
}

/// 剥掉尾随标点：`,.;:!?|` 直接剥；`)`/`]`/`}` 只在候选内没有对应开括号时剥，
/// 于是 `https://en.wikipedia.org/wiki/Foo_(bar)` 与 `(https://a.com)` 都对。
fn strip_trailing(text: &str, start: usize, mut end: usize) -> usize {
    let bytes = text.as_bytes();
    while end > start {
        let last = bytes[end - 1];
        match last {
            b',' | b'.' | b';' | b':' | b'!' | b'?' | b'|' => {}
            b')' | b']' | b'}' => {
                let (open, close) = match last {
                    b')' => (b'(', b')'),
                    b']' => (b'[', b']'),
                    _ => (b'{', b'}'),
                };
                let opens = bytes[start..end].iter().filter(|&&b| b == open).count();
                let closes = bytes[start..end].iter().filter(|&&b| b == close).count();
                if closes <= opens {
                    break;
                }
            }
            _ => break,
        }
        end -= 1;
    }
    end
}

/// ASCII 大小写不敏感的前缀判断。
fn starts_with_ignore_case(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.len() >= needle.len() && haystack[..needle.len()].eq_ignore_ascii_case(needle)
}

/// 下一个字符的长度：扫描器按字符步进，保证起点始终落在字符边界。
fn next_char_len(text: &str, i: usize) -> usize {
    text[i..].chars().next().map_or(1, |c| c.len_utf8())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 检测结果 → 命中原文（按 span 切回原文，顺带验证区间自洽）。
    fn hits(text: &str) -> Vec<&str> {
        detect_links(text)
            .into_iter()
            .map(|span| &text[span.start..span.end])
            .collect()
    }

    /// 单条文本只应命中一个片段，返回它。
    fn one(text: &str) -> LinkSpan {
        let spans = detect_links(text);
        assert_eq!(spans.len(), 1, "应恰好命中一个片段：{text:?} → {spans:?}");
        spans.into_iter().next().unwrap()
    }

    #[test]
    fn 识别带协议头的链接并剥尾随标点() {
        let text = "访问 https://example.com/a，另见 http://foo.dev/b.";
        assert_eq!(
            hits(text),
            vec!["https://example.com/a", "http://foo.dev/b"]
        );

        assert_eq!(
            hits("mailto:support@example.com"),
            vec!["mailto:support@example.com"]
        );
        assert_eq!(
            hits("ftp://ftp.example.org/pub/file.tar.gz"),
            vec!["ftp://ftp.example.org/pub/file.tar.gz"]
        );
        assert_eq!(hits("file:///Users/a/b.rs"), vec!["file:///Users/a/b.rs"]);
        assert_eq!(
            hits("https://a.com/path?x=1&y=2#frag"),
            vec!["https://a.com/path?x=1&y=2#frag"]
        );
        assert_eq!(hits("见 https://a.com。"), vec!["https://a.com"]);
        assert_eq!(hits("https://a.com,"), vec!["https://a.com"]);
        assert_eq!(hits("https://a.com;"), vec!["https://a.com"]);
        assert_eq!(hits("URL: https://a.com:"), vec!["https://a.com"]);
        assert_eq!(hits("真的吗 https://a.com?!"), vec!["https://a.com"]);
        // 光杆 scheme 没有目标，不算链接。
        assert!(detect_links("http://").is_empty());
        assert!(detect_links("mailto:").is_empty());
    }

    #[test]
    fn 括号成对保留不成对剥掉() {
        assert_eq!(
            hits("(https://example.com/x)"),
            vec!["https://example.com/x"]
        );
        assert_eq!(
            hits("https://en.wikipedia.org/wiki/Foo_(bar)"),
            vec!["https://en.wikipedia.org/wiki/Foo_(bar)"]
        );
        assert_eq!(hits("https://a.com/b))"), vec!["https://a.com/b"]);
        assert_eq!(hits("[https://a.com/x]"), vec!["https://a.com/x"]);
        assert_eq!(hits("{https://a.com/x}"), vec!["https://a.com/x"]);
    }

    #[test]
    fn 裸域名白名单与负例() {
        assert_eq!(hits("example.com"), vec!["example.com"]);
        assert_eq!(hits("github.io"), vec!["github.io"]);
        assert_eq!(hits("foo.dev"), vec!["foo.dev"]);
        assert_eq!(hits("请访问 example.com 吧"), vec!["example.com"]);

        // 非白名单 TLD、命名空间、方法调用、常见文件名都不点亮。
        assert!(detect_links("foo.bar").is_empty());
        assert!(detect_links("System.Net").is_empty());
        assert!(detect_links("df.info()").is_empty());
        assert!(detect_links("main.rs").is_empty());
        assert!(detect_links("README.md").is_empty());
        assert!(detect_links("let x = a.b.c();").is_empty());
        assert!(detect_links("v1.2.3").is_empty());
        // 词首判定：标识符（下划线隔开）里的域名尾巴不点亮。
        assert!(detect_links("foo_bar.example.com").is_empty());
    }

    #[test]
    fn 裸域名可带www前缀路径与端口() {
        // `bar` 不在白名单，但 `www.` 前缀本身就算链接。
        assert_eq!(hits("www.foo.bar"), vec!["www.foo.bar"]);
        assert_eq!(
            hits("example.com/a/b?q=1#frag"),
            vec!["example.com/a/b?q=1#frag"]
        );
        assert_eq!(hits("example.com:8080"), vec!["example.com:8080"]);
        assert_eq!(hits("(example.com/a)"), vec!["example.com/a"]);
        // 句末的点属于句子，不属于域名。
        assert_eq!(hits("看 example.com."), vec!["example.com"]);
        // 端口后粘着标识符字符：只留主机，不把 `:8080x` 当端口吞进来。
        assert_eq!(hits("example.com:8080x"), vec!["example.com"]);
        // 端口越界（> 65535）也不是端口。
        assert_eq!(hits("example.com:99999"), vec!["example.com"]);
    }

    #[test]
    fn 文件路径() {
        assert_eq!(hits("/Users/a/b.rs"), vec!["/Users/a/b.rs"]);
        assert_eq!(hits("~/src/main.rs"), vec!["~/src/main.rs"]);
        assert_eq!(hits("./build.rs"), vec!["./build.rs"]);
        assert_eq!(hits("../x/y.txt"), vec!["../x/y.txt"]);
        assert_eq!(hits("/Users/a/b.rs:12:3"), vec!["/Users/a/b.rs:12:3"]);
        assert_eq!(hits("~/src/main.rs:12:3"), vec!["~/src/main.rs:12:3"]);
        assert_eq!(hits("打开 /tmp/x.txt."), vec!["/tmp/x.txt"]);
        assert_eq!(hits("见 /tmp/a.rs。"), vec!["/tmp/a.rs"]);
        assert_eq!(hits("--config=/etc/foo"), vec!["/etc/foo"]);
        assert_eq!(hits("(/Users/a)"), vec!["/Users/a"]);
        // 没有前缀的相对路径不算路径。
        assert!(detect_links("a/b").is_empty());
        assert!(detect_links("src/main.rs").is_empty());
        assert!(detect_links("2024/01/02").is_empty());
        // 只有斜杠没有目标。
        assert!(detect_links("/").is_empty());
        assert!(detect_links("a / b").is_empty());
        assert!(detect_links("//").is_empty());
    }

    #[test]
    fn ipv4端口() {
        assert_eq!(hits("192.168.1.1:8080"), vec!["192.168.1.1:8080"]);
        assert_eq!(hits("连 10.0.0.1:22 试试"), vec!["10.0.0.1:22"]);
        // 裸 IP 不点亮（与版本号难以区分），非法八位组/端口不点亮。
        assert!(detect_links("192.168.1.1").is_empty());
        assert!(detect_links("999.1.1.1:80").is_empty());
        assert!(detect_links("10.0.0.1:99999").is_empty());
        // URL / 路径内部的 IP 由整段候选吃掉，不重复命中。
        assert_eq!(
            hits("http://192.168.1.1:8080/admin 和 /tmp/192.168.1.1:8080"),
            vec!["http://192.168.1.1:8080/admin", "/tmp/192.168.1.1:8080"]
        );
    }

    #[test]
    fn 同一行多链接升序不重叠() {
        let text = "看 https://a.com/x、example.org/y 和 /tmp/z.rs:3 以及 10.0.0.1:22";
        let spans = detect_links(text);
        assert_eq!(
            spans
                .iter()
                .map(|s| &text[s.start..s.end])
                .collect::<Vec<_>>(),
            vec![
                "https://a.com/x",
                "example.org/y",
                "/tmp/z.rs:3",
                "10.0.0.1:22"
            ]
        );
        for pair in spans.windows(2) {
            assert!(pair[0].start < pair[0].end);
            assert!(
                pair[0].end <= pair[1].start,
                "片段必须升序且不重叠：{spans:?}"
            );
        }
    }

    #[test]
    fn 端点落在字符边界() {
        let text = "「https://example.com/path」中文，👀 还有 example.com。";
        let spans = detect_links(text);
        assert_eq!(
            spans
                .iter()
                .map(|s| &text[s.start..s.end])
                .collect::<Vec<_>>(),
            vec!["https://example.com/path", "example.com"]
        );
        for span in &spans {
            assert!(text.is_char_boundary(span.start) && text.is_char_boundary(span.end));
            // 片段内不含非 ASCII：中文标点不吞进去，端点自然合法。
            assert!(text[span.start..span.end].is_ascii());
        }
    }

    #[test]
    fn 空文本与控制字符() {
        assert!(detect_links("").is_empty());
        assert!(detect_links("   ").is_empty());
        // ESC（CSI 起始）与 CR 都是终止符。
        assert_eq!(hits("见 https://a.com\u{1b}[0m"), vec!["https://a.com"]);
        assert_eq!(hits("https://a.com\r"), vec!["https://a.com"]);
    }

    #[test]
    fn 片段url为规整后目标() {
        let text = "example.com 和 /tmp/a.rs";
        let spans = detect_links(text);
        assert_eq!(spans.len(), 2);
        // 区间切回原文得到命中文本，`url` 已是可直接打开的目标。
        assert_eq!(&text[spans[0].start..spans[0].end], "example.com");
        assert_eq!(spans[0].url, "https://example.com");
        assert_eq!(&text[spans[1].start..spans[1].end], "/tmp/a.rs");
        assert_eq!(spans[1].url, "/tmp/a.rs");
    }

    #[test]
    fn 归一化裸域名补协议头() {
        assert_eq!(normalize_target("example.com", None), "https://example.com");
        assert_eq!(
            normalize_target("example.com/a?b=1#c", None),
            "https://example.com/a?b=1#c"
        );
        assert_eq!(
            normalize_target("example.com:8080", None),
            "https://example.com:8080"
        );
        assert_eq!(
            normalize_target("192.168.1.1:8080", None),
            "https://192.168.1.1:8080"
        );
        // 已带 scheme 的原样（含未知 scheme），非白名单 TLD 文本也原样。
        assert_eq!(
            normalize_target("https://example.com", None),
            "https://example.com"
        );
        assert_eq!(normalize_target("mailto:a@b.com", None), "mailto:a@b.com");
        assert_eq!(
            normalize_target("ssh://git@host/repo", None),
            "ssh://git@host/repo"
        );
        assert_eq!(normalize_target("foo.bar", None), "foo.bar");
        assert_eq!(normalize_target("", None), "");
    }

    #[test]
    fn 归一化路径原样() {
        assert_eq!(
            normalize_target("/Users/a/b.rs:12:3", Some("/tmp")),
            "/Users/a/b.rs:12:3"
        );
        assert_eq!(normalize_target("./a.rs", None), "./a.rs");
        assert_eq!(normalize_target("../a.rs", None), "../a.rs");
        assert_eq!(normalize_target("./a.rs", Some("/tmp")), "./a.rs");
    }

    #[test]
    fn 波浪号仅在未传cwd时展开家目录() {
        let home = std::env::var("HOME").expect("测试环境应有 HOME");
        assert_eq!(normalize_target("~/a.rs", None), format!("{home}/a.rs"));
        assert_eq!(
            normalize_target("~/a.rs:12:3", None),
            format!("{home}/a.rs:12:3")
        );
        // 传了 cwd：交给调用方在正确上下文里解析，本地 HOME 不参与。
        assert_eq!(normalize_target("~/a.rs", Some("/tmp")), "~/a.rs");
    }

    #[test]
    fn 单条文本只命中一个片段() {
        assert_eq!(one("https://a.com/x").url, "https://a.com/x");
        assert_eq!(one("（详见 https://a.com/x）").url, "https://a.com/x");
    }
}
