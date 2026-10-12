//! Launcher 覆盖层：标签导航器（⌘⇧O）与命令面板（⌘⇧P）共用的列表选择浮层。
//!
//! 照搬 Kaku/WezTerm 的 launcher 形态：贴屏幕上缘的无标题栏浮窗，顶部过滤
//! 框、中间候选列表、底部按键提示。两个入口只差标题与数据源（打开的标签页 /
//! 可执行命令），交互与排版完全共用——拆成两个控件必然变成两套近似实现，
//! 改一处漏一处。
//!
//! 接线方式（父代理负责）：
//! ```ignore
//! // 打开：self.launcher = Launcher::command_palette(self.launcher_items());
//! // 每帧：浮层在 Foreground 层，不受调用先后影响，压在所有面板与弹窗之上。
//! if let Some(key) = self.launcher.show(ctx) {
//!     self.run_launcher_action(&key); // key 即 LauncherItem::key
//! }
//! ```

use crate::theme::{tokens, Theme};

/// 浮窗目标外宽（内容宽 = 外宽 - 两侧内边距）。
const WINDOW_W: f32 = 560.0;
/// 浮窗内边距。
const PAD: f32 = 10.0;
/// 列表可见高度上限（约 12 行，超出滚动）。
const LIST_MAX_H: f32 = 320.0;
/// 行高。
const ROW_H: f32 = 26.0;
/// 行内左右留白。
const ROW_PAD_X: f32 = 8.0;
/// 快捷键片的内边距与高度。
const CHIP_PAD_X: f32 = 6.0;
const CHIP_H: f32 = 17.0;
/// 文本列与快捷键片之间的最小间距。
const CHIP_GAP: f32 = 8.0;
/// 字号：主文本 / 次文本 / 快捷键。
const TITLE_SIZE: f32 = 12.5;
const DETAIL_SIZE: f32 = 11.0;
const SHORTCUT_SIZE: f32 = 10.5;
/// 有次文本时主文本列的宽度占比：定宽分栏让 detail 在左侧对齐成一列，
/// 命令面板里扫读副信息（目录、快捷键说明）比「跟着标题浮动」快得多。
const TITLE_COLUMN: f32 = 0.55;
/// PageUp/PageDown 步长：一屏 12 行，取 10 让前后屏留一点重叠，翻页不丢上下文。
const PAGE_STEP: usize = 10;
/// 空态占位高度（两行行高，不至于让浮窗塌成一条）。
const EMPTY_H: f32 = ROW_H * 2.0;
/// 浮窗顶端距离屏幕顶边的距离（贴上部而非正中：命令面板的视线习惯）。
const TOP_OFFSET: f32 = 88.0;

/// 一条可选项。
#[derive(Debug, Clone)]
pub struct LauncherItem {
    /// 返回值：父代理用它映射到具体动作（切换标签 / 执行命令）。
    pub key: String,
    /// 主文本。
    pub title: String,
    /// 次文本（灰字，可空）。
    pub detail: String,
    /// 右侧快捷键提示（可空）。
    pub shortcut: String,
}

/// 覆盖层状态：两个入口各持一个实例。
pub struct Launcher {
    /// 浮窗标题，同时作为窗口 Id 的来源（两个实例因此互不覆盖）。
    title: &'static str,
    open: bool,
    /// 过滤串：打开期间由输入框编辑，关闭时清空。
    query: String,
    items: Vec<LauncherItem>,
    /// 选中项在**过滤后**列表中的下标（不是 `items` 的下标）。
    selected: usize,
    /// 本帧是打开后的第一帧：把焦点交给过滤框、选中归零、列表滚回顶部。
    just_opened: bool,
}

impl Launcher {
    /// 标签导航器（⌘⇧O）：列出所有标签页。
    pub fn tab_navigator(items: Vec<LauncherItem>) -> Self {
        Self::new("标签导航", items)
    }

    /// 命令面板（⌘⇧P）：列出所有可执行命令。
    pub fn command_palette(items: Vec<LauncherItem>) -> Self {
        Self::new("命令面板", items)
    }

    fn new(title: &'static str, items: Vec<LauncherItem>) -> Self {
        Self {
            title,
            open: true,
            query: String::new(),
            items,
            selected: 0,
            just_opened: true,
        }
    }

    /// 是否处于打开状态（父代理据此决定要不要吞掉全局快捷键）。
    pub fn is_open(&self) -> bool {
        self.open
    }

    /// 收起浮层并回到初始状态：下次打开是干净的过滤串与选中行。
    pub fn close(&mut self) {
        self.open = false;
        self.query.clear();
        self.selected = 0;
        self.just_opened = false;
    }

    /// 打开期间每帧同步数据源（标签页增减、命令启停）。
    ///
    /// 选中项按 `key` 跟随而不是按下标保持：数据源在中间增删时，按下标保持
    /// 会让高亮跳到别的条目上（标签导航里就是「焦点自己跑到另一个标签」）。
    pub fn set_items(&mut self, items: Vec<LauncherItem>) {
        let prev_key = self.selected_key();
        self.items = items;
        // 旧选中项还在新数据源里 → 选中跟过去；已消失 → 交给 clamp_selection
        // 夹回范围（不能悬在原下标上，那个位置可能是别的条目）。
        let hits = filter_items(&self.query, &self.items);
        let follow = prev_key.and_then(|key| {
            hits.iter()
                .position(|&idx| self.items.get(idx).is_some_and(|i| i.key == key))
        });
        if let Some(pos) = follow {
            self.selected = pos;
        }
        self.clamp_selection();
    }

    /// 当前选中项的 key（按过滤后的列表算）。
    fn selected_key(&self) -> Option<String> {
        let hits = filter_items(&self.query, &self.items);
        hits.get(self.selected)
            .map(|&idx| self.items[idx].key.clone())
    }

    /// 把选中下标夹回过滤后列表范围内；列表为空时归 0。
    fn clamp_selection(&mut self) {
        let len = filter_items(&self.query, &self.items).len();
        self.selected = if len == 0 {
            0
        } else {
            self.selected.min(len - 1)
        };
    }

    /// 渲染浮层；`Some(key)` = 用户确认选择（Enter / 单击），`None` = 未确认。
    ///
    /// Esc 关闭并返回 `None`；确认后自动关闭——此后 `show` 直接返回 `None`，
    /// 父代理不需要再判断 `is_open`。
    pub fn show(&mut self, ctx: &egui::Context) -> Option<String> {
        if !self.open {
            return None;
        }
        let theme = crate::theme::current_theme();
        let title = self.title;
        // egui 的 Window 闭包按字段捕获外部变量，闭包里改状态会与「整体借用
        // self」的方法调用打架（`selected_key` 之类）。先把要改的状态搬进
        // 局部变量，帧末一次性回写。
        let mut query = std::mem::take(&mut self.query);
        let mut selected = self.selected;
        let just_opened = std::mem::take(&mut self.just_opened);
        let items = &self.items;
        let input_id = egui::Id::new(("launcher_input", title));

        let mut result: Option<String> = None;
        let mut close_requested = false;

        egui::Window::new(title)
            .id(egui::Id::new(("launcher_window", title)))
            // 覆盖层要压在中央面板与其它弹窗之上（Kaku/WezTerm 的命令面板
            // 都是最顶层浮层），故用 Foreground 而不是默认的 Middle。
            .order(egui::Order::Foreground)
            .anchor(egui::Align2::CENTER_TOP, [0.0, TOP_OFFSET])
            .movable(false)
            .resizable(false)
            .collapsible(false)
            .title_bar(false)
            .frame(
                egui::Frame::new()
                    .fill(theme.bg_panel)
                    .stroke(egui::Stroke::new(1.0, theme.border))
                    .corner_radius(tokens::RADIUS_SM)
                    .inner_margin(egui::Margin::same(PAD as i8))
                    .shadow(egui::epaint::Shadow {
                        offset: [0, 10],
                        blur: 24,
                        spread: 0,
                        color: egui::Color32::from_black_alpha(96),
                    }),
            )
            .show(ctx, |ui| {
                ui.set_min_width(WINDOW_W - 2.0 * PAD);
                ui.spacing_mut().item_spacing.y = 6.0;

                // 过滤框复用弹窗表单的输入框样式（bg_elevated 底 + 焦点 accent
                // 边框）。焦点在打开帧与「无人持焦点」帧交还给它：egui 0.36 在
                // Text/Key 事件帧后可能清掉焦点（kittest 必现，同 terminal_view
                // 的坑），空焦点帧不补，紧接着输入的第一个字符就丢了。
                if just_opened || ctx.memory(|m| m.focused().is_none()) {
                    // 焦点在构建输入框**之前**补：TextEdit 是先看
                    // `has_focus(id)` 再处理事件，晚一步当帧的按键就被丢弃。
                    ctx.memory_mut(|m| m.request_focus(input_id));
                }
                let filter = crate::dialog::form_input(
                    ui,
                    input_id,
                    &mut query,
                    "输入以过滤…",
                    ui.available_width(),
                    false,
                    false,
                );
                // 过滤条件变了 = 候选集换了，选中回第 0 行并把列表滚回顶部。
                let mut scroll_to_selected = just_opened || filter.changed();
                if filter.changed() {
                    selected = 0;
                }

                let hits = filter_items(&query, items);
                let len = hits.len();
                // 数据源收缩后选中的下标可能越界，先夹回范围再处理键盘。
                if selected >= len {
                    selected = len.saturating_sub(1);
                }
                ui.add_space(2.0);

                // 键盘统一在这里处理（渲染行之前，选中当帧即生效）：输入框是
                // singleline，↑↓ 只是移动光标、Ctrl+N/P 与翻页键它一概不认，
                // 与这里的消费不冲突。
                if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape)) {
                    close_requested = true;
                }
                if len > 0 && !close_requested {
                    let down = ctx
                        .input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowDown))
                        || ctx.input_mut(|i| i.consume_key(egui::Modifiers::CTRL, egui::Key::N));
                    if down {
                        selected = (selected + 1) % len;
                        scroll_to_selected = true;
                    }
                    // 环形：首行 ↑ 落到末行、末行 ↓ 回到首行（列表选择器手感）。
                    let up = ctx
                        .input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowUp))
                        || ctx.input_mut(|i| i.consume_key(egui::Modifiers::CTRL, egui::Key::P));
                    if up {
                        selected = (selected + len - 1) % len;
                        scroll_to_selected = true;
                    }
                    if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::PageDown))
                    {
                        selected = (selected + PAGE_STEP).min(len - 1);
                        scroll_to_selected = true;
                    }
                    if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::PageUp)) {
                        selected = selected.saturating_sub(PAGE_STEP);
                        scroll_to_selected = true;
                    }
                    if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Enter)) {
                        // 用 `get` 而不是下标：越界只会「没反应」，不会 panic。
                        if let Some(item) = hits.get(selected).and_then(|&idx| items.get(idx)) {
                            result = Some(item.key.clone());
                            close_requested = true;
                        }
                    }
                }

                let mut selected_rect: Option<egui::Rect> = None;
                egui::ScrollArea::vertical()
                    .id_salt(("launcher_list", title))
                    .max_height(LIST_MAX_H)
                    .auto_shrink([false, true])
                    .show(ui, |ui| {
                        // 行自己带内边距，行间不留缝。
                        ui.spacing_mut().item_spacing.y = 0.0;
                        if len == 0 {
                            empty_hint(ui, theme, &query);
                            return;
                        }
                        for (vis_idx, &item_idx) in hits.iter().enumerate() {
                            let Some(item) = items.get(item_idx) else {
                                continue;
                            };
                            let is_selected = vis_idx == selected;
                            let (row_rect, _) = ui.allocate_exact_size(
                                egui::vec2(ui.available_width(), ROW_H),
                                egui::Sense::hover(),
                            );
                            if is_selected {
                                selected_rect = Some(row_rect);
                            }
                            paint_row(ui, row_rect, item, is_selected, theme);
                            // 行点击区：显式 `ui.interact` + 稳定 Id，并且必须在
                            // 行内容**之后**注册——egui 的命中规则是「后注册者在
                            // 顶层」，先注册会被行内文字挡住（sftp 条目行同款写
                            // 法，那里的注释记着这条踩坑）。
                            let response = ui
                                .interact(
                                    row_rect,
                                    egui::Id::new(("launcher_row", title, item.key.as_str())),
                                    egui::Sense::click(),
                                )
                                .on_hover_cursor(egui::CursorIcon::PointingHand);
                            if response.clicked() {
                                result = Some(item.key.clone());
                                close_requested = true;
                            }
                        }
                        // 键盘移动后把选中行滚进视口（`align = None` = 只滚到
                        // 刚好可见，鼠标浏览时不动滚动位置）。
                        let scroll_target = if scroll_to_selected {
                            selected_rect
                        } else {
                            None
                        };
                        if let Some(rect) = scroll_target {
                            ui.scroll_to_rect(rect, None);
                        }
                    });

                ui.add_space(4.0);
                ui.label(
                    egui::RichText::new("↑↓ 选择 · Enter 确认 · Esc 关闭")
                        .size(10.5)
                        .color(theme.text_muted),
                );
            });

        self.query = query;
        self.selected = selected;
        if close_requested {
            self.close();
        }
        result
    }
}

/// 画一行：底色 → 主/次文本 → 右侧快捷键片。
///
/// 底色必须在文字之前画（painter 先画的在下层），所以悬停态用指针位置判定，
/// 而不是等行点击区的 `Response::hovered()`——那个响应在行内容之后才注册。
fn paint_row(
    ui: &mut egui::Ui,
    row_rect: egui::Rect,
    item: &LauncherItem,
    selected: bool,
    theme: &Theme,
) {
    // 上下各收 1px：相邻行的底色不糊成一片。
    let visual = row_rect.shrink2(egui::vec2(2.0, 1.0));
    if selected {
        ui.painter()
            .rect_filled(visual, tokens::RADIUS_ITEM, theme.bg_elevated);
    } else if ui.rect_contains_pointer(row_rect) {
        // 悬停底色取前景色的低透明叠加：深色主题提亮、浅色主题压暗，两套
        // 主题都成立（写死白色叠加在浅色主题里几乎看不见）。
        let fg = theme.text_primary;
        ui.painter().rect_filled(
            visual,
            tokens::RADIUS_ITEM,
            egui::Color32::from_rgba_unmultiplied(fg.r(), fg.g(), fg.b(), 16),
        );
    }

    // 快捷键片的宽度要先量出来：文本列按「行宽 - 片宽」截断，不量就会叠字。
    // 量出来的 Galley 在 epaint 的字体缓存里，逐帧重复测量只是查缓存。
    let chip_w = if item.shortcut.is_empty() {
        0.0
    } else {
        let galley = ui.painter().layout_no_wrap(
            item.shortcut.clone(),
            egui::FontId::monospace(SHORTCUT_SIZE),
            theme.text_secondary,
        );
        galley.size().x + CHIP_PAD_X * 2.0
    };
    let text_w = (row_rect.width() - ROW_PAD_X * 2.0 - chip_w - CHIP_GAP).max(32.0);

    let mut row = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(row_rect)
            .layout(egui::Layout::left_to_right(egui::Align::Center)),
    );
    row.spacing_mut().item_spacing.x = 0.0;
    row.add_space(ROW_PAD_X);
    let title_w = if item.detail.is_empty() {
        text_w
    } else {
        (text_w * TITLE_COLUMN).max(24.0)
    };
    // 主/次文本都用真 Label（不是 painter 画字）：辅助功能与 kittest 的
    // `get_by_label` 都按它们找行。
    row.add_sized(
        [title_w, ROW_H],
        egui::Label::new(
            egui::RichText::new(item.title.as_str())
                .size(TITLE_SIZE)
                .color(theme.text_primary),
        )
        .halign(egui::Align::LEFT)
        .truncate(),
    );
    if !item.detail.is_empty() {
        row.add_space(6.0);
        row.add_sized(
            [(text_w - title_w - 6.0).max(16.0), ROW_H],
            egui::Label::new(
                egui::RichText::new(item.detail.as_str())
                    .size(DETAIL_SIZE)
                    .color(theme.text_muted),
            )
            .halign(egui::Align::LEFT)
            .truncate(),
        );
    }
    if chip_w > 0.0 {
        row.add_space(CHIP_GAP);
        let (chip_rect, _) =
            row.allocate_exact_size(egui::vec2(chip_w, CHIP_H), egui::Sense::hover());
        // 选中行底色本身就是 bg_elevated，片段改用面板色才看得出边界；
        // 普通行反过来用浮层色，让快捷键从面板底上浮出来。
        row.painter().rect_filled(
            chip_rect,
            tokens::RADIUS_ITEM,
            if selected {
                theme.bg_panel
            } else {
                theme.bg_elevated
            },
        );
        row.painter().rect_stroke(
            chip_rect,
            tokens::RADIUS_ITEM,
            egui::Stroke::new(1.0, theme.border),
            egui::StrokeKind::Inside,
        );
        row.put(
            chip_rect,
            egui::Label::new(
                egui::RichText::new(item.shortcut.as_str())
                    .monospace()
                    .size(SHORTCUT_SIZE)
                    .color(theme.text_secondary),
            ),
        );
    }
}

/// 空态：过滤无命中，或数据源本身为空。
fn empty_hint(ui: &mut egui::Ui, theme: &Theme, query: &str) {
    let text = if query.is_empty() {
        "暂无可选项"
    } else {
        "无匹配项"
    };
    let (rect, _) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), EMPTY_H),
        egui::Sense::hover(),
    );
    // `put` 用居中布局，文案自然落在占位块正中。
    ui.put(
        rect,
        egui::Label::new(
            egui::RichText::new(text)
                .size(TITLE_SIZE)
                .color(theme.text_muted),
        ),
    );
}

/// 过滤 + 打分：返回命中项在 `items` 中的下标。
///
/// 空过滤保持数据源顺序（标签页顺序、命令注册顺序就是用户预期顺序）；有过滤
/// 时按分数降序、同分保持原顺序。
fn filter_items(query: &str, items: &[LauncherItem]) -> Vec<usize> {
    let mut hits: Vec<(usize, i32)> = Vec::new();
    for (idx, item) in items.iter().enumerate() {
        // title 不中时退到 detail 再试：命令面板的副文本常是用户同样会敲的
        // 关键词（目录、主机名）。
        let score = fuzzy_score(query, &item.title).or_else(|| fuzzy_score(query, &item.detail));
        if let Some(score) = score {
            hits.push((idx, score));
        }
    }
    if !query.is_empty() {
        // 同分保持原顺序（`sort_by_key` 是稳定排序）。
        hits.sort_by_key(|&(idx, score)| (-score, idx));
    }
    hits.into_iter().map(|(idx, _)| idx).collect()
}

/// 模糊匹配打分：`query` 的字符按顺序出现在 `text` 里（大小写不敏感）即命中。
///
/// 打分让「连续命中」与「开头命中」排在前面（Kaku/WezTerm 的命令面板都按命中
/// 质量排序，而不是只判能不能匹配）：
/// - 每命中一个字符 +10；
/// - 命中在文本开头 +25（用户多半记得命令/标签名的头一个词）；
/// - 与上一个命中相邻（连续子串）+15；
/// - 中间每跳过一个字符 -1，跳得越散越差（上限 8 个字符的惩罚，免得一条长
///   detail 因为空白多就被永久压到最后）；
///
/// 未命中返回 `None`；空 `query` 视为命中且得 0 分（调用方据此保持原顺序）。
fn fuzzy_score(query: &str, text: &str) -> Option<i32> {
    if query.is_empty() {
        return Some(0);
    }
    let text_chars: Vec<char> = text.to_lowercase().chars().collect();
    let mut score = 0i32;
    // text_chars 中下一个可匹配的位置。
    let mut cursor = 0usize;
    let mut prev: Option<usize> = None;
    for qc in query.to_lowercase().chars() {
        // 取最早的命中：早期命中给后面留出更多「连续」的机会，也让开头加分的
        // 判断真正落在前缀上。
        let found = text_chars[cursor..]
            .iter()
            .position(|c| *c == qc)
            .map(|offset| cursor + offset)?;
        score += 10;
        if found == 0 {
            score += 25;
        }
        match prev {
            Some(p) if found == p + 1 => score += 15,
            Some(p) => score -= ((found - p - 1).min(8)) as i32,
            None => {}
        }
        prev = Some(found);
        cursor = found + 1;
    }
    Some(score)
}

#[cfg(test)]
mod tests {
    use super::*;
    use kittest::Queryable;
    use std::cell::RefCell;
    use std::rc::Rc;

    fn item(key: &str, title: &str, detail: &str, shortcut: &str) -> LauncherItem {
        LauncherItem {
            key: key.to_string(),
            title: title.to_string(),
            detail: detail.to_string(),
            shortcut: shortcut.to_string(),
        }
    }

    /// 三条候选：一条中文标题、一条带 detail、一条纯命令。
    fn sample() -> Vec<LauncherItem> {
        vec![
            item("tab.1", "zsh · 主目录", "~/Desktop/WorkSpace", "⇧⌘1"),
            item("tab.2", "部署服务", "/srv/deploy", "⇧⌘2"),
            item("cmd.new", "新建标签页", "⌘T 新建本地终端", "⌘T"),
        ]
    }

    // ==================== 模糊匹配（纯函数） ====================

    #[test]
    fn 模糊匹配命中与不命中() {
        assert!(fuzzy_score("quo", "Quota").is_some(), "子序列应命中");
        assert!(fuzzy_score("q", "Quota").is_some());
        assert!(fuzzy_score("xyz", "Quota").is_none(), "乱序/缺字符不应命中");
        assert!(fuzzy_score("uoq", "Quota").is_none(), "顺序颠倒不应命中");
        // 空查询视为命中且不加分。
        assert_eq!(fuzzy_score("", "Quota"), Some(0));
    }

    #[test]
    fn 模糊匹配连续优于跳字符() {
        let contiguous = fuzzy_score("tab", "table").expect("连续子串应命中");
        let gapped = fuzzy_score("tab", "t-a-b").expect("跨字符应命中");
        assert!(
            contiguous > gapped,
            "连续命中应排在跳字符之前：连续 {contiguous} 跳字符 {gapped}"
        );
    }

    #[test]
    fn 模糊匹配开头优先且大小写不敏感() {
        let prefix = fuzzy_score("tab", "tab x").expect("应命中");
        let middle = fuzzy_score("tab", "x tab").expect("应命中");
        assert!(
            prefix > middle,
            "开头命中应优先：开头 {prefix} 中段 {middle}"
        );
        assert_eq!(fuzzy_score("TAB", "tab x"), Some(prefix), "大小写不敏感");
        assert_eq!(
            fuzzy_score("tab", "TAB X"),
            Some(prefix),
            "文本侧同样不敏感"
        );
    }

    #[test]
    fn 中文查询按字符而不是字节匹配() {
        // 逐字节比较会把「标签」拆成 6 个字节，命中质量断言就不成立了。
        assert!(fuzzy_score("标签", "关闭标签页").is_some());
        assert!(fuzzy_score("签标", "标签").is_none());
    }

    // ==================== 过滤排序 ====================

    #[test]
    fn 空过滤保持数据源顺序() {
        let l = Launcher::command_palette(sample());
        assert_eq!(filter_items("", &l.items), vec![0, 1, 2]);
    }

    #[test]
    fn 过滤按命中质量排序且同分保持原顺序() {
        let l = Launcher::command_palette(vec![
            item("a", "关闭标签页", "", ""),
            item("b", "打开标签导航", "", ""),
            item("c", "标签", "", ""),
        ]);
        // "c" 是前缀命中（得分最高）；"a"/"b" 都是中段命中且同分 → 保持原顺序。
        assert_eq!(filter_items("标签", &l.items), vec![2, 0, 1]);
    }

    #[test]
    fn 过滤能命中次文本() {
        let l = Launcher::command_palette(sample());
        // "deploy" 只出现在第二条的 detail 里。
        assert_eq!(filter_items("deploy", &l.items), vec![1]);
        assert!(filter_items("不存在", &l.items).is_empty());
    }

    // ==================== 数据源同步 ====================

    #[test]
    fn 同步数据源按key跟随选中项() {
        let mut l = Launcher::command_palette(vec![
            item("a", "第一", "", ""),
            item("b", "第二", "", ""),
            item("c", "第三", "", ""),
        ]);
        l.selected = 2;
        // 数据源开头插入一项：按下标会漂到「第二」，按 key 才跟得住。
        l.set_items(vec![
            item("x", "新增", "", ""),
            item("a", "第一", "", ""),
            item("b", "第二", "", ""),
            item("c", "第三", "", ""),
        ]);
        assert_eq!(
            l.selected_key().as_deref(),
            Some("c"),
            "选中项应按 key 跟随"
        );
    }

    #[test]
    fn 过滤后同步数据源选中不越界() {
        let mut l = Launcher::command_palette(vec![
            item("a", "标签一", "", ""),
            item("b", "标签二", "", ""),
            item("c", "关闭窗口", "", ""),
        ]);
        l.query = "标签".to_string();
        assert_eq!(filter_items(&l.query, &l.items).len(), 2, "两条含「标签」");
        l.selected = 1;
        // 新数据源把「标签二」删掉了：匹配项从 2 条降到 1 条，选中必须夹回 0，
        // 否则下一帧 show 会拿着越界下标去取候选行。
        l.set_items(vec![item("a", "标签一", "", "")]);
        assert_eq!(filter_items(&l.query, &l.items).len(), 1);
        assert_eq!(l.selected, 0);
    }

    #[test]
    fn 关闭后不再渲染也不返回结果() {
        let ctx = egui::Context::default();
        let mut l = Launcher::tab_navigator(sample());
        assert!(l.is_open());
        // 打开状态在非测试环境由 ctx.run 驱动；这里只验证关闭分支不再产出。
        l.close();
        assert!(!l.is_open());
        assert!(l.show(&ctx).is_none());
    }

    // ==================== kittest 渲染与交互 ====================

    /// kittest 驱动器：照抄父代理的接法（每帧 `show`），只在确认时记录返回值
    /// ——关闭后的帧返回 `None`，不能把上一次的确认结果覆盖掉。
    fn drive(
        launcher: Rc<RefCell<Launcher>>,
        confirmed: Rc<RefCell<Option<String>>>,
    ) -> egui_kittest::Harness<'static> {
        egui_kittest::Harness::builder()
            .with_size(egui::Vec2::new(900.0, 640.0))
            .build_ui(move |ui| {
                let ctx = ui.ctx().clone();
                if let Some(key) = launcher.borrow_mut().show(&ctx) {
                    *confirmed.borrow_mut() = Some(key);
                }
            })
    }

    #[test]
    fn kittest_打开渲染标题候选行与提示() {
        let launcher = Rc::new(RefCell::new(Launcher::command_palette(sample())));
        let confirmed = Rc::new(RefCell::new(None));
        let mut harness = drive(launcher, confirmed);
        harness.run_steps(3);

        // 浮窗标题（Window 节点的无障碍 label）与每行的主文本都是真 Label。
        let window = harness.get_by_label("命令面板");
        harness.get_by_label("zsh · 主目录");
        harness.get_by_label("部署服务");
        harness.get_by_label("新建标签页");
        harness.get_by_label("↑↓ 选择 · Enter 确认 · Esc 关闭");
        // 居中偏上：只断言水平居中（垂直位置由 TOP_OFFSET 定）。
        let center_x = window.rect().center().x;
        let viewport_x = harness.ctx.content_rect().center().x;
        assert!(
            (center_x - viewport_x).abs() < 1.0,
            "浮窗应水平居中：浮窗中心 {center_x}，视口中心 {viewport_x}"
        );
    }

    #[test]
    fn kittest_标签导航标题与行() {
        let launcher = Rc::new(RefCell::new(Launcher::tab_navigator(sample())));
        let confirmed = Rc::new(RefCell::new(None));
        let mut harness = drive(launcher, confirmed);
        harness.run_steps(3);
        harness.get_by_label("标签导航");
        harness.get_by_label("部署服务");
    }

    #[test]
    fn kittest_输入过滤后只剩命中行() {
        let launcher = Rc::new(RefCell::new(Launcher::command_palette(sample())));
        let confirmed = Rc::new(RefCell::new(None));
        let mut harness = drive(launcher, confirmed);
        harness.run_steps(3);
        assert!(harness.query_by_label("部署服务").is_some());

        // 打开帧已把焦点交给过滤框，这里模拟键盘输入。
        harness.event(egui::Event::Text("部署".to_string()));
        harness.run_steps(2);
        harness.get_by_label("部署服务");
        assert!(
            harness.query_by_label("zsh · 主目录").is_none(),
            "未命中的行应被过滤掉"
        );
        assert!(harness.query_by_label("新建标签页").is_none());

        // 再补一个字符到「无匹配项」：空态文案出现，候选行全消失。
        harness.event(egui::Event::Text("xyz".to_string()));
        harness.run_steps(2);
        harness.get_by_label("无匹配项");
        assert!(harness.query_by_label("部署服务").is_none());
    }

    #[test]
    fn kittest_回车确认选中并自动关闭() {
        let launcher = Rc::new(RefCell::new(Launcher::command_palette(sample())));
        let confirmed = Rc::new(RefCell::new(None));
        let mut harness = drive(launcher.clone(), confirmed.clone());
        harness.run_steps(3);

        harness.key_press(egui::Key::ArrowDown);
        harness.run_steps(2);
        harness.key_press(egui::Key::Enter);
        harness.run_steps(2);

        assert_eq!(
            confirmed.borrow().as_deref(),
            Some("tab.2"),
            "Enter 应返回选中项的 key"
        );
        assert!(!launcher.borrow().is_open(), "确认后浮层应自动关闭");
        assert!(
            harness.query_by_label("命令面板").is_none(),
            "关闭后不应再渲染候选行"
        );
    }

    #[test]
    fn kittest_方向键与ctrl_n环形移动选中() {
        let launcher = Rc::new(RefCell::new(Launcher::command_palette(sample())));
        let confirmed = Rc::new(RefCell::new(None));
        let mut harness = drive(launcher, confirmed.clone());
        harness.run_steps(3);

        // Ctrl+N 下移一行，再两次 ↑：首行 ↑ 应环绕到末行。
        harness.key_press_modifiers(egui::Modifiers::CTRL, egui::Key::N);
        harness.run_steps(2);
        harness.key_press(egui::Key::ArrowUp);
        harness.run_steps(2);
        harness.key_press(egui::Key::ArrowUp);
        harness.run_steps(2);
        harness.key_press(egui::Key::Enter);
        harness.run_steps(2);

        assert_eq!(confirmed.borrow().as_deref(), Some("cmd.new"));
    }

    #[test]
    fn kittest_翻页键按整屏步长移动选中() {
        // 30 条候选：列表只画得下 12 行，翻页键要让选中跨屏移动。
        let items: Vec<LauncherItem> = (0..30)
            .map(|i| item(&format!("k{i}"), &format!("命令{i}"), "", ""))
            .collect();
        let launcher = Rc::new(RefCell::new(Launcher::command_palette(items)));
        let confirmed = Rc::new(RefCell::new(None));
        let mut harness = drive(launcher.clone(), confirmed.clone());
        harness.run_steps(3);

        harness.key_press(egui::Key::PageDown);
        harness.run_steps(2);
        assert_eq!(
            launcher.borrow().selected,
            PAGE_STEP,
            "PageDown 应下移一屏（{PAGE_STEP} 行）"
        );
        harness.key_press(egui::Key::PageUp);
        harness.run_steps(2);
        assert_eq!(launcher.borrow().selected, 0, "PageUp 应回到首行");

        harness.key_press(egui::Key::Enter);
        harness.run_steps(2);
        assert_eq!(confirmed.borrow().as_deref(), Some("k0"));
    }

    #[test]
    fn kittest_单击行确认() {
        let launcher = Rc::new(RefCell::new(Launcher::command_palette(sample())));
        let confirmed = Rc::new(RefCell::new(None));
        let mut harness = drive(launcher.clone(), confirmed.clone());
        harness.run_steps(3);

        harness.get_by_label("部署服务").click();
        harness.run_steps(3);

        assert_eq!(confirmed.borrow().as_deref(), Some("tab.2"));
        assert!(!launcher.borrow().is_open());
    }

    #[test]
    fn kittest_esc键关闭返回无结果() {
        let launcher = Rc::new(RefCell::new(Launcher::command_palette(sample())));
        let confirmed = Rc::new(RefCell::new(None));
        let mut harness = drive(launcher.clone(), confirmed.clone());
        harness.run_steps(3);

        harness.key_press(egui::Key::Escape);
        harness.run_steps(2);

        assert!(confirmed.borrow().is_none(), "Esc 不应产生确认结果");
        assert!(!launcher.borrow().is_open());
        assert!(harness.query_by_label("命令面板").is_none());
    }
}
