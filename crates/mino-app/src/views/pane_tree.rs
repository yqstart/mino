//! 分屏布局树：按二叉树把窗口矩形递归切给各窗格，并提供拆分/关闭/调比/
//! 方向导航等纯树操作。
//!
//! 只做几何与结构变换：不持有状态、不画 UI——窗格与会话的映射、焦点、
//! 快捷键都留在 app 层，核心逻辑可以脱离 egui 上下文直接构造与断言。
//!
//! 轴向沿用 WezTerm `SplitHorizontal` 的命名语义：`Horizontal` 是左右并排
//! （Kaku `Cmd+D`），`Vertical` 是上下堆叠（Kaku `Cmd+Shift+D`）；名字描述
//! 窗格的排布方向，而不是分隔线本身的方向。

use egui::{pos2, Rect};

/// 拆分方向。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SplitAxis {
    /// 左右并排：`first` 在左，新窗格出现在右侧。
    Horizontal,
    /// 上下堆叠：`first` 在上，新窗格出现在下方。
    Vertical,
}

/// 布局树节点：叶子持窗格 id，内部节点持拆分方向与 `first` 的占比。
#[derive(Clone, Debug, PartialEq)]
pub enum PaneNode {
    /// 单个窗格。
    Leaf(u64),
    /// 一次拆分：`first` 占左/上段，`second` 占右/下段。
    Split {
        axis: SplitAxis,
        /// `first` 的占比；`adjust_ratio` 把它钳制在 [MIN_RATIO, MAX_RATIO]。
        ratio: f32,
        first: Box<PaneNode>,
        second: Box<PaneNode>,
    },
}

/// 分隔条比例的下限/上限：两侧各留一成余量，连按快捷键或拖拽都不会把窗格
/// 压成不可用的窄条。
const MIN_RATIO: f32 = 0.1;
const MAX_RATIO: f32 = 0.9;

/// 构造只有单个窗格的树。
pub fn leaf(id: u64) -> PaneNode {
    PaneNode::Leaf(id)
}

/// 按 [`layout`] 的输出顺序收集叶子 id（深度优先，先 `first` 后 `second`）。
pub fn leaves(node: &PaneNode) -> Vec<u64> {
    let mut out = Vec::new();
    collect_leaves(node, &mut out);
    out
}

fn collect_leaves(node: &PaneNode, out: &mut Vec<u64>) {
    match node {
        PaneNode::Leaf(id) => out.push(*id),
        PaneNode::Split { first, second, .. } => {
            collect_leaves(first, out);
            collect_leaves(second, out);
        }
    }
}

/// 树中是否存在窗格 `id`。
fn contains_leaf(node: &PaneNode, id: u64) -> bool {
    match node {
        PaneNode::Leaf(x) => *x == id,
        PaneNode::Split { first, second, .. } => {
            contains_leaf(first, id) || contains_leaf(second, id)
        }
    }
}

/// `node` 是否恰为窗格 `id` 的叶子。
fn is_leaf_node(node: &PaneNode, id: u64) -> bool {
    matches!(node, PaneNode::Leaf(x) if *x == id)
}

/// 按树把 `rect` 分配成各窗格的矩形，结果**追加**到 `out`（不清空）。
///
/// 输出顺序稳定：深度优先、先 `first` 后 `second`。`gap` 是相邻窗格的空隙
/// （逻辑点）：每个分隔条占住 gap，一半落在 first 侧、一半落在 second 侧，
/// 于是窗格之间的空隙恰为 gap，而最外层边缘不留空隙（窗格贴齐 `rect`）。
pub fn layout(node: &PaneNode, rect: Rect, gap: f32, out: &mut Vec<(u64, Rect)>) {
    match node {
        PaneNode::Leaf(id) => out.push((*id, rect)),
        PaneNode::Split {
            axis,
            ratio,
            first,
            second,
        } => {
            let (first_rect, second_rect) = split_rects(rect, *axis, *ratio, gap);
            layout(first, first_rect, gap, out);
            layout(second, second_rect, gap, out);
        }
    }
}

/// 计算一次拆分：分隔条中线落在 ratio 位置，两侧各退 gap/2。
///
/// 窗口比 gap 还窄等退化情形下把窗格压到 0 尺寸而不是让它越过对侧——反向
/// 矩形（min > max）在绘制与命中里都是坏状态，宁可退化也不要。
fn split_rects(rect: Rect, axis: SplitAxis, ratio: f32, gap: f32) -> (Rect, Rect) {
    let half = (gap * 0.5).max(0.0);
    let ratio = ratio.clamp(0.0, 1.0);
    match axis {
        SplitAxis::Horizontal => {
            let cut = rect.min.x + rect.width() * ratio;
            let first_end = (cut - half).max(rect.min.x);
            let second_start = (cut + half).min(rect.max.x);
            (
                Rect::from_min_max(rect.min, pos2(first_end, rect.max.y)),
                Rect::from_min_max(pos2(second_start, rect.min.y), rect.max),
            )
        }
        SplitAxis::Vertical => {
            let cut = rect.min.y + rect.height() * ratio;
            let first_end = (cut - half).max(rect.min.y);
            let second_start = (cut + half).min(rect.max.y);
            (
                Rect::from_min_max(rect.min, pos2(rect.max.x, first_end)),
                Rect::from_min_max(pos2(rect.min.x, second_start), rect.max),
            )
        }
    }
}

/// 把叶子 `target` 一分为二：旧窗格留在 `first`（左/上），新窗格 `new_id`
/// 出现在 `second`（右/下，与 WezTerm 一致），ratio 取 0.5。
/// 树中没有 `target` 时返回 false，不做任何修改。
pub fn split_leaf(node: &mut PaneNode, target: u64, new_id: u64, axis: SplitAxis) -> bool {
    match node {
        PaneNode::Leaf(id) => {
            if *id != target {
                return false;
            }
            *node = PaneNode::Split {
                axis,
                ratio: 0.5,
                first: Box::new(PaneNode::Leaf(target)),
                second: Box::new(PaneNode::Leaf(new_id)),
            };
            true
        }
        PaneNode::Split { first, second, .. } => {
            split_leaf(first, target, new_id, axis) || split_leaf(second, target, new_id, axis)
        }
    }
}

/// [`remove_leaf`] 的结果。
#[derive(Clone, Debug, PartialEq)]
pub enum RemoveOutcome {
    /// 已移除：兄弟子树就地顶替父节点的位置（父若是根，根即变成那个兄弟）。
    Removed,
    /// 目标叶子是整棵树唯一的叶子：移除后树为空，调用方应丢弃这棵树
    /// （例如关闭对应标签页）。`node` 保持原样，不能再当有效布局使用。
    Emptied,
    /// 树中没有该 id，未做任何修改。
    NotFound,
}

/// 从树中移除叶子 `id`。
pub fn remove_leaf(node: &mut PaneNode, id: u64) -> RemoveOutcome {
    match node {
        PaneNode::Leaf(x) => {
            if *x == id {
                RemoveOutcome::Emptied
            } else {
                RemoveOutcome::NotFound
            }
        }
        PaneNode::Split { first, second, .. } => {
            // 目标恰为直接子叶子：本节点（可能正是根）被兄弟顶替
            let promoted = if is_leaf_node(first, id) {
                Some((**second).clone())
            } else if is_leaf_node(second, id) {
                Some((**first).clone())
            } else {
                None
            };
            if let Some(promoted) = promoted {
                *node = promoted;
                return RemoveOutcome::Removed;
            }
            // 更深层：递归进含目标的子树，替换在那一层就地完成。
            // 子 Split 不会返回 Emptied——Emptied 只可能是「整树即叶子」。
            if contains_leaf(first, id) {
                return remove_leaf(first, id);
            }
            if contains_leaf(second, id) {
                return remove_leaf(second, id);
            }
            RemoveOutcome::NotFound
        }
    }
}

/// 调整包含 `near` 的、离它最近的、方向等于 `axis` 的分隔条比例：
/// `ratio += delta`，钳制到 [MIN_RATIO, MAX_RATIO]；找不到返回 false。
///
/// 「最近」= 根到 `near` 路径上最深的那一个：沿途不断下探，只有更深的子树
/// 里没有同轴分隔条时才轮到本节点；本节点不同轴就继续上溯交给祖先。
pub fn adjust_ratio(node: &mut PaneNode, near: u64, axis: SplitAxis, delta: f32) -> bool {
    match node {
        PaneNode::Leaf(_) => false,
        PaneNode::Split {
            axis: node_axis,
            ratio,
            first,
            second,
        } => {
            let deeper = if contains_leaf(first, near) {
                adjust_ratio(first, near, axis, delta)
            } else if contains_leaf(second, near) {
                adjust_ratio(second, near, axis, delta)
            } else {
                return false;
            };
            if deeper {
                true
            } else if *node_axis == axis {
                *ratio = (*ratio + delta).clamp(MIN_RATIO, MAX_RATIO);
                true
            } else {
                false
            }
        }
    }
}

/// 翻转包含 `id` 的、离它最近的那一层拆分的轴向（左右并排 ⇄ 上下堆叠），
/// 比例保持（Kaku `Cmd+Shift+S` TogglePaneSplitDirection）。
///
/// 树里没有该窗格、或它就是树里唯一的叶子（没有可翻的拆分）时返回 false。
/// 翻转只交换 `first` 的排布方向，不交换两个子树——`first` 始终待在左上侧，
/// 避免窗格位置跳变。
pub fn flip_axis(node: &mut PaneNode, id: u64) -> bool {
    match node {
        PaneNode::Leaf(_) => false,
        PaneNode::Split {
            axis,
            first,
            second,
            ..
        } => {
            if contains_leaf(first, id) {
                if flip_axis(first, id) {
                    return true;
                }
            } else if contains_leaf(second, id) {
                if flip_axis(second, id) {
                    return true;
                }
            } else {
                return false;
            }
            // 更深的拆分都不含 id：本层就是最近的一层
            *axis = match *axis {
                SplitAxis::Horizontal => SplitAxis::Vertical,
                SplitAxis::Vertical => SplitAxis::Horizontal,
            };
            true
        }
    }
}

/// 几何邻居：`from` 在 (dx, dy) 方向上的最近窗格。
///
/// 先按方向过滤：候选在移动方向上的近侧边缘必须越过 `from` 的远侧边缘
/// （不重叠的布局里即「整个位于该方向」）。再排序：正交方向（上下/左右）的
/// 中心距离优先，方向上的中心距离次之——正对面的窗格优先于斜向更近的。
///
/// 调用方一次只给一个方向分量（方向键导航）；两个分量都非 0 时按水平处理，
/// 全为 0 或 `from` 不存在时返回 None。
pub fn neighbor(rects: &[(u64, Rect)], from: u64, dx: i32, dy: i32) -> Option<u64> {
    let from_rect = rects
        .iter()
        .find(|(id, _)| *id == from)
        .map(|(_, rect)| *rect)?;
    let horizontal = dx != 0;
    let sign = if horizontal { dx.signum() } else { dy.signum() };
    if sign == 0 {
        return None;
    }
    let from_center = from_rect.center();
    let mut best: Option<(f32, f32, u64)> = None;
    for &(id, rect) in rects {
        if id == from {
            continue;
        }
        let beyond = if horizontal {
            if sign > 0 {
                rect.min.x >= from_rect.max.x
            } else {
                rect.max.x <= from_rect.min.x
            }
        } else if sign > 0 {
            rect.min.y >= from_rect.max.y
        } else {
            rect.max.y <= from_rect.min.y
        };
        if !beyond {
            continue;
        }
        let center = rect.center();
        let (across, along) = if horizontal {
            (
                (center.y - from_center.y).abs(),
                sign as f32 * (center.x - from_center.x),
            )
        } else {
            (
                (center.x - from_center.x).abs(),
                sign as f32 * (center.y - from_center.y),
            )
        };
        // 严格小于：并列时保留先出现的候选，结果只依赖输入顺序，不依赖浮点比较的末次状态
        if best
            .is_none_or(|(across_best, along_best, _)| (across, along) < (across_best, along_best))
        {
            best = Some((across, along, id));
        }
    }
    best.map(|(_, _, id)| id)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 100×50 的标准矩形：比例与 gap 的断言都基于它的心算。
    fn full_rect() -> Rect {
        Rect::from_min_max(pos2(0.0, 0.0), pos2(100.0, 50.0))
    }

    /// 三窗格 L 形：左列上下两个（1 上 / 2 下），右侧 3 通栏——
    /// 3 个窗格的切分形状只有「一列分两个 + 另一列通栏」这类。
    fn l_shape() -> PaneNode {
        PaneNode::Split {
            axis: SplitAxis::Horizontal,
            ratio: 0.5,
            first: Box::new(PaneNode::Split {
                axis: SplitAxis::Vertical,
                ratio: 0.5,
                first: Box::new(leaf(1)),
                second: Box::new(leaf(2)),
            }),
            second: Box::new(leaf(3)),
        }
    }

    fn rects_of(node: &PaneNode, rect: Rect, gap: f32) -> Vec<(u64, Rect)> {
        let mut out = Vec::new();
        layout(node, rect, gap, &mut out);
        out
    }

    fn rect_of(rects: &[(u64, Rect)], id: u64) -> Rect {
        rects
            .iter()
            .find(|(x, _)| *x == id)
            .map(|(_, r)| *r)
            .expect("窗格应存在")
    }

    fn root_ratio(node: &PaneNode) -> f32 {
        match node {
            PaneNode::Split { ratio, .. } => *ratio,
            PaneNode::Leaf(_) => panic!("根应为拆分节点"),
        }
    }

    fn root_axis(node: &PaneNode) -> SplitAxis {
        match node {
            PaneNode::Split { axis, .. } => *axis,
            PaneNode::Leaf(_) => panic!("根应为拆分节点"),
        }
    }

    fn first_child(node: &PaneNode) -> &PaneNode {
        match node {
            PaneNode::Split { first, .. } => first,
            PaneNode::Leaf(_) => panic!("需要拆分节点"),
        }
    }

    #[test]
    fn 左右拆分两窗格时间隙恰为gap且贴外缘() {
        let mut tree = leaf(1);
        assert!(split_leaf(&mut tree, 1, 2, SplitAxis::Horizontal));

        let out = rects_of(&tree, full_rect(), 8.0);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].0, 1, "先 first");
        assert_eq!(out[1].0, 2, "后 second");

        let first = out[0].1;
        let second = out[1].1;
        assert_eq!(second.min.x - first.max.x, 8.0, "相邻窗格间隙恰为 gap");
        assert_eq!(
            (first.max.x + second.min.x) * 0.5,
            50.0,
            "分隔条中线落在 ratio 位置"
        );
        assert_eq!(first.min, full_rect().min, "first 贴外缘");
        assert_eq!(second.max, full_rect().max, "second 贴外缘");
        assert!(!first.intersects(second), "两窗格不重叠");
        assert_eq!(
            first.area() + second.area(),
            100.0 * 50.0 - 8.0 * 50.0,
            "面积 = 原矩形 - 分隔条"
        );

        // layout 是追加语义：再算一遍得到第二组而非覆盖
        let mut doubled = out.clone();
        layout(&tree, full_rect(), 8.0, &mut doubled);
        assert_eq!(doubled.len(), 4, "layout 追加而非清空");
    }

    #[test]
    fn 上下拆分间隙落在纵向分隔处() {
        let mut tree = leaf(1);
        assert!(split_leaf(&mut tree, 1, 2, SplitAxis::Vertical));

        let out = rects_of(&tree, full_rect(), 8.0);
        let first = out[0].1;
        let second = out[1].1;
        assert_eq!(second.min.y - first.max.y, 8.0, "相邻窗格间隙恰为 gap");
        assert_eq!(first.min, full_rect().min);
        assert_eq!(second.max, full_rect().max);
        assert!(!first.intersects(second));
        assert_eq!(first.area() + second.area(), 100.0 * 50.0 - 8.0 * 100.0);
    }

    #[test]
    fn 拆分新窗格在右下且结果不重叠() {
        let mut tree = leaf(1);
        assert!(
            !split_leaf(&mut tree, 9, 2, SplitAxis::Horizontal),
            "目标不存在"
        );
        assert_eq!(tree, leaf(1), "失败不改动树");

        assert!(split_leaf(&mut tree, 1, 2, SplitAxis::Horizontal));
        assert!(split_leaf(&mut tree, 1, 3, SplitAxis::Vertical));
        assert_eq!(leaves(&tree), vec![1, 3, 2], "深度优先：先 first 后 second");

        let square = Rect::from_min_max(pos2(0.0, 0.0), pos2(100.0, 100.0));
        let out = rects_of(&tree, square, 0.0);
        let (r1, r2, r3) = (rect_of(&out, 1), rect_of(&out, 2), rect_of(&out, 3));
        assert_eq!(r3.min.y, r1.max.y, "新窗格 3 在 1 的下方");
        assert_eq!(r2.min.x, r1.max.x, "新窗格 2 在 1 的右侧");
        for (a, b) in [(r1, r2), (r1, r3), (r2, r3)] {
            assert_eq!(a.intersect(b).area(), 0.0, "窗格之间不能重叠");
        }
    }

    #[test]
    fn l形布局几何与叶子顺序() {
        let square = Rect::from_min_max(pos2(0.0, 0.0), pos2(100.0, 100.0));
        let out = rects_of(&l_shape(), square, 0.0);
        assert_eq!(
            out.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
        assert_eq!(
            rect_of(&out, 1),
            Rect::from_min_max(pos2(0.0, 0.0), pos2(50.0, 50.0))
        );
        assert_eq!(
            rect_of(&out, 2),
            Rect::from_min_max(pos2(0.0, 50.0), pos2(50.0, 100.0))
        );
        assert_eq!(
            rect_of(&out, 3),
            Rect::from_min_max(pos2(50.0, 0.0), pos2(100.0, 100.0))
        );
        assert_eq!(leaves(&l_shape()), vec![1, 2, 3]);
    }

    #[test]
    fn 邻居四方向含l形() {
        let square = Rect::from_min_max(pos2(0.0, 0.0), pos2(100.0, 100.0));
        let out = rects_of(&l_shape(), square, 0.0);

        assert_eq!(neighbor(&out, 1, 1, 0), Some(3), "左上向右 → 右侧通栏 3");
        assert_eq!(neighbor(&out, 1, 0, 1), Some(2), "左上向下 → 左下");
        assert_eq!(neighbor(&out, 2, 0, -1), Some(1), "左下向上 → 左上");
        assert_eq!(
            neighbor(&out, 3, -1, 0),
            Some(1),
            "通栏 3 向左：1、2 等距，并列取输出顺序里先出现的"
        );
        assert_eq!(neighbor(&out, 2, 1, 0), Some(3), "左下向右 → 右侧通栏 3");

        assert_eq!(neighbor(&out, 1, -1, 0), None, "左侧没有窗格");
        assert_eq!(neighbor(&out, 1, 0, -1), None, "上方没有窗格");
        assert_eq!(neighbor(&out, 3, 1, 0), None, "右侧没有窗格");
        assert_eq!(neighbor(&out, 3, 0, 1), None, "下方没有窗格");
        assert_eq!(neighbor(&out, 3, 0, -1), None, "上方没有窗格");
        assert_eq!(neighbor(&out, 99, 1, 0), None, "from 不存在");
        assert_eq!(neighbor(&out, 1, 0, 0), None, "零方向");
    }

    #[test]
    fn 邻居取方向上最近的窗格() {
        // 一排三窗格：1 宽 50，2、3 各 25
        let tree = PaneNode::Split {
            axis: SplitAxis::Horizontal,
            ratio: 0.5,
            first: Box::new(leaf(1)),
            second: Box::new(PaneNode::Split {
                axis: SplitAxis::Horizontal,
                ratio: 0.5,
                first: Box::new(leaf(2)),
                second: Box::new(leaf(3)),
            }),
        };
        let out = rects_of(&tree, full_rect(), 0.0);
        assert_eq!(
            neighbor(&out, 1, 1, 0),
            Some(2),
            "向右取最近的 2，而不是更远的 3"
        );
        assert_eq!(neighbor(&out, 3, -1, 0), Some(2), "向左同理");
    }

    #[test]
    fn 移除叶子三态与兄弟上提() {
        let mut only = leaf(7);
        assert_eq!(
            remove_leaf(&mut only, 7),
            RemoveOutcome::Emptied,
            "整树即目标叶子"
        );
        assert_eq!(remove_leaf(&mut only, 8), RemoveOutcome::NotFound);

        let mut two = PaneNode::Split {
            axis: SplitAxis::Horizontal,
            ratio: 0.3,
            first: Box::new(leaf(1)),
            second: Box::new(leaf(2)),
        };
        assert_eq!(remove_leaf(&mut two, 1), RemoveOutcome::Removed);
        assert_eq!(two, leaf(2), "根被兄弟顶替");
        assert_eq!(
            rects_of(&two, full_rect(), 8.0),
            vec![(2, full_rect())],
            "上提后独占整块区域，不留 gap"
        );

        // 深一层：父节点不是根，父的位置被兄弟顶替
        let mut nested = PaneNode::Split {
            axis: SplitAxis::Horizontal,
            ratio: 0.5,
            first: Box::new(leaf(1)),
            second: Box::new(PaneNode::Split {
                axis: SplitAxis::Vertical,
                ratio: 0.5,
                first: Box::new(leaf(2)),
                second: Box::new(leaf(3)),
            }),
        };
        assert_eq!(remove_leaf(&mut nested, 2), RemoveOutcome::Removed);
        assert_eq!(
            nested,
            PaneNode::Split {
                axis: SplitAxis::Horizontal,
                ratio: 0.5,
                first: Box::new(leaf(1)),
                second: Box::new(leaf(3)),
            },
            "兄弟 3 顶替父节点"
        );
        assert_eq!(remove_leaf(&mut nested, 1), RemoveOutcome::Removed);
        assert_eq!(nested, leaf(3), "根被兄弟顶替");
        assert_eq!(remove_leaf(&mut nested, 99), RemoveOutcome::NotFound);
        assert_eq!(nested, leaf(3), "未找到的不改动树");
    }

    #[test]
    fn 调整比例钳制到合法区间() {
        let mut tree = PaneNode::Split {
            axis: SplitAxis::Horizontal,
            ratio: 0.5,
            first: Box::new(leaf(1)),
            second: Box::new(leaf(2)),
        };
        assert!(adjust_ratio(&mut tree, 1, SplitAxis::Horizontal, 0.8));
        assert_eq!(root_ratio(&tree), 0.9, "上限钳制");
        assert!(adjust_ratio(&mut tree, 1, SplitAxis::Horizontal, -0.8));
        assert_eq!(root_ratio(&tree), 0.1, "下限钳制");
        assert!(
            !adjust_ratio(&mut tree, 9, SplitAxis::Horizontal, 0.1),
            "目标不存在"
        );
        assert!(
            !adjust_ratio(&mut tree, 1, SplitAxis::Vertical, 0.1),
            "没有同轴分隔条"
        );
        assert_eq!(root_ratio(&tree), 0.1, "失败时不改动比例");

        // 已钳在边界：仍返回 true（分隔条命中，只是不再变化）
        assert!(adjust_ratio(&mut tree, 1, SplitAxis::Horizontal, -0.05));
        assert_eq!(root_ratio(&tree), 0.1);
    }

    #[test]
    fn 调整比例选离目标最近的同轴分隔条() {
        let mut tree = PaneNode::Split {
            axis: SplitAxis::Horizontal,
            ratio: 0.5,
            first: Box::new(PaneNode::Split {
                axis: SplitAxis::Vertical,
                ratio: 0.5,
                first: Box::new(leaf(1)),
                second: Box::new(leaf(2)),
            }),
            second: Box::new(leaf(3)),
        };

        assert!(
            adjust_ratio(&mut tree, 1, SplitAxis::Vertical, 0.1),
            "竖向命中最深的内层"
        );
        assert_eq!(root_ratio(&tree), 0.5, "外层不动");
        let PaneNode::Split { ratio, .. } = first_child(&tree) else {
            panic!("first 应为拆分节点");
        };
        assert_eq!(*ratio, 0.6, "内层（离 1 最近）被调整");

        assert!(
            adjust_ratio(&mut tree, 2, SplitAxis::Horizontal, 0.1),
            "横向只能命中外层"
        );
        assert_eq!(root_ratio(&tree), 0.6);
        assert!(
            !adjust_ratio(&mut tree, 3, SplitAxis::Vertical, 0.1),
            "3 的路径上没有竖向分隔条"
        );

        // 同轴嵌套：命中最深（最近）的那一个
        let mut nested = PaneNode::Split {
            axis: SplitAxis::Horizontal,
            ratio: 0.5,
            first: Box::new(PaneNode::Split {
                axis: SplitAxis::Horizontal,
                ratio: 0.5,
                first: Box::new(leaf(1)),
                second: Box::new(leaf(2)),
            }),
            second: Box::new(leaf(3)),
        };
        assert!(adjust_ratio(&mut nested, 1, SplitAxis::Horizontal, 0.1));
        assert_eq!(root_ratio(&nested), 0.5, "外层不动");
        let PaneNode::Split { ratio, .. } = first_child(&nested) else {
            panic!("first 应为拆分节点");
        };
        assert_eq!(*ratio, 0.6, "内层（离 1 最近）被调整");
    }

    #[test]
    fn 翻转最近一层拆分方向() {
        let mut only = leaf(1);
        assert!(!flip_axis(&mut only, 1), "单窗格没有拆分可翻转");
        assert!(!flip_axis(&mut only, 9), "目标不存在");

        // L 形：根 H，左子树 V(1|2)。翻转 1 → 只有左子树的轴向变化
        let mut tree = l_shape();
        assert!(flip_axis(&mut tree, 1));
        assert_eq!(root_axis(&tree), SplitAxis::Horizontal, "根不动");
        assert_eq!(root_ratio(&tree), 0.5, "比例保持");
        let PaneNode::Split { axis, .. } = first_child(&tree) else {
            panic!("first 应为拆分节点");
        };
        assert_eq!(*axis, SplitAxis::Horizontal, "内层 V 翻成 H");
        let square = Rect::from_min_max(pos2(0.0, 0.0), pos2(100.0, 100.0));
        let out = rects_of(&tree, square, 0.0);
        let (r1, r2) = (rect_of(&out, 1), rect_of(&out, 2));
        assert_eq!(r1.min.y, r2.min.y, "1、2 现在左右并排");
        assert_eq!(r2.min.x, r1.max.x);

        assert!(flip_axis(&mut tree, 1));
        assert_eq!(tree, l_shape(), "翻两次还原");

        // 翻转 3：只有根含 3，根被翻转
        let mut tree = l_shape();
        assert!(flip_axis(&mut tree, 3));
        assert_eq!(root_axis(&tree), SplitAxis::Vertical);
        assert_eq!(root_ratio(&tree), 0.5);
        assert!(!flip_axis(&mut tree, 99), "目标不存在");
    }

    #[test]
    fn 退化尺寸不产生反向矩形() {
        let mut tree = leaf(1);
        assert!(split_leaf(&mut tree, 1, 2, SplitAxis::Horizontal));
        // gap 比矩形还宽：窗格被压到 0 尺寸，但不能出现 min > max
        let out = rects_of(
            &tree,
            Rect::from_min_max(pos2(0.0, 0.0), pos2(4.0, 10.0)),
            16.0,
        );
        for (id, rect) in &out {
            assert!(
                rect.min.x <= rect.max.x && rect.min.y <= rect.max.y,
                "窗格 {id} 出现反向矩形: {rect:?}"
            );
        }
    }
}
