// 终端行网格的自管 GPU 绘制着色器。
//
// 与 egui 自带的 `egui.wgsl` 保持同一套颜色语义（sRGB gamma 输出、premultiplied
// alpha 混合、可选抖动），差别只在顶点变换：
//
// * egui 每帧把 Galley 顶点重新 tessellate 成绝对屏幕坐标；
// * mino 把「行内相对坐标」的顶点常驻显存，行位置由 `Locals.row_origin`
//   在顶点着色器里加上——因此滚动只是换一个 uniform，顶点零重传。
//
// uv 在本着色器里从「纹素坐标」归一化（顶点缓冲里存的是 epaint 的原始 uv，
// 即纹素坐标；避免在 CPU 侧每行每顶点做一次除法）。

struct VertexOutput {
    @location(0) tex_coord: vec2<f32>,
    @location(1) color: vec4<f32>, // gamma 0-1
    @builtin(position) position: vec4<f32>,
};

struct Locals {
    /// 屏幕尺寸（点）= screen_size_px / pixels_per_point。
    screen_size: vec2<f32>,
    /// 本行原点（点，已在 CPU 侧按 ppp 取整）。
    row_origin: vec2<f32>,
        /// 图集尺寸（纹素）。由 CPU 从**将被采样的那张 GPU 纹理**读出后写入，
    /// 与回调实际使用的纹理严格同代（比 CPU 侧字体状态更可靠）。
    atlas_size: vec2<f32>,
    /// 1 = 启用抖动（与 egui 生产路径一致）。
    dithering: u32,
    /// 1 = 手写双线性过滤（egui 的 kittest 快照模式用）。
    predictable_texture_filtering: u32,
};

@group(0) @binding(0) var<uniform> r_locals: Locals;

// -----------------------------------------------
// 与 egui.wgsl 同源的辅助函数（抖动与 gamma 转换必须完全一致，
// 否则自管路径与其余 egui 控件的颜色会有可见色差）。
fn interleaved_gradient_noise(n: vec2<f32>) -> f32 {
    let f = 0.06711056 * n.x + 0.00583715 * n.y;
    return fract(52.9829189 * fract(f));
}

fn dither_interleaved(rgb: vec3<f32>, levels: f32, frag_coord: vec4<f32>) -> vec3<f32> {
    var noise = interleaved_gradient_noise(frag_coord.xy);
    // scale down the noise slightly to ensure flat colors aren't getting dithered
    noise = (noise - 0.5) * 0.95;
    return rgb + noise / (levels - 1.0);
}

// 0-1 linear from 0-1 sRGB gamma
fn linear_from_gamma_rgb(srgb: vec3<f32>) -> vec3<f32> {
    let cutoff = srgb < vec3<f32>(0.04045);
    let lower = srgb / vec3<f32>(12.92);
    let higher = pow((srgb + vec3<f32>(0.055)) / vec3<f32>(1.055), vec3<f32>(2.4));
    return select(higher, lower, cutoff);
}

// [u8; 4] SRGB as u32 -> [r, g, b, a] in 0.-1
fn unpack_color(color: u32) -> vec4<f32> {
    return vec4<f32>(
        f32(color & 255u),
        f32((color >> 8u) & 255u),
        f32((color >> 16u) & 255u),
        f32((color >> 24u) & 255u),
    ) / 255.0;
}

fn position_from_screen(screen_pos: vec2<f32>) -> vec4<f32> {
    return vec4<f32>(
        2.0 * screen_pos.x / r_locals.screen_size.x - 1.0,
        1.0 - 2.0 * screen_pos.y / r_locals.screen_size.y,
        0.0,
        1.0,
    );
}

@vertex
fn vs_main(
    @location(0) a_pos: vec2<f32>,
    @location(1) a_tex_coord: vec2<f32>,
    @location(2) a_color: u32,
) -> VertexOutput {
    var out: VertexOutput;
    // 行内相对坐标 + 本行原点 = 绝对屏幕坐标（与 epaint 的取整结果一致）。
    let screen_pos = a_pos + r_locals.row_origin;
    // 顶点里是 epaint 的纹素 uv，这里归一化。尺寸**直接取自被采样的那张纹理**
    // （`textureDimensions`），不依赖 CPU 侧记录：egui-wgpu 在 UI 帧之后才把
    // 图集变化上传成纹理，任何 CPU 侧记录的尺寸都可能与当前纹理差一帧——
    // 差一点点就会让 uv 落向图集左上角的白像素，整屏变成实心方块。
    out.tex_coord = a_tex_coord / r_locals.atlas_size;
    out.color = unpack_color(a_color);
    out.position = position_from_screen(screen_pos);
    return out;
}

// -----------------------------------------------
@group(1) @binding(0) var r_tex_color: texture_2d<f32>;
@group(1) @binding(1) var r_tex_sampler: sampler;

fn sample_texture(in: VertexOutput) -> vec4<f32> {
    if r_locals.predictable_texture_filtering == 0 {
        return textureSample(r_tex_color, r_tex_sampler, in.tex_coord);
    } else {
        // 手写双线性过滤：四抽样点取纹理像素中心，跨 GPU 可复现
        // （egui 快照测试用它保证图像稳定）。
        let texture_size = vec2<i32>(textureDimensions(r_tex_color, 0));
        let texture_size_f = vec2<f32>(texture_size);
        let pixel_coord = in.tex_coord * texture_size_f - 0.5;
        let pixel_fract = fract(pixel_coord);
        let pixel_floor = vec2<i32>(floor(pixel_coord));

        let max_coord = texture_size - vec2<i32>(1, 1);
        let p00 = clamp(pixel_floor + vec2<i32>(0, 0), vec2<i32>(0, 0), max_coord);
        let p10 = clamp(pixel_floor + vec2<i32>(1, 0), vec2<i32>(0, 0), max_coord);
        let p01 = clamp(pixel_floor + vec2<i32>(0, 1), vec2<i32>(0, 0), max_coord);
        let p11 = clamp(pixel_floor + vec2<i32>(1, 1), vec2<i32>(0, 0), max_coord);

        let tl = textureLoad(r_tex_color, p00, 0);
        let tr = textureLoad(r_tex_color, p10, 0);
        let bl = textureLoad(r_tex_color, p01, 0);
        let br = textureLoad(r_tex_color, p11, 0);

        let top = mix(tl, tr, pixel_fract.x);
        let bottom = mix(bl, br, pixel_fract.x);
        return mix(top, bottom, pixel_fract.y);
    }
}

@fragment
fn fs_main_linear_framebuffer(in: VertexOutput) -> @location(0) vec4<f32> {
    // 目标格式是 sRGB-aware 时，输出前转回线性空间。
    let tex_gamma = sample_texture(in);
    var out_color_gamma = in.color * tex_gamma;
    if r_locals.dithering == 1 {
        let out_color_gamma_rgb = dither_interleaved(out_color_gamma.rgb, 256.0, in.position);
        out_color_gamma = vec4<f32>(out_color_gamma_rgb, out_color_gamma.a);
    }
    let out_color_linear = linear_from_gamma_rgb(out_color_gamma.rgb);
    return vec4<f32>(out_color_linear, out_color_gamma.a);
}

@fragment
fn fs_main_gamma_framebuffer(in: VertexOutput) -> @location(0) vec4<f32> {
    // 目标格式是普通 8bit（Rgba8Unorm 等）时直接输出 gamma 空间颜色。
    let tex_gamma = sample_texture(in);
    var out_color_gamma = in.color * tex_gamma;
    if r_locals.dithering == 1 {
        let out_color_gamma_rgb = dither_interleaved(out_color_gamma.rgb, 256.0, in.position);
        out_color_gamma = vec4<f32>(out_color_gamma_rgb, out_color_gamma.a);
    }
    return out_color_gamma;
}
