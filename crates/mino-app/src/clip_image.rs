//! 剪贴板图片 → `omp @路径` 桥接（Path-Bridge）。
//!
//! 终端与 shell 之间是 PTY 字节管道，只能传文本；`omp` 吃图方式是
//! `@/path/to.png` 这类文件路径。剪贴板里的截图只有像素、没有路径，
//! 因此图片粘贴必须先落盘、再向 PTY 送 `@转义路径` token。
//!
//! 优先级（`paste_image_token`）：
//!
//! 1. `arboard::get_text()` 有文本 → 文本优先（拼写纠错后的复制等），忽略图片；
//! 2. macOS `NSPasteboard NSPasteboardTypeFileURL` 有文件 → 直接用原路径（不复制字节）；
//! 3. `arboard::get_image()` 有像素 → 编码 PNG 落盘到 `paste_dir()`。
//!
//! spike 实测（2026-09-11，macOS）：
//!
//! - 截图：`get_text` 为空、`get_image` 命中（921x386）；
//! - Finder 复制文件：`get_text` 为文件名、`get_image` 误命中预览图（1024x1024）。
//!
//! 因此 macOS 文件分支必须在图片分支之前，且不能以 `get_text` 是否为空判断。

use std::path::{Path, PathBuf};

/// 剪贴板图片源 RGBA 上限（`w*h*4`，约 20MiB；超限直接拒绝，引导存文件后拖入）。
pub const MAX_IMAGE_RGBA_BYTES: usize = 20 * 1024 * 1024;

/// 图片粘贴落盘目录（`OS` 临时目录下，不污染仓库与配置）。
pub fn paste_dir() -> PathBuf {
    std::env::temp_dir().join("mino-paste")
}

/// 图片粘贴结果：落盘路径或沿用原文件路径。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PastedImage {
    /// 写入 PTY 的 token（`@` + shell 转义路径）。
    pub token: String,
    /// 本次是否新落盘（远程上传用：新落盘才需经 SFTP 上传）。
    pub staged: Option<PathBuf>,
}

/// 剪贴板读取抽象（正式走 `arboard` + `NSPasteboard`，测试注入 `Fake`）。
pub trait ClipboardReader {
    /// 剪贴板文本（`arboard::get_text` 语义；失败/无内容返回 `None`）。
    fn clipboard_text(&mut self) -> Option<String>;
    /// 写入剪贴板文本（OSC52 程序复制用；失败返回 `Err`，调用方 toast）。
    fn set_clipboard_text(&mut self, text: &str) -> Result<(), String>;
    /// macOS 文件路径（`NSFilenamesPboardType`；非 macOS 或无文件返回空）。
    fn clipboard_file_paths(&self) -> Vec<PathBuf>;
    /// 剪贴板像素（`arboard::get_image` 语义；`width/height/RGBA`）。
    fn clipboard_image(&mut self) -> Option<(usize, usize, Vec<u8>)>;
}

/// 真实剪贴板（`arboard` 像素 + `NSPasteboard` 文件路径）。
pub struct SystemClipboard {
    clipboard: Option<arboard::Clipboard>,
}

impl SystemClipboard {
    pub fn new() -> Self {
        Self {
            clipboard: arboard::Clipboard::new().ok(),
        }
    }
}

impl Default for SystemClipboard {
    fn default() -> Self {
        Self::new()
    }
}

impl ClipboardReader for SystemClipboard {
    fn clipboard_text(&mut self) -> Option<String> {
        let text = self.clipboard.as_mut()?.get_text().ok()?;
        (!text.is_empty()).then_some(text)
    }

    fn set_clipboard_text(&mut self, text: &str) -> Result<(), String> {
        self.clipboard
            .as_mut()
            .ok_or_else(|| "剪贴板不可用".to_string())?
            .set_text(text)
            .map_err(|e| format!("写入剪贴板失败：{e}"))
    }

    #[cfg(target_os = "macos")]
    fn clipboard_file_paths(&self) -> Vec<PathBuf> {
        macos_file_paths()
    }

    #[cfg(not(target_os = "macos"))]
    fn clipboard_file_paths(&self) -> Vec<PathBuf> {
        Vec::new()
    }

    fn clipboard_image(&mut self) -> Option<(usize, usize, Vec<u8>)> {
        let image = self.clipboard.as_mut()?.get_image().ok()?;
        let (width, height) = (image.width, image.height);
        let bytes: Vec<u8> = image.into_owned_bytes().into_owned();
        (!bytes.is_empty()).then_some((width, height, bytes))
    }
}

/// macOS 剪贴板文件路径（无文件/失败返回空）。
///
/// 同时读两路（spike 实测 2026-09-11）：
/// - `NSPasteboardTypeFileURL`：现代 API，但 Finder 给的是
///   `file:///.file/id=...` 这类 file-id URL，需经 `NSURL.path` 解析；
/// - `NSFilenamesPboardType`：已废弃但直接给路径字符串，解析失败时兜底。
#[cfg(target_os = "macos")]
fn macos_file_paths() -> Vec<PathBuf> {
    use objc2::rc::autoreleasepool;
    use objc2_app_kit::{NSPasteboard, NSPasteboardTypeFileURL};

    // SAFETY: AppKit 提供的 extern static，只读访问。
    let file_url_type = unsafe { NSPasteboardTypeFileURL };
    let pb = NSPasteboard::generalPasteboard();
    let Some(items) = pb.pasteboardItems() else {
        return Vec::new();
    };
    let mut paths = Vec::new();
    autoreleasepool(|pool| {
        for item in items.iter() {
            let Some(url_string) = item.stringForType(file_url_type) else {
                continue;
            };
            // SAFETY: 字符串在池内复制为 owned，不逃逸出池。
            let url_text = unsafe { url_string.to_str(pool) }.to_owned();
            if let Some(path) = resolve_file_url(&url_text) {
                paths.push(path);
            }
        }
    });
    if !paths.is_empty() {
        return paths;
    }
    macos_filenames_fallback()
}

/// `file://` URL（含 file-id 形式）→ 本地路径。
#[cfg(target_os = "macos")]
fn resolve_file_url(url_text: &str) -> Option<PathBuf> {
    use objc2::rc::autoreleasepool;
    use objc2_foundation::{NSString, NSURL};

    // 普通路径（`file:///tmp/a%20b.png`）先走轻量解析，避免 NSURL 开销。
    if !url_text.contains("/.file/id=") {
        return url_text
            .strip_prefix("file://")
            .map(|rest| {
                let path_part = rest.split('?').next().unwrap_or(rest);
                PathBuf::from(percent_decode(path_part))
            })
            .filter(|path| !path.as_os_str().is_empty());
    }
    // file-id 形式必须经 NSURL 解析（直接 strip 得到的是无意义的 id 路径）。
    autoreleasepool(|pool| {
        let ns = NSString::from_str(url_text);
        let url = NSURL::URLWithString(&ns)?;
        let path = url.path()?;
        // SAFETY: 路径在池内复制为 owned，不逃逸出池。
        let text = unsafe { path.to_str(pool) }.to_owned();
        (!text.is_empty()).then(|| PathBuf::from(text))
    })
}

/// 老 API 兜底：`NSFilenamesPboardType` 直接给路径字符串数组。
#[cfg(target_os = "macos")]
#[allow(deprecated)]
fn macos_filenames_fallback() -> Vec<PathBuf> {
    use objc2::rc::{autoreleasepool, Retained};
    use objc2_app_kit::{NSFilenamesPboardType, NSPasteboard};
    use objc2_foundation::{NSArray, NSString};

    // SAFETY: AppKit 提供的 extern static，只读访问。
    let filenames_type = unsafe { NSFilenamesPboardType };
    let pb = NSPasteboard::generalPasteboard();
    let Some(obj) = pb.propertyListForType(filenames_type) else {
        return Vec::new();
    };
    // propertyList 是 `NSArray<NSString>`；元素即文件路径。
    let ptr = Retained::as_ptr(&obj) as *const NSArray<NSString>;
    // SAFETY: 类型来自系统 propertyList（探针已验证为路径字符串数组）。
    let arr: &NSArray<NSString> = unsafe { &*ptr };
    autoreleasepool(|pool| {
        arr.iter()
            // SAFETY: 路径字符串在池内复制为 owned，不逃逸出池。
            .map(|item| PathBuf::from(unsafe { item.to_str(pool) }.to_owned()))
            .filter(|path| !path.as_os_str().is_empty())
            .collect()
    })
}

/// 百分号解码（`file://` URL → 本地路径；非法序列原样保留）。
///
/// macOS 文件分支用（`resolve_file_url`）；非 macOS 构建无调用者，
/// 用 `cfg_attr` 压住 `-D warnings` 的 `dead_code`（CI 在 Linux 跑 clippy）。
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn percent_decode(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    let bytes = input.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(high), Some(low)) = (hex_nibble(bytes[i + 1]), hex_nibble(bytes[i + 2])) {
                output.push((high << 4 | low) as char);
                i += 3;
                continue;
            }
        }
        output.push(bytes[i] as char);
        i += 1;
    }
    output
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// RGBA 像素编码为 PNG 字节（`image 0.25`；尺寸为 0 或长度不匹配返回 `Err`）。
pub fn encode_png(width: usize, height: usize, rgba: &[u8]) -> Result<Vec<u8>, String> {
    if width == 0 || height == 0 {
        return Err("图片尺寸为 0".to_string());
    }
    if rgba.len() != width.saturating_mul(height).saturating_mul(4) {
        return Err(format!(
            "像素长度不匹配：{}x{} 需要 {} 字节，实际 {} 字节",
            width,
            height,
            width.saturating_mul(height).saturating_mul(4),
            rgba.len()
        ));
    }
    let img = image::RgbaImage::from_raw(width as u32, height as u32, rgba.to_vec())
        .ok_or_else(|| "构造图片失败".to_string())?;
    let mut bytes = Vec::new();
    image::ImageEncoder::write_image(
        image::codecs::png::PngEncoder::new(&mut bytes),
        img.as_raw(),
        width as u32,
        height as u32,
        image::ExtendedColorType::Rgba8,
    )
    .map_err(|e| format!("PNG 编码失败：{e}"))?;
    Ok(bytes)
}

/// 按 PNG 字节构造落盘文件名（`mino-{毫秒}-{sha短8}.png`；去重：同字节同名）。
pub fn staged_file_name(png: &[u8]) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut hasher = DefaultHasher::new();
    png.hash(&mut hasher);
    let digest = hasher.finish();
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    format!("mino-{millis}-{digest:08x}.png")
}

/// PNG 字节原子落盘（先写 `tmp-<pid>` 再 `rename`；`unix` 下 `0600`）。
pub fn stage_png_atomic(png: &[u8]) -> Result<PathBuf, String> {
    let dir = paste_dir();
    std::fs::create_dir_all(&dir).map_err(|e| format!("创建粘贴目录失败：{e}"))?;
    let path = dir.join(staged_file_name(png));
    if path.exists() {
        return Ok(path);
    }
    let tmp = dir.join(format!(
        "{}.tmp-{}",
        path.file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "mino.png".to_string()),
        std::process::id()
    ));
    let result = (|| {
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&tmp)?;
            use std::io::Write;
            file.write_all(png)?;
            file.sync_all()?;
        }
        #[cfg(not(unix))]
        {
            use std::io::Write;
            let mut file = std::fs::File::create(&tmp)?;
            file.write_all(png)?;
            file.sync_all()?;
        }
        std::fs::rename(&tmp, &path)?;
        Ok::<_, std::io::Error>(())
    })();
    if let Err(e) = result {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("图片落盘失败：{e}"));
    }
    Ok(path)
}

/// 路径转 `omp` 图片 token（`@` + shell 转义；转义逻辑在 `terminal_view`）。
pub fn omp_image_token(path: &Path, escape: impl Fn(&Path) -> String) -> String {
    format!("@{}", escape(path))
}

/// 远端路径转 token（`SFTP` 上传完成后 `app.rs` 用；远端无 `Path` 语义，
/// 复用同一套单引号转义：POSIX 远端与本地转义规则一致）。
pub fn shell_escape_for_token(text: &str) -> String {
    if text.chars().all(is_token_char) {
        return text.to_string();
    }
    let mut escaped = String::with_capacity(text.len() + 2);
    escaped.push('\'');
    for character in text.chars() {
        if character == '\'' {
            escaped.push_str("'\\''");
        } else {
            escaped.push(character);
        }
    }
    escaped.push('\'');
    escaped
}

fn is_token_char(character: char) -> bool {
    character.is_alphanumeric()
        || matches!(
            character,
            '/' | '_' | '-' | '.' | '~' | '@' | '%' | '+' | '=' | ':' | ','
        )
}

/// 剪贴板图片粘贴主入口（优先级：文本 > macOS 文件 > 像素落盘）。
///
/// - 有文本 → `Ok(None)`（走文本老路，调用方继续处理 `Paste` 事件）；
/// - 有文件 → `Ok(Some)`（`staged=None`，直接用原路径）；
/// - 有像素 → 编码落盘后 `Ok(Some)`（`staged=Some`）；
/// - 全无 → `Ok(None)`（静默无操作，不 toast）。
pub fn paste_image_token(
    reader: &mut dyn ClipboardReader,
    escape: impl Fn(&Path) -> String,
) -> Result<Option<PastedImage>, String> {
    if reader.clipboard_text().is_some() {
        return Ok(None);
    }
    let files = reader.clipboard_file_paths();
    if let Some(path) = files.first() {
        return Ok(Some(PastedImage {
            token: omp_image_token(path, &escape),
            staged: None,
        }));
    }
    let Some((width, height, rgba)) = reader.clipboard_image() else {
        return Ok(None);
    };
    if rgba.len() > MAX_IMAGE_RGBA_BYTES {
        return Err("剪贴板图片过大，请先存成文件后拖入终端".to_string());
    }
    let png = encode_png(width, height, &rgba)?;
    let path = stage_png_atomic(&png)?;
    Ok(Some(PastedImage {
        token: omp_image_token(&path, &escape),
        staged: Some(path),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeClipboard {
        text: Option<String>,
        files: Vec<PathBuf>,
        image: Option<(usize, usize, Vec<u8>)>,
    }

    impl ClipboardReader for FakeClipboard {
        fn clipboard_text(&mut self) -> Option<String> {
            self.text.clone()
        }

        fn set_clipboard_text(&mut self, text: &str) -> Result<(), String> {
            self.text = Some(text.to_owned());
            Ok(())
        }

        fn clipboard_file_paths(&self) -> Vec<PathBuf> {
            self.files.clone()
        }

        fn clipboard_image(&mut self) -> Option<(usize, usize, Vec<u8>)> {
            self.image.clone()
        }
    }

    fn identity_escape(path: &Path) -> String {
        path.to_string_lossy().into_owned()
    }

    /// 文本优先：有文本时忽略图片（拼写纠错后的复制等）。
    #[test]
    fn 有文本时忽略剪贴板图片() {
        let mut reader = FakeClipboard {
            text: Some("hello".to_string()),
            files: Vec::new(),
            image: Some((1, 1, vec![0u8; 4])),
        };
        assert_eq!(
            paste_image_token(&mut reader, identity_escape).unwrap(),
            None
        );
    }

    /// macOS 文件优先于像素（Finder 复制文件的预览图不能被当成截图落盘）。
    #[test]
    fn 文件路径优先于像素() {
        let mut reader = FakeClipboard {
            text: None,
            files: vec![PathBuf::from("/tmp/a b.png")],
            image: Some((2, 2, vec![0u8; 16])),
        };
        let pasted = paste_image_token(&mut reader, identity_escape)
            .unwrap()
            .expect("应返回文件 token");
        assert_eq!(pasted.token, "@/tmp/a b.png");
        assert_eq!(pasted.staged, None);
    }

    /// 像素落盘：token 带 `@` 且文件真实存在。
    #[test]
    fn 像素落盘生成token() {
        let mut reader = FakeClipboard {
            text: None,
            files: Vec::new(),
            image: Some((2, 1, vec![255u8; 8])),
        };
        let pasted = paste_image_token(&mut reader, identity_escape)
            .unwrap()
            .expect("应返回落盘 token");
        assert!(pasted.token.starts_with("@"));
        let staged = pasted.staged.expect("应为新落盘");
        assert!(staged.exists(), "落盘文件应存在：{}", staged.display());
        assert_eq!(staged.extension().and_then(|e| e.to_str()), Some("png"));
        let _ = std::fs::remove_file(&staged);
    }

    /// 空剪贴板静默无操作。
    #[test]
    fn 空剪贴板返回空() {
        let mut reader = FakeClipboard {
            text: None,
            files: Vec::new(),
            image: None,
        };
        assert_eq!(
            paste_image_token(&mut reader, identity_escape).unwrap(),
            None
        );
    }

    /// 非法像素返回 Err（不落盘、不写 PTY）。
    #[test]
    fn 非法像素返回错误() {
        assert!(encode_png(0, 1, &[]).is_err());
        assert!(encode_png(2, 2, &[0u8; 3]).is_err());
    }

    /// PNG 编码可回读且尺寸一致。
    #[test]
    fn 编码回读一致() {
        let png = encode_png(2, 1, &[255u8; 8]).expect("编码失败");
        let img = image::load_from_memory(&png).expect("回读失败");
        assert_eq!((img.width(), img.height()), (2, 1));
    }

    /// 远端 token 转义：普通路径原样，空格/单引号按 shell 规则包裹。
    #[test]
    fn 远端路径转义() {
        assert_eq!(
            shell_escape_for_token("/home/u/mino-1.png"),
            "/home/u/mino-1.png"
        );
        assert_eq!(
            shell_escape_for_token("/home/u/my pic.png"),
            "'/home/u/my pic.png'"
        );
        assert_eq!(
            shell_escape_for_token("/home/u/a'b.png"),
            "'/home/u/a'\\''b.png'"
        );
    }

    /// 超限像素直接拒绝（不编码、不落盘）。
    ///
    /// 用最小超限向量验证分支（`1x1` 声明配超长 `rgba`，`encode_png` 前即拒绝）。
    #[test]
    fn 超限像素拒绝() {
        // `width/height` 仅用于长度判定，不实际编码（超限分支先返回）。
        let oversized = vec![0u8; MAX_IMAGE_RGBA_BYTES + 4];
        let mut reader = FakeClipboard {
            text: None,
            files: Vec::new(),
            image: Some((1, 1, oversized)),
        };
        let result = paste_image_token(&mut reader, identity_escape);
        assert!(result.is_err(), "超限图片应返回 Err，实际：{result:?}");
    }
}
