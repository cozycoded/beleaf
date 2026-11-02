//! beleaf engine test: the Rust brush engine (brush-core) drawn on the graphics card with wgpu.
//!
//! - Windows: a native window; the pen is read straight from Windows Ink (pressure, tilt, every
//!   in-between sample), no browser in the way.
//! - Browser: the same code compiled to WebAssembly, drawing with WebGL2.
//!
//! How a stroke is drawn: every dab is a textured quad. Main-tip dabs go into one mask and
//! dual-tip dabs into another, both with "max" blending (so overlapping dabs don't build up, like
//! Clip Studio). The stroke shows as colour × main mask × dual mask over the layer; when the pen
//! lifts, the whole stroke is redrawn once with its final taper and merged into the layer.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use brush_core::{Brush, Dab, Stroke};
use wgpu::util::DeviceExt;
use winit::application::ApplicationHandler;
use winit::event::{ElementState, MouseButton, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoop};
use winit::window::{Window, WindowId};

/* ---------------- shared input and settings ---------------- */

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Kind {
    Down,
    Move,
    Up,
}

#[derive(Clone, Copy, Debug)]
pub struct Sample {
    pub x: f32,
    pub y: f32,
    pub pressure: f32,
    pub kind: Kind,
    pub t: f64,
}

static INPUT: Mutex<Vec<Sample>> = Mutex::new(Vec::new());
pub fn push(s: Sample) {
    if let Ok(mut q) = INPUT.lock() {
        q.push(s);
    }
}

/// What the pen last reported, for the on-screen readout.
#[derive(Clone, Copy, Default)]
pub struct PenInfo {
    pub pressure: f32,
    pub tilt_x: i32,
    pub tilt_y: i32,
    pub source: u8, // 0 none, 1 Windows Ink pen, 2 mouse, 3 browser pointer
    pub samples: u32,
}
pub static PEN: Mutex<PenInfo> = Mutex::new(PenInfo { pressure: 0.0, tilt_x: 0, tilt_y: 0, source: 0, samples: 0 });

pub struct Settings {
    pub color: [f32; 3],
    pub size: f32,
    pub opacity: f32,
    pub dual: bool,
    pub taper: bool,
    pub follow: bool,
    pub clear: bool,
}
pub static SETTINGS: Mutex<Settings> = Mutex::new(Settings { color: [0.16, 0.12, 0.14], size: 28.0, opacity: 1.0, dual: true, taper: true, follow: true, clear: false });

fn brush_from_settings() -> (Brush, [f32; 3], f32) {
    let s = SETTINGS.lock().unwrap();
    let mut b = Brush::textured_acrylic(s.size);
    if !s.dual {
        b.dual = None;
    }
    if !s.taper {
        b.taper_in = 0.0;
        b.taper_out = 0.0;
    }
    b.follow = s.follow;
    (b, s.color, s.opacity)
}

#[cfg(not(target_arch = "wasm32"))]
fn now_ms() -> f64 {
    use std::sync::OnceLock;
    static T0: OnceLock<std::time::Instant> = OnceLock::new();
    T0.get_or_init(std::time::Instant::now).elapsed().as_secs_f64() * 1000.0
}
#[cfg(target_arch = "wasm32")]
fn now_ms() -> f64 {
    web_sys::window().and_then(|w| w.performance()).map(|p| p.now()).unwrap_or(0.0)
}

/* ---------------- shaders ---------------- */

const DAB_WGSL: &str = r#"
struct U { canvas: vec2<f32>, aspect: f32, pad: f32, };
@group(0) @binding(0) var<uniform> u: U;
@group(0) @binding(1) var tip: texture_2d<f32>;
@group(0) @binding(2) var smp: sampler;
struct VO { @builtin(position) pos: vec4<f32>, @location(0) uv: vec2<f32>, @location(1) a: f32, };
@vertex
fn vs(@builtin(vertex_index) vi: u32, @location(0) p: vec2<f32>, @location(1) size: f32, @location(2) ang: f32, @location(3) alpha: f32) -> VO {
  var c = array<vec2<f32>, 4>(vec2<f32>(-0.5, -0.5), vec2<f32>(0.5, -0.5), vec2<f32>(-0.5, 0.5), vec2<f32>(0.5, 0.5));
  let q = c[vi];
  var sw = size;
  var sh = size;
  if (u.aspect >= 1.0) { sh = size / u.aspect; } else { sw = size * u.aspect; }
  let l = vec2<f32>(q.x * sw, q.y * sh);
  let cs = cos(ang);
  let sn = sin(ang);
  let r = vec2<f32>(l.x * cs - l.y * sn, l.x * sn + l.y * cs);
  let px = p + r;
  var o: VO;
  o.pos = vec4<f32>(px.x / u.canvas.x * 2.0 - 1.0, 1.0 - px.y / u.canvas.y * 2.0, 0.0, 1.0);
  o.uv = q + vec2<f32>(0.5, 0.5);
  o.a = alpha;
  return o;
}
@fragment
fn fs(i: VO) -> @location(0) vec4<f32> {
  let t = textureSample(tip, smp, i.uv).r * i.a;
  return vec4<f32>(t, t, t, t);
}
"#;

const RECT_VS: &str = r#"
struct VO { @builtin(position) pos: vec4<f32>, @location(0) uv: vec2<f32>, };
fn rect_vs(vi: u32, rect: vec4<f32>) -> VO {
  var c = array<vec2<f32>, 4>(vec2<f32>(0.0, 0.0), vec2<f32>(1.0, 0.0), vec2<f32>(0.0, 1.0), vec2<f32>(1.0, 1.0));
  let q = c[vi];
  var o: VO;
  o.pos = vec4<f32>(mix(rect.x, rect.z, q.x), mix(rect.y, rect.w, q.y), 0.0, 1.0);
  o.uv = q;
  return o;
}
"#;

const COMP_WGSL: &str = r#"
struct U { rect: vec4<f32>, color: vec4<f32>, };
@group(0) @binding(0) var<uniform> u: U;
@group(0) @binding(1) var m1: texture_2d<f32>;
@group(0) @binding(2) var m2: texture_2d<f32>;
@group(0) @binding(3) var smp: sampler;
@vertex fn vs(@builtin(vertex_index) vi: u32) -> VO { return rect_vs(vi, u.rect); }
@fragment fn fs(i: VO) -> @location(0) vec4<f32> {
  let a = textureSample(m1, smp, i.uv).r * textureSample(m2, smp, i.uv).r * u.color.a;
  return vec4<f32>(u.color.rgb * a, a);
}
"#;

const BLIT_WGSL: &str = r#"
struct U { rect: vec4<f32>, };
@group(0) @binding(0) var<uniform> u: U;
@group(0) @binding(1) var tex: texture_2d<f32>;
@group(0) @binding(2) var smp: sampler;
@vertex fn vs(@builtin(vertex_index) vi: u32) -> VO { return rect_vs(vi, u.rect); }
@fragment fn fs(i: VO) -> @location(0) vec4<f32> { return textureSample(tex, smp, i.uv); }
"#;

/* ---------------- GPU state ---------------- */

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct Inst {
    pos: [f32; 2],
    size: f32,
    angle: f32,
    alpha: f32,
}

struct Tip {
    view: wgpu::TextureView,
    aspect: f32,
}

struct Gfx {
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    cw: u32,
    ch: u32,
    layer: wgpu::Texture,
    mask_main: wgpu::Texture,
    mask_dual: wgpu::Texture,
    dab_pipe: wgpu::RenderPipeline,
    comp_surface: wgpu::RenderPipeline,
    comp_layer: wgpu::RenderPipeline,
    blit_pipe: wgpu::RenderPipeline,
    dab_bg_main: wgpu::BindGroup,
    dab_bg_dual: wgpu::BindGroup,
    comp_bg_live: wgpu::BindGroup,
    comp_bg_commit: wgpu::BindGroup,
    blit_bg: wgpu::BindGroup,
    ub_live: wgpu::Buffer,
    ub_commit: wgpu::Buffer,
    ub_blit: wgpu::Buffer,
    stroking: bool,
    stroke_color: [f32; 4],
    dual_on: bool,
}

fn tip_from_bytes(bytes: &[u8]) -> (u32, u32, Vec<u8>) {
    let w = u32::from_le_bytes(bytes[0..4].try_into().unwrap());
    let h = u32::from_le_bytes(bytes[4..8].try_into().unwrap());
    // a 1-pixel empty border so the edges never smear when sampled
    let (pw, ph) = (w + 2, h + 2);
    let mut d = vec![0u8; (pw * ph) as usize];
    for y in 0..h {
        for x in 0..w {
            d[((y + 1) * pw + x + 1) as usize] = bytes[8 + (y * w + x) as usize];
        }
    }
    (pw, ph, d)
}

fn make_tip(device: &wgpu::Device, queue: &wgpu::Queue, bytes: &[u8]) -> Tip {
    let (w, h, data) = tip_from_bytes(bytes);
    // mip levels so small dabs of a big tip still look smooth
    let mut levels = vec![(w, h, data)];
    while levels.len() < 8 {
        let (pw, ph, pd) = levels.last().unwrap();
        if *pw <= 2 || *ph <= 2 {
            break;
        }
        let (nw, nh) = ((pw / 2).max(1), (ph / 2).max(1));
        let mut nd = vec![0u8; (nw * nh) as usize];
        for y in 0..nh {
            for x in 0..nw {
                let mut s = 0u32;
                for (dx, dy) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
                    let sx = (x * 2 + dx).min(pw - 1);
                    let sy = (y * 2 + dy).min(ph - 1);
                    s += pd[(sy * pw + sx) as usize] as u32;
                }
                nd[(y * nw + x) as usize] = (s / 4) as u8;
            }
        }
        levels.push((nw, nh, nd));
    }
    let tex = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("tip"),
        size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
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
    Tip { view: tex.create_view(&Default::default()), aspect: w as f32 / h as f32 }
}

fn target(format: wgpu::TextureFormat, blend: wgpu::BlendState) -> Option<wgpu::ColorTargetState> {
    Some(wgpu::ColorTargetState { format, blend: Some(blend), write_mask: wgpu::ColorWrites::ALL })
}

fn pipeline(device: &wgpu::Device, layout: &wgpu::BindGroupLayout, module: &wgpu::ShaderModule, buffers: &[wgpu::VertexBufferLayout], t: Option<wgpu::ColorTargetState>) -> wgpu::RenderPipeline {
    let pl = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor { label: None, bind_group_layouts: &[layout], push_constant_ranges: &[] });
    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: None,
        layout: Some(&pl),
        vertex: wgpu::VertexState { module, entry_point: Some("vs"), compilation_options: Default::default(), buffers },
        fragment: Some(wgpu::FragmentState { module, entry_point: Some("fs"), compilation_options: Default::default(), targets: &[t] }),
        primitive: wgpu::PrimitiveState { topology: wgpu::PrimitiveTopology::TriangleStrip, ..Default::default() },
        depth_stencil: None,
        multisample: wgpu::MultisampleState::default(),
        multiview: None,
        cache: None,
    })
}

fn bgl(device: &wgpu::Device, textures: u32) -> wgpu::BindGroupLayout {
    let mut e = vec![wgpu::BindGroupLayoutEntry {
        binding: 0,
        visibility: wgpu::ShaderStages::VERTEX | wgpu::ShaderStages::FRAGMENT,
        ty: wgpu::BindingType::Buffer { ty: wgpu::BufferBindingType::Uniform, has_dynamic_offset: false, min_binding_size: None },
        count: None,
    }];
    for i in 0..textures {
        e.push(wgpu::BindGroupLayoutEntry {
            binding: 1 + i,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Texture { sample_type: wgpu::TextureSampleType::Float { filterable: true }, view_dimension: wgpu::TextureViewDimension::D2, multisampled: false },
            count: None,
        });
    }
    e.push(wgpu::BindGroupLayoutEntry { binding: 1 + textures, visibility: wgpu::ShaderStages::FRAGMENT, ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering), count: None });
    device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor { label: None, entries: &e })
}

fn ubuf(device: &wgpu::Device, data: &[f32]) -> wgpu::Buffer {
    device.create_buffer_init(&wgpu::util::BufferInitDescriptor { label: None, contents: bytemuck::cast_slice(data), usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST })
}

fn bind(device: &wgpu::Device, layout: &wgpu::BindGroupLayout, ub: &wgpu::Buffer, views: &[&wgpu::TextureView], smp: &wgpu::Sampler) -> wgpu::BindGroup {
    let mut e = vec![wgpu::BindGroupEntry { binding: 0, resource: ub.as_entire_binding() }];
    for (i, v) in views.iter().enumerate() {
        e.push(wgpu::BindGroupEntry { binding: 1 + i as u32, resource: wgpu::BindingResource::TextureView(*v) });
    }
    e.push(wgpu::BindGroupEntry { binding: 1 + views.len() as u32, resource: wgpu::BindingResource::Sampler(smp) });
    device.create_bind_group(&wgpu::BindGroupDescriptor { label: None, layout, entries: &e })
}

fn canvas_tex(device: &wgpu::Device, w: u32, h: u32, format: wgpu::TextureFormat) -> wgpu::Texture {
    device.create_texture(&wgpu::TextureDescriptor {
        label: None,
        size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
        view_formats: &[],
    })
}

fn clear_pass(enc: &mut wgpu::CommandEncoder, view: &wgpu::TextureView, v: f64) {
    enc.begin_render_pass(&wgpu::RenderPassDescriptor {
        label: None,
        color_attachments: &[Some(wgpu::RenderPassColorAttachment { view, resolve_target: None, ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color { r: v, g: v, b: v, a: v }), store: wgpu::StoreOp::Store } })],
        depth_stencil_attachment: None,
        timestamp_writes: None,
        occlusion_query_set: None,
    });
}

const PREMUL_OVER: wgpu::BlendState = wgpu::BlendState {
    color: wgpu::BlendComponent { src_factor: wgpu::BlendFactor::One, dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha, operation: wgpu::BlendOperation::Add },
    alpha: wgpu::BlendComponent { src_factor: wgpu::BlendFactor::One, dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha, operation: wgpu::BlendOperation::Add },
};
const MAX_BLEND: wgpu::BlendState = wgpu::BlendState {
    color: wgpu::BlendComponent { src_factor: wgpu::BlendFactor::One, dst_factor: wgpu::BlendFactor::One, operation: wgpu::BlendOperation::Max },
    alpha: wgpu::BlendComponent { src_factor: wgpu::BlendFactor::One, dst_factor: wgpu::BlendFactor::One, operation: wgpu::BlendOperation::Max },
};

impl Gfx {
    async fn new(window: Arc<Window>) -> Gfx {
        let size = window.inner_size();
        let (w, h) = (size.width.max(800), size.height.max(600));
        // the drawing area: the whole screen on Windows (so making the window bigger shows more), the browser window on the web
        #[cfg(not(target_arch = "wasm32"))]
        let (cw0, ch0) = window.current_monitor().map(|m| (m.size().width, m.size().height)).unwrap_or((w, h));
        #[cfg(target_arch = "wasm32")]
        let (cw0, ch0) = web_sys::window()
            .map(|wn| {
                let d = wn.device_pixel_ratio();
                let iw = wn.inner_width().ok().and_then(|v| v.as_f64()).unwrap_or(1200.0);
                let ih = wn.inner_height().ok().and_then(|v| v.as_f64()).unwrap_or(800.0);
                ((iw * d) as u32, (ih * d) as u32)
            })
            .unwrap_or((w, h));
        #[cfg(target_arch = "wasm32")]
        let backends = wgpu::Backends::GL;
        #[cfg(not(target_arch = "wasm32"))]
        let backends = wgpu::Backends::PRIMARY;
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor { backends, ..Default::default() });
        let surface = instance.create_surface(window.clone()).expect("surface");
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions { power_preference: wgpu::PowerPreference::HighPerformance, compatible_surface: Some(&surface), force_fallback_adapter: false })
            .await
            .expect("no graphics adapter");
        #[cfg(target_arch = "wasm32")]
        let limits = wgpu::Limits::downlevel_webgl2_defaults().using_resolution(adapter.limits());
        #[cfg(not(target_arch = "wasm32"))]
        let limits = wgpu::Limits::default().using_resolution(adapter.limits());
        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor { label: None, required_features: wgpu::Features::empty(), required_limits: limits, memory_hints: Default::default() }, None)
            .await
            .expect("device");
        let caps = surface.get_capabilities(&adapter);
        let format = caps.formats.iter().copied().find(|f| !f.is_srgb()).unwrap_or(caps.formats[0]);
        let present_mode = if caps.present_modes.contains(&wgpu::PresentMode::Mailbox) { wgpu::PresentMode::Mailbox } else { wgpu::PresentMode::AutoVsync };
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width: w,
            height: h,
            present_mode,
            alpha_mode: caps.alpha_modes[0],
            view_formats: vec![],
            desired_maximum_frame_latency: 1,
        };
        surface.configure(&device, &config);

        let maxd = device.limits().max_texture_dimension_2d;
        let (cw, ch) = (cw0.max(w).min(maxd), ch0.max(h).min(maxd));
        let layer = canvas_tex(&device, cw, ch, wgpu::TextureFormat::Rgba8Unorm);
        let mask_main = canvas_tex(&device, cw, ch, wgpu::TextureFormat::R8Unorm);
        let mask_dual = canvas_tex(&device, cw, ch, wgpu::TextureFormat::R8Unorm);
        let smp = device.create_sampler(&wgpu::SamplerDescriptor {
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        let tip_main = make_tip(&device, &queue, include_bytes!("../assets/tip_main.bin"));
        let tip_dual = make_tip(&device, &queue, include_bytes!("../assets/tip_dual.bin"));

        let dab_mod = device.create_shader_module(wgpu::ShaderModuleDescriptor { label: Some("dab"), source: wgpu::ShaderSource::Wgsl(DAB_WGSL.into()) });
        let comp_mod = device.create_shader_module(wgpu::ShaderModuleDescriptor { label: Some("comp"), source: wgpu::ShaderSource::Wgsl(format!("{RECT_VS}{COMP_WGSL}").into()) });
        let blit_mod = device.create_shader_module(wgpu::ShaderModuleDescriptor { label: Some("blit"), source: wgpu::ShaderSource::Wgsl(format!("{RECT_VS}{BLIT_WGSL}").into()) });
        let dab_l = bgl(&device, 1);
        let comp_l = bgl(&device, 2);
        let blit_l = bgl(&device, 1);
        let inst_layout = wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<Inst>() as u64,
            step_mode: wgpu::VertexStepMode::Instance,
            attributes: &wgpu::vertex_attr_array![0 => Float32x2, 1 => Float32, 2 => Float32, 3 => Float32],
        };
        let dab_pipe = pipeline(&device, &dab_l, &dab_mod, &[inst_layout], target(wgpu::TextureFormat::R8Unorm, MAX_BLEND));
        let comp_surface = pipeline(&device, &comp_l, &comp_mod, &[], target(format, PREMUL_OVER));
        let comp_layer = pipeline(&device, &comp_l, &comp_mod, &[], target(wgpu::TextureFormat::Rgba8Unorm, PREMUL_OVER));
        let blit_pipe = pipeline(&device, &blit_l, &blit_mod, &[], target(format, wgpu::BlendState::REPLACE));

        let ub_dab_main = ubuf(&device, &[cw as f32, ch as f32, tip_main.aspect, 0.0]);
        let ub_dab_dual = ubuf(&device, &[cw as f32, ch as f32, tip_dual.aspect, 0.0]);
        let ub_live = ubuf(&device, &[0.0; 8]);
        let ub_commit = ubuf(&device, &[-1.0, 1.0, 1.0, -1.0, 0.0, 0.0, 0.0, 1.0]);
        let ub_blit = ubuf(&device, &[0.0; 4]);
        let mm = mask_main.create_view(&Default::default());
        let md = mask_dual.create_view(&Default::default());
        let lv = layer.create_view(&Default::default());
        let g = Gfx {
            dab_bg_main: bind(&device, &dab_l, &ub_dab_main, &[&tip_main.view], &smp),
            dab_bg_dual: bind(&device, &dab_l, &ub_dab_dual, &[&tip_dual.view], &smp),
            comp_bg_live: bind(&device, &comp_l, &ub_live, &[&mm, &md], &smp),
            comp_bg_commit: bind(&device, &comp_l, &ub_commit, &[&mm, &md], &smp),
            blit_bg: bind(&device, &blit_l, &ub_blit, &[&lv], &smp),
            surface,
            device,
            queue,
            config,
            cw,
            ch,
            layer,
            mask_main,
            mask_dual,
            dab_pipe,
            comp_surface,
            comp_layer,
            blit_pipe,
            ub_live,
            ub_commit,
            ub_blit,
            stroking: false,
            stroke_color: [0.0, 0.0, 0.0, 1.0],
            dual_on: true,
        };
        // the paper: start with an opaque white layer
        let mut enc = g.device.create_command_encoder(&Default::default());
        clear_pass(&mut enc, &lv, 1.0);
        g.queue.submit([enc.finish()]);
        g
    }

    fn resize(&mut self, w: u32, h: u32) {
        if w == 0 || h == 0 {
            return;
        }
        self.config.width = w;
        self.config.height = h;
        self.surface.configure(&self.device, &self.config);
    }

    /// Where the canvas sits in the window: centred, scaled down only if the window is smaller.
    fn view(&self) -> (f32, f32, f32) {
        let (w, h) = (self.config.width as f32, self.config.height as f32);
        let s = (w / self.cw as f32).min(h / self.ch as f32).min(1.0);
        ((w - self.cw as f32 * s) / 2.0, (h - self.ch as f32 * s) / 2.0, s)
    }
    fn to_canvas(&self, x: f32, y: f32) -> (f32, f32) {
        let (ox, oy, s) = self.view();
        ((x - ox) / s, (y - oy) / s)
    }

    fn draw_dabs(&self, enc: &mut wgpu::CommandEncoder, dabs: &[Dab]) {
        for (dual, bg, tex) in [(false, &self.dab_bg_main, &self.mask_main), (true, &self.dab_bg_dual, &self.mask_dual)] {
            let inst: Vec<Inst> = dabs.iter().filter(|d| d.dual == dual && d.size > 0.05).map(|d| Inst { pos: [d.x, d.y], size: d.size, angle: d.angle, alpha: d.alpha }).collect();
            if inst.is_empty() {
                continue;
            }
            let buf = self.device.create_buffer_init(&wgpu::util::BufferInitDescriptor { label: None, contents: bytemuck::cast_slice(&inst), usage: wgpu::BufferUsages::VERTEX });
            let view = tex.create_view(&Default::default());
            let mut rp = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: None,
                color_attachments: &[Some(wgpu::RenderPassColorAttachment { view: &view, resolve_target: None, ops: wgpu::Operations { load: wgpu::LoadOp::Load, store: wgpu::StoreOp::Store } })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            rp.set_pipeline(&self.dab_pipe);
            rp.set_bind_group(0, bg, &[]);
            rp.set_vertex_buffer(0, buf.slice(..));
            rp.draw(0..4, 0..inst.len() as u32);
        }
    }

    fn begin_stroke(&mut self, enc: &mut wgpu::CommandEncoder, color: [f32; 3], opacity: f32, dual: bool) {
        self.stroking = true;
        self.stroke_color = [color[0], color[1], color[2], opacity];
        self.dual_on = dual;
        clear_pass(enc, &self.mask_main.create_view(&Default::default()), 0.0);
        // without a dual tip the dual mask is all 1s, so it changes nothing
        clear_pass(enc, &self.mask_dual.create_view(&Default::default()), if dual { 0.0 } else { 1.0 });
    }

    fn commit_stroke(&mut self, enc: &mut wgpu::CommandEncoder, all: &[Dab]) {
        // redraw the whole stroke once with its final taper, then merge it into the layer
        clear_pass(enc, &self.mask_main.create_view(&Default::default()), 0.0);
        clear_pass(enc, &self.mask_dual.create_view(&Default::default()), if self.dual_on { 0.0 } else { 1.0 });
        self.draw_dabs(enc, all);
        self.queue.write_buffer(&self.ub_commit, 16, bytemuck::cast_slice(&self.stroke_color));
        let lv = self.layer.create_view(&Default::default());
        {
            let mut rp = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: None,
                color_attachments: &[Some(wgpu::RenderPassColorAttachment { view: &lv, resolve_target: None, ops: wgpu::Operations { load: wgpu::LoadOp::Load, store: wgpu::StoreOp::Store } })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            rp.set_pipeline(&self.comp_layer);
            rp.set_bind_group(0, &self.comp_bg_commit, &[]);
            rp.draw(0..4, 0..1);
        }
        self.stroking = false;
    }

    fn render(&mut self, enc: wgpu::CommandEncoder) {
        let mut enc = enc;
        let frame = match self.surface.get_current_texture() {
            Ok(f) => f,
            Err(_) => {
                self.surface.configure(&self.device, &self.config);
                self.queue.submit([enc.finish()]);
                return;
            }
        };
        let fv = frame.texture.create_view(&Default::default());
        let (ox, oy, s) = self.view();
        let (w, h) = (self.config.width as f32, self.config.height as f32);
        let rect = [ox / w * 2.0 - 1.0, 1.0 - oy / h * 2.0, (ox + self.cw as f32 * s) / w * 2.0 - 1.0, 1.0 - (oy + self.ch as f32 * s) / h * 2.0];
        self.queue.write_buffer(&self.ub_blit, 0, bytemuck::cast_slice(&rect));
        let mut live = rect.to_vec();
        live.extend_from_slice(&self.stroke_color);
        self.queue.write_buffer(&self.ub_live, 0, bytemuck::cast_slice(&live));
        {
            let mut rp = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: None,
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &fv,
                    resolve_target: None,
                    ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color { r: 0.06, g: 0.06, b: 0.07, a: 1.0 }), store: wgpu::StoreOp::Store },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            rp.set_pipeline(&self.blit_pipe);
            rp.set_bind_group(0, &self.blit_bg, &[]);
            rp.draw(0..4, 0..1);
            if self.stroking {
                rp.set_pipeline(&self.comp_surface);
                rp.set_bind_group(0, &self.comp_bg_live, &[]);
                rp.draw(0..4, 0..1);
            }
        }
        self.queue.submit([enc.finish()]);
        frame.present();
    }
}

/* ---------------- app ---------------- */

struct State {
    window: Option<Arc<Window>>,
    gfx: Option<Gfx>,
    stroke: Option<Stroke>,
    mouse_down: bool,
    cursor: (f32, f32),
    seed: u32,
    last_title: f64,
}

struct App {
    st: Rc<RefCell<State>>,
}

impl App {
    fn frame(&mut self) {
        let mut st = self.st.borrow_mut();
        let st = &mut *st;
        let Some(g) = st.gfx.as_mut() else { return };
        let mut enc = g.device.create_command_encoder(&Default::default());
        {
            let mut s = SETTINGS.lock().unwrap();
            if s.clear {
                s.clear = false;
                clear_pass(&mut enc, &g.layer.create_view(&Default::default()), 1.0);
            }
        }
        let samples: Vec<Sample> = std::mem::take(&mut *INPUT.lock().unwrap());
        let mut fresh: Vec<Dab> = Vec::new();
        for s in samples {
            let (x, y) = g.to_canvas(s.x, s.y);
            match s.kind {
                Kind::Down => {
                    let (b, color, op) = brush_from_settings();
                    let dual = b.dual.is_some();
                    st.seed = st.seed.wrapping_mul(1664525).wrapping_add(1013904223);
                    let mut stroke = Stroke::new(b, st.seed);
                    g.draw_dabs(&mut enc, &fresh);
                    fresh.clear();
                    g.begin_stroke(&mut enc, color, op, dual);
                    fresh.extend(stroke.add(x, y, s.pressure, s.t));
                    st.stroke = Some(stroke);
                }
                Kind::Move => {
                    if let Some(stroke) = st.stroke.as_mut() {
                        fresh.extend(stroke.add(x, y, s.pressure, s.t));
                    }
                }
                Kind::Up => {
                    if let Some(mut stroke) = st.stroke.take() {
                        fresh.extend(stroke.add(x, y, s.pressure, s.t));
                        let all = stroke.finish();
                        fresh.clear();
                        g.commit_stroke(&mut enc, &all);
                    }
                }
            }
        }
        g.draw_dabs(&mut enc, &fresh);
        g.render(enc);

        // native: show what the pen reports in the title bar
        #[cfg(not(target_arch = "wasm32"))]
        {
        if now_ms() - st.last_title > 250.0 {
            st.last_title = now_ms();
            if let Some(w) = &st.window {
                let p = *PEN.lock().unwrap();
                let s = SETTINGS.lock().unwrap();
                let src = ["no input yet", "Windows Ink pen", "mouse", "pointer"][p.source as usize];
                w.set_title(&format!(
                    "beleaf engine test · size {:.0} (keys [ ]) · {} · pressure {:.2} · tilt {} / {} · dual {} (D) · taper {} (T) · C clears · 1-6 colours",
                    s.size, src, p.pressure, p.tilt_x, p.tilt_y, if s.dual { "on" } else { "off" }, if s.taper { "on" } else { "off" }
                ));
            }
        }
        }
    }
}

fn key(c: &str) {
    let mut s = SETTINGS.lock().unwrap();
    let pal = [[0.16, 0.12, 0.14], [0.85, 0.35, 0.25], [0.95, 0.65, 0.35], [0.36, 0.55, 0.94], [0.36, 0.78, 0.66], [0.55, 0.43, 0.72]];
    match c {
        "[" => s.size = (s.size / 1.15).max(1.0),
        "]" => s.size = (s.size * 1.15).min(400.0),
        "d" | "D" => s.dual = !s.dual,
        "t" | "T" => s.taper = !s.taper,
        "f" | "F" => s.follow = !s.follow,
        "c" | "C" => s.clear = true,
        _ => {
            if let Ok(n) = c.parse::<usize>() {
                if (1..=6).contains(&n) {
                    s.color = pal[n - 1];
                }
            }
        }
    }
}

impl ApplicationHandler for App {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        if self.st.borrow().window.is_some() {
            return;
        }
        #[allow(unused_mut)]
        let mut attrs = Window::default_attributes().with_title("beleaf engine test").with_inner_size(winit::dpi::LogicalSize::new(1280.0, 820.0));
        #[cfg(target_arch = "wasm32")]
        {
            use winit::platform::web::WindowAttributesExtWebSys;
            attrs = attrs.with_append(true);
        }
        let window = Arc::new(el.create_window(attrs).expect("window"));
        #[cfg(windows)]
        unsafe {
            winpen::install(&window);
        }
        #[cfg(target_arch = "wasm32")]
        webpen::install(&window);
        self.st.borrow_mut().window = Some(window.clone());
        #[cfg(not(target_arch = "wasm32"))]
        {
            let g = pollster::block_on(Gfx::new(window.clone()));
            self.st.borrow_mut().gfx = Some(g);
        }
        #[cfg(target_arch = "wasm32")]
        {
            let st = self.st.clone();
            wasm_bindgen_futures::spawn_local(async move {
                let g = Gfx::new(window.clone()).await;
                st.borrow_mut().gfx = Some(g);
                window.request_redraw();
            });
        }
    }

    fn window_event(&mut self, el: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => el.exit(),
            WindowEvent::Resized(s) => {
                if let Some(g) = self.st.borrow_mut().gfx.as_mut() {
                    g.resize(s.width, s.height);
                }
            }
            WindowEvent::RedrawRequested => self.frame(),
            // the mouse (the pen comes in separately, straight from Windows Ink / the browser)
            #[cfg(not(target_arch = "wasm32"))]
            WindowEvent::CursorMoved { position, .. } => {
                let mut st = self.st.borrow_mut();
                st.cursor = (position.x as f32, position.y as f32);
                if st.mouse_down {
                    push(Sample { x: st.cursor.0, y: st.cursor.1, pressure: 0.6, kind: Kind::Move, t: now_ms() });
                }
            }
            #[cfg(not(target_arch = "wasm32"))]
            WindowEvent::MouseInput { state, button: MouseButton::Left, .. } => {
                let mut st = self.st.borrow_mut();
                let down = state == ElementState::Pressed;
                if down == st.mouse_down {
                    return;
                }
                st.mouse_down = down;
                push(Sample { x: st.cursor.0, y: st.cursor.1, pressure: 0.6, kind: if down { Kind::Down } else { Kind::Up }, t: now_ms() });
                let mut p = PEN.lock().unwrap();
                p.source = 2;
                p.pressure = 0.6;
            }
            WindowEvent::KeyboardInput { event, .. } => {
                if event.state == ElementState::Pressed {
                    if let winit::keyboard::Key::Character(c) = &event.logical_key {
                        key(c.as_str());
                    }
                }
            }
            _ => {}
        }
    }

    fn about_to_wait(&mut self, _el: &ActiveEventLoop) {
        if let Some(w) = &self.st.borrow().window {
            w.request_redraw();
        }
    }
}

/* ---------------- Windows Ink: read the pen straight from Windows ---------------- */
#[cfg(windows)]
mod winpen {
    use super::*;
    use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
    use windows_sys::Win32::Graphics::Gdi::ScreenToClient;
    use windows_sys::Win32::UI::Input::Pointer::{GetPointerPenInfo, GetPointerPenInfoHistory, GetPointerType, POINTER_FLAG_INCONTACT, POINTER_PEN_INFO};
    use windows_sys::Win32::UI::Shell::{DefSubclassProc, SetWindowSubclass};
    use windows_sys::Win32::UI::WindowsAndMessaging::{POINTER_INPUT_TYPE, PT_PEN, WM_POINTERDOWN, WM_POINTERUP, WM_POINTERUPDATE};
    use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};

    pub unsafe fn install(window: &Window) {
        if let Ok(h) = window.window_handle() {
            if let RawWindowHandle::Win32(w) = h.as_raw() {
                let hwnd = w.hwnd.get() as HWND;
                SetWindowSubclass(hwnd, Some(proc_), 0xBEEF, 0);
            }
        }
    }

    static DOWN: Mutex<bool> = Mutex::new(false);

    unsafe extern "system" fn proc_(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM, _id: usize, _data: usize) -> LRESULT {
        if msg == WM_POINTERDOWN || msg == WM_POINTERUPDATE || msg == WM_POINTERUP {
            let id = (wp & 0xFFFF) as u32;
            let mut ty: POINTER_INPUT_TYPE = 0;
            if GetPointerType(id, &mut ty) != 0 && ty == PT_PEN {
                let mut one: POINTER_PEN_INFO = std::mem::zeroed();
                if GetPointerPenInfo(id, &mut one) != 0 {
                    // every sample Windows collected since the last message, oldest first
                    let mut n = one.pointerInfo.historyCount.clamp(1, 64);
                    let mut hist: [POINTER_PEN_INFO; 64] = std::mem::zeroed();
                    let ok = GetPointerPenInfoHistory(id, &mut n, hist.as_mut_ptr()) != 0;
                    let list: Vec<POINTER_PEN_INFO> = if ok { hist[..n.min(64) as usize].iter().rev().copied().collect() } else { vec![one] };
                    let t = now_ms();
                    let mut down = DOWN.lock().unwrap();
                    for pi in list.iter() {
                        let mut pt: POINT = pi.pointerInfo.ptPixelLocation;
                        ScreenToClient(hwnd, &mut pt);
                        let pressure = pi.pressure as f32 / 1024.0;
                        let contact = pi.pointerInfo.pointerFlags & POINTER_FLAG_INCONTACT != 0;
                        {
                            let mut p = PEN.lock().unwrap();
                            p.pressure = if contact { pressure } else { 0.0 };
                            p.tilt_x = pi.tiltX;
                            p.tilt_y = pi.tiltY;
                            p.source = 1;
                            p.samples = p.samples.wrapping_add(1);
                        }
                        let kind = if contact && !*down {
                            *down = true;
                            Some(Kind::Down)
                        } else if contact {
                            Some(Kind::Move)
                        } else if *down {
                            *down = false;
                            Some(Kind::Up)
                        } else {
                            None
                        };
                        if let Some(kind) = kind {
                            push(Sample { x: pt.x as f32, y: pt.y as f32, pressure, kind, t });
                        }
                    }
                    if msg == WM_POINTERUP && *down {
                        *down = false;
                        let mut pt: POINT = one.pointerInfo.ptPixelLocation;
                        ScreenToClient(hwnd, &mut pt);
                        push(Sample { x: pt.x as f32, y: pt.y as f32, pressure: 0.0, kind: Kind::Up, t });
                    }
                    return 0; // handled: no fake mouse events for the pen
                }
            }
        }
        DefSubclassProc(hwnd, msg, wp, lp)
    }
}

/* ---------------- browser: pointer events with pressure ---------------- */
#[cfg(target_arch = "wasm32")]
mod webpen {
    use super::*;
    use wasm_bindgen::prelude::*;
    use wasm_bindgen::JsCast;
    use winit::platform::web::WindowExtWebSys;

    pub fn install(window: &Window) {
        let Some(canvas) = window.canvas() else { return };
        let style = canvas.style();
        let _ = style.set_property("width", "100%");
        let _ = style.set_property("height", "100%");
        let _ = style.set_property("touch-action", "none");
        let _ = style.set_property("display", "block");
        let c2 = canvas.clone();
        let handler = move |kind: Kind| {
            let c = c2.clone();
            Closure::<dyn FnMut(web_sys::PointerEvent)>::new(move |e: web_sys::PointerEvent| {
                if e.pointer_type() == "mouse" && kind == Kind::Down && e.button() != 0 {
                    return;
                }
                let dpr = web_sys::window().map(|w| w.device_pixel_ratio()).unwrap_or(1.0) as f32;
                let r = c.get_bounding_client_rect();
                let x = (e.client_x() as f32 - r.left() as f32) * dpr;
                let y = (e.client_y() as f32 - r.top() as f32) * dpr;
                let pressure = if e.pointer_type() == "mouse" { 0.6 } else { e.pressure() };
                if kind == Kind::Down {
                    let _ = c.set_pointer_capture(e.pointer_id());
                }
                if kind == Kind::Move && e.buttons() == 0 {
                    return;
                }
                {
                    let mut p = PEN.lock().unwrap();
                    p.pressure = pressure;
                    p.tilt_x = e.tilt_x();
                    p.tilt_y = e.tilt_y();
                    p.source = 3;
                }
                push(Sample { x, y, pressure, kind, t: now_ms() });
                e.prevent_default();
            })
        };
        for (name, kind) in [("pointerdown", Kind::Down), ("pointermove", Kind::Move), ("pointerup", Kind::Up), ("pointercancel", Kind::Up)] {
            let cb = handler(kind);
            let _ = canvas.add_event_listener_with_callback(name, cb.as_ref().unchecked_ref());
            cb.forget();
        }
    }

    #[wasm_bindgen]
    pub fn set_color(r: f32, g: f32, b: f32) {
        SETTINGS.lock().unwrap().color = [r, g, b];
    }
    #[wasm_bindgen]
    pub fn set_size(s: f32) {
        SETTINGS.lock().unwrap().size = s.clamp(1.0, 400.0);
    }
    #[wasm_bindgen]
    pub fn set_flag(name: &str, on: bool) {
        let mut s = SETTINGS.lock().unwrap();
        match name {
            "dual" => s.dual = on,
            "taper" => s.taper = on,
            "follow" => s.follow = on,
            _ => {}
        }
    }
    #[wasm_bindgen]
    pub fn clear_canvas() {
        SETTINGS.lock().unwrap().clear = true;
    }
    #[wasm_bindgen]
    pub fn pen_info() -> String {
        let p = *PEN.lock().unwrap();
        format!("pressure {:.2} · tilt {} / {}", p.pressure, p.tilt_x, p.tilt_y)
    }
    #[wasm_bindgen(start)]
    pub fn start() {
        console_error_panic_hook::set_once();
        super::run();
    }
}

pub fn run() {
    let event_loop = EventLoop::new().expect("event loop");
    let app = App { st: Rc::new(RefCell::new(State { window: None, gfx: None, stroke: None, mouse_down: false, cursor: (0.0, 0.0), seed: 12345, last_title: 0.0 })) };
    #[cfg(not(target_arch = "wasm32"))]
    {
        let mut app = app;
        event_loop.run_app(&mut app).expect("run");
    }
    #[cfg(target_arch = "wasm32")]
    {
        use winit::platform::web::EventLoopExtWebSys;
        event_loop.spawn_app(app);
    }
}
