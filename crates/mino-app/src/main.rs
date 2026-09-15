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
/// 推迟首帧。主等宽字体、符号 fallback、界面字体合计约 10MB 仍同步装配
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

    // ==================== 等宽主字体与中文 fallback（按平台） ====================
    #[cfg(target_os = "macos")]
    let mono_candidates = [
        "/System/Library/Fonts/SFNSMono.ttf", // SF Mono
        "/System/Library/Fonts/Menlo.ttc",    // Menlo
        "/System/Library/Fonts/Supplemental/Menlo.ttc",
    ];
    #[cfg(target_os = "windows")]
    let mono_candidates = [
        "C:\\Windows\\Fonts\\CascadiaMono.ttf", // Cascadia Code
        "C:\\Windows\\Fonts\\consola.ttf",      // Consolas
    ];
    #[cfg(all(unix, not(target_os = "macos")))]
    let mono_candidates = [
        "/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf", // DejaVu Sans Mono
        "/usr/share/fonts/truetype/noto/NotoSansMono-Regular.ttf",
        "/usr/share/fonts/noto/NotoSansMono-Regular.ttf",
    ];

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

    let mut loaded_mono = false;
    for path in mono_candidates {
        if let Ok(bytes) = std::fs::read(path) {
            fonts.font_data.insert(
                "mino_mono".to_owned(),
                std::sync::Arc::new(FontData::from_owned(bytes)),
            );
            fonts
                .families
                .get_mut(&FontFamily::Monospace)
                .unwrap()
                .insert(0, "mino_mono".to_owned());
            loaded_mono = true;
            log::info!("加载等宽字体：{path}");
            break;
        }
    }
    if !loaded_mono {
        log::warn!("未找到系统等宽字体，使用默认字体");
    }

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
    // SF Mono 缺少 ➜（U+279C）、❯（U+276F）等常用 zsh 提示符符号，
    // 缺字形会被 egui 渲染为 `?` 替换符（用户反馈提示符显示错乱）。
    // Menlo 同为等宽字体且完整覆盖这些符号（宽度一致，行内布局不会错位），
    // 追加到等宽族、排在中文 fallback 之前。
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
            fonts
                .families
                .get_mut(&FontFamily::Monospace)
                .unwrap()
                // 符号 fallback 必须先于 egui 默认 Hack/NotoEmoji，
                // 但排在主等宽字体之后，保证优先使用同宽 Menlo 字形。
                .insert(if loaded_mono { 1 } else { 0 }, "mino_mono_sym".to_owned());
            log::info!("加载等宽符号 fallback 字体：{path}");
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

    /// 等宽字体链应包含符号 fallback（Menlo），
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
        // 中文不能抢在 Menlo 之前（否则 ➜/❯ 会被中文字体抢先匹配）。
        loader.wait_ready(&ctx);
        // `add_font` 的生效在下一帧 begin_pass：再跑一帧让定义落地。
        let mut output = ctx.run_ui(egui::RawInput::default(), |_| {});
        output.textures_delta.clear();
        let definitions = ctx.fonts(|f| f.definitions().clone());
        let mono = definitions
            .families
            .get(&FontFamily::Monospace)
            .expect("Monospace 族缺失");
        assert!(
            mono.iter().any(|f| f == "mino_mono"),
            "Monospace 族应包含主等宽字体 mino_mono，实际：{mono:?}"
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
        // Menlo 符号 fallback 仅 macOS 加载（SF Mono 缺 ➜/❯ 等字形）；
        // Linux/Windows 使用自带等宽字体，不适用该断言。
        #[cfg(target_os = "macos")]
        {
            assert!(
                mono.iter().any(|f| f == "mino_mono_sym"),
                "Monospace 族应包含 mino_mono_sym，实际：{mono:?}"
            );
            // 符号 fallback 必须排在中文 fallback 之前，且紧邻主等宽字体，
            // 不能被 egui 内置 Hack/NotoEmoji 抢先匹配。
            let pos_sym = mono.iter().position(|f| f == "mino_mono_sym");
            let pos_mono = mono.iter().position(|f| f == "mino_mono");
            let pos_cjk = mono.iter().position(|f| f == "mino_cjk");
            if let (Some(s), Some(m)) = (pos_sym, pos_mono) {
                assert_eq!(s, m + 1, "mino_mono_sym 应紧邻主等宽字体，实际：{mono:?}");
            }
            if let (Some(s), Some(c)) = (pos_sym, pos_cjk) {
                assert!(s < c, "mino_mono_sym 应排在 mino_cjk 之前，实际：{mono:?}");
            }
        }
    }
}
