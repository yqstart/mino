//! 终端行网格的自管 GPU 渲染。
//!
//! egui 每帧都会把全部 Shape 重新 tessellate 成顶点并整块上传（`Context::tessellate`
//! 明确不做跨帧复用）。对终端这种「内容几乎全是字形、只有少数行在变」的场景，
//! 这意味着每帧重传整屏顶点——即使只敲了一个字符。
//!
//! 这里把每行的字形顶点常驻显存：行内容不变就不重传，滚动只改 uniform 里的
//! 行原点（顶点零重传），空闲帧零上传。
//!
//! 关键约束（踩过的坑，见各处注释）：
//! * 回调的 `prepare`/`paint` 内**绝不**访问 `egui_wgpu::Renderer`：kittest 在
//!   `update_buffers`/`render` 期间持写锁、eframe 渲染期持读锁，回调内再取锁
//!   必然死锁。图集绑定只在 UI 线程帧内刷新。
//! * 顶点 uv 是 epaint 的**纹素坐标**，归一化在顶点着色器里做（`a_tex_coord / atlas_size`）。
//! * uv 与图集尺寸强绑定：图集换代必须整体重建（见 `TerminalView` 的看门狗）。

use std::collections::HashMap;
use std::sync::Arc;

use eframe::egui_wgpu;
use egui_wgpu::wgpu;

/// 每行 uniform 的字节数（`Locals`：vec2 ×3 + u32 ×2 = 32 字节）。
///
/// 用 256 字节对齐是为了满足 `min_uniform_buffer_offset_alignment` 的常见值；
/// `TerminalGpu::new` 会读设备 limit 并在需要时放大（见 `uniform_stride`）。
const UNIFORM_BASE_STRIDE: u64 = 256;

/// 支持的最大行数（uniform 与动态偏移的数量上限）。
pub const MAX_ROWS: usize = 256;

/// `Locals` 的实际字节数（写入 uniform 块用）。
const LOCALS_SIZE: usize = 32;

/// 字体图集绑定（UI 线程刷新，回调只读）。
pub struct AtlasBinding {
    pub bind_group: wgpu::BindGroup,
    /// 图集尺寸（纹素）——着色器用它把纹素 uv 归一化。
    pub size: [usize; 2],
}

/// 进程内一次性的终端 GPU 资源（管线、绑定布局、采样器、图集绑定）。
pub struct TerminalGpu {
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,
    pub target_format: wgpu::TextureFormat,
    pub pipeline: wgpu::RenderPipeline,
    /// 图集 bind group 的布局（UI 线程重建图集绑定时用）。
    pub atlas_layout: wgpu::BindGroupLayout,
    /// 行 uniform 的绑定布局（`UniformBlock` 建 bind group 用）。
    pub uniform_layout: wgpu::BindGroupLayout,
    /// 每行 uniform 的步长（已按设备对齐要求放大）。
    pub uniform_stride: u64,
    /// 字体图集所在渲染器：**只在 UI 线程帧内取读锁**，回调内绝不触碰。
    pub renderer: Arc<egui::mutex::RwLock<egui_wgpu::Renderer>>,
}

impl TerminalGpu {
    /// 依据 eframe 的 `RenderState` 建管线。
    ///
    /// 管线参数逐项对齐 egui 自己的管线（颜色格式、混合、采样数、顶点布局），
    /// 否则自管绘制的颜色与相邻 egui 控件会有可见差异。
    pub fn new(state: &egui_wgpu::RenderState) -> Self {
        let device = state.device.clone();
        let queue = state.queue.clone();
        let target_format = state.target_format;

        // 采样数必须与 egui 的管线一致：mino 不设置 `NativeOptions::multisampling`，
        // eframe 传 0 → egui 取 `max(1)`；kittest 用 `RendererOptions::PREDICTABLE`
        // 也是 1。将来启用 MSAA 时这里必须同步。
        let msaa = 1;

        let uniform_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("mino_terminal_uniform_layout"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX | wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    has_dynamic_offset: true,
                    min_binding_size: std::num::NonZeroU64::new(LOCALS_SIZE as u64),
                    ty: wgpu::BufferBindingType::Uniform,
                },
                count: None,
            }],
        });

        let atlas_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("mino_terminal_atlas_layout"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    // 顶点阶段也要用（`textureDimensions` 取图集尺寸归一化 uv），
                    // 只声明 FRAGMENT 会让管线创建失败（验证错误直接 abort）。
                    visibility: wgpu::ShaderStages::VERTEX | wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        multisampled: false,
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });

        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("mino_terminal_pipeline_layout"),
            bind_group_layouts: &[Some(&uniform_layout), Some(&atlas_layout)],
            immediate_size: 0,
        });

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("mino_terminal_shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("terminal_gpu.wgsl").into()),
        });

        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("mino_terminal_pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                entry_point: Some("vs_main"),
                module: &shader,
                buffers: &[Some(wgpu::VertexBufferLayout {
                    // 与 `epaint::Vertex` 的布局一致：pos(vec2) + uv(vec2) + color(u32)。
                    array_stride: std::mem::size_of::<epaint::Vertex>() as u64,
                    step_mode: wgpu::VertexStepMode::Vertex,
                    attributes: &wgpu::vertex_attr_array![0 => Float32x2, 1 => Float32x2, 2 => Uint32],
                })],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            },
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                cull_mode: None,
                ..Default::default()
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState {
                count: msaa,
                mask: !0,
                alpha_to_coverage_enabled: false,
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some(if target_format.is_srgb() {
                    "fs_main_linear_framebuffer"
                } else {
                    "fs_main_gamma_framebuffer"
                }),
                targets: &[Some(wgpu::ColorTargetState {
                    format: target_format,
                    // 与 egui 相同的预乘 alpha 混合。
                    blend: Some(wgpu::BlendState {
                        color: wgpu::BlendComponent {
                            src_factor: wgpu::BlendFactor::One,
                            dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
                            operation: wgpu::BlendOperation::Add,
                        },
                        alpha: wgpu::BlendComponent {
                            src_factor: wgpu::BlendFactor::OneMinusDstAlpha,
                            dst_factor: wgpu::BlendFactor::One,
                            operation: wgpu::BlendOperation::Add,
                        },
                    }),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            }),
            multiview_mask: None,
            cache: None,
        });

        let uniform_stride = UNIFORM_BASE_STRIDE.max(u64::from(
            device.limits().min_uniform_buffer_offset_alignment,
        ));

        Self {
            device,
            queue,
            target_format,
            pipeline,
            atlas_layout,
            uniform_layout,
            uniform_stride,
            renderer: state.renderer.clone(),
        }
    }

    /// 用当前字体图集刷新绑定（**只能在 UI 线程帧内调用**）。
    ///
    /// 图集纹理归 egui-wgpu 的 `Renderer` 管理；拿到它的纹理视图后自建
    /// bind group（egui 自己的那个绑定的是它的 uniform 布局，不能复用）。
    /// 当前**已被上传到 GPU** 的字体图集尺寸 `(宽, 高)` 纹素。
    ///
    /// 这是「本帧绘制将采样的那张纹理」的尺寸：egui-wgpu 在 UI 帧之后才把
    /// 图集变化上传成纹理，所以 UI 帧内读到的就是上一次上传的结果，正是回调
    /// 绘制时要用的那张。用 `font_image_size()`（CPU 侧、可能更新）判断会与
    /// 实际纹理差一帧——那正是「整屏方块」与「字形错位」两种症状的来源。
    pub fn gpu_atlas_size(&self) -> Option<[usize; 2]> {
        let renderer = self.renderer.read();
        let texture = renderer.texture(&egui::TextureId::Managed(0))?;
        let texture = texture.texture.as_ref()?;
        Some([texture.width() as usize, texture.height() as usize])
    }

    /// 返回当前图集绑定；`None` 表示图集尚未建好（调用方回退 egui 路径）。
    ///
    /// 由调用方持有返回的 `Arc`，回调只 clone 引用——因此这里不需要任何锁，
    /// 也避免在回调（渲染线程持 `Renderer` 锁期间）里再取锁。
    pub fn refresh_atlas(&self) -> Option<Arc<AtlasBinding>> {
        let (view, size) = {
            let renderer = self.renderer.read();
            let texture = renderer.texture(&egui::TextureId::Managed(0))?;
            let texture = texture.texture.as_ref()?;
            (
                texture.create_view(&wgpu::TextureViewDescriptor::default()),
                [texture.width() as usize, texture.height() as usize],
            )
        };
        let sampler = self.device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("mino_terminal_atlas_sampler"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        let bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("mino_terminal_atlas_bind_group"),
            layout: &self.atlas_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&sampler),
                },
            ],
        });
        Some(Arc::new(AtlasBinding { bind_group, size }))
    }
}

/// 一行在顶点/索引缓冲里的槽位。
#[derive(Clone, Copy, Debug)]
struct RowSlot {
    v_off: u64,
    v_len: u64,
    i_off: u64,
    i_len: u64,
    v_cap: u64,
    i_cap: u64,
}

/// 行顶点/索引缓冲 + 槽位分配器。
///
/// 行的内容变化只重传该行的字节区间；滚动完全不重传（行顶点是行内相对坐标）。
/// 每行有独立的容量，字数增加时重新分配该行槽位（而不是整块重建）。
pub struct RowBuffers {
    vbo: Option<wgpu::Buffer>,
    ibo: Option<wgpu::Buffer>,
    v_cap: u64,
    i_cap: u64,
    v_end: u64,
    i_end: u64,
    free_v: Vec<(u64, u64)>,
    free_i: Vec<(u64, u64)>,
    slots: HashMap<i32, RowSlot>,
    /// 本帧上传的字节数（HUD 读数）。
    pub uploaded_bytes: usize,
}

const V_START: u64 = 4 * 1024 * 1024;
const I_START: u64 = 2 * 1024 * 1024;

impl Default for RowBuffers {
    fn default() -> Self {
        Self {
            vbo: None,
            ibo: None,
            v_cap: V_START,
            i_cap: I_START,
            v_end: 0,
            i_end: 0,
            free_v: Vec::new(),
            free_i: Vec::new(),
            slots: HashMap::new(),
            uploaded_bytes: 0,
        }
    }
}

/// first-fit 分配：从空洞里挑第一个放得下的，否则从末尾追加。
fn alloc_in(free: &mut Vec<(u64, u64)>, end: &mut u64, size: u64) -> Option<u64> {
    if let Some(index) = free.iter().position(|(_, len)| *len >= size) {
        let (off, len) = free.swap_remove(index);
        if len > size {
            free.push((off + size, len - size));
            free.sort_unstable();
        }
        return Some(off);
    }
    let off = *end;
    *end += size;
    Some(off)
}

/// 归还区间并合并相邻空洞。
fn free_in(free: &mut Vec<(u64, u64)>, off: u64, len: u64) {
    if len == 0 {
        return;
    }
    free.push((off, len));
    free.sort_unstable();
    let mut merged: Vec<(u64, u64)> = Vec::with_capacity(free.len());
    for (o, l) in free.drain(..) {
        match merged.last_mut() {
            Some(last) if last.0 + last.1 == o => last.1 += l,
            _ => merged.push((o, l)),
        }
    }
    *free = merged;
}

impl RowBuffers {
    /// 确保缓冲存在且容量足够。
    fn ensure_buffers(&mut self, gpu: &TerminalGpu) {
        let needed_v = self.v_cap;
        let needed_i = self.i_cap;
        let recreate = match (&self.vbo, &self.ibo) {
            (Some(v), Some(i)) => v.size() < needed_v || i.size() < needed_i,
            _ => true,
        };
        if !recreate {
            return;
        }
        // 扩容：新建更大的缓冲，旧内容由调用方（`TerminalView`）按缓存的
        // 行网格重灌——这里只保证容量与分配器状态自洽。
        let new_v = if self.vbo.is_some() {
            self.v_cap * 2
        } else {
            self.v_cap
        };
        let new_i = if self.ibo.is_some() {
            self.i_cap * 2
        } else {
            self.i_cap
        };
        self.v_cap = new_v.max(needed_v);
        self.i_cap = new_i.max(needed_i);
        self.vbo = Some(gpu.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("mino_terminal_vbo"),
            size: self.v_cap,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        }));
        self.ibo = Some(gpu.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("mino_terminal_ibo"),
            size: self.i_cap,
            usage: wgpu::BufferUsages::INDEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        }));
        // 缓冲换了，旧槽位偏移全部失效。
        self.slots.clear();
        self.free_v.clear();
        self.free_i.clear();
        self.v_end = 0;
        self.i_end = 0;
    }

    /// 上传（或重传）一行的顶点与索引。
    ///
    /// 返回 `false` 表示容量不足且无法扩容（调用方回退到 egui 路径）。
    pub fn upload_row(&mut self, gpu: &TerminalGpu, grid_line: i32, mesh: &egui::Mesh) -> bool {
        self.ensure_buffers(gpu);
        let v_bytes = (mesh.vertices.len() * std::mem::size_of::<epaint::Vertex>()) as u64;
        let i_bytes = (mesh.indices.len() * std::mem::size_of::<u32>()) as u64;
        if v_bytes == 0 || i_bytes == 0 {
            // 空行：释放旧槽位即可。
            self.remove_row(grid_line);
            return true;
        }
        if v_bytes + self.v_end > self.v_cap * 2 || i_bytes + self.i_end > self.i_cap * 2 {
            return false;
        }

        let needs_realloc = match self.slots.get(&grid_line) {
            Some(slot) => v_bytes > slot.v_cap || i_bytes > slot.i_cap,
            None => true,
        };
        if needs_realloc {
            if self.slots.contains_key(&grid_line) {
                self.remove_row(grid_line);
            }
            // 容量翻倍冗余，避免字号/内容小幅增长就搬迁。
            let v_cap = (v_bytes * 2).max(256);
            let i_cap = (i_bytes * 2).max(256);
            let v_off = alloc_in(&mut self.free_v, &mut self.v_end, v_cap);
            let i_off = alloc_in(&mut self.free_i, &mut self.i_end, i_cap);
            let (Some(v_off), Some(i_off)) = (v_off, i_off) else {
                return false;
            };
            // 增长到超出缓冲时扩容后重试（`ensure_buffers` 已按需放大）。
            if v_off + v_cap > self.v_cap || i_off + i_cap > self.i_cap {
                self.v_cap = (v_off + v_cap).next_power_of_two().max(self.v_cap * 2);
                self.i_cap = (i_off + i_cap).next_power_of_two().max(self.i_cap * 2);
                self.slots.clear();
                self.free_v.clear();
                self.free_i.clear();
                self.v_end = 0;
                self.i_end = 0;
                self.ensure_buffers(gpu);
                return self.upload_row(gpu, grid_line, mesh);
            }
            self.slots.insert(
                grid_line,
                RowSlot {
                    v_off,
                    v_len: v_bytes,
                    i_off,
                    i_len: i_bytes,
                    v_cap,
                    i_cap,
                },
            );
        }

        let Some(slot) = self.slots.get(&grid_line).copied() else {
            return false;
        };
        let (Some(vbo), Some(ibo)) = (self.vbo.as_ref(), self.ibo.as_ref()) else {
            return false;
        };
        let v_bytes_slice = bytemuck::cast_slice(&mesh.vertices);
        gpu.queue
            .write_buffer(vbo, slot.v_off, &v_bytes_slice[..v_bytes as usize]);
        let i_bytes_slice = bytemuck::cast_slice(&mesh.indices);
        gpu.queue
            .write_buffer(ibo, slot.i_off, &i_bytes_slice[..i_bytes as usize]);
        self.uploaded_bytes += v_bytes as usize + i_bytes as usize;
        if let Some(slot) = self.slots.get_mut(&grid_line) {
            slot.v_len = v_bytes;
            slot.i_len = i_bytes;
        }
        true
    }

    /// 释放一行的槽位（行内容为空或该行不再需要）。
    pub fn remove_row(&mut self, grid_line: i32) {
        if let Some(slot) = self.slots.remove(&grid_line) {
            free_in(&mut self.free_v, slot.v_off, slot.v_cap);
            free_in(&mut self.free_i, slot.i_off, slot.i_cap);
        }
    }

    /// 全量失效（图集换代 / 缩放变化 / 主题变化）。
    pub fn clear(&mut self) {
        self.slots.clear();
        self.free_v.clear();
        self.free_i.clear();
        self.v_end = 0;
        self.i_end = 0;
    }

    /// 该行当前是否有可用槽位。
    ///
    /// 缓冲扩容（`ensure_buffers`）会换缓冲并清空全部槽位，调用方的
    /// “已上传”记录随之失效——必须用它复查，否则该行在 GPU 上没有顶点。
    pub fn has_row(&self, grid_line: i32) -> bool {
        self.slots.contains_key(&grid_line)
    }

    /// 原子缓冲与索引缓冲（未创建时为 `None`）。
    pub fn buffers(&self) -> (Option<&wgpu::Buffer>, Option<&wgpu::Buffer>) {
        (self.vbo.as_ref(), self.ibo.as_ref())
    }

    /// 取一行在缓冲中的绘制参数 `(index_start, index_count, base_vertex)`。
    pub fn draw_params(&self, grid_line: i32) -> Option<(u32, u32, i32)> {
        let slot = self.slots.get(&grid_line)?;
        Some((
            (slot.i_off / 4) as u32,
            (slot.i_len / 4) as u32,
            (slot.v_off / std::mem::size_of::<epaint::Vertex>() as u64) as i32,
        ))
    }
}

/// 一次绘制调用（一行）。
#[derive(Clone, Copy, Debug)]
pub struct RowDraw {
    pub index_start: u32,
    pub index_count: u32,
    pub base_vertex: i32,
    pub uniform_offset: u32,
}

/// 终端整屏的自管绘制回调。
pub struct TerminalCallback {
    pub gpu: Arc<TerminalGpu>,
    vbo: Arc<wgpu::Buffer>,
    ibo: Arc<wgpu::Buffer>,
    uniform_bind_group: Arc<wgpu::BindGroup>,
    atlas: Arc<AtlasBinding>,
    draws: Vec<RowDraw>,
}

impl TerminalCallback {
    /// 组装回调（调用方保证 `draws` 与 uniform 数据已写入 `uniform`）。
    pub fn new(
        gpu: Arc<TerminalGpu>,
        vbo: Arc<wgpu::Buffer>,
        ibo: Arc<wgpu::Buffer>,
        uniform_bind_group: Arc<wgpu::BindGroup>,
        atlas: Arc<AtlasBinding>,
        draws: Vec<RowDraw>,
    ) -> Self {
        Self {
            gpu,
            vbo,
            ibo,
            uniform_bind_group,
            atlas,
            draws,
        }
    }
}

impl egui_wgpu::CallbackTrait for TerminalCallback {
    fn paint(
        &self,
        info: egui::PaintCallbackInfo,
        pass: &mut wgpu::RenderPass<'static>,
        _resources: &egui_wgpu::CallbackResources,
    ) {
        if self.draws.is_empty() {
            return;
        }
        // egui 默认把 viewport 设成回调矩形；这里改用全屏 viewport，
        // 让顶点里的绝对屏幕坐标直接映射到 NDC（egui 的注释明确允许覆盖）。
        let [w, h] = info.screen_size_px;
        pass.set_viewport(0.0, 0.0, w as f32, h as f32, 0.0, 1.0);
        pass.set_pipeline(&self.gpu.pipeline);
        pass.set_bind_group(1, &self.atlas.bind_group, &[]);
        pass.set_vertex_buffer(0, self.vbo.slice(..));
        pass.set_index_buffer(self.ibo.slice(..), wgpu::IndexFormat::Uint32);
        for draw in &self.draws {
            pass.set_bind_group(0, self.uniform_bind_group.as_ref(), &[draw.uniform_offset]);
            pass.draw_indexed(
                draw.index_start..draw.index_start + draw.index_count,
                draw.base_vertex,
                0..1,
            );
        }
    }
}

/// uniform 块（每行一个 `Locals`，按设备对齐步长排布）。
pub struct UniformBlock {
    pub buffer: Arc<wgpu::Buffer>,
    pub bind_group: Arc<wgpu::BindGroup>,
    stride: u64,
    bytes: Vec<u8>,
}

impl UniformBlock {
    pub fn new(gpu: &TerminalGpu, layout: &wgpu::BindGroupLayout) -> Self {
        let stride = gpu.uniform_stride;
        let buffer = Arc::new(gpu.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("mino_terminal_uniform"),
            size: stride * MAX_ROWS as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        }));
        let bind_group = Arc::new(gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("mino_terminal_uniform_bind_group"),
            layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                    buffer: &buffer,
                    offset: 0,
                    size: std::num::NonZeroU64::new(LOCALS_SIZE as u64),
                }),
            }],
        }));
        Self {
            buffer,
            bind_group,
            stride,
            bytes: vec![0; (stride * MAX_ROWS as u64) as usize],
        }
    }

    /// 写入第 `slot` 行的 uniform（`Locals` 32 字节）。
    pub fn write_row(
        &mut self,
        slot: usize,
        screen_size: [f32; 2],
        row_origin: [f32; 2],
        atlas_size: [f32; 2],
    ) {
        let base = slot * self.stride as usize;
        if base + LOCALS_SIZE > self.bytes.len() {
            return;
        }
        let mut put = |offset: usize, value: f32| {
            self.bytes[base + offset..base + offset + 4].copy_from_slice(&value.to_le_bytes());
        };
        put(0, screen_size[0]);
        put(4, screen_size[1]);
        put(8, row_origin[0]);
        put(12, row_origin[1]);
        put(16, atlas_size[0]);
        put(20, atlas_size[1]);
        self.bytes[base + 24..base + 28].copy_from_slice(&1u32.to_le_bytes()); // dithering
        self.bytes[base + 28..base + 32].copy_from_slice(&0u32.to_le_bytes()); // predictable filtering
    }

    /// 提交整块 uniform（每帧一次）。
    pub fn flush(&self, queue: &wgpu::Queue, rows: usize) {
        let len = (rows as u64 * self.stride).min(self.buffer.size()) as usize;
        if len > 0 {
            queue.write_buffer(&self.buffer, 0, &self.bytes[..len]);
        }
    }

    pub fn offset_of(&self, slot: usize) -> u32 {
        (slot as u64 * self.stride) as u32
    }
}
