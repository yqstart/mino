//! 弹窗与表单共享基元：外壳、按钮、输入框、发丝线、头像。
//!
//! 设置、新建连接、更新、SFTP 确认四个弹窗之前各用一套头部、圆角和
//! 按钮样式，看起来像四个应用。这里收敛到同一套：14px 窗口圆角、
//! 56px 自绘头部、卡片 10px、行与控件 7px；主按钮只用 accent 实心一种。

use eframe::egui;

/// 头部高度（四个弹窗统一）。
pub const HEADER_H: f32 = 56.0;
/// 头部 logo 尺寸。
pub const HEADER_LOGO: f32 = 32.0;
/// 头部标题块固定高度：两行标题文字（14px + 间距 1px + 10.5px ≈ 34px）
/// 向上取整。`vertical` 子布局会撑满整行高度、内容贴顶（设置标题偏上
/// 的根因）；定高块作为固定尺寸部件参与外层垂直居中，双行文字在
/// 56px 头部内自然居中。
/// 注意块内顶部垫 2.5px：kittest 实测标题块中心比右侧 ESC 胶囊中心高约
/// 3.5px（两行文字在块内贴顶，块中心≠文字中心），垫 2.5px 后差距收到
/// 1px 内，肉眼与 ESC / 关闭按钮居中。
pub const HEADER_TITLE_H: f32 = 36.0;
/// 头部标题块宽度：容纳"保存后可在主机列表中一键连接"等副标题，
/// 窄了会被截断。实测按当前字号需要约 230px。
pub const HEADER_TITLE_W: f32 = 260.0;
/// 主按钮高度。
pub const BTN_H: f32 = 30.0;

/// 弹窗外壳 Frame：深底 + 细边框 + 14px 圆角，内容自管内边距。
pub fn shell_frame(theme: &crate::theme::Theme) -> egui::Frame {
    egui::Frame::new()
        .fill(theme.bg_app)
        .stroke(egui::Stroke::new(1.0, theme.border))
        .corner_radius(egui::CornerRadius::same(14))
        .inner_margin(egui::Margin::same(0))
}

/// 小确认框外壳：浮层底（比弹窗壳高一层），12px 圆角。
pub fn confirm_frame(theme: &crate::theme::Theme) -> egui::Frame {
    egui::Frame::new()
        .fill(theme.bg_elevated)
        .stroke(egui::Stroke::new(1.0, theme.border))
        .corner_radius(egui::CornerRadius::same(12))
        .inner_margin(egui::Margin::same(0))
}

/// 内容卡片：面板底 + 10px 圆角。
pub fn card_frame(theme: &crate::theme::Theme) -> egui::Frame {
    egui::Frame::new()
        .fill(theme.bg_panel)
        .stroke(egui::Stroke::new(1.0, theme.border))
        .corner_radius(egui::CornerRadius::same(10))
        .inner_margin(egui::Margin::symmetric(16, 12))
}

/// 内嵌信息区（更新说明、错误提示）：8px 圆角。
///
/// 底色必须比所在容器高一层才看得出边界：`card_frame` 底是 `bg_panel`，
/// 所以这里用 `bg_elevated`（曾同样 `bg_panel`，和卡片融成一块看不出边界）。
pub fn inset_frame(theme: &crate::theme::Theme) -> egui::Frame {
    egui::Frame::new()
        .fill(theme.bg_elevated)
        .stroke(egui::Stroke::new(1.0, theme.border))
        .corner_radius(egui::CornerRadius::same(8))
        .inner_margin(egui::Margin::same(10))
}

/// 弹窗头部标题块：固定尺寸两行标题（主标题 14px + 副标题 10.5px）。
///
/// 注意：不能用 `vertical()` 包两行——`vertical` 是自动尺寸部件，撑满
/// 外层 `left_to_right(Center)` 整行高度后内容贴顶（设置标题偏上的根因）。
/// 这里用 `allocate_exact_size` 先占固定 260×36，再 `new_child` 进去写字，
/// 固定块参与外层垂直居中，标题整体回到头部正中。
pub fn header_title(ui: &mut egui::Ui, id_salt: &str, title: &str, subtitle: &str) {
    let theme = crate::theme::current_theme();
    let title_rect = ui
        .allocate_exact_size(
            egui::vec2(HEADER_TITLE_W, HEADER_TITLE_H),
            egui::Sense::hover(),
        )
        .0;
    let mut text = ui.new_child(
        egui::UiBuilder::new()
            .id_salt(id_salt)
            .max_rect(title_rect)
            .layout(egui::Layout::top_down(egui::Align::Min)),
    );
    text.spacing_mut().item_spacing.y = 1.0;
    // 块内居中：两行是贴顶排的，块高固定 36 而文字只有 ~29-31，
    // 剩余空隙全堆在底部 → 整块视觉偏上（本次问题的根因）。
    // 按实际字高算顶部垫距，把空隙上下均分；字高随系统字体变化
    // （PingFang / 测试字体行高不同），写死 2.5px 换台机器就偏。
    let title_h = text
        .painter()
        .layout_no_wrap(
            title.to_owned(),
            egui::FontId::proportional(14.0),
            egui::Color32::TRANSPARENT,
        )
        .size()
        .y;
    let sub_h = text
        .painter()
        .layout_no_wrap(
            subtitle.to_owned(),
            egui::FontId::proportional(10.5),
            egui::Color32::TRANSPARENT,
        )
        .size()
        .y;
    // +1.0 是纯中文标题的光学补偿：CJK 字形没有 Latin 下行部，
    // 几何居中看起来仍偏高约 1px，垫下去才和右侧 ESC 胶囊齐平。
    let pad = ((HEADER_TITLE_H - (title_h + 1.0 + sub_h)) * 0.5).max(0.0) + 1.0;
    text.add_space(pad);
    text.label(
        egui::RichText::new(title)
            .strong()
            .size(14.0)
            .color(theme.text_primary),
    );
    text.label(
        egui::RichText::new(subtitle)
            .size(10.5)
            .color(theme.text_muted),
    );
}

/// 光标处的发丝分隔线（左右贴当前内容边，不全出血）。
pub fn hairline(ui: &mut egui::Ui) {
    let theme = crate::theme::current_theme();
    let y = ui.cursor().top();
    ui.painter().line_segment(
        [
            egui::pos2(ui.cursor().left(), y),
            egui::pos2(ui.max_rect().right(), y),
        ],
        egui::Stroke::new(1.0, theme.border.gamma_multiply(0.55)),
    );
}

/// 虚线圆角框（空状态用）：实线框会被误读成一张卡片，虚线表示"这里是空的"。
///
/// 沿圆角矩形轮廓按弧长打点，虚 5px / 隙 4px，转角不断线。
pub fn dashed_rounded_rect(ui: &mut egui::Ui, rect: egui::Rect, radius: f32, color: egui::Color32) {
    const DASH: f32 = 5.0;
    const GAP: f32 = 4.0;
    let r = radius
        .min(rect.width() * 0.5)
        .min(rect.height() * 0.5)
        .max(0.0);
    // 轮廓采样：上边 → 右上弧 → 右边 → 右下弧 → 下边 → 左下弧 → 左边 → 左上弧。
    let mut pts = vec![egui::pos2(rect.left() + r, rect.top())];
    pts.push(egui::pos2(rect.right() - r, rect.top()));
    push_arc(
        &mut pts,
        egui::pos2(rect.right() - r, rect.top() + r),
        r,
        -90.0,
        0.0,
    );
    pts.push(egui::pos2(rect.right(), rect.bottom() - r));
    push_arc(
        &mut pts,
        egui::pos2(rect.right() - r, rect.bottom() - r),
        r,
        0.0,
        90.0,
    );
    pts.push(egui::pos2(rect.left() + r, rect.bottom()));
    push_arc(
        &mut pts,
        egui::pos2(rect.left() + r, rect.bottom() - r),
        r,
        90.0,
        180.0,
    );
    pts.push(egui::pos2(rect.left(), rect.top() + r));
    push_arc(
        &mut pts,
        egui::pos2(rect.left() + r, rect.top() + r),
        r,
        180.0,
        270.0,
    );
    // 按弧长走虚实相位。
    let stroke = egui::Stroke::new(1.0, color);
    let painter = ui.painter();
    let mut phase = 0.0;
    for w in pts.windows(2) {
        let (mut a, b) = (w[0], w[1]);
        let seg_len = a.distance(b);
        if seg_len <= 0.01 {
            continue;
        }
        let dir = (b - a) / seg_len;
        let mut t = 0.0;
        while t < seg_len {
            let remain = seg_len - t;
            if phase < DASH {
                // 虚线段内：画到段尾或虚线用尽。
                let draw = (DASH - phase).min(remain);
                painter.line_segment([a + dir * t, a + dir * (t + draw)], stroke);
                t += draw;
                phase += draw;
            } else {
                // 间隙内：跳过。
                let skip = (DASH + GAP - phase).min(remain);
                t += skip;
                phase += skip;
            }
            if phase >= DASH + GAP - 0.01 {
                phase = 0.0;
            }
            a = b; // 占位，避免未使用警告（实际用 dir*t 推进）。
            let _ = a;
        }
    }
}

/// 圆弧采样（角度制，顺时针为正），首点由调用方保证已存在。
fn push_arc(
    pts: &mut Vec<egui::Pos2>,
    center: egui::Pos2,
    radius: f32,
    start_deg: f32,
    end_deg: f32,
) {
    if radius <= 0.5 {
        return;
    }
    let steps = ((end_deg - start_deg).abs() / 12.0).ceil().max(2.0) as usize;
    for i in 1..=steps {
        let deg = start_deg + (end_deg - start_deg) * (i as f32 / steps as f32);
        let rad = deg.to_radians();
        pts.push(center + egui::vec2(radius * rad.cos(), radius * rad.sin()));
    }
}

/// 主按钮：accent 实心 + 白字（全应用唯一的主动作样式）。
pub fn primary_button<'a>(theme: &'a crate::theme::Theme, label: &'a str) -> egui::Button<'a> {
    egui::Button::new(
        egui::RichText::new(label)
            .size(13.0)
            .color(crate::theme::tokens::ACCENT_FG),
    )
    .fill(theme.accent)
    .stroke(egui::Stroke::NONE)
    .corner_radius(crate::theme::tokens::RADIUS_ITEM)
    .min_size(egui::vec2(88.0, BTN_H))
}

/// 次按钮：浮层底 + 细边框（取消/稍后/关闭统一用它）。
pub fn secondary_button<'a>(theme: &'a crate::theme::Theme, label: &'a str) -> egui::Button<'a> {
    egui::Button::new(
        egui::RichText::new(label)
            .size(12.5)
            .color(theme.text_primary),
    )
    .fill(theme.bg_elevated)
    .stroke(egui::Stroke::new(1.0, theme.border))
    .corner_radius(crate::theme::tokens::RADIUS_ITEM)
    .min_size(egui::vec2(64.0, BTN_H))
}

/// 字段行内小操作（"使用当前终端目录"这类填充型辅助动作）。
///
/// 不能与保存/取消同排等权：它是某个字段的附属填充，放在字段名同行右端。
/// 次按钮的缩小版（浮层底 + 细边框 + 22px 高），字号 11.5，比页脚按钮低一层。
pub fn field_action_button<'a>(theme: &'a crate::theme::Theme, label: &'a str) -> egui::Button<'a> {
    egui::Button::new(
        egui::RichText::new(label)
            .size(11.5)
            .color(theme.text_secondary),
    )
    .fill(theme.bg_elevated)
    .stroke(egui::Stroke::new(1.0, theme.border))
    .corner_radius(crate::theme::tokens::RADIUS_ITEM)
    .min_size(egui::vec2(0.0, 22.0))
}

/// 危险主按钮（确认删除）：danger 实心 + 白字。
pub fn danger_button<'a>(theme: &'a crate::theme::Theme, label: &'a str) -> egui::Button<'a> {
    egui::Button::new(
        egui::RichText::new(label)
            .size(13.0)
            .color(crate::theme::tokens::ACCENT_FG),
    )
    .fill(theme.danger)
    .stroke(egui::Stroke::NONE)
    .corner_radius(crate::theme::tokens::RADIUS_ITEM)
    .min_size(egui::vec2(76.0, BTN_H - 2.0))
}

/// 头部右侧的幽灵关闭按钮（26×26，"×" 次要色）。
pub fn close_icon_button(ui: &mut egui::Ui, hover_text: &str) -> bool {
    let theme = crate::theme::current_theme();
    ui.add(
        egui::Button::new(
            egui::RichText::new("×")
                .size(16.0)
                .color(theme.text_secondary),
        )
        .fill(egui::Color32::TRANSPARENT)
        .stroke(egui::Stroke::NONE)
        .min_size(egui::vec2(26.0, 26.0))
        .corner_radius(crate::theme::tokens::RADIUS_ITEM),
    )
    .on_hover_text(hover_text)
    .on_hover_cursor(egui::CursorIcon::PointingHand)
    .clicked()
}

/// 分组小标题（12px 主色，不再用琥珀色大喊）。
pub fn section_title(ui: &mut egui::Ui, text: &str) {
    let theme = crate::theme::current_theme();
    ui.label(
        egui::RichText::new(text)
            .strong()
            .size(12.0)
            .color(theme.text_primary),
    );
}

/// 字段名（11px 次弱色）。
pub fn field_label(ui: &mut egui::Ui, text: &str) {
    let theme = crate::theme::current_theme();
    ui.label(egui::RichText::new(text).size(11.0).color(theme.text_muted));
}

/// 单色扁平头像：平时浅 accent 底 + accent 字，选中时 accent 实心 + 白字。
///
/// 之前是 accent→accent2 绿到琥珀的渐变，每个头像都很抢；仪器列表里
/// 头像只是行首定位符，压扁后整列安静一个层级。
pub fn paint_avatar(
    painter: &egui::Painter,
    rect: egui::Rect,
    initial: char,
    theme: &crate::theme::Theme,
    selected: bool,
) {
    let radius = rect.width().min(rect.height()) * 0.32;
    if selected {
        painter.rect_filled(rect, radius, theme.accent);
    } else {
        painter.rect_filled(rect, radius, theme.accent.gamma_multiply(0.16));
        painter.rect_stroke(
            rect,
            radius,
            egui::Stroke::new(1.0, theme.accent.gamma_multiply(0.45)),
            egui::StrokeKind::Inside,
        );
    }
    painter.text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        initial.to_string(),
        egui::FontId::proportional((rect.width() * 0.42).max(10.0)),
        if selected {
            egui::Color32::WHITE
        } else {
            theme.accent
        },
    );
}

/// 输入框统一样式：圆角深色底、垂直居中、焦点 accent 边框、错误 danger 边框。
/// TextEdit 默认 `Align2::LEFT_TOP`（单行输入框文字偏上），这里改为垂直居中。
///
/// **egui 0.36 坑：提供自定义 frame 时 `.margin()` 被整体丢弃**
/// （`frame.unwrap_or_else(|| Frame::new().inner_margin(margin))`），
/// 而 `Frame::new()` 默认 `inner_margin` 为 ZERO——内边距必须挂在自定义 frame 上。
#[allow(clippy::too_many_arguments)]
pub fn form_input(
    ui: &mut egui::Ui,
    id: egui::Id,
    value: &mut String,
    hint: &str,
    width: f32,
    password: bool,
    error: bool,
) -> egui::Response {
    let theme = crate::theme::current_theme();
    let focused = ui.memory(|m| m.has_focus(id));
    let frame = egui::Frame::new()
        .fill(theme.bg_elevated)
        .stroke(if error {
            egui::Stroke::new(1.2, theme.danger)
        } else {
            egui::Stroke::new(
                if focused { 1.5 } else { 1.0 },
                if focused { theme.accent } else { theme.border },
            )
        })
        .corner_radius(crate::theme::tokens::RADIUS_ITEM)
        .inner_margin(egui::Margin::symmetric(10, 6));
    let mut edit = egui::TextEdit::singleline(value)
        .id(id)
        .hint_text(hint)
        .vertical_align(egui::Align::Center)
        .frame(frame)
        .text_color(theme.text_primary);
    if password {
        edit = edit.password(true);
    }
    ui.add_sized([width, 30.0], edit)
}

/// 认证方式分段开关（密码 / 私钥）：选中段 accent 软底 + accent 字。
pub fn auth_segmented(ui: &mut egui::Ui, current: &mut usize) {
    let theme = crate::theme::current_theme();
    egui::Frame::new()
        .fill(theme.bg_panel)
        .stroke(egui::Stroke::new(1.0, theme.border))
        .corner_radius(crate::theme::tokens::RADIUS_ITEM as u8)
        .inner_margin(egui::Margin::same(2))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 2.0;
                for (idx, label) in ["密码", "私钥"].iter().enumerate() {
                    let selected = *current == idx;
                    let btn = egui::Button::new(egui::RichText::new(*label).size(12.0).color(
                        if selected {
                            theme.accent
                        } else {
                            theme.text_secondary
                        },
                    ))
                    .fill(if selected {
                        theme.accent_soft
                    } else {
                        egui::Color32::TRANSPARENT
                    })
                    .stroke(egui::Stroke::NONE)
                    .min_size(egui::vec2(64.0, 26.0))
                    .corner_radius(5.0);
                    if ui
                        .add(btn)
                        .on_hover_cursor(egui::CursorIcon::PointingHand)
                        .clicked()
                    {
                        *current = idx;
                    }
                }
            });
        });
}
