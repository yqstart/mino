//! 主题与配色（Kaku 基准）。
//!
//! 全面照搬 Kaku 终端（WezTerm 系）的配色体系：深色「Kaku Dark」（Aura 系，
//! 背景 `#15141b`、紫色光标 `#8e6ad9`）与浅色「Kaku Light」（Flexoki 系，
//! 米白 `#FFFCF0`），跟随 macOS 系统外观自动切换。
//!
//! 与 Kaku 一致的关键约定：
//! - 应用底色 == 终端底色 == 标签栏底色（`#15141b`），只有面板（surface
//!   `#1f1d28`）与激活标签（surface_active `#29263c`）逐级提亮；
//! - ANSI 调色板原样搬自 Kaku 的 `color_schemes['Kaku Dark'/'Kaku Light']`
//!   （含「cyan 与 green 同色」「ANSI black 是浅灰」这类 Kaku 特有取值）；
//! - `color_overrides` / `foreground_color_overrides`：Kaku 为 Claude Code
//!   等 TUI 补对比度用的前景/背景替换表，渲染层按表替换；
//! - 粗体不改变颜色（Kaku `bold_brightens_ansi_colors = false`）。

use alacritty_terminal::vte::ansi::Rgb;
use egui::{Color32, Context, CornerRadius, Stroke, Visuals};

/// 设计 token（Kaku Dark 基准；控件半径按 WezTerm 覆盖层风格取小值）。
pub mod tokens {
    use egui::Color32;

    // ==================== 背景分层（Kaku Dark） ====================
    // 应用 / 标签栏 / 终端共用同一底色（Kaku 把三者都设为 `#15141b`），
    // 面板与浮层用 surface 系提亮。
    pub const BG_APP: Color32 = Color32::from_rgb(0x15, 0x14, 0x1b);
    pub const BG_HEADER: Color32 = Color32::from_rgb(0x15, 0x14, 0x1b);
    pub const BG_PANEL: Color32 = Color32::from_rgb(0x1f, 0x1d, 0x28);
    pub const BG_ELEVATED: Color32 = Color32::from_rgb(0x29, 0x26, 0x3c);
    pub const BG_TERMINAL: Color32 = Color32::from_rgb(0x15, 0x14, 0x1b);

    pub const BORDER_SUBTLE: Color32 =
        Color32::from_rgba_unmultiplied_const(0xd5, 0xd4, 0xd6, 0x1a);

    pub const TEXT_PRIMARY: Color32 = Color32::from_rgb(0xd5, 0xd4, 0xd6);
    pub const TEXT_SECONDARY: Color32 = Color32::from_rgb(0xa8, 0xa6, 0xae);
    pub const TEXT_MUTED: Color32 = Color32::from_rgb(0x6d, 0x6d, 0x6d);

    // Kaku 的强调色 = 光标紫（选择/焦点/链接同源）；辅助强调 = 琥珀。
    pub const ACCENT: Color32 = Color32::from_rgb(0x8e, 0x6a, 0xd9);
    pub const ACCENT_2: Color32 = Color32::from_rgb(0xda, 0xae, 0x76);
    pub const ACCENT_SOFT: Color32 = Color32::from_rgba_unmultiplied_const(0x8e, 0x6a, 0xd9, 0x2e);
    pub const ACCENT_FG: Color32 = Color32::from_rgb(0xff, 0xff, 0xff);
    pub const FOCUS_RING: Color32 = Color32::from_rgba_unmultiplied_const(0x8e, 0x6a, 0xd9, 0x90);

    pub const SUCCESS: Color32 = Color32::from_rgb(0x58, 0xd8, 0xad);
    pub const WARNING: Color32 = Color32::from_rgb(0xda, 0xae, 0x76);
    pub const DANGER: Color32 = Color32::from_rgb(0xd8, 0x5d, 0x5d);

    // WezTerm 覆盖层控件是小圆角（对话框/按钮 4-6px），不做大圆角气泡。
    pub const RADIUS_SM: f32 = 6.0;
    pub const RADIUS_ITEM: f32 = 4.0;
}

// ==================== 终端调色板（Kaku Dark / Kaku Light 原值） ====================

/// Kaku Dark ANSI 0-15（对应 kaku.lua `config.color_schemes['Kaku Dark']`）。
pub const PALETTE_KAKU_DARK: [Rgb; 16] = [
    Rgb {
        r: 0xc8,
        g: 0xc6,
        b: 0xcc,
    }, // black（Kaku 把 ANSI black 渲染为浅灰）
    Rgb {
        r: 0xd8,
        g: 0x5d,
        b: 0x5d,
    }, // red
    Rgb {
        r: 0x58,
        g: 0xd8,
        b: 0xad,
    }, // green
    Rgb {
        r: 0xda,
        g: 0xae,
        b: 0x76,
    }, // yellow（橙）
    Rgb {
        r: 0x68,
        g: 0xaf,
        b: 0xda,
    }, // blue
    Rgb {
        r: 0x8e,
        g: 0x6a,
        b: 0xd9,
    }, // magenta（紫）
    Rgb {
        r: 0x58,
        g: 0xd8,
        b: 0xad,
    }, // cyan（与 green 同色，Kaku 特有）
    Rgb {
        r: 0xd5,
        g: 0xd4,
        b: 0xd6,
    }, // white
    Rgb {
        r: 0x6d,
        g: 0x6d,
        b: 0x6d,
    }, // bright black（灰）
    Rgb {
        r: 0xd8,
        g: 0x5d,
        b: 0x5d,
    }, // bright red
    Rgb {
        r: 0x58,
        g: 0xd8,
        b: 0xad,
    }, // bright green
    Rgb {
        r: 0xda,
        g: 0xae,
        b: 0x76,
    }, // bright yellow
    Rgb {
        r: 0x90,
        g: 0xc9,
        b: 0xe6,
    }, // bright blue
    Rgb {
        r: 0x8e,
        g: 0x6a,
        b: 0xd9,
    }, // bright magenta
    Rgb {
        r: 0x58,
        g: 0xd8,
        b: 0xad,
    }, // bright cyan
    Rgb {
        r: 0xd5,
        g: 0xd4,
        b: 0xd6,
    }, // bright white
];

/// Kaku Light ANSI 0-15（对应 kaku.lua `config.color_schemes['Kaku Light']`）。
pub const PALETTE_KAKU_LIGHT: [Rgb; 16] = [
    Rgb {
        r: 0x10,
        g: 0x0f,
        b: 0x0f,
    }, // black
    Rgb {
        r: 0xaf,
        g: 0x30,
        b: 0x29,
    }, // red-600
    Rgb {
        r: 0x53,
        g: 0x69,
        b: 0x07,
    }, // green-700
    Rgb {
        r: 0x8e,
        g: 0x6b,
        b: 0x02,
    }, // yellow-700
    Rgb {
        r: 0x20,
        g: 0x5e,
        b: 0xa6,
    }, // blue-600
    Rgb {
        r: 0xa0,
        g: 0x2f,
        b: 0x6f,
    }, // magenta-600
    Rgb {
        r: 0x1c,
        g: 0x6c,
        b: 0x66,
    }, // cyan-700
    Rgb {
        r: 0x57,
        g: 0x56,
        b: 0x53,
    }, // base-700
    Rgb {
        r: 0x6f,
        g: 0x6e,
        b: 0x69,
    }, // bright black（base-600）
    Rgb {
        r: 0xc0,
        g: 0x3e,
        b: 0x35,
    }, // red-500
    Rgb {
        r: 0x66,
        g: 0x79,
        b: 0x0d,
    }, // green-600
    Rgb {
        r: 0x8e,
        g: 0x6b,
        b: 0x02,
    }, // yellow-700（与 ansi 黄一致）
    Rgb {
        r: 0x31,
        g: 0x71,
        b: 0xb2,
    }, // blue-500
    Rgb {
        r: 0xb7,
        g: 0x45,
        b: 0x83,
    }, // magenta-500
    Rgb {
        r: 0x27,
        g: 0x7c,
        b: 0x75,
    }, // cyan-500
    Rgb {
        r: 0x40,
        g: 0x3e,
        b: 0x3c,
    }, // base-800
];

/// Kaku Dark 的背景替换表（`color_overrides`）：把 Claude Code 等 TUI 的
/// 低对比底色替换为可读的深色。
pub const DARK_BG_OVERRIDES: &[(Rgb, Rgb)] = &[
    (
        Rgb {
            r: 0xc8,
            g: 0xc6,
            b: 0xcc,
        },
        Rgb {
            r: 0x15,
            g: 0x14,
            b: 0x1b,
        },
    ), // ANSI black 背景
    (
        Rgb {
            r: 0x6d,
            g: 0x6d,
            b: 0x6d,
        },
        Rgb {
            r: 0x3a,
            g: 0x39,
            b: 0x42,
        },
    ), // ANSI 8
    (
        Rgb {
            r: 0x6e,
            g: 0x6e,
            b: 0x6e,
        },
        Rgb {
            r: 0x3a,
            g: 0x39,
            b: 0x42,
        },
    ), // Claude Code true color 变体
    (
        Rgb {
            r: 0x8e,
            g: 0xc3,
            b: 0xff,
        },
        Rgb {
            r: 0x3a,
            g: 0x39,
            b: 0x42,
        },
    ), // Claude Code 蓝色标题背景
    (
        Rgb {
            r: 0xd5,
            g: 0xd4,
            b: 0xd6,
        },
        Rgb {
            r: 0x4a,
            g: 0x49,
            b: 0x54,
        },
    ), // ANSI 7/15 背景（比 ANSI 8 底更亮一档）
];

/// Kaku Dark 的前景替换表（`foreground_color_overrides`）：黑底真彩前景
/// 在深色主题上不可读，统一提亮。
pub const DARK_FG_OVERRIDES: &[(Rgb, Rgb)] = &[
    (
        Rgb { r: 0, g: 0, b: 0 },
        Rgb {
            r: 0xd5,
            g: 0xd4,
            b: 0xd6,
        },
    ),
    (
        Rgb {
            r: 0x11,
            g: 0x0f,
            b: 0x18,
        },
        Rgb {
            r: 0xd5,
            g: 0xd4,
            b: 0xd6,
        },
    ),
    (
        Rgb {
            r: 0x15,
            g: 0x14,
            b: 0x1b,
        },
        Rgb {
            r: 0xd5,
            g: 0xd4,
            b: 0xd6,
        },
    ),
    (
        Rgb {
            r: 0x1a,
            g: 0x1a,
            b: 0x1a,
        },
        Rgb {
            r: 0xd5,
            g: 0xd4,
            b: 0xd6,
        },
    ),
    (
        Rgb {
            r: 0x1c,
            g: 0x1c,
            b: 0x1c,
        },
        Rgb {
            r: 0xd5,
            g: 0xd4,
            b: 0xd6,
        },
    ),
];

/// Kaku Light 的背景替换表。
pub const LIGHT_BG_OVERRIDES: &[(Rgb, Rgb)] = &[
    (
        Rgb {
            r: 0x57,
            g: 0x56,
            b: 0x53,
        },
        Rgb {
            r: 0xf2,
            g: 0xf0,
            b: 0xeb,
        },
    ),
    (
        Rgb {
            r: 0x58,
            g: 0x57,
            b: 0x54,
        },
        Rgb {
            r: 0xf2,
            g: 0xf0,
            b: 0xeb,
        },
    ),
    (
        Rgb {
            r: 0x22,
            g: 0x5f,
            b: 0xa6,
        },
        Rgb {
            r: 0xf2,
            g: 0xf0,
            b: 0xeb,
        },
    ),
    (
        Rgb {
            r: 0x20,
            g: 0x5e,
            b: 0xa6,
        },
        Rgb {
            r: 0xc9,
            g: 0xdd,
            b: 0xf0,
        },
    ),
    (
        Rgb {
            r: 0x1c,
            g: 0x6c,
            b: 0x66,
        },
        Rgb {
            r: 0xf2,
            g: 0xf0,
            b: 0xeb,
        },
    ),
    (
        Rgb {
            r: 0x53,
            g: 0x69,
            b: 0x07,
        },
        Rgb {
            r: 0xf2,
            g: 0xf0,
            b: 0xeb,
        },
    ),
    (
        Rgb {
            r: 0x8e,
            g: 0x6b,
            b: 0x02,
        },
        Rgb {
            r: 0xf2,
            g: 0xf0,
            b: 0xeb,
        },
    ),
    (
        Rgb {
            r: 0x40,
            g: 0x3e,
            b: 0x3c,
        },
        Rgb {
            r: 0xe8,
            g: 0xe6,
            b: 0xdb,
        },
    ),
];

/// Kaku Light 的前景替换表（米白底上的浅黄文字压暗）。
pub const LIGHT_FG_OVERRIDES: &[(Rgb, Rgb)] = &[
    (
        Rgb {
            r: 0xff,
            g: 0xff,
            b: 0xdb,
        },
        Rgb {
            r: 0x57,
            g: 0x56,
            b: 0x53,
        },
    ),
    (
        Rgb {
            r: 0xff,
            g: 0xff,
            b: 0xdc,
        },
        Rgb {
            r: 0x57,
            g: 0x56,
            b: 0x53,
        },
    ),
];

/// xterm 256 色表固定部分（index ≥ 16：216 立方色 + 24 灰阶，与主题调色板无关）。
/// 声明即初始化（惰性求值一次），避免逐 cell 现算乘除（全彩色屏时每帧上万次）。
static XTERM_FIXED: std::sync::LazyLock<[Rgb; 240]> = std::sync::LazyLock::new(|| {
    std::array::from_fn(|i| {
        let n = i as u8 + 16;
        if n < 232 {
            let n2 = n - 16;
            let r = n2 / 36;
            let g = (n2 % 36) / 6;
            let b = n2 % 6;
            let level = |v: u8| if v == 0 { 0 } else { 55 + v * 40 };
            Rgb {
                r: level(r),
                g: level(g),
                b: level(b),
            }
        } else {
            let v = 8 + (n - 232) * 10;
            Rgb { r: v, g: v, b: v }
        }
    })
});

/// xterm 256 色表（16 基本 + 216 立方色 + 24 灰阶）。
pub fn xterm256(index: u8, palette: [Rgb; 16]) -> Rgb {
    if index < 16 {
        palette[index as usize]
    } else {
        XTERM_FIXED[(index - 16) as usize]
    }
}

// ==================== 主题定义（Kaku Dark / Kaku Light） ====================

pub struct Theme {
    /// 主题名（持久化用；配置里存名称而非下标）。
    pub name: &'static str,
    pub light: bool,
    pub bg_app: Color32,
    pub bg_header: Color32,
    pub bg_panel: Color32,
    pub bg_elevated: Color32,
    pub border: Color32,
    pub text_primary: Color32,
    pub text_secondary: Color32,
    pub text_muted: Color32,
    pub accent: Color32,
    pub accent2: Color32,
    pub accent_soft: Color32,
    /// accent 填充之上的文字色（深色主题白、浅色主题近黑）。
    pub accent_fg: Color32,
    pub success: Color32,
    pub warning: Color32,
    pub danger: Color32,
    /// 注意力点色（Kaku 的 tab attention/bell 指示，深色橙 / 浅色金）。
    pub attention: Color32,
    pub term_bg: Rgb,
    pub term_fg: Rgb,
    pub term_cursor: Rgb,
    pub term_palette: [Rgb; 16],
    /// 终端选择背景（Kaku Dark 是 55% 紫、Kaku Light 是不透明米灰）。
    pub term_selection_bg: Color32,
    /// 背景替换表（`color_overrides`）。
    pub bg_overrides: &'static [(Rgb, Rgb)],
    /// 前景替换表（`foreground_color_overrides`）。
    pub fg_overrides: &'static [(Rgb, Rgb)],
}

pub const THEME_DARK_NAME: &str = "Kaku Dark";
pub const THEME_LIGHT_NAME: &str = "Kaku Light";

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
/// 0 = Kaku Dark，1 = Kaku Light。
static CURRENT_THEME: AtomicUsize = AtomicUsize::new(0);
static THEME_REVISION: AtomicU64 = AtomicU64::new(0);

pub fn current_theme() -> &'static Theme {
    &THEMES[CURRENT_THEME.load(Ordering::Relaxed)]
}

/// 当前主题下标（0 深 / 1 浅）。
pub fn current_theme_index() -> usize {
    CURRENT_THEME.load(Ordering::Relaxed)
}

/// 按名称查找主题下标；未知名称返回 `None`（含已废弃的旧主题名）。
pub fn theme_index_by_name(name: &str) -> Option<usize> {
    if name == THEME_DARK_NAME {
        Some(0)
    } else if name == THEME_LIGHT_NAME {
        Some(1)
    } else {
        None
    }
}

/// 按配置的外观偏好求有效主题下标：
/// `auto`/未知 → 跟随系统外观；`dark` / `light` → 强制。
pub fn resolve_appearance(appearance: &str, system_dark: bool) -> usize {
    match appearance {
        "dark" => 0,
        "light" => 1,
        _ => usize::from(!system_dark),
    }
}

/// 当前主题修订号；主题改变时递增，供终端等缓存消费者失效。
pub fn theme_revision() -> u64 {
    THEME_REVISION.load(Ordering::Acquire)
}

pub fn set_theme(ctx: &Context, index: usize) {
    let index = index.min(THEMES.len() - 1);
    let changed = CURRENT_THEME.swap(index, Ordering::AcqRel) != index;
    if changed {
        THEME_REVISION.fetch_add(1, Ordering::AcqRel);
    }
    // 即使下标未变也重放样式：系统外观变化需要重新套用（幂等）。
    apply_theme(ctx, current_theme());
}

pub fn apply_theme(ctx: &Context, theme: &Theme) {
    ctx.set_theme(if theme.light {
        egui::ThemePreference::Light
    } else {
        egui::ThemePreference::Dark
    });
    let egui_theme = if theme.light {
        egui::Theme::Light
    } else {
        egui::Theme::Dark
    };
    let mut style = (*ctx.style_of(egui_theme)).clone();
    let mut visuals = if theme.light {
        Visuals::light()
    } else {
        Visuals::dark()
    };

    visuals.panel_fill = theme.bg_app;
    visuals.window_fill = theme.bg_elevated;
    visuals.extreme_bg_color = theme.bg_panel;
    visuals.faint_bg_color = theme.bg_header;
    visuals.override_text_color = Some(theme.text_primary);
    visuals.hyperlink_color = theme.accent;
    visuals.selection.bg_fill = theme.accent_soft;
    visuals.selection.stroke = Stroke::new(1.0, theme.accent);

    for w in [
        &mut visuals.widgets.inactive,
        &mut visuals.widgets.hovered,
        &mut visuals.widgets.active,
        &mut visuals.widgets.noninteractive,
    ] {
        w.corner_radius = CornerRadius::same(tokens::RADIUS_SM as u8);
    }
    visuals.widgets.inactive.bg_fill = theme.bg_panel;
    visuals.widgets.inactive.weak_bg_fill = theme.bg_panel;
    visuals.widgets.inactive.fg_stroke = Stroke::new(1.0, theme.text_primary);
    visuals.widgets.inactive.bg_stroke = Stroke::new(1.0, theme.border);

    // hover：accent 低透明度叠加，避免所有控件都变成一块纯色底。
    let hover_overlay =
        Color32::from_rgba_unmultiplied(theme.accent.r(), theme.accent.g(), theme.accent.b(), 20);
    visuals.widgets.hovered.bg_fill = hover_overlay;
    visuals.widgets.hovered.weak_bg_fill = hover_overlay;
    visuals.widgets.hovered.fg_stroke = Stroke::new(1.0, theme.text_primary);
    visuals.widgets.hovered.bg_stroke = Stroke::new(1.0, Color32::TRANSPARENT);

    visuals.widgets.active.bg_fill = theme.accent_soft;
    visuals.widgets.active.weak_bg_fill = theme.accent_soft;
    visuals.widgets.active.fg_stroke = Stroke::new(1.0, theme.text_primary);
    visuals.widgets.active.bg_stroke = Stroke::new(1.0, theme.accent);

    visuals.widgets.noninteractive.fg_stroke = Stroke::new(1.0, theme.text_secondary);
    visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0, theme.border);

    style.spacing.item_spacing = egui::vec2(9.0, 7.0);
    style.spacing.button_padding = egui::vec2(11.0, 5.0);
    style.spacing.interact_size = egui::vec2(30.0, 26.0);

    // 全局滚动条：细窄浮动、不占布局宽度，平时半透明、hover 加深。
    style.spacing.scroll = egui::style::ScrollStyle {
        floating: true,
        bar_width: 8.0,
        floating_width: 3.0,
        floating_allocated_width: 0.0,
        foreground_color: false,
        dormant_background_opacity: 0.0,
        active_background_opacity: 0.25,
        ..Default::default()
    };

    visuals.window_corner_radius = CornerRadius::same(10);
    visuals.window_shadow = egui::Shadow {
        offset: [0, 6],
        blur: 24,
        spread: 0,
        color: if theme.light {
            Color32::from_rgba_unmultiplied(0x10, 0x0f, 0x0f, 0x40)
        } else {
            Color32::from_rgba_unmultiplied(0x00, 0x00, 0x00, 0xd0)
        },
    };
    visuals.popup_shadow = egui::Shadow {
        offset: [0, 4],
        blur: 16,
        spread: 0,
        color: if theme.light {
            Color32::from_rgba_unmultiplied(0x10, 0x0f, 0x0f, 0x30)
        } else {
            Color32::from_rgba_unmultiplied(0x00, 0x00, 0x00, 0xb0)
        },
    };

    // 所有可点击控件 hover 时显示小手光标（egui 默认不设置；
    // Button/SelectableLabel/ComboBox 等标准控件读取该字段）。
    // 注意：TextEdit 内部显式设置 I-beam，不受影响。
    visuals.interact_cursor = Some(egui::CursorIcon::PointingHand);

    style.visuals = visuals;
    ctx.set_style_of(egui_theme, style);
}

/// 滑块（`egui::Slider`）专用配色。
///
/// egui 的滑块没有独立的轨道配色项：轨道恒定使用
/// `widgets.inactive.bg_fill`，手柄填充按交互态取 `bg_fill`——两者同源。
/// 全局该字段是面板色（`bg_panel`），而设置弹窗卡片底同为面板色：
/// 轨道与卡片**完全同色**，只能看到手柄的 1px 描边轮廓（用户现象
/// 「滑块轨道看不清」）。调用方用 `ui.scope` 局部应用本函数，不影响其它
/// 控件：浅色半透明轨道 + accent 已填充 + 亮描边手柄，拖拽时手柄填充 accent。
pub fn apply_slider_style(style: &mut egui::Style, theme: &Theme) {
    style.visuals.slider_trailing_fill = true;
    // 已填充段（滑块当前值一侧）用不透明 accent，与主题色一致。
    style.visuals.selection.bg_fill = theme.accent;
    // 未填充轨道与静止态手柄填充：在两种主题下都可辨的叠加色。
    let track = if theme.light {
        Color32::from_rgba_unmultiplied(0x10, 0x0f, 0x0f, 0x26)
    } else {
        Color32::from_rgba_unmultiplied(0xff, 0xff, 0xff, 0x26)
    };
    let widgets = &mut style.visuals.widgets;
    widgets.inactive.bg_fill = track;
    widgets.inactive.fg_stroke = Stroke::new(1.5, theme.text_primary);
    widgets.hovered.bg_fill = theme.accent;
    widgets.hovered.fg_stroke = Stroke::new(1.5, theme.accent_fg);
    widgets.active.bg_fill = theme.accent;
    widgets.active.fg_stroke = Stroke::new(1.5, theme.accent_fg);
}

pub static THEMES: [Theme; 2] = [
    Theme {
        name: THEME_DARK_NAME,
        light: false,
        bg_app: tokens::BG_APP,
        bg_header: tokens::BG_HEADER,
        bg_panel: tokens::BG_PANEL,
        bg_elevated: tokens::BG_ELEVATED,
        border: tokens::BORDER_SUBTLE,
        text_primary: tokens::TEXT_PRIMARY,
        text_secondary: tokens::TEXT_SECONDARY,
        text_muted: tokens::TEXT_MUTED,
        accent: tokens::ACCENT,
        accent2: tokens::ACCENT_2,
        accent_soft: tokens::ACCENT_SOFT,
        accent_fg: tokens::ACCENT_FG,
        success: tokens::SUCCESS,
        warning: tokens::WARNING,
        danger: tokens::DANGER,
        attention: Color32::from_rgb(0xda, 0xae, 0x76),
        term_bg: Rgb {
            r: 0x15,
            g: 0x14,
            b: 0x1b,
        },
        term_fg: Rgb {
            r: 0xd5,
            g: 0xd4,
            b: 0xd6,
        },
        term_cursor: Rgb {
            r: 0x8e,
            g: 0x6a,
            b: 0xd9,
        },
        term_palette: PALETTE_KAKU_DARK,
        term_selection_bg: Color32::from_rgba_unmultiplied_const(0x8e, 0x6a, 0xd9, 0x8c),
        bg_overrides: DARK_BG_OVERRIDES,
        fg_overrides: DARK_FG_OVERRIDES,
    },
    Theme {
        name: THEME_LIGHT_NAME,
        light: true,
        bg_app: Color32::from_rgb(0xff, 0xfc, 0xf0),
        bg_header: Color32::from_rgb(0xff, 0xfc, 0xf0),
        bg_panel: Color32::from_rgb(0xf2, 0xf0, 0xeb),
        bg_elevated: Color32::from_rgb(0xe8, 0xe6, 0xdb),
        border: Color32::from_rgb(0xe8, 0xe1, 0xd0),
        text_primary: Color32::from_rgb(0x10, 0x0f, 0x0f),
        text_secondary: Color32::from_rgb(0x57, 0x56, 0x53),
        text_muted: Color32::from_rgb(0x6f, 0x6e, 0x69),
        accent: Color32::from_rgb(0x8e, 0x6a, 0xd9),
        accent2: Color32::from_rgb(0x8e, 0x6b, 0x02),
        accent_soft: Color32::from_rgba_unmultiplied_const(0x8e, 0x6a, 0xd9, 0x26),
        accent_fg: Color32::from_rgb(0xff, 0xfc, 0xf0),
        success: Color32::from_rgb(0x53, 0x69, 0x07),
        warning: Color32::from_rgb(0x8e, 0x6b, 0x02),
        danger: Color32::from_rgb(0xaf, 0x30, 0x29),
        attention: Color32::from_rgb(0xad, 0x83, 0x01),
        term_bg: Rgb {
            r: 0xff,
            g: 0xfc,
            b: 0xf0,
        },
        term_fg: Rgb {
            r: 0x10,
            g: 0x0f,
            b: 0x0f,
        },
        term_cursor: Rgb {
            r: 0x34,
            g: 0x33,
            b: 0x31,
        },
        term_palette: PALETTE_KAKU_LIGHT,
        term_selection_bg: Color32::from_rgb(0xe8, 0xe6, 0xdb),
        bg_overrides: LIGHT_BG_OVERRIDES,
        fg_overrides: LIGHT_FG_OVERRIDES,
    },
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 深色主题取值与kaku一致() {
        let t = &THEMES[0];
        assert_eq!(t.name, "Kaku Dark");
        assert!(!t.light);
        assert_eq!(
            t.term_bg,
            Rgb {
                r: 0x15,
                g: 0x14,
                b: 0x1b
            }
        );
        assert_eq!(
            t.term_fg,
            Rgb {
                r: 0xd5,
                g: 0xd4,
                b: 0xd6
            }
        );
        assert_eq!(
            t.term_cursor,
            Rgb {
                r: 0x8e,
                g: 0x6a,
                b: 0xd9
            }
        );
        // 应用/标签栏/终端同底色（Kaku 的关键视觉约定）。
        assert_eq!(t.bg_app, t.bg_header);
        assert_eq!(t.bg_app, Color32::from_rgb(0x15, 0x14, 0x1b));
        assert_eq!(t.term_palette[2], t.term_palette[6], "cyan 与 green 同色");
        assert_ne!(t.term_palette[0], t.term_bg, "ANSI black 是浅灰而非底色");
    }

    #[test]
    fn 浅色主题取值与kaku一致() {
        let t = &THEMES[1];
        assert_eq!(t.name, "Kaku Light");
        assert!(t.light);
        assert_eq!(
            t.term_bg,
            Rgb {
                r: 0xff,
                g: 0xfc,
                b: 0xf0
            }
        );
        assert_eq!(
            t.term_fg,
            Rgb {
                r: 0x10,
                g: 0x0f,
                b: 0x0f
            }
        );
        assert_eq!(
            t.term_palette[4],
            Rgb {
                r: 0x20,
                g: 0x5e,
                b: 0xa6
            }
        );
    }

    #[test]
    fn 外观偏好解析() {
        assert_eq!(resolve_appearance("dark", false), 0);
        assert_eq!(resolve_appearance("light", true), 1);
        assert_eq!(resolve_appearance("auto", true), 0);
        assert_eq!(resolve_appearance("auto", false), 1);
        // 旧主题名（如「深色」「深蓝」「霓虹」）一律按 auto 处理。
        assert_eq!(resolve_appearance("深蓝", true), 0);
        assert_eq!(resolve_appearance("深蓝", false), 1);
        assert_eq!(theme_index_by_name("Kaku Dark"), Some(0));
        assert_eq!(theme_index_by_name("Kaku Light"), Some(1));
        assert_eq!(theme_index_by_name("霓虹"), None);
    }

    #[test]
    fn 替换表自洽() {
        // 每个替换条目的键值都不同，且深浅两套互不混用。
        for table in [
            DARK_BG_OVERRIDES,
            DARK_FG_OVERRIDES,
            LIGHT_BG_OVERRIDES,
            LIGHT_FG_OVERRIDES,
        ] {
            for (from, to) in table {
                assert_ne!(from, to);
            }
        }
        let t = &THEMES[0];
        assert!(t.bg_overrides.iter().any(|(from, _)| *from
            == Rgb {
                r: 0x6d,
                g: 0x6d,
                b: 0x6d
            }));
    }
}
