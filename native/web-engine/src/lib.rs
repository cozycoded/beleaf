//! The Rust brush engine for beleaf in the browser.
//!
//! It answers the same calls as beleaf's JavaScript StrokeEngine: begin, add, end, flush, reset.
//! Its output is a WebGL canvas holding the stroke so far (colour × coverage, premultiplied), which
//! beleaf draws over the layer exactly like it draws the old engine's output.
//!
//! Inside: brush-core turns pen samples into dabs; every dab is a textured quad drawn on the GPU into
//! a "main" mask (max or build-up blending) and a "dual" mask; flush/end combine
//! colour × main × dual × paper texture into the canvas.
#![cfg(target_arch = "wasm32")]

use brush_core::{Brush, ColorJitter, Dab, Dual, Jitter, Spray, Stroke, TiltSize};
use serde::Deserialize;
use wasm_bindgen::prelude::*;
use wgpu::util::DeviceExt;

/* ---------------- the brush settings beleaf sends (a trimmed copy of its brush object) ---------------- */

#[derive(Deserialize, Default)]
#[serde(default)]
struct Dyn {
    on: bool,
    min: f32,
    curve: Vec<[f32; 2]>,
}
#[derive(Deserialize, Default)]
#[serde(default)]
struct Taper {
    #[serde(rename = "in")]
    tin: f32,
    out: f32,
    ratio: Option<f32>,
    #[serde(rename = "bySpeed")]
    by_speed: bool,
}
#[derive(Deserialize, Default)]
#[serde(default)]
struct JitterP {
    size: f32,
    opacity: f32,
    angle: f32,
    round: f32,
    scatter: f32,
}
#[derive(Deserialize, Default)]
#[serde(default)]
struct DualP {
    on: bool,
    size: f32,
    spacing: f32,
    spray: bool,
    count: f32,
    area: f32,
    #[serde(rename = "randomAngle")]
    random_angle: bool,
}
#[derive(Deserialize, Default)]
#[serde(default)]
struct TexP {
    on: bool,
    density: f32,
    scale: f32,
}
#[derive(Deserialize, Default)]
#[serde(default)]
struct SprayP {
    on: bool,
    particle: f32,
    count: f32,
    #[serde(rename = "randomAngle")]
    random_angle: bool,
}
#[derive(Deserialize, Default)]
#[serde(default)]
struct TiltP {
    on: bool,
    min: f32,
    max: f32,
    curve: Vec<[f32; 2]>,
}
#[derive(Deserialize, Default)]
#[serde(default)]
struct ColJ {
    hue: f32,
    sat: f32,
    val: f32,
    #[serde(rename = "perStroke")]
    per_stroke: bool,
}
#[derive(Deserialize)]
#[serde(default)]
struct Params {
    size: f32,
    spacing: f32,
    flow: f32,
    angle: f32,
    follow: bool,
    stab: f32,
    roundness: f32,
    build: bool,
    aa: f32,
    #[serde(rename = "sizeDyn")]
    size_dyn: Dyn,
    #[serde(rename = "opDyn")]
    op_dyn: Dyn,
    taper: Taper,
    jitter: JitterP,
    dual: DualP,
    tex: TexP,
    spray: SprayP,
    #[serde(rename = "tiltDyn")]
    tilt_dyn: TiltP,
    #[serde(rename = "tiltOp")]
    tilt_op: TiltP,
    #[serde(rename = "tiltRound")]
    tilt_round: f32,
    #[serde(rename = "angleTilt")]
    angle_tilt: bool,
    #[serde(rename = "colorJitter")]
    color_jitter: ColJ,
    mix: f32,
    #[serde(rename = "blendOnly")]
    blend_only: bool,
    #[serde(rename = "mixMode")]
    mix_mode: String,
    opacity: f32,
}
impl Default for Params {
    fn default() -> Self {
        Params { size: 10.0, spacing: 8.0, flow: 100.0, angle: 0.0, follow: false, stab: 0.0, roundness: 100.0, build: false, aa: 2.0, size_dyn: Dyn::default(), op_dyn: Dyn::default(), taper: Taper::default(), jitter: JitterP::default(), dual: DualP::default(), tex: TexP::default(), spray: SprayP::default(), tilt_dyn: TiltP::default(), tilt_op: TiltP::default(), tilt_round: 0.0, angle_tilt: false, color_jitter: ColJ::default(), mix: 0.0, blend_only: false, mix_mode: String::new(), opacity: 100.0 }
    }
}
impl Params {
    fn brush(&self) -> Brush {
        Brush {
            size: self.size,
            spacing: self.spacing,
            flow: self.flow,
            angle: self.angle,
            follow: self.follow,
            stab: self.stab,
            size_min: if self.size_dyn.on { Some(self.size_dyn.min) } else { None },
            op_min: if self.op_dyn.on { Some(self.op_dyn.min) } else { None },
            size_curve: self.size_dyn.curve.clone(),
            op_curve: self.op_dyn.curve.clone(),
            roundness: self.roundness,
            jitter: Jitter { size: self.jitter.size, opacity: self.jitter.opacity, angle: self.jitter.angle, round: self.jitter.round, scatter: self.jitter.scatter },
            taper_in: self.taper.tin,
            taper_out: self.taper.out,
            taper_ratio: self.taper.ratio,
            taper_by_speed: self.taper.by_speed,
            dual: if self.dual.on {
                Some(Dual { size: self.dual.size, spacing: self.dual.spacing, spray: self.dual.spray, count: self.dual.count.max(1.0) as u32, area: self.dual.area, random_angle: self.dual.random_angle })
            } else {
                None
            },
            spray: if self.spray.on { Some(Spray { particle: self.spray.particle, count: self.spray.count.round().max(1.0) as u32, random_angle: self.spray.random_angle }) } else { None },
            tilt_size: if self.tilt_dyn.on { Some(TiltSize { min: self.tilt_dyn.min, max: self.tilt_dyn.max, curve: self.tilt_dyn.curve.clone() }) } else { None },
            tilt_op: if self.tilt_op.on { Some(self.tilt_op.min.clamp(0.0, 100.0)) } else { None },
            tilt_round: self.tilt_round,
            angle_tilt: self.angle_tilt,
            color_jitter: {
                let j = &self.color_jitter;
                if j.hue != 0.0 || j.sat != 0.0 || j.val != 0.0 { Some(ColorJitter { hue: j.hue, sat: j.sat, val: j.val, per_stroke: j.per_stroke }) } else { None }
            },
            mix: self.mix,
            blend_only: self.blend_only,
            mix_paint: self.mix_mode != "normal",
            opacity: self.opacity,
            build: self.build,
        }
    }
}

/* ---------------- shaders ---------------- */

const DAB_WGSL: &str = r#"
struct U { canvas: vec2<f32>, aspect: f32, binary: f32, };
@group(0) @binding(0) var<uniform> u: U;
@group(0) @binding(1) var tip: texture_2d<f32>;
@group(0) @binding(2) var smp: sampler;
struct VO { @builtin(position) pos: vec4<f32>, @location(0) uv: vec2<f32>, @location(1) a: f32, @location(2) col: vec3<f32>, };
@vertex
fn vs(@builtin(vertex_index) vi: u32, @location(0) p: vec2<f32>, @location(1) size: f32, @location(2) ang: f32, @location(3) alpha: f32, @location(4) rnd: f32, @location(5) col: vec3<f32>) -> VO {
  var c = array<vec2<f32>, 4>(vec2<f32>(-0.5, -0.5), vec2<f32>(0.5, -0.5), vec2<f32>(-0.5, 0.5), vec2<f32>(0.5, 0.5));
  let q = c[vi];
  var sw = size;
  var sh = size;
  if (u.aspect >= 1.0) { sh = size / u.aspect; } else { sw = size * u.aspect; }
  sh = sh * max(0.05, rnd);
  let l = vec2<f32>(q.x * sw, q.y * sh);
  let cs = cos(ang);
  let sn = sin(ang);
  let r = vec2<f32>(l.x * cs - l.y * sn, l.x * sn + l.y * cs);
  let px = p + r;
  var o: VO;
  o.pos = vec4<f32>(px.x / u.canvas.x * 2.0 - 1.0, 1.0 - px.y / u.canvas.y * 2.0, 0.0, 1.0);
  o.uv = q + vec2<f32>(0.5, 0.5);
  o.a = alpha;
  o.col = col;
  return o;
}
fn cover(i: VO) -> f32 {
  var t = textureSample(tip, smp, i.uv).r;
  if (u.binary > 0.5) { t = step(0.5, t); }
  return t * i.a;
}
@fragment
fn fs(i: VO) -> @location(0) vec4<f32> {
  let v = cover(i);
  return vec4<f32>(v, v, v, v);
}
// per-dab colour (colour jitter): premultiplied colour laid over, un-premultiplied when composed
@fragment
fn fs_col(i: VO) -> @location(0) vec4<f32> {
  let v = cover(i);
  return vec4<f32>(i.col * v, v);
}
"#;

const COMP_WGSL: &str = r#"
struct U { color: vec4<f32>, tex: vec4<f32>, };
@group(0) @binding(0) var<uniform> u: U;
@group(0) @binding(1) var m1: texture_2d<f32>;
@group(0) @binding(2) var m2: texture_2d<f32>;
@group(0) @binding(3) var paper: texture_2d<f32>;
@group(0) @binding(4) var smp: sampler;
@group(0) @binding(5) var rep: sampler;
@group(0) @binding(6) var cols: texture_2d<f32>;
struct VO { @builtin(position) pos: vec4<f32>, @location(0) uv: vec2<f32>, };
@vertex
fn vs(@builtin(vertex_index) vi: u32) -> VO {
  var c = array<vec2<f32>, 4>(vec2<f32>(0.0, 0.0), vec2<f32>(1.0, 0.0), vec2<f32>(0.0, 1.0), vec2<f32>(1.0, 1.0));
  let q = c[vi];
  var o: VO;
  o.pos = vec4<f32>(q.x * 2.0 - 1.0, 1.0 - q.y * 2.0, 0.0, 1.0);
  o.uv = q;
  return o;
}
@fragment
fn fs(i: VO) -> @location(0) vec4<f32> {
  var a = textureSample(m1, smp, i.uv).r * textureSample(m2, smp, i.uv).r;
  // paper texture: tex.x = on, tex.y = density, tex.z = scale
  let g = textureSample(paper, rep, i.pos.xy / (256.0 * max(0.25, u.tex.z)));
  a = a * mix(1.0, 1.0 - u.tex.y * (1.0 - g.r), u.tex.x);
  var rgb = u.color.rgb;
  // tex.w = 1: every dab has its own colour, kept in `cols`
  if (u.tex.w > 0.5) {
    let c = textureSample(cols, smp, i.uv);
    if (c.a > 0.002) { rgb = c.rgb / c.a; }
  }
  return vec4<f32>(rgb * a, a);
}
"#;

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct Inst {
    pos: [f32; 2],
    size: f32,
    angle: f32,
    alpha: f32,
    round: f32,
    col: [f32; 3],
}

const MAX_BLEND: wgpu::BlendState = wgpu::BlendState {
    color: wgpu::BlendComponent { src_factor: wgpu::BlendFactor::One, dst_factor: wgpu::BlendFactor::One, operation: wgpu::BlendOperation::Max },
    alpha: wgpu::BlendComponent { src_factor: wgpu::BlendFactor::One, dst_factor: wgpu::BlendFactor::One, operation: wgpu::BlendOperation::Max },
};
// colour layer: premultiplied colour over what's there
const COL_BLEND: wgpu::BlendState = wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING;
// build-up: each dab adds on top (like the dual tip in beleaf)
const OVER_BLEND: wgpu::BlendState = wgpu::BlendState {
    color: wgpu::BlendComponent { src_factor: wgpu::BlendFactor::One, dst_factor: wgpu::BlendFactor::OneMinusSrc, operation: wgpu::BlendOperation::Add },
    alpha: wgpu::BlendComponent { src_factor: wgpu::BlendFactor::One, dst_factor: wgpu::BlendFactor::OneMinusSrc, operation: wgpu::BlendOperation::Add },
};

struct TipTex {
    view: wgpu::TextureView,
    aspect: f32,
}

fn make_tip(device: &wgpu::Device, queue: &wgpu::Queue, w: u32, h: u32, src: &[u8]) -> TipTex {
    // one empty pixel around the edge so sampling never smears the border
    let (pw, ph) = (w + 2, h + 2);
    let mut d = vec![0u8; (pw * ph) as usize];
    for y in 0..h {
        for x in 0..w {
            d[((y + 1) * pw + x + 1) as usize] = src[(y * w + x) as usize];
        }
    }
    let mut levels = vec![(pw, ph, d)];
    while levels.len() < 10 {
        let (lw, lh, ld) = levels.last().unwrap();
        if *lw <= 2 || *lh <= 2 {
            break;
        }
        let (nw, nh) = ((lw / 2).max(1), (lh / 2).max(1));
        let mut nd = vec![0u8; (nw * nh) as usize];
        for y in 0..nh {
            for x in 0..nw {
                let mut s = 0u32;
                for (dx, dy) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
                    s += ld[(((y * 2 + dy).min(lh - 1)) * lw + (x * 2 + dx).min(lw - 1)) as usize] as u32;
                }
                nd[(y * nw + x) as usize] = (s / 4) as u8;
            }
        }
        levels.push((nw, nh, nd));
    }
    let tex = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("tip"),
        size: wgpu::Extent3d { width: pw, height: ph, depth_or_array_layers: 1 },
        mip_level_count: levels.len() as u32,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::R8Unorm,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    for (i, (lw, lh, ld)) in levels.iter().enumerate() {
        queue.write_texture(
            wgpu::ImageCopyTexture { texture: &tex, mip_level: i as u32, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
            ld,
            wgpu::ImageDataLayout { offset: 0, bytes_per_row: Some(*lw), rows_per_image: Some(*lh) },
            wgpu::Extent3d { width: *lw, height: *lh, depth_or_array_layers: 1 },
        );
    }
    TipTex { view: tex.create_view(&Default::default()), aspect: w as f32 / h as f32 }
}

fn r8(device: &wgpu::Device, w: u32, h: u32, usage: wgpu::TextureUsages) -> wgpu::Texture {
    tex2d(device, w, h, usage, wgpu::TextureFormat::R8Unorm)
}
fn tex2d(device: &wgpu::Device, w: u32, h: u32, usage: wgpu::TextureUsages, format: wgpu::TextureFormat) -> wgpu::Texture {
    device.create_texture(&wgpu::TextureDescriptor {
        label: None,
        size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage,
        view_formats: &[],
    })
}

fn clear(enc: &mut wgpu::CommandEncoder, view: &wgpu::TextureView, v: f64) {
    enc.begin_render_pass(&wgpu::RenderPassDescriptor {
        label: None,
        color_attachments: &[Some(wgpu::RenderPassColorAttachment { view, resolve_target: None, ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color { r: v, g: v, b: v, a: v }), store: wgpu::StoreOp::Store } })],
        depth_stencil_attachment: None,
        timestamp_writes: None,
        occlusion_query_set: None,
    });
}

#[wasm_bindgen]
pub struct GpuEngine {
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    smp: wgpu::Sampler,
    rep: wgpu::Sampler,
    dab_layout: wgpu::BindGroupLayout,
    comp_layout: wgpu::BindGroupLayout,
    pipe_max: wgpu::RenderPipeline,
    pipe_over: wgpu::RenderPipeline,
    pipe_col: wgpu::RenderPipeline,
    comp_pipe: wgpu::RenderPipeline,
    mask_main: wgpu::Texture,
    mask_dual: wgpu::Texture,
    cols: wgpu::Texture,
    per_dab_col: bool,
    paper: wgpu::Texture,
    tips: [Option<TipTex>; 2],
    ub_dab: [wgpu::Buffer; 2],
    ub_comp: wgpu::Buffer,
    comp_bg: wgpu::BindGroup,
    stroke: Option<Stroke>,
    sampler: Option<js_sys::Function>,
    build: bool,
    binary: bool,
    dual_on: bool,
    tex: [f32; 4],
    color: [f32; 4],
    pending: Vec<Dab>,
    dirty: Option<[f32; 4]>,
    bbox: Option<[f32; 4]>,
    seed: u32,
}

fn grow(r: Option<[f32; 4]>, d: &Dab) -> Option<[f32; 4]> {
    let e = d.size * 0.75 + 4.0;
    let n = [d.x - e, d.y - e, d.x + e, d.y + e];
    Some(match r {
        None => n,
        Some(o) => [o[0].min(n[0]), o[1].min(n[1]), o[2].max(n[2]), o[3].max(n[3])],
    })
}

#[wasm_bindgen]
impl GpuEngine {
    /// Set up WebGL on `canvas` (create its webgl2 context first with preserveDrawingBuffer: true).
    pub async fn create(canvas: web_sys::HtmlCanvasElement, w: u32, h: u32) -> Result<GpuEngine, JsValue> {
        console_error_panic_hook::set_once();
        canvas.set_width(w);
        canvas.set_height(h);
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor { backends: wgpu::Backends::GL, ..Default::default() });
        let surface = instance.create_surface(wgpu::SurfaceTarget::Canvas(canvas)).map_err(|e| JsValue::from_str(&format!("surface: {e}")))?;
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions { power_preference: wgpu::PowerPreference::HighPerformance, compatible_surface: Some(&surface), force_fallback_adapter: false })
            .await
            .ok_or_else(|| JsValue::from_str("no WebGL2 adapter"))?;
        let limits = wgpu::Limits::downlevel_webgl2_defaults().using_resolution(adapter.limits());
        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor { label: None, required_features: wgpu::Features::empty(), required_limits: limits, memory_hints: Default::default() }, None)
            .await
            .map_err(|e| JsValue::from_str(&format!("device: {e}")))?;
        let caps = surface.get_capabilities(&adapter);
        let format = caps.formats.iter().copied().find(|f| !f.is_srgb()).unwrap_or(caps.formats[0]);
        let alpha_mode = if caps.alpha_modes.contains(&wgpu::CompositeAlphaMode::PreMultiplied) { wgpu::CompositeAlphaMode::PreMultiplied } else { caps.alpha_modes[0] };
        let config = wgpu::SurfaceConfiguration { usage: wgpu::TextureUsages::RENDER_ATTACHMENT, format, width: w, height: h, present_mode: wgpu::PresentMode::Fifo, alpha_mode, view_formats: vec![], desired_maximum_frame_latency: 1 };
        surface.configure(&device, &config);

        let smp = device.create_sampler(&wgpu::SamplerDescriptor { mag_filter: wgpu::FilterMode::Linear, min_filter: wgpu::FilterMode::Linear, mipmap_filter: wgpu::FilterMode::Linear, ..Default::default() });
        let rep = device.create_sampler(&wgpu::SamplerDescriptor { address_mode_u: wgpu::AddressMode::Repeat, address_mode_v: wgpu::AddressMode::Repeat, ..Default::default() });
        let entry_tex = |b: u32| wgpu::BindGroupLayoutEntry { binding: b, visibility: wgpu::ShaderStages::FRAGMENT, ty: wgpu::BindingType::Texture { sample_type: wgpu::TextureSampleType::Float { filterable: true }, view_dimension: wgpu::TextureViewDimension::D2, multisampled: false }, count: None };
        let entry_ub = wgpu::BindGroupLayoutEntry { binding: 0, visibility: wgpu::ShaderStages::VERTEX | wgpu::ShaderStages::FRAGMENT, ty: wgpu::BindingType::Buffer { ty: wgpu::BufferBindingType::Uniform, has_dynamic_offset: false, min_binding_size: None }, count: None };
        let entry_smp = |b: u32| wgpu::BindGroupLayoutEntry { binding: b, visibility: wgpu::ShaderStages::FRAGMENT, ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering), count: None };
        let dab_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor { label: None, entries: &[entry_ub, entry_tex(1), entry_smp(2)] });
        let comp_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor { label: None, entries: &[entry_ub, entry_tex(1), entry_tex(2), entry_tex(3), entry_smp(4), entry_smp(5), entry_tex(6)] });
        let dab_mod = device.create_shader_module(wgpu::ShaderModuleDescriptor { label: Some("dab"), source: wgpu::ShaderSource::Wgsl(DAB_WGSL.into()) });
        let comp_mod = device.create_shader_module(wgpu::ShaderModuleDescriptor { label: Some("comp"), source: wgpu::ShaderSource::Wgsl(COMP_WGSL.into()) });
        let mk = |layout: &wgpu::BindGroupLayout, module: &wgpu::ShaderModule, fs: &str, buffers: &[wgpu::VertexBufferLayout], fmt: wgpu::TextureFormat, blend: wgpu::BlendState| {
            let pl = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor { label: None, bind_group_layouts: &[layout], push_constant_ranges: &[] });
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: None,
                layout: Some(&pl),
                vertex: wgpu::VertexState { module, entry_point: Some("vs"), compilation_options: Default::default(), buffers },
                fragment: Some(wgpu::FragmentState { module, entry_point: Some(fs), compilation_options: Default::default(), targets: &[Some(wgpu::ColorTargetState { format: fmt, blend: Some(blend), write_mask: wgpu::ColorWrites::ALL })] }),
                primitive: wgpu::PrimitiveState { topology: wgpu::PrimitiveTopology::TriangleStrip, ..Default::default() },
                depth_stencil: None,
                multisample: wgpu::MultisampleState::default(),
                multiview: None,
                cache: None,
            })
        };
        let inst = wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<Inst>() as u64,
            step_mode: wgpu::VertexStepMode::Instance,
            attributes: &wgpu::vertex_attr_array![0 => Float32x2, 1 => Float32, 2 => Float32, 3 => Float32, 4 => Float32, 5 => Float32x3],
        };
        let pipe_max = mk(&dab_layout, &dab_mod, "fs", &[inst.clone()], wgpu::TextureFormat::R8Unorm, MAX_BLEND);
        let pipe_over = mk(&dab_layout, &dab_mod, "fs", &[inst.clone()], wgpu::TextureFormat::R8Unorm, OVER_BLEND);
        let pipe_col = mk(&dab_layout, &dab_mod, "fs_col", &[inst], wgpu::TextureFormat::Rgba8Unorm, COL_BLEND);
        let comp_pipe = mk(&comp_layout, &comp_mod, "fs", &[], format, wgpu::BlendState::REPLACE);

        let usage = wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING;
        let mask_main = r8(&device, w, h, usage);
        let mask_dual = r8(&device, w, h, usage);
        let cols = tex2d(&device, w, h, usage, wgpu::TextureFormat::Rgba8Unorm);
        let paper = r8(&device, 256, 256, wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST);
        let ub = |n: usize| device.create_buffer(&wgpu::BufferDescriptor { label: None, size: (n * 4) as u64, usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST, mapped_at_creation: false });
        let ub_dab = [ub(4), ub(4)];
        let ub_comp = ub(8);
        let comp_bg = Self::comp_bind(&device, &comp_layout, &ub_comp, &mask_main, &mask_dual, &paper, &cols, &smp, &rep);
        let mut e = GpuEngine {
            surface,
            device,
            queue,
            config,
            smp,
            rep,
            dab_layout,
            comp_layout,
            pipe_max,
            pipe_over,
            pipe_col,
            comp_pipe,
            mask_main,
            mask_dual,
            cols,
            per_dab_col: false,
            paper,
            tips: [None, None],
            ub_dab,
            ub_comp,
            comp_bg,
            stroke: None,
            sampler: None,
            build: false,
            binary: false,
            dual_on: false,
            tex: [0.0; 4],
            color: [0.0, 0.0, 0.0, 1.0],
            pending: Vec::new(),
            dirty: None,
            bbox: None,
            seed: 9,
        };
        e.reset();
        Ok(e)
    }

    fn comp_bind(device: &wgpu::Device, layout: &wgpu::BindGroupLayout, ub: &wgpu::Buffer, m1: &wgpu::Texture, m2: &wgpu::Texture, paper: &wgpu::Texture, cols: &wgpu::Texture, smp: &wgpu::Sampler, rep: &wgpu::Sampler) -> wgpu::BindGroup {
        let (v1, v2, v3, v4) = (m1.create_view(&Default::default()), m2.create_view(&Default::default()), paper.create_view(&Default::default()), cols.create_view(&Default::default()));
        device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: None,
            layout,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: ub.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(&v1) },
                wgpu::BindGroupEntry { binding: 2, resource: wgpu::BindingResource::TextureView(&v2) },
                wgpu::BindGroupEntry { binding: 3, resource: wgpu::BindingResource::TextureView(&v3) },
                wgpu::BindGroupEntry { binding: 4, resource: wgpu::BindingResource::Sampler(smp) },
                wgpu::BindGroupEntry { binding: 5, resource: wgpu::BindingResource::Sampler(rep) },
                wgpu::BindGroupEntry { binding: 6, resource: wgpu::BindingResource::TextureView(&v4) },
            ],
        })
    }

    /// The canvas changed size (a new or resized document).
    pub fn resize(&mut self, w: u32, h: u32) {
        if w == 0 || h == 0 || (w == self.config.width && h == self.config.height) {
            return;
        }
        self.config.width = w;
        self.config.height = h;
        self.surface.configure(&self.device, &self.config);
        let usage = wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING;
        self.mask_main = r8(&self.device, w, h, usage);
        self.mask_dual = r8(&self.device, w, h, usage);
        self.cols = tex2d(&self.device, w, h, usage, wgpu::TextureFormat::Rgba8Unorm);
        self.comp_bg = Self::comp_bind(&self.device, &self.comp_layout, &self.ub_comp, &self.mask_main, &self.mask_dual, &self.paper, &self.cols, &self.smp, &self.rep);
        self.reset();
    }

    /// A tip picture: slot 0 = main tip, 1 = dual tip; `data` is w×h coverage bytes (0-255).
    pub fn set_tip(&mut self, slot: u8, w: u32, h: u32, data: &[u8]) {
        if (w * h) as usize > data.len() || w == 0 || h == 0 {
            return;
        }
        self.tips[(slot as usize).min(1)] = Some(make_tip(&self.device, &self.queue, w, h, data));
    }

    /// The paper texture: 256×256 bytes (255 = ink allowed).
    pub fn set_texture(&mut self, data: &[u8]) {
        if data.len() < 65536 {
            return;
        }
        self.queue.write_texture(
            wgpu::ImageCopyTexture { texture: &self.paper, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
            &data[..65536],
            wgpu::ImageDataLayout { offset: 0, bytes_per_row: Some(256), rows_per_image: Some(256) },
            wgpu::Extent3d { width: 256, height: 256, depth_or_array_layers: 1 },
        );
    }

    /// For mixing brushes: `f(x, y, r)` returns the layer's colour under the brush as [r, g, b (0-255), coverage 0-1].
    /// Applies to the next `begin`; pass undefined for none.
    pub fn set_sampler(&mut self, f: Option<js_sys::Function>) {
        self.sampler = f;
    }

    /// Start a stroke. `params` is the brush as JSON; colour 0..1.
    pub fn begin(&mut self, params: &str, r: f32, g: f32, b: f32) -> bool {
        let p: Params = match serde_json::from_str(params) {
            Ok(p) => p,
            Err(_) => return false,
        };
        self.reset();
        let brush = p.brush();
        self.dual_on = brush.dual.is_some() && self.tips[1].is_some();
        self.build = p.build;
        self.binary = p.aa == 0.0;
        let mixing = brush.mix > 0.0 && self.sampler.is_some();
        self.per_dab_col = mixing || brush.color_jitter.as_ref().is_some_and(|j| !j.per_stroke);
        self.tex = [if p.tex.on { 1.0 } else { 0.0 }, p.tex.density / 100.0, p.tex.scale / 100.0, if self.per_dab_col { 1.0 } else { 0.0 }];
        self.seed = self.seed.wrapping_mul(1664525).wrapping_add(1013904223);
        let mut brush = brush;
        if !self.dual_on {
            brush.dual = None;
        }
        let mut stroke = Stroke::new(brush, self.seed);
        stroke.set_color([r, g, b]);
        if mixing {
            let f = self.sampler.take().unwrap();
            stroke.set_sampler(Box::new(move |x, y, rad| {
                let v = f.call3(&JsValue::NULL, &JsValue::from_f64(x as f64), &JsValue::from_f64(y as f64), &JsValue::from_f64(rad as f64)).unwrap_or(JsValue::NULL);
                let a = js_sys::Array::from(&v);
                if a.length() < 4 {
                    return [0.0; 4];
                }
                let n = |i: u32| a.get(i).as_f64().unwrap_or(0.0) as f32;
                [n(0) / 255.0, n(1) / 255.0, n(2) / 255.0, n(3)]
            }));
        }
        self.sampler = None;
        let c = stroke.color();
        self.color = [c[0], c[1], c[2], 1.0];
        self.stroke = Some(stroke);
        let mut enc = self.device.create_command_encoder(&Default::default());
        clear(&mut enc, &self.mask_main.create_view(&Default::default()), 0.0);
        clear(&mut enc, &self.cols.create_view(&Default::default()), 0.0);
        clear(&mut enc, &self.mask_dual.create_view(&Default::default()), if self.dual_on { 0.0 } else { 1.0 });
        self.queue.submit([enc.finish()]);
        true
    }

    /// One pen sample (document px, pressure 0..1, tilt 0 flat..1 upright, azimuth radians, time ms).
    /// Tilt and azimuth are undefined when the pen doesn't report them. Drawn on the next flush.
    pub fn add(&mut self, x: f32, y: f32, p: f32, tilt: Option<f32>, az: Option<f32>, t: f64) {
        if let Some(s) = self.stroke.as_mut() {
            let new = s.add_pen(x, y, p, tilt.filter(|v| v.is_finite()), az.filter(|v| v.is_finite()), t);
            for d in &new {
                if !d.dual {
                    self.dirty = grow(self.dirty, d);
                    self.bbox = grow(self.bbox, d);
                }
            }
            self.pending.extend(new);
        }
    }

    fn draw(&mut self, enc: &mut wgpu::CommandEncoder, dabs: &[Dab]) {
        let (w, h) = (self.config.width as f32, self.config.height as f32);
        for slot in 0..2usize {
            let Some(tip) = &self.tips[slot] else { continue };
            let dual = slot == 1;
            if dual && !self.dual_on {
                continue;
            }
            let inst: Vec<Inst> = dabs
                .iter()
                .filter(|d| d.dual == dual && d.size > 0.3 && d.alpha > 0.0)
                .map(|d| Inst { pos: [d.x, d.y], size: d.size, angle: d.angle, alpha: d.alpha * if d.size < 1.5 { d.size / 1.5 } else { 1.0 }, round: d.round, col: d.col })
                .collect();
            if inst.is_empty() {
                continue;
            }
            self.queue.write_buffer(&self.ub_dab[slot], 0, bytemuck::cast_slice(&[w, h, tip.aspect, if self.binary && !dual { 1.0 } else { 0.0 }]));
            let bg = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: None,
                layout: &self.dab_layout,
                entries: &[
                    wgpu::BindGroupEntry { binding: 0, resource: self.ub_dab[slot].as_entire_binding() },
                    wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(&tip.view) },
                    wgpu::BindGroupEntry { binding: 2, resource: wgpu::BindingResource::Sampler(&self.smp) },
                ],
            });
            let buf = self.device.create_buffer_init(&wgpu::util::BufferInitDescriptor { label: None, contents: bytemuck::cast_slice(&inst), usage: wgpu::BufferUsages::VERTEX });
            let target = if dual { &self.mask_dual } else { &self.mask_main };
            let view = target.create_view(&Default::default());
            let mut rp = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: None,
                color_attachments: &[Some(wgpu::RenderPassColorAttachment { view: &view, resolve_target: None, ops: wgpu::Operations { load: wgpu::LoadOp::Load, store: wgpu::StoreOp::Store } })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            rp.set_pipeline(if dual || self.build { &self.pipe_over } else { &self.pipe_max });
            rp.set_bind_group(0, &bg, &[]);
            rp.set_vertex_buffer(0, buf.slice(..));
            rp.draw(0..4, 0..inst.len() as u32);
            drop(rp);
            if !dual && self.per_dab_col {
                let view = self.cols.create_view(&Default::default());
                let mut rp = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: None,
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment { view: &view, resolve_target: None, ops: wgpu::Operations { load: wgpu::LoadOp::Load, store: wgpu::StoreOp::Store } })],
                    depth_stencil_attachment: None,
                    timestamp_writes: None,
                    occlusion_query_set: None,
                });
                rp.set_pipeline(&self.pipe_col);
                rp.set_bind_group(0, &bg, &[]);
                rp.set_vertex_buffer(0, buf.slice(..));
                rp.draw(0..4, 0..inst.len() as u32);
            }
        }
    }

    fn compose(&mut self, enc: wgpu::CommandEncoder) {
        let mut enc = enc;
        let mut u = self.color.to_vec();
        u.extend_from_slice(&self.tex);
        self.queue.write_buffer(&self.ub_comp, 0, bytemuck::cast_slice(&u));
        let frame = match self.surface.get_current_texture() {
            Ok(f) => f,
            Err(_) => {
                self.surface.configure(&self.device, &self.config);
                self.queue.submit([enc.finish()]);
                return;
            }
        };
        let view = frame.texture.create_view(&Default::default());
        {
            let mut rp = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: None,
                color_attachments: &[Some(wgpu::RenderPassColorAttachment { view: &view, resolve_target: None, ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT), store: wgpu::StoreOp::Store } })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            rp.set_pipeline(&self.comp_pipe);
            rp.set_bind_group(0, &self.comp_bg, &[]);
            rp.draw(0..4, 0..1);
        }
        self.queue.submit([enc.finish()]);
        frame.present();
    }

    /// Draw what's new and update the canvas. Returns the changed area [x0, y0, x1, y1] (empty if nothing).
    pub fn flush(&mut self) -> Vec<f32> {
        let Some(r) = self.dirty.take() else { return Vec::new() };
        let dabs = std::mem::take(&mut self.pending);
        let mut enc = self.device.create_command_encoder(&Default::default());
        self.draw(&mut enc, &dabs);
        self.compose(enc);
        r.to_vec()
    }

    /// The pen lifted: redraw the whole stroke with its final taper.
    pub fn end(&mut self) {
        let Some(mut s) = self.stroke.take() else { return };
        let all = s.finish();
        for d in all.iter().filter(|d| !d.dual) {
            self.bbox = grow(self.bbox, d);
        }
        self.pending.clear();
        self.dirty = None;
        let mut enc = self.device.create_command_encoder(&Default::default());
        clear(&mut enc, &self.mask_main.create_view(&Default::default()), 0.0);
        clear(&mut enc, &self.mask_dual.create_view(&Default::default()), if self.dual_on { 0.0 } else { 1.0 });
        clear(&mut enc, &self.cols.create_view(&Default::default()), 0.0);
        self.draw(&mut enc, &all);
        self.compose(enc);
    }

    /// The whole stroke's area [x0, y0, x1, y1] (empty if nothing drawn).
    pub fn bbox(&self) -> Vec<f32> {
        self.bbox.map(|b| b.to_vec()).unwrap_or_default()
    }

    /// Forget the stroke and clear the canvas.
    pub fn reset(&mut self) {
        self.stroke = None;
        self.pending.clear();
        self.dirty = None;
        self.bbox = None;
        let mut enc = self.device.create_command_encoder(&Default::default());
        clear(&mut enc, &self.mask_main.create_view(&Default::default()), 0.0);
        clear(&mut enc, &self.mask_dual.create_view(&Default::default()), 1.0);
        self.compose(enc);
    }
}
