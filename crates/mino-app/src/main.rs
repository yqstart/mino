//! Mino 应用入口。

pub mod anim;
mod app;
pub mod clip_image;
pub mod dialog;
mod native;
pub mod perf;
pub mod theme;
pub mod views;
mod workdir;

use app::{MinoApp, PRODUCT_NAME};
use eframe::egui;

/// 各平台中文 fallback 字体候选路径（按优先级）。
///
/// 单独成函数：装配线程与后台加载线程共用同一份候选列表，避免两处
/// 顺序不一致（顺序决定实际选中的字体）。
fn cjk_candidates() -> &'static [&'static str] {
    #[cfg(target_os = "macos")]
    {
        &[
            "/System/Library/Fonts/PingFang.ttc",
            "/System/Library/Fonts/STHeiti Light.ttc",
        ]
    }
    #[cfg(target_os = "windows")]
    {
        &[
            "C:\\Windows\\Fonts\\msyh.ttc", // 微软雅黑
            "C:\\Windows\\Fonts\\msyhbd.ttc",
        ]
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        &[
            "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc", // Noto Sans CJK
            "/usr/share/fonts/truetype/wqy/wqy-microhei.ttc",         // 文泉驿微米黑
            "/usr/share/fonts/truetype/droid/DroidSansFallbackFull.ttf",
        ]
    }
}

/// 中文 fallback 字体加载器：后台读取 + 就绪后并入字体定义。
///
/// 中文 fallback 是启动期最大的一笔字体 IO（本机实测 `STHeiti Light.ttc`
/// 55.8MB，PingFang.ttc 78MB，均为单一 .ttc 全量读入），同步读完会明显
/// 推迟首帧。打包等宽字体（约 1.1MB）、符号兜底、界面字体仍同步装配
/// （首帧就必须有正确字形），中文 fallback 交给后台线程，就绪后由 UI
/// 线程把字节并入当前 `FontDefinitions` 再 `set_fonts` 一次。
///
/// 未就绪的短暂窗口里中文会按 egui 默认链路渲染，通常发生在一帧以内。
pub struct CjkFontLoader {
    rx: std::sync::mpsc::Receiver<Option<(String, Vec<u8>)>>,
    /// 已并入或已确认无可用字体（之后不再轮询）。
    done: bool,
    /// 后台读取开始时间（启动打点用）。
    started_at: std::time::Instant,
}

impl CjkFontLoader {
    /// 启动后台读取（立即返回，不阻塞调用方）。
    fn start() -> Self {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let found = cjk_candidates().iter().find_map(|path| {
                std::fs::read(path)
                    .ok()
                    .map(|bytes| (path.to_string(), bytes))
            });
            // 接收端已销毁（应用退出）时忽略发送失败。
            let _ = tx.send(found);
        });
        Self {
            rx,
            done: false,
            started_at: std::time::Instant::now(),
        }
    }

    /// 就绪则并入字体定义；返回 true 表示本次调用真正应用了字体。
    pub fn poll(&mut self, ctx: &egui::Context) -> bool {
        if self.done {
            return false;
        }
        match self.rx.try_recv() {
            Ok(Some((path, bytes))) => {
                self.done = true;
                apply_cjk_font(ctx, &path, bytes);
                log::info!(
                    "中文 fallback 字体就绪：{path}（后台读取耗时 {:.0}ms）",
                    self.started_at.elapsed().as_secs_f32() * 1000.0
                );
                true
            }
            Ok(None) => {
                self.done = true;
                log::warn!("未找到系统中文字体，中文按 egui 默认链路渲染");
                false
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => false,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                self.done = true;
                false
            }
        }
    }

    /// 阻塞到中文 fallback 可用（测试与需要"首帧即有中文"的场景）。
    pub fn wait_ready(&mut self, ctx: &egui::Context) {
        if self.done {
            return;
        }
        if let Ok(Some((path, bytes))) = self.rx.recv() {
            self.done = true;
            apply_cjk_font(ctx, &path, bytes);
        }
        self.done = true;
    }
}

/// 把中文字体字节增量并入两个族的末尾（`add_font` + `Lowest` 优先级）。
///
/// 不能用"克隆当前定义再 `set_fonts`"：egui 的 `set_fonts` 会比较整份
/// `FontDefinitions`（含全部 TTF 字节）决定是否更新，跨帧调用时被比较的
/// 旧定义可能还是首帧前的那份，极易误判 `==` 而吞掉并入——这正是中文字体
/// 在测试里"读到了却没装上"的根因。`add_font` 直接追加到现行定义之后，
/// 主字体/符号 fallback 的既有顺序原样保留（中文仍排最后）。
fn apply_cjk_font(ctx: &egui::Context, path: &str, bytes: Vec<u8>) {
    use egui::{FontData, FontFamily};
    use epaint::text::{FontInsert, FontPriority, InsertFontFamily};
    ctx.add_font(FontInsert {
        name: "mino_cjk".to_owned(),
        data: FontData::from_owned(bytes),
        families: [FontFamily::Proportional, FontFamily::Monospace]
            .into_iter()
            .map(|family| InsertFontFamily {
                family,
                priority: FontPriority::Lowest,
            })
            .collect(),
    });
    log::info!("已并入中文 fallback 字体：{path}");
}

/// 装配字体：主等宽字体 + 符号 fallback + 界面字体同步，中文 fallback 后台。
pub(crate) fn setup_fonts(ctx: &egui::Context) -> CjkFontLoader {
    use egui::{FontData, FontDefinitions, FontFamily};

    let mut fonts = FontDefinitions::default();
    let started = std::time::Instant::now();

    // ==================== 等宽主字体（JetBrains Mono，打包随附） ====================
    // 大多数现代终端（Ghostty 默认、Warp 可选、JetBrains IDE 系默认）都在用
    // JetBrains Mono：0/O、l/1/I 区分明确，自带 ➜/❯/⚡/powerline/制表符字形，
    // 终端提示符不再依赖 Menlo 补字。OFL 1.1 允许随应用打包（见 fonts/OFL.txt）。
    // 注意 egui 0.36 的限制（`FontId` 只有 size + family，无 weight/italics
    // 字段）：`mino_mono_bold` 等名字只是族内 fallback 顺序，layout 时不会
    // 按"粗体段→粗体文件"选择——shaping 永远走族内第一个含该字形的字体
    // （`CachedFamily::find_face_for_char`），即 Regular；`TextFormat`
    // 只管颜色/下划线（粗体=前景增亮仍在 `singleline_job` 里做），`italics`
    // 只在 tessellate 时把字形整体剪切（`text_layout.rs:1173`）。但四个文件
    // 仍有价值：① 链内字形互补——Regular 缺的字（如某些粗体专用符号）由
    // Bold 补上；② 将来 egui 支持字重时零改动生效；③ 打包体积仅 1.1MB。
    // 不要删成只留 Regular：删了省不下多少，fallback 覆盖面反而变窄。
    let bundled_mono: [(&str, &[u8]); 4] = [
        (
            "mino_mono",
            include_bytes!("../fonts/JetBrainsMono-Regular.ttf"),
        ),
        (
            "mino_mono_italic",
            include_bytes!("../fonts/JetBrainsMono-Italic.ttf"),
        ),
        (
            "mino_mono_bold",
            include_bytes!("../fonts/JetBrainsMono-Bold.ttf"),
        ),
        (
            "mino_mono_bold_italic",
            include_bytes!("../fonts/JetBrainsMono-BoldItalic.ttf"),
        ),
    ];
    for (name, bytes) in bundled_mono {
        fonts.font_data.insert(
            name.to_owned(),
            std::sync::Arc::new(FontData::from_static(bytes)),
        );
    }
    {
        let mono = fonts.families.get_mut(&FontFamily::Monospace).unwrap();
        // 打包字重插到链首，egui 默认的 Hack/NotoEmoji/emoji-icon 保留在链尾：
        // 彩色 emoji（如 ✨🔥）只有 NotoEmoji 能画，`clear()` 会让它们变方块。
        // JetBrains Mono 的 emoji 区是单色符号（如 ⚡ U+26A1），与彩色 emoji
        // 不冲突——按字形覆盖各取所需。
        for (i, name) in [
            "mino_mono",
            "mino_mono_italic",
            "mino_mono_bold",
            "mino_mono_bold_italic",
        ]
        .iter()
        .enumerate()
        {
            mono.insert(i, (*name).to_owned());
        }
    }
    log::info!("加载打包等宽字体：JetBrains Mono（Regular/Italic/Bold/BoldItalic）");

    // ==================== 平台兜底（打包字体永远优先，仅防御） ====================
    // `include_bytes!` 编译期嵌入，打包字体不可能缺失；此分支仅防御"有人手
    // 动删掉打包字体文件又重新编译"的极端情况。注意 `.ttc` 是字体集合：
    // `FontData.index` 默认为 0（取集合第一个字重，Menlo.ttc[0] = Regular），
    // 从 ttc 读到的字节与从 ttf 读到的语义一致，都是"一个文件的全部字节"。
    #[cfg(target_os = "macos")]
    let mono_fallbacks = [
        "/System/Library/Fonts/SFNSMono.ttf", // SF Mono
        "/System/Library/Fonts/Menlo.ttc",    // Menlo
        "/System/Library/Fonts/Supplemental/Menlo.ttc",
    ];
    #[cfg(target_os = "windows")]
    let mono_fallbacks = [
        "C:\\Windows\\Fonts\\CascadiaMono.ttf", // Cascadia Code
        "C:\\Windows\\Fonts\\consola.ttf",      // Consolas
    ];
    #[cfg(all(unix, not(target_os = "macos")))]
    let mono_fallbacks = [
        "/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf", // DejaVu Sans Mono
        "/usr/share/fonts/truetype/noto/NotoSansMono-Regular.ttf",
        "/usr/share/fonts/noto/NotoSansMono-Regular.ttf",
    ];

    for path in mono_fallbacks {
        if let Ok(bytes) = std::fs::read(path) {
            // 同名复用：fallback 只在打包字体之后被查到，不抢字形。
            if !fonts.font_data.contains_key("mino_mono_sys") {
                fonts.font_data.insert(
                    "mino_mono_sys".to_owned(),
                    std::sync::Arc::new(FontData::from_owned(bytes)),
                );
                fonts
                    .families
                    .get_mut(&FontFamily::Monospace)
                    .unwrap()
                    .push("mino_mono_sys".to_owned());
                log::info!("加载等宽兜底字体：{path}");
            }
            break;
        }
    }

    // ==================== 比例界面字体（按平台） ====================
    // egui 默认比例字体的中英文与符号容易来自不同字体，尤其是 `⌘T` 这类
    // 快捷键提示会出现字面高度和视觉重量不一致。优先使用系统 UI 字体，
    // 让普通界面文字拥有稳定的字形与字距。
    #[cfg(target_os = "macos")]
    let ui_candidates = [
        "/System/Library/Fonts/SFNS.ttf", // SF Pro 系统界面字体
        "/System/Library/Fonts/HelveticaNeue.ttc",
    ];
    #[cfg(target_os = "windows")]
    let ui_candidates = [
        "C:\\Windows\\Fonts\\segoeui.ttf", // Segoe UI
        "C:\\Windows\\Fonts\\segoeuil.ttf",
    ];
    #[cfg(all(unix, not(target_os = "macos")))]
    let ui_candidates = [
        "/usr/share/fonts/truetype/noto/NotoSans-Regular.ttf",
        "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
    ];

    let mut loaded_ui = false;
    for path in ui_candidates {
        if let Ok(bytes) = std::fs::read(path) {
            fonts.font_data.insert(
                "mino_ui".to_owned(),
                std::sync::Arc::new(FontData::from_owned(bytes)),
            );
            fonts
                .families
                .get_mut(&FontFamily::Proportional)
                .unwrap()
                .insert(0, "mino_ui".to_owned());
            loaded_ui = true;
            log::info!("加载界面字体：{path}");
            break;
        }
    }
    if !loaded_ui {
        log::warn!("未找到系统界面字体，使用默认比例字体");
    }

    // ==================== 等宽符号 fallback（Menlo） ====================
    // JetBrains Mono 自带 ➜（U+279C）/❯（U+276F）/⚡/powerline/制表符字形，
    // 主链路已不再缺字。Menlo 保留为符号兜底（排在中文 fallback 之前、系统
    // 兜底之前）：只补 JetBrains Mono 没有的生僻符号（如 ⬆ U+2B06），正常
    // 提示符走主字体、同宽不断裂。
    #[cfg(target_os = "macos")]
    let symbol_candidates = [
        "/System/Library/Fonts/Menlo.ttc",
        "/System/Library/Fonts/Supplemental/Menlo.ttc",
    ];
    #[cfg(not(target_os = "macos"))]
    let symbol_candidates: [&str; 0] = [];

    for path in symbol_candidates {
        if let Ok(bytes) = std::fs::read(path) {
            fonts.font_data.insert(
                "mino_mono_sym".to_owned(),
                std::sync::Arc::new(FontData::from_owned(bytes)),
            );
            // 符号兜底排在四个打包字重之后、中文 fallback 与 egui 默认之前：
            // 打包字重是 `insert(0..)` 后的 [0..4]，此处 push 到主链末尾。
            fonts
                .families
                .get_mut(&FontFamily::Monospace)
                .unwrap()
                .push("mino_mono_sym".to_owned());
            log::info!("加载等宽符号兜底字体：{path}");
            break;
        }
    }

    // 中文 fallback 追加到等宽族末尾；文件最大（本机 STHeiti 55.8MB），
    // 交给后台线程读取，就绪后由 UI 线程并入（见 `CjkFontLoader`）。
    let loader = CjkFontLoader::start();

    ctx.set_fonts(fonts);
    log::info!(
        "同步装配字体完成（{:.0}ms），中文 fallback 后台加载中",
        started.elapsed().as_secs_f32() * 1000.0
    );
    loader
}

/// 加载应用图标（assets/icon.png → IconData）。
/// eframe 在 macOS 上会通过 NSApp 将其设置为 Dock 图标（运行时设置，
/// 无 .app bundle 的 debug 构建也能生效）。
fn load_icon() -> Option<egui::IconData> {
    let bytes = include_bytes!("../assets/icon.png");
    match image::load_from_memory_with_format(bytes, image::ImageFormat::Png) {
        Ok(img) => {
            let rgba = img.to_rgba8();
            let (width, height) = rgba.dimensions();
            Some(egui::IconData {
                rgba: rgba.into_raw(),
                width,
                height,
            })
        }
        Err(e) => {
            log::warn!("加载应用图标失败：{e}");
            None
        }
    }
}

/// 崩溃日志路径：`~/.config/mino/crash.log`（与主机配置同目录）。
///
/// 不放临时目录：`/tmp` 会被系统清理，且用户报告闪退时需要的是一个
/// 能直接发出来的稳定路径。
fn crash_log_path() -> std::path::PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    std::path::PathBuf::from(home)
        .join(".config")
        .join("mino")
        .join("crash.log")
}

fn main() -> eframe::Result {
    env_logger::init();

    // ==================== 崩溃日志捕获 ====================
    // panic 信息必须落盘：从 Finder/Dock 启动时 stderr 无人接收，闪退
    // 现场会完全丢失（用户只能看到窗口消失）。追加写入固定文件，保留
    // 多次崩溃的历史；下次启动由 MinoApp 读取并提示，用户可直接提供该文件。
    std::panic::set_hook(Box::new(move |info| {
        let location = info
            .location()
            .map(|l| format!("{}:{}", l.file(), l.line()))
            .unwrap_or_else(|| "未知位置".into());
        let backtrace = std::backtrace::Backtrace::force_capture();
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs().to_string())
            .unwrap_or_else(|_| "未知".into());
        let entry = format!(
            "=== {PRODUCT_NAME} 崩溃 ts={timestamp} {location} ===\n信息：{info}\n堆栈：\n{backtrace}\n"
        );

        let mut written = None;
        for path in [
            crash_log_path(),
            std::env::temp_dir().join("mino-panic.log"),
        ] {
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let ok = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .and_then(|mut file| std::io::Write::write_all(&mut file, entry.as_bytes()))
                .is_ok();
            if ok {
                written = Some(path);
                break;
            }
        }

        match written {
            Some(path) => {
                eprintln!("{PRODUCT_NAME} 发生崩溃，详情已写入 {}", path.display());
            }
            None => eprintln!("{PRODUCT_NAME} 发生崩溃，且崩溃日志写入失败"),
        }
    }));

    let mut viewport = egui::ViewportBuilder::default()
        .with_inner_size([960.0, 640.0])
        .with_min_inner_size([400.0, 300.0])
        .with_title(PRODUCT_NAME)
        // 保留 macOS 的 Titled 窗口样式，避免无边框窗口退出时触发 AppKit 的
        // NSTouchBarFinderObservation 崩溃；标题栏本身仍做成透明并与内容重叠，
        // 红绿灯使用 macOS 原生按钮，保证悬浮图标与其他应用一致。
        .with_decorations(true)
        .with_fullsize_content_view(true)
        .with_title_shown(false)
        .with_titlebar_buttons_shown(true)
        .with_titlebar_shown(false);
    // 设置应用图标（macOS Dock 图标由 eframe 运行时写入 NSApp）。
    if let Some(icon) = load_icon() {
        viewport = viewport.with_icon(icon);
    }
    let native_options = eframe::NativeOptions {
        viewport,
        // 每次新建应用窗口时由 eframe 在主显示器上计算居中位置。
        // 不设置时 macOS/winit 可能沿用系统默认的左侧摆放位置。
        centered: true,
        // macOS 15/26 在退出阶段保存 NSWindow 的位置和尺寸时，可能触发
        // AppKit 的 NSTouchBarFinderObservation 重复移除观察者，最终以
        // SIGABRT 退出。mino 没有依赖 eframe 的窗口位置持久化，关闭它可
        // 避开这条系统崩溃路径；应用自己的终端/主机配置仍正常保存。
        persist_window: false,
        // mino 是单窗口常驻应用，不需要在窗口关闭后把控制权交还给调用方。
        // eframe 0.36 的默认值会走 macOS run_app_on_demand；该路径在
        // 长时间按需重绘、后台 PTY 事件与无标题栏窗口组合下可能出现事件
        // 不再驱动 UI 的假死。使用常规 run_app 保持 AppKit 事件循环持续运行。
        run_and_return: false,
        ..Default::default()
    };

    eframe::run_native(
        PRODUCT_NAME,
        native_options,
        Box::new(|cc| {
            let cjk_fonts = setup_fonts(&cc.egui_ctx);
            // 应用窗口圆角与透明背景（非 macOS/测试环境静默跳过）。
            native::apply_rounded_window(cc);
            let mut app = MinoApp::new(cc);
            app.set_cjk_font_loader(cjk_fonts);
            Ok(Box::new(app))
        }),
    )
}

#[cfg(test)]
mod font_tests {
    use super::*;
    use egui::FontFamily;

    /// 等宽字体链应以打包的 JetBrains Mono 开头（含四字重），
    /// 否则 ➜/❯ 等 zsh 提示符符号会渲染为 `?` 替换符（回归测试）。
    #[test]
    fn 等宽字体链含符号fallback() {
        let ctx = egui::Context::default();
        // 先跑一帧初始化字体系统（Context::fonts 在首次 run 前不可用）；
        // set_fonts 延迟到下一帧 begin_pass 生效，因此跑两帧。
        // 中文 fallback 由下面的 `wait_ready` 显式等待后单独断言。
        let mut loader: Option<CjkFontLoader> = None;
        let mut output = ctx.run_ui(egui::RawInput::default(), |ctx| {
            loader = Some(setup_fonts(ctx));
        });
        output.textures_delta.clear();
        let mut loader = loader.expect("setup_fonts 应返回加载器");
        // 中文不能抢在符号兜底之前（否则 ➜/❯ 会被中文字体抢先匹配）。
        loader.wait_ready(&ctx);
        // `add_font` 的生效在下一帧 begin_pass：再跑一帧让定义落地。
        let mut output = ctx.run_ui(egui::RawInput::default(), |_| {});
        output.textures_delta.clear();
        let definitions = ctx.fonts(|f| f.definitions().clone());
        let mono = definitions
            .families
            .get(&FontFamily::Monospace)
            .expect("Monospace 族缺失");
        // 四字重都在链首：族内字形互补、顺序固定，正体永远第一。
        for (i, name) in [
            "mino_mono",
            "mino_mono_italic",
            "mino_mono_bold",
            "mino_mono_bold_italic",
        ]
        .iter()
        .enumerate()
        {
            assert_eq!(
                mono.get(i),
                Some(&name.to_string()),
                "Monospace 族前四位应为 JetBrains Mono 四字重，实际：{mono:?}"
            );
        }
        // 打包字形自带提示符符号：主链路不断言 has_glyphs（字体是否真实生效
        // 由下面的字形测试覆盖），这里只保证链路顺序。
        // egui 默认 fallback（Hack/NotoEmoji/emoji-icon）必须保留在链尾：
        // 彩色 emoji 只有 NotoEmoji 能画，丢了它们 emoji 全变方块。
        assert!(
            mono.iter().any(|f| f == "Hack"),
            "egui 默认 Hack fallback 不应被误删，实际：{mono:?}"
        );
        assert!(
            mono.iter().any(|f| f == "NotoEmoji-Regular"),
            "egui 默认 emoji fallback 不应被误删，实际：{mono:?}"
        );
        #[cfg(target_os = "macos")]
        {
            let proportional = definitions
                .families
                .get(&FontFamily::Proportional)
                .expect("Proportional 族缺失");
            assert!(
                proportional.iter().any(|f| f == "mino_ui"),
                "macOS 比例字体应包含 mino_ui，实际：{proportional:?}"
            );
        }
        // Menlo 符号兜底仅 macOS 加载（补 JetBrains Mono 没有的生僻符号）；
        // Linux/Windows 使用自带等宽字体，不适用该断言。
        #[cfg(target_os = "macos")]
        {
            assert!(
                mono.iter().any(|f| f == "mino_mono_sym"),
                "Monospace 族应包含 mino_mono_sym，实际：{mono:?}"
            );
            // 符号兜底必须排在中文 fallback 之前、四个打包字重之后，
            // 不能被 egui 内置 Hack/NotoEmoji 或中文字体抢先匹配。
            let pos_sym = mono.iter().position(|f| f == "mino_mono_sym");
            let pos_bold_italic = mono.iter().position(|f| f == "mino_mono_bold_italic");
            let pos_cjk = mono.iter().position(|f| f == "mino_cjk");
            if let (Some(s), Some(b)) = (pos_sym, pos_bold_italic) {
                assert!(s > b, "mino_mono_sym 应排在打包字重之后，实际：{mono:?}");
            }
            if let (Some(s), Some(c)) = (pos_sym, pos_cjk) {
                assert!(s < c, "mino_mono_sym 应排在 mino_cjk 之前，实际：{mono:?}");
            }
        }
    }

    /// JetBrains Mono 打包字形必须真实生效：➜/❯ 不走替换符，且四字重
    /// 文件都真实装入（只查字体链不断言渲染，会被"链对了但字节坏"漏掉，
    /// 所以这里直接查字形覆盖与 font_data）。
    #[test]
    fn 打包字体符号真实生效() {
        let ctx = egui::Context::default();
        let mut loader: Option<CjkFontLoader> = None;
        let mut output = ctx.run_ui(egui::RawInput::default(), |ctx| {
            loader = Some(setup_fonts(ctx));
        });
        output.textures_delta.clear();
        let mut loader = loader.expect("setup_fonts 应返回加载器");
        loader.wait_ready(&ctx);
        let mut output = ctx.run_ui(egui::RawInput::default(), |_| {});
        output.textures_delta.clear();
        ctx.fonts_mut(|f| {
            // 提示符符号在主字体里就有（不依赖 Menlo 兜底）。
            for ch in ["➜", "❯", "⚡"] {
                assert!(
                    f.has_glyphs(&egui::FontId::monospace(13.0), ch),
                    "JetBrains Mono 应自带 {ch} 字形"
                );
            }
            // 四字重文件都真实装入（坏字节/缺文件会在这里暴露）。
            let definitions = f.definitions();
            for name in [
                "mino_mono",
                "mino_mono_italic",
                "mino_mono_bold",
                "mino_mono_bold_italic",
            ] {
                assert!(
                    definitions.font_data.contains_key(name),
                    "字重 {name} 应已装入，实际：{:?}",
                    definitions.font_data.keys().collect::<Vec<_>>()
                );
            }
        });
    }
}
