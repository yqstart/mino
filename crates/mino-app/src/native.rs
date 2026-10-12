//! macOS 原生窗口定制（非 macOS 平台为空实现）。
//!
//! 窗口使用透明的全尺寸标题栏，隐藏标题文字但保留 macOS 原生按钮；
//! 这里通过 AppKit 给窗口 contentView 的 layer 设置圆角 + 裁切，
//! 并将窗口背景设为透明，使圆角外侧露出桌面，形成整体圆角观感。
//! 开了毛玻璃（Kaku `macos_window_background_blur` > 0）时，再在 contentView
//! 下方铺一层 `NSVisualEffectView` 材质。
//!
//! 只应在真实 AppKit 窗口环境调用（eframe 主线程创建期）；
//! kittest 测试环境没有真实 NSWindow（句柄匹配失败），静默跳过。

#[cfg(target_os = "macos")]
use objc2_app_kit::NSView;

/// 窗口四角圆角半径（逻辑像素，与 `theme.rs` 的大圆角面板观感一致）。
/// 仅 macOS 使用（Linux/Windows 的 native.rs 为空实现），加 cfg 防 `-D warnings` dead_code。
#[cfg(target_os = "macos")]
const WINDOW_CORNER_RADIUS: f64 = 12.0;

/// 给应用窗口应用整体圆角。失败（非 macOS / 无 AppKit 句柄）时静默忽略。
pub fn apply_rounded_window(cc: &eframe::CreationContext<'_>) {
    #[cfg(target_os = "macos")]
    apply_rounded_window_macos(cc);
    #[cfg(not(target_os = "macos"))]
    let _ = cc;
}

/// 给窗口铺一层毛玻璃材质（Kaku `macos_window_background_blur`）。
///
/// 失败（非 macOS / 无 AppKit 句柄）时静默忽略——没有材质就是普通透明窗口，
/// 不影响功能。
pub fn apply_window_blur(cc: &eframe::CreationContext<'_>) {
    #[cfg(target_os = "macos")]
    apply_window_blur_macos(cc);
    #[cfg(not(target_os = "macos"))]
    let _ = cc;
}

/// macOS 实现：拿到 contentView → 在其**最底层**插入 `NSVisualEffectView`
/// （窗口背景模糊材质），窗口背景本身保持透明。
///
/// 说明：AppKit 不公开模糊半径，`macos_window_background_blur` 的数值在这里
/// 只表示"开"（> 0），材质取 `UnderWindowBackground`（随系统深浅色自适应）。
#[cfg(target_os = "macos")]
fn apply_window_blur_macos(cc: &eframe::CreationContext<'_>) {
    use objc2_app_kit::{
        NSAutoresizingMaskOptions, NSVisualEffectBlendingMode, NSVisualEffectMaterial,
        NSVisualEffectState, NSVisualEffectView, NSWindowOrderingMode,
    };
    use raw_window_handle::{HasWindowHandle as _, RawWindowHandle};

    let Ok(handle) = cc.window_handle() else {
        return;
    };
    let RawWindowHandle::AppKit(handle) = handle.as_raw() else {
        return;
    };
    let ns_view_ptr = handle.ns_view.as_ptr().cast::<NSView>();
    if ns_view_ptr.is_null() {
        return;
    }
    // SAFETY: 句柄来自窗口系统，指针在窗口生命周期内有效（与 eframe 内部一致）。
    let ns_view: &NSView = unsafe { ns_view_ptr.as_ref() }.unwrap();

    // SAFETY: `NSVisualEffectView` 是 AppKit 的普通视图，在窗口创建期、
    // 主线程上构造与插入子视图是常规用法。
    unsafe {
        let effect = NSVisualEffectView::new(objc2_foundation::MainThreadMarker::new_unchecked());
        effect.setFrame(ns_view.bounds());
        // 跟随父视图尺寸变化（全尺寸铺满）。
        effect.setAutoresizingMask(
            NSAutoresizingMaskOptions::ViewWidthSizable
                | NSAutoresizingMaskOptions::ViewHeightSizable,
        );
        effect.setBlendingMode(NSVisualEffectBlendingMode::BehindWindow);
        effect.setMaterial(NSVisualEffectMaterial::UnderWindowBackground);
        effect.setState(NSVisualEffectState::Active);
        // 插到最底层：它在内容视图（wgpu 的 CAMetalLayer）之下，只有窗口
        // 背景透明处才透出材质。
        let relative_to = ns_view.subviews().firstObject();
        ns_view.addSubview_positioned_relativeTo(
            &effect,
            NSWindowOrderingMode::Below,
            relative_to.as_deref(),
        );
    }
}

/// macOS 实现：拿到 contentView → 打开 layer 支持 → 设圆角 + 裁切 → 窗口背景透明。
#[cfg(target_os = "macos")]
fn apply_rounded_window_macos(cc: &eframe::CreationContext<'_>) {
    use raw_window_handle::{HasWindowHandle as _, RawWindowHandle};

    // 从 eframe 创建的窗口拿 AppKit 句柄；失败说明不是真实窗口（如测试环境）。
    let Ok(handle) = cc.window_handle() else {
        return;
    };
    let RawWindowHandle::AppKit(handle) = handle.as_raw() else {
        return;
    };
    let ns_view_ptr = handle.ns_view.as_ptr().cast::<NSView>();
    if ns_view_ptr.is_null() {
        return;
    }
    // SAFETY: 句柄来自窗口系统，指针在窗口生命周期内有效（与 eframe 内部一致）。
    let ns_view: &NSView = unsafe { ns_view_ptr.as_ref() }.unwrap();

    apply_corner_radius(ns_view);

    // 让窗口背景透明（圆角外侧露出桌面），并关闭不透明标记。
    if let Some(ns_window) = ns_view.window() {
        set_window_transparent(&ns_window);
    }
}

/// 给 view 的 backing layer 设圆角并裁切越界内容。
#[cfg(target_os = "macos")]
fn apply_corner_radius(ns_view: &NSView) {
    // 打开 layer 支持（`wantsLayer`），随后 `layer()` 才有值。
    ns_view.setWantsLayer(true);
    if let Some(layer) = ns_view.layer() {
        layer.setCornerRadius(WINDOW_CORNER_RADIUS);
        layer.setMasksToBounds(true);
    }
}

/// 窗口背景设为透明并标记非不透明（否则圆角外侧是黑/白方块）。
#[cfg(target_os = "macos")]
fn set_window_transparent(ns_window: &objc2_app_kit::NSWindow) {
    use objc2_app_kit::NSColor;
    ns_window.setOpaque(false);
    ns_window.setBackgroundColor(Some(&NSColor::clearColor()));
}
