//! 性能测量 HUD：帧耗时 / FPS / 终端渲染分段耗时。
//!
//! 用调用方每帧传入的耗时打点（无锁刷新），渲染时以滑动窗口平均展示。
//! 纯 UI 线程使用，不做跨线程共享——HUD 本身不引入性能损耗。

use std::time::Instant;

/// 单条耗时指标的滑动平均。
struct Sliding {
    /// 历史样本（最大长度）。
    samples: Vec<f32>,
}

impl Sliding {
    fn new() -> Self {
        Self {
            samples: Vec::with_capacity(128),
        }
    }

    fn push(&mut self, v: f32) {
        self.samples.push(v);
        if self.samples.len() > 128 {
            self.samples.remove(0);
        }
    }

    /// 平均耗时（毫秒）。
    fn avg_ms(&self) -> Option<f32> {
        if self.samples.is_empty() {
            return None;
        }
        let sum: f32 = self.samples.iter().sum();
        Some(sum / self.samples.len() as f32)
    }
}

/// 全局性能统计（单实例，UI 线程使用）。
pub struct PerfStats {
    frame_times: Sliding,
    build_times: Sliding,
    layout_times: Sliding,
    paint_times: Sliding,
    /// 每帧推给 egui 的 Shape 数（终端区域）。
    shapes: Sliding,
    /// 每帧重建文本数据的行数。
    rows_rebuilt: Sliding,
    /// 每帧复用缓存的行数。
    rows_reused: Sliding,
    /// 每帧上传到 GPU 的字节数（自管渲染路径；其余路径恒 0）。
    upload_bytes: Sliding,
    /// 上一帧结束时间（计算 FPS）。
    last_frame: Option<Instant>,
    /// 每秒帧数滑动平均。
    fps: Sliding,
    /// 首帧耗时（应用构造 → 第一帧渲染完成，毫秒）。
    startup_ms: Option<f32>,
    /// 终端就绪耗时（应用构造 → 首个终端会话挂上标签，毫秒）。
    terminal_ready_ms: Option<f32>,
}

impl Default for PerfStats {
    fn default() -> Self {
        Self::new()
    }
}

impl PerfStats {
    pub fn new() -> Self {
        Self {
            frame_times: Sliding::new(),
            build_times: Sliding::new(),
            layout_times: Sliding::new(),
            paint_times: Sliding::new(),
            shapes: Sliding::new(),
            rows_rebuilt: Sliding::new(),
            rows_reused: Sliding::new(),
            upload_bytes: Sliding::new(),
            last_frame: None,
            fps: Sliding::new(),
            startup_ms: None,
            terminal_ready_ms: None,
        }
    }

    /// 记录首帧耗时（毫秒，只应调用一次）。
    pub fn set_startup_ms(&mut self, ms: f32) {
        self.startup_ms = Some(ms);
    }

    /// 记录终端就绪耗时（毫秒，只应调用一次）。
    pub fn set_terminal_ready_ms(&mut self, ms: f32) {
        self.terminal_ready_ms = Some(ms);
    }

    /// 帧开始（记录起始时间）。
    pub fn begin_frame(&mut self) {
        let now = Instant::now();
        if let Some(last) = self.last_frame {
            let dt = now.duration_since(last).as_secs_f32();
            if dt > 0.0 {
                self.fps.push(1.0 / dt);
            }
        }
        self.last_frame = Some(now);
    }

    /// 帧结束（记录整帧耗时，毫秒）。
    pub fn end_frame(&mut self) {
        if let Some(last) = self.last_frame {
            let t = last.elapsed().as_secs_f32() * 1000.0;
            self.frame_times.push(t);
        }
    }

    /// 记录终端锁内构建耗时（毫秒）。
    pub fn add_build(&mut self, ms: f32) {
        self.build_times.push(ms);
    }

    /// 记录终端文本布局耗时（毫秒）。
    pub fn add_layout(&mut self, ms: f32) {
        self.layout_times.push(ms);
    }

    /// 记录绘制耗时（毫秒）。
    pub fn add_paint(&mut self, ms: f32) {
        self.paint_times.push(ms);
    }

    /// 记录终端每帧的规模计数（Shape 数 / 重建行数 / 复用行数 / 上传字节）。
    ///
    /// 这些是渲染优化唯一可观察的证据：只靠帧耗时无法区分「CPU 侧重建」与
    /// 「GPU 侧上传」谁是瓶颈（两者优化手段完全不同）。
    pub fn add_terminal_counts(
        &mut self,
        shapes: usize,
        rows_rebuilt: usize,
        rows_reused: usize,
        upload_bytes: usize,
    ) {
        self.shapes.push(shapes as f32);
        self.rows_rebuilt.push(rows_rebuilt as f32);
        self.rows_reused.push(rows_reused as f32);
        self.upload_bytes.push(upload_bytes as f32);
    }

    /// 汇总一行展示文本。
    pub fn summary(&self) -> String {
        let f = |v: Option<f32>| match v {
            Some(v) => format!("{v:.2}"),
            None => "—".to_string(),
        };
        let u = |v: Option<f32>| match v {
            Some(v) => format!("{v:.0}"),
            None => "—".to_string(),
        };
        let fps = self.fps.avg_ms().map(|v| v as u32);
        let fps = fps.map(|v| v.to_string()).unwrap_or_else(|| "—".into());
        // 行重建按「重建/总数」（总数 = 重建 + 复用），KB 保留一位小数够看量级。
        let rows = match (self.rows_rebuilt.avg_ms(), self.rows_reused.avg_ms()) {
            (Some(rebuilt), Some(reused)) => format!("{rebuilt:.0}/{}", rebuilt + reused),
            _ => "—".to_string(),
        };
        let upload_kb = self.upload_bytes.avg_ms().map(|v| v / 1024.0);
        let upload = match upload_kb {
            Some(kb) => format!("{kb:.1}"),
            None => "—".to_string(),
        };
        format!(
            "帧 {}ms | FPS {} | 构建 {}ms | 布局 {}ms | 绘制 {}ms | shapes {} | 行重建 {} | 上传 {}KB{}",
            f(self.frame_times.avg_ms()),
            fps,
            f(self.build_times.avg_ms()),
            f(self.layout_times.avg_ms()),
            f(self.paint_times.avg_ms()),
            u(self.shapes.avg_ms()),
            rows,
            upload,
            startup_suffix(self.startup_ms, self.terminal_ready_ms),
        )
    }
}

/// 启动打点后缀：首帧耗时 +（若已就绪）终端就绪耗时。
///
/// 单独函数便于单测（HUD 文本格式是给人看的关键读数，不能静默丢字段）。
fn startup_suffix(startup_ms: Option<f32>, terminal_ready_ms: Option<f32>) -> String {
    let mut suffix = String::new();
    if let Some(ms) = startup_ms {
        suffix.push_str(&format!(" | 首帧 {ms:.0}ms"));
    }
    if let Some(ms) = terminal_ready_ms {
        suffix.push_str(&format!(" | 终端 {ms:.0}ms"));
    }
    suffix
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 打点后 HUD 必须真实反映启动耗时（启动慢是用户可感知的第一体验，
    /// 这两个读数缺了就无法验证异步化的效果）。
    #[test]
    fn 性能汇总包含启动打点() {
        let mut perf = PerfStats::new();
        assert!(
            !perf.summary().contains("首帧"),
            "未打点时不应出现启动读数：{}",
            perf.summary()
        );
        perf.set_startup_ms(123.4);
        perf.set_terminal_ready_ms(456.7);
        let summary = perf.summary();
        assert!(summary.contains("首帧 123ms"), "首帧读数缺失：{summary}");
        assert!(
            summary.contains("终端 457ms"),
            "终端就绪读数缺失：{summary}"
        );
    }

    /// Shape 数 / 行重建 / 上传字节是渲染优化唯一可观察的证据，
    /// 缺任何一项都无法判断瓶颈在 CPU 侧重建还是 GPU 侧上传。
    #[test]
    fn 性能汇总包含终端规模计数() {
        let mut perf = PerfStats::new();
        assert!(
            perf.summary().contains("行重建 —"),
            "未打点时不应伪造行重建读数：{}",
            perf.summary()
        );
        perf.add_terminal_counts(120, 3, 37, 8192);
        let summary = perf.summary();
        assert!(summary.contains("shapes 120"), "Shape 数缺失：{summary}");
        assert!(summary.contains("行重建 3/40"), "行重建缺失：{summary}");
        assert!(summary.contains("上传 8.0KB"), "上传量缺失：{summary}");
    }
}
