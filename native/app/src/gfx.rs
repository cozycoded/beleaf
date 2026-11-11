//! The canvas on the graphics card: layers, the stroke being drawn, undo snapshots, and showing it all
//! on screen zoomed, moved and turned.
//!
//! A stroke works like the engine test: every dab is a textured quad drawn into two masks (main tip and
//! dual tip, "max" blending so overlaps don't build up). While drawing, the masks are shown right above
//! the current layer (or cut out of it for the eraser); when the pen lifts, the whole stroke is redrawn
//! once with its final taper and merged into the layer.

use brush_core::Dab;
use wgpu::util::DeviceExt;

pub const LAYER_FMT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;
const UNI: u64 = 256; // one uniform slot per draw (dynamic offsets)
const MAX_DRAWS: u64 = 160;

const DAB_WGSL: &str = r#"
struct U { canvas: vec2<f32>, aspect: f32, pad: f32, };
@group(0) @binding(0) var<uniform> u: U;
@group(0) @binding(1) var tip: texture_2d<f32>;
@group(0) @binding(2) var smp: sampler;
struct VO { @builtin(position) pos: vec4<f32>, @location(0) uv: vec2<f32>, @location(1) a: f32, };
@vertex
fn vs(@builtin(vertex_index) vi: u32, @location(0) p: vec2<f32>, @location(1) size: f32, @location(2) ang: f32, @location(3) alpha: f32, @location(4) round: f32) -> VO {
  var c = array<vec2<f32>, 4>(vec2<f32>(-0.5, -0.5), vec2<f32>(0.5, -0.5), vec2<f32>(-0.5, 0.5), vec2<f32>(0.5, 0.5));
  let q = c[vi];
  var sw = size;
  var sh = size * round;
  if (u.aspect >= 1.0) { sh = sh / u.aspect; } else { sw = sw * u.aspect; }
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

// Draws the canvas (or a layer of it) on screen through the view: m0/m1 map canvas px to screen px.
// mode 0 = paper colour, 1 = layer, 2 = layer with the eraser stroke cut out, 3 = the live brush stroke,
// 4 = whole-canvas merge of the stroke into a layer (no view, straight NDC)
const VIEW_WGSL: &str = r#"
struct U { m0: vec4<f32>, m1: vec4<f32>, screen: vec4<f32>, color: vec4<f32>, };
@group(0) @binding(0) var<uniform> u: U;
@group(0) @binding(1) var tex: texture_2d<f32>;
@group(0) @binding(2) var mk1: texture_2d<f32>;
@group(0) @binding(3) var mk2: texture_2d<f32>;
@group(0) @binding(4) var smp: sampler;
struct VO { @builtin(position) pos: vec4<f32>, @location(0) uv: vec2<f32>, };
@vertex fn vs(@builtin(vertex_index) vi: u32) -> VO {
  var c = array<vec2<f32>, 4>(vec2<f32>(0.0, 0.0), vec2<f32>(1.0, 0.0), vec2<f32>(0.0, 1.0), vec2<f32>(1.0, 1.0));
  let q = c[vi];
  var o: VO;
  o.uv = q;
  if (u.screen.w > 3.5) {
    o.pos = vec4<f32>(q.x * 2.0 - 1.0, 1.0 - q.y * 2.0, 0.0, 1.0);
    return o;
  }
  let p = q * vec2<f32>(u.m0.w, u.m1.w);
  let s = vec2<f32>(u.m0.x * p.x + u.m0.y * p.y + u.m0.z, u.m1.x * p.x + u.m1.y * p.y + u.m1.z);
  o.pos = vec4<f32>(s.x / u.screen.x * 2.0 - 1.0, 1.0 - s.y / u.screen.y * 2.0, 0.0, 1.0);
  return o;
}
@fragment fn fs(i: VO) -> @location(0) vec4<f32> {
  let mode = u.screen.w;
  if (mode < 0.5) { return vec4<f32>(u.color.rgb * u.color.a, u.color.a); }
  let m = textureSample(mk1, smp, i.uv).r * textureSample(mk2, smp, i.uv).r;
  let t = textureSample(tex, smp, i.uv);
  if (mode < 1.5) { return t * u.screen.z; }
  if (mode < 2.5) { return t * (1.0 - m * u.color.a) * u.screen.z; }
  let a = m * u.color.a * u.screen.z;
  return vec4<f32>(u.color.rgb * a, a);
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
}

pub struct Tip {
    bg: wgpu::BindGroup,
}

pub struct Layer {
    pub id: u32,
    pub name: String,
    pub visible: bool,
    pub opacity: f32,
    pub tex: wgpu::Texture,
    bg: wgpu::BindGroup,
}

/// One undo step's pixels: the changed rectangle of a layer, before and after.
pub struct Snap {
    pub layer: u32,
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
    pub before: wgpu::Texture,
    pub after: Option<wgpu::Texture>,
}

/// The view: canvas px → screen px = rotate(r) · scale(s) · p + (tx, ty)
#[derive(Clone, Copy, Debug)]
pub struct View {
    pub tx: f32,
    pub ty: f32,
    pub s: f32,
    pub r: f32,
}
impl View {
    pub fn to_screen(&self, x: f32, y: f32) -> (f32, f32) {
        let (c, sn) = (self.r.cos(), self.r.sin());
        (c * self.s * x - sn * self.s * y + self.tx, sn * self.s * x + c * self.s * y + self.ty)
    }
    pub fn to_canvas(&self, x: f32, y: f32) -> (f32, f32) {
        let (c, sn) = (self.r.cos(), self.r.sin());
        let (dx, dy) = ((x - self.tx) / self.s, (y - self.ty) / self.s);
        (c * dx + sn * dy, -sn * dx + c * dy)
    }
}

pub struct Gfx {
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,
    pub cw: u32,
    pub ch: u32,
    pub layers: Vec<Layer>,
    pub paper: [f32; 3],
    next_id: u32,
    mask_main: wgpu::Texture,
    mask_dual: wgpu::Texture,
    mm_view: wgpu::TextureView,
    md_view: wgpu::TextureView,
    smp: wgpu::Sampler,
    view_l: wgpu::BindGroupLayout,
    ubuf: wgpu::Buffer,
    pipe_screen: wgpu::RenderPipeline,
    pipe_merge: wgpu::RenderPipeline,
    pipe_erase: wgpu::RenderPipeline,
    dab_pipe: wgpu::RenderPipeline,
    dab_l: wgpu::BindGroupLayout,
    dab_ub: Vec<wgpu::Buffer>,
    tip_views: Vec<(wgpu::TextureView, f32)>,
    pub tips: Vec<Tip>,
    misc_bg: wgpu::BindGroup,
    pub stroking: bool,
    pub stroke_erase: bool,
    pub stroke_color: [f32; 4],
    dual_on: bool,
}

fn tex2d(device: &wgpu::Device, w: u32, h: u32, format: wgpu::TextureFormat, copy: bool) -> wgpu::Texture {
    let mut usage = wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING;
    if copy {
        usage |= wgpu::TextureUsages::COPY_SRC | wgpu::TextureUsages::COPY_DST;
    }
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

fn clear_pass(enc: &mut wgpu::CommandEncoder, view: &wgpu::TextureView, v: [f64; 4]) {
    enc.begin_render_pass(&wgpu::RenderPassDescriptor {
        label: None,
        color_attachments: &[Some(wgpu::RenderPassColorAttachment { view, resolve_target: None, ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color { r: v[0], g: v[1], b: v[2], a: v[3] }), store: wgpu::StoreOp::Store } })],
        depth_stencil_attachment: None,
        timestamp_writes: None,
        occlusion_query_set: None,
    });
}

const PREMUL_OVER: wgpu::BlendState = wgpu::BlendState {
    color: wgpu::BlendComponent { src_factor: wgpu::BlendFactor::One, dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha, operation: wgpu::BlendOperation::Add },
    alpha: wgpu::BlendComponent { src_factor: wgpu::BlendFactor::One, dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha, operation: wgpu::BlendOperation::Add },
};
const ERASE: wgpu::BlendState = wgpu::BlendState {
    color: wgpu::BlendComponent { src_factor: wgpu::BlendFactor::Zero, dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha, operation: wgpu::BlendOperation::Add },
    alpha: wgpu::BlendComponent { src_factor: wgpu::BlendFactor::Zero, dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha, operation: wgpu::BlendOperation::Add },
};
const MAX_BLEND: wgpu::BlendState = wgpu::BlendState {
    color: wgpu::BlendComponent { src_factor: wgpu::BlendFactor::One, dst_factor: wgpu::BlendFactor::One, operation: wgpu::BlendOperation::Max },
    alpha: wgpu::BlendComponent { src_factor: wgpu::BlendFactor::One, dst_factor: wgpu::BlendFactor::One, operation: wgpu::BlendOperation::Max },
};

fn pipeline(device: &wgpu::Device, layout: &wgpu::BindGroupLayout, module: &wgpu::ShaderModule, buffers: &[wgpu::VertexBufferLayout], format: wgpu::TextureFormat, blend: wgpu::BlendState) -> wgpu::RenderPipeline {
    let pl = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor { label: None, bind_group_layouts: &[layout], push_constant_ranges: &[] });
    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: None,
        layout: Some(&pl),
        vertex: wgpu::VertexState { module, entry_point: Some("vs"), compilation_options: Default::default(), buffers },
        fragment: Some(wgpu::FragmentState {
            module,
            entry_point: Some("fs"),
            compilation_options: Default::default(),
            targets: &[Some(wgpu::ColorTargetState { format, blend: Some(blend), write_mask: wgpu::ColorWrites::ALL })],
        }),
        primitive: wgpu::PrimitiveState { topology: wgpu::PrimitiveTopology::TriangleStrip, ..Default::default() },
        depth_stencil: None,
        multisample: wgpu::MultisampleState::default(),
        multiview: None,
        cache: None,
    })
}

fn tex_entry(binding: u32) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::FRAGMENT,
        ty: wgpu::BindingType::Texture { sample_type: wgpu::TextureSampleType::Float { filterable: true }, view_dimension: wgpu::TextureViewDimension::D2, multisampled: false },
        count: None,
    }
}
fn smp_entry(binding: u32) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry { binding, visibility: wgpu::ShaderStages::FRAGMENT, ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering), count: None }
}
fn ub_entry(dynamic: bool) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding: 0,
        visibility: wgpu::ShaderStages::VERTEX | wgpu::ShaderStages::FRAGMENT,
        ty: wgpu::BindingType::Buffer { ty: wgpu::BufferBindingType::Uniform, has_dynamic_offset: dynamic, min_binding_size: wgpu::BufferSize::new(64) },
        count: None,
    }
}

/// A tip image from its bytes: u32 width, u32 height, then one byte per pixel.
fn tip_from_bytes(bytes: &[u8]) -> (u32, u32, Vec<u8>) {
    let w = u32::from_le_bytes(bytes[0..4].try_into().unwrap());
    let h = u32::from_le_bytes(bytes[4..8].try_into().unwrap());
    let (pw, ph) = (w + 2, h + 2);
    let mut d = vec![0u8; (pw * ph) as usize];
    for y in 0..h {
        for x in 0..w {
            d[((y + 1) * pw + x + 1) as usize] = bytes[8 + (y * w + x) as usize];
        }
    }
    (pw, ph, d)
}
/// A round tip drawn in code: hardness 1 = crisp edge, 0 = soft all the way.
pub fn round_tip(hardness: f32) -> Vec<u8> {
    let n = 128u32;
    let mut out = Vec::with_capacity(8 + (n * n) as usize);
    out.extend_from_slice(&n.to_le_bytes());
    out.extend_from_slice(&n.to_le_bytes());
    let r = n as f32 / 2.0 - 1.0;
    for y in 0..n {
        for x in 0..n {
            let d = ((x as f32 + 0.5 - n as f32 / 2.0).powi(2) + (y as f32 + 0.5 - n as f32 / 2.0).powi(2)).sqrt() / r;
            let v = if hardness >= 0.99 {
                (1.0 - (d - 1.0) * r).clamp(0.0, 1.0) // 1 px antialiased edge
            } else {
                let h = hardness.clamp(0.0, 0.98);
                if d <= h { 1.0 } else { (1.0 - (d - h) / (1.0 - h)).clamp(0.0, 1.0).powf(1.6) }
            };
            out.push((v * 255.0).round() as u8);
        }
    }
    out
}

fn make_tip_view(device: &wgpu::Device, queue: &wgpu::Queue, bytes: &[u8]) -> (wgpu::TextureView, f32) {
    let (w, h, data) = tip_from_bytes(bytes);
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
        // rows must be padded to 256 bytes for the copy
        let bpr = (*lw + 255) / 256 * 256;
        let mut padded = vec![0u8; (bpr * lh) as usize];
        for y in 0..*lh {
            padded[(y * bpr) as usize..(y * bpr + lw) as usize].copy_from_slice(&ld[(y * lw) as usize..((y + 1) * lw) as usize]);
        }
        queue.write_texture(
            wgpu::ImageCopyTexture { texture: &tex, mip_level: i as u32, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
            &padded,
            wgpu::ImageDataLayout { offset: 0, bytes_per_row: Some(bpr), rows_per_image: Some(*lh) },
            wgpu::Extent3d { width: *lw, height: *lh, depth_or_array_layers: 1 },
        );
    }
    (tex.create_view(&Default::default()), w as f32 / h as f32)
}

impl Gfx {
    /// tips: the tip images every brush can use (index = Brush tip number)
    pub fn new(device: wgpu::Device, queue: wgpu::Queue, screen_format: wgpu::TextureFormat, cw: u32, ch: u32, tips: &[Vec<u8>]) -> Gfx {
        let maxd = device.limits().max_texture_dimension_2d;
        let (cw, ch) = (cw.min(maxd).max(16), ch.min(maxd).max(16));
        let mask_main = tex2d(&device, cw, ch, wgpu::TextureFormat::R8Unorm, false);
        let mask_dual = tex2d(&device, cw, ch, wgpu::TextureFormat::R8Unorm, false);
        let mm_view = mask_main.create_view(&Default::default());
        let md_view = mask_dual.create_view(&Default::default());
        let smp = device.create_sampler(&wgpu::SamplerDescriptor { mag_filter: wgpu::FilterMode::Linear, min_filter: wgpu::FilterMode::Linear, mipmap_filter: wgpu::FilterMode::Linear, ..Default::default() });

        let view_l = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor { label: None, entries: &[ub_entry(true), tex_entry(1), tex_entry(2), tex_entry(3), smp_entry(4)] });
        let dab_l = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor { label: None, entries: &[ub_entry(false), tex_entry(1), smp_entry(2)] });
        let view_mod = device.create_shader_module(wgpu::ShaderModuleDescriptor { label: Some("view"), source: wgpu::ShaderSource::Wgsl(VIEW_WGSL.into()) });
        let dab_mod = device.create_shader_module(wgpu::ShaderModuleDescriptor { label: Some("dab"), source: wgpu::ShaderSource::Wgsl(DAB_WGSL.into()) });
        let pipe_screen = pipeline(&device, &view_l, &view_mod, &[], screen_format, PREMUL_OVER);
        let pipe_merge = pipeline(&device, &view_l, &view_mod, &[], LAYER_FMT, PREMUL_OVER);
        let pipe_erase = pipeline(&device, &view_l, &view_mod, &[], LAYER_FMT, ERASE);
        let inst_layout = wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<Inst>() as u64,
            step_mode: wgpu::VertexStepMode::Instance,
            attributes: &wgpu::vertex_attr_array![0 => Float32x2, 1 => Float32, 2 => Float32, 3 => Float32, 4 => Float32],
        };
        let dab_pipe = pipeline(&device, &dab_l, &dab_mod, &[inst_layout], wgpu::TextureFormat::R8Unorm, MAX_BLEND);
        let ubuf = device.create_buffer(&wgpu::BufferDescriptor { label: Some("view uniforms"), size: UNI * MAX_DRAWS, usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST, mapped_at_creation: false });

        let tip_views: Vec<(wgpu::TextureView, f32)> = tips.iter().map(|b| make_tip_view(&device, &queue, b)).collect();
        let mut dab_ub = Vec::new();
        let mut tip_bgs = Vec::new();
        for (v, aspect) in &tip_views {
            let ub = device.create_buffer_init(&wgpu::util::BufferInitDescriptor { label: None, contents: bytemuck::cast_slice(&[cw as f32, ch as f32, *aspect, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]), usage: wgpu::BufferUsages::UNIFORM });
            let bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: None,
                layout: &dab_l,
                entries: &[wgpu::BindGroupEntry { binding: 0, resource: ub.as_entire_binding() }, wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(v) }, wgpu::BindGroupEntry { binding: 2, resource: wgpu::BindingResource::Sampler(&smp) }],
            });
            dab_ub.push(ub);
            tip_bgs.push(Tip { bg });
        }
        let misc_bg = Self::view_bg(&device, &view_l, &ubuf, &mm_view, &mm_view, &md_view, &smp);
        Gfx {
            device,
            queue,
            cw,
            ch,
            layers: Vec::new(),
            paper: [1.0, 1.0, 1.0],
            next_id: 1,
            mask_main,
            mask_dual,
            mm_view,
            md_view,
            smp,
            view_l,
            ubuf,
            pipe_screen,
            pipe_merge,
            pipe_erase,
            dab_pipe,
            dab_l,
            dab_ub,
            tip_views,
            tips: tip_bgs,
            misc_bg,
            stroking: false,
            stroke_erase: false,
            stroke_color: [0.0, 0.0, 0.0, 1.0],
            dual_on: false,
        }
    }

    fn view_bg(device: &wgpu::Device, l: &wgpu::BindGroupLayout, ub: &wgpu::Buffer, tex: &wgpu::TextureView, m1: &wgpu::TextureView, m2: &wgpu::TextureView, smp: &wgpu::Sampler) -> wgpu::BindGroup {
        device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: None,
            layout: l,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding { buffer: ub, offset: 0, size: wgpu::BufferSize::new(64) }) },
                wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(tex) },
                wgpu::BindGroupEntry { binding: 2, resource: wgpu::BindingResource::TextureView(m1) },
                wgpu::BindGroupEntry { binding: 3, resource: wgpu::BindingResource::TextureView(m2) },
                wgpu::BindGroupEntry { binding: 4, resource: wgpu::BindingResource::Sampler(smp) },
            ],
        })
    }

    /// The same renderer for a canvas of another size (the old layers go).
    pub fn rebuilt(self, screen_format: wgpu::TextureFormat, w: u32, h: u32, tips: &[Vec<u8>]) -> Gfx {
        let paper = self.paper;
        let mut g = Gfx::new(self.device, self.queue, screen_format, w, h, tips);
        g.paper = paper;
        g
    }

    /* ---------------- layers ---------------- */

    /// A new empty layer (not yet placed in the list).
    pub fn make_layer(&mut self, name: &str) -> Layer {
        let tex = tex2d(&self.device, self.cw, self.ch, LAYER_FMT, true);
        let v = tex.create_view(&Default::default());
        let mut enc = self.device.create_command_encoder(&Default::default());
        clear_pass(&mut enc, &v, [0.0; 4]);
        self.queue.submit([enc.finish()]);
        let bg = Self::view_bg(&self.device, &self.view_l, &self.ubuf, &v, &self.mm_view, &self.md_view, &self.smp);
        let id = self.next_id;
        self.next_id += 1;
        Layer { id, name: name.to_string(), visible: true, opacity: 1.0, tex, bg }
    }

    pub fn index_of(&self, id: u32) -> Option<usize> {
        self.layers.iter().position(|l| l.id == id)
    }

    /// Fill a layer from straight (not premultiplied) RGBA pixels the size of the canvas.
    pub fn upload_layer(&self, idx: usize, rgba_straight: &[u8]) {
        let (w, h) = (self.cw, self.ch);
        let mut p = rgba_straight.to_vec();
        for px in p.chunks_exact_mut(4) {
            let a = px[3] as u32;
            if a < 255 {
                px[0] = ((px[0] as u32 * a + 127) / 255) as u8;
                px[1] = ((px[1] as u32 * a + 127) / 255) as u8;
                px[2] = ((px[2] as u32 * a + 127) / 255) as u8;
            }
        }
        self.queue.write_texture(
            wgpu::ImageCopyTexture { texture: &self.layers[idx].tex, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
            &p,
            wgpu::ImageDataLayout { offset: 0, bytes_per_row: Some(w * 4), rows_per_image: Some(h) },
            wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
        );
    }

    /// Read a layer back as straight RGBA (for saving).
    pub fn read_layer(&self, idx: usize) -> Vec<u8> {
        let (w, h) = (self.cw, self.ch);
        let bpr = (w * 4 + 255) / 256 * 256;
        let buf = self.device.create_buffer(&wgpu::BufferDescriptor { label: None, size: (bpr * h) as u64, usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ, mapped_at_creation: false });
        let mut enc = self.device.create_command_encoder(&Default::default());
        enc.copy_texture_to_buffer(
            wgpu::ImageCopyTexture { texture: &self.layers[idx].tex, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
            wgpu::ImageCopyBuffer { buffer: &buf, layout: wgpu::ImageDataLayout { offset: 0, bytes_per_row: Some(bpr), rows_per_image: Some(h) } },
            wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
        );
        self.queue.submit([enc.finish()]);
        let slice = buf.slice(..);
        slice.map_async(wgpu::MapMode::Read, |_| {});
        self.device.poll(wgpu::Maintain::Wait);
        let data = slice.get_mapped_range();
        let mut out = vec![0u8; (w * h * 4) as usize];
        for y in 0..h as usize {
            let src = &data[y * bpr as usize..y * bpr as usize + (w * 4) as usize];
            out[y * (w * 4) as usize..(y + 1) * (w * 4) as usize].copy_from_slice(src);
        }
        drop(data);
        buf.unmap();
        // premultiplied → straight
        for px in out.chunks_exact_mut(4) {
            let a = px[3] as u32;
            if a > 0 && a < 255 {
                px[0] = ((px[0] as u32 * 255 + a / 2) / a).min(255) as u8;
                px[1] = ((px[1] as u32 * 255 + a / 2) / a).min(255) as u8;
                px[2] = ((px[2] as u32 * 255 + a / 2) / a).min(255) as u8;
            }
        }
        out
    }

    /* ---------------- undo snapshots ---------------- */

    fn copy_rect(&self, enc: &mut wgpu::CommandEncoder, src: &wgpu::Texture, sx: u32, sy: u32, dst: &wgpu::Texture, dx: u32, dy: u32, w: u32, h: u32) {
        enc.copy_texture_to_texture(
            wgpu::ImageCopyTexture { texture: src, mip_level: 0, origin: wgpu::Origin3d { x: sx, y: sy, z: 0 }, aspect: wgpu::TextureAspect::All },
            wgpu::ImageCopyTexture { texture: dst, mip_level: 0, origin: wgpu::Origin3d { x: dx, y: dy, z: 0 }, aspect: wgpu::TextureAspect::All },
            wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
        );
    }

    /// Copy part of a layer before it changes.
    pub fn snap_before(&self, idx: usize, x: u32, y: u32, w: u32, h: u32) -> Snap {
        let t = tex2d(&self.device, w, h, LAYER_FMT, true);
        let mut enc = self.device.create_command_encoder(&Default::default());
        self.copy_rect(&mut enc, &self.layers[idx].tex, x, y, &t, 0, 0, w, h);
        self.queue.submit([enc.finish()]);
        Snap { layer: self.layers[idx].id, x, y, w, h, before: t, after: None }
    }
    pub fn snap_after(&self, s: &mut Snap) {
        let Some(idx) = self.index_of(s.layer) else { return };
        let t = tex2d(&self.device, s.w, s.h, LAYER_FMT, true);
        let mut enc = self.device.create_command_encoder(&Default::default());
        self.copy_rect(&mut enc, &self.layers[idx].tex, s.x, s.y, &t, 0, 0, s.w, s.h);
        self.queue.submit([enc.finish()]);
        s.after = Some(t);
    }
    /// Put a snapshot back (before = undo, after = redo).
    pub fn restore(&self, s: &Snap, before: bool) {
        let Some(idx) = self.index_of(s.layer) else { return };
        let src = if before { &s.before } else { match &s.after { Some(t) => t, None => return } };
        let mut enc = self.device.create_command_encoder(&Default::default());
        self.copy_rect(&mut enc, src, 0, 0, &self.layers[idx].tex, s.x, s.y, s.w, s.h);
        self.queue.submit([enc.finish()]);
    }
    pub fn clear_layer(&self, idx: usize) {
        let v = self.layers[idx].tex.create_view(&Default::default());
        let mut enc = self.device.create_command_encoder(&Default::default());
        clear_pass(&mut enc, &v, [0.0; 4]);
        self.queue.submit([enc.finish()]);
    }

    /* ---------------- strokes ---------------- */

    pub fn draw_dabs(&self, enc: &mut wgpu::CommandEncoder, dabs: &[Dab], tip: usize, dual_tip: usize) {
        for (dual, t, tex) in [(false, tip, &self.mm_view), (true, dual_tip, &self.md_view)] {
            let inst: Vec<Inst> = dabs.iter().filter(|d| d.dual == dual && d.size > 0.05).map(|d| Inst { pos: [d.x, d.y], size: d.size, angle: d.angle, alpha: d.alpha, round: d.round.max(0.05) }).collect();
            if inst.is_empty() {
                continue;
            }
            let buf = self.device.create_buffer_init(&wgpu::util::BufferInitDescriptor { label: None, contents: bytemuck::cast_slice(&inst), usage: wgpu::BufferUsages::VERTEX });
            let mut rp = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: None,
                color_attachments: &[Some(wgpu::RenderPassColorAttachment { view: tex, resolve_target: None, ops: wgpu::Operations { load: wgpu::LoadOp::Load, store: wgpu::StoreOp::Store } })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            rp.set_pipeline(&self.dab_pipe);
            rp.set_bind_group(0, &self.tips[t.min(self.tips.len() - 1)].bg, &[]);
            rp.set_vertex_buffer(0, buf.slice(..));
            rp.draw(0..4, 0..inst.len() as u32);
        }
    }

    pub fn begin_stroke(&mut self, color: [f32; 3], opacity: f32, dual: bool, erase: bool) {
        self.stroking = true;
        self.stroke_erase = erase;
        self.stroke_color = [color[0], color[1], color[2], opacity];
        self.dual_on = dual;
        let mut enc = self.device.create_command_encoder(&Default::default());
        clear_pass(&mut enc, &self.mm_view, [0.0; 4]);
        let d = if dual { 0.0 } else { 1.0 };
        clear_pass(&mut enc, &self.md_view, [d, d, d, d]);
        self.queue.submit([enc.finish()]);
    }

    pub fn cancel_stroke(&mut self) {
        self.stroking = false;
    }

    /// Redraw the whole stroke once with its final shape and merge it into the layer.
    pub fn commit_stroke(&mut self, idx: usize, all: &[Dab], tip: usize, dual_tip: usize) {
        let mut enc = self.device.create_command_encoder(&Default::default());
        clear_pass(&mut enc, &self.mm_view, [0.0; 4]);
        let d = if self.dual_on { 0.0 } else { 1.0 };
        clear_pass(&mut enc, &self.md_view, [d, d, d, d]);
        self.draw_dabs(&mut enc, all, tip, dual_tip);
        let u = [0.0f32, 0.0, 0.0, self.cw as f32, 0.0, 0.0, 0.0, self.ch as f32, 1.0, 1.0, 1.0, 4.0, self.stroke_color[0], self.stroke_color[1], self.stroke_color[2], self.stroke_color[3]];
        self.queue.write_buffer(&self.ubuf, 0, bytemuck::cast_slice(&u));
        let lv = self.layers[idx].tex.create_view(&Default::default());
        {
            let mut rp = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: None,
                color_attachments: &[Some(wgpu::RenderPassColorAttachment { view: &lv, resolve_target: None, ops: wgpu::Operations { load: wgpu::LoadOp::Load, store: wgpu::StoreOp::Store } })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            rp.set_pipeline(if self.stroke_erase { &self.pipe_erase } else { &self.pipe_merge });
            rp.set_bind_group(0, &self.misc_bg, &[0]);
            rp.draw(0..4, 0..1);
        }
        self.queue.submit([enc.finish()]);
        self.stroking = false;
    }

    /* ---------------- on screen ---------------- */

    /// Draw the canvas into the screen's frame. Uniform slot 0 is used by commit_stroke, so draws start at 1.
    pub fn render(&self, enc: &mut wgpu::CommandEncoder, target: &wgpu::TextureView, sw: f32, sh: f32, view: &View, active: usize, bg: [f32; 3]) {
        let (c, s) = (view.r.cos() * view.s, view.r.sin() * view.s);
        let base = |mode: f32, op: f32, col: [f32; 4]| -> [f32; 16] { [c, -s, view.tx, self.cw as f32, s, c, view.ty, self.ch as f32, sw, sh, op, mode, col[0], col[1], col[2], col[3]] };
        // what to draw, in order: (bind group, uniforms)
        let mut draws: Vec<(&wgpu::BindGroup, [f32; 16])> = vec![(&self.misc_bg, base(0.0, 1.0, [self.paper[0], self.paper[1], self.paper[2], 1.0]))];
        for (i, l) in self.layers.iter().enumerate() {
            if !l.visible {
                continue;
            }
            if i == active && self.stroking && self.stroke_erase {
                draws.push((&l.bg, base(2.0, l.opacity, self.stroke_color)));
            } else {
                draws.push((&l.bg, base(1.0, l.opacity, [0.0; 4])));
            }
            if i == active && self.stroking && !self.stroke_erase {
                draws.push((&self.misc_bg, base(3.0, l.opacity, self.stroke_color)));
            }
        }
        let n = draws.len().min(MAX_DRAWS as usize - 1);
        for (k, (_, u)) in draws.iter().take(n).enumerate() {
            self.queue.write_buffer(&self.ubuf, UNI * (k as u64 + 1), bytemuck::cast_slice(u));
        }
        let mut rp = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: None,
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: target,
                resolve_target: None,
                ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color { r: bg[0] as f64, g: bg[1] as f64, b: bg[2] as f64, a: 1.0 }), store: wgpu::StoreOp::Store },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
        });
        rp.set_pipeline(&self.pipe_screen);
        for (k, (bgp, _)) in draws.iter().take(n).enumerate() {
            rp.set_bind_group(0, *bgp, &[(UNI * (k as u64 + 1)) as u32]);
            rp.draw(0..4, 0..1);
        }
    }

    pub fn tip_count(&self) -> usize {
        self.tip_views.len()
    }
    #[allow(dead_code)]
    fn _unused(&self) -> (&wgpu::Texture, &wgpu::Texture, &wgpu::BindGroupLayout, &Vec<wgpu::Buffer>) {
        (&self.mask_main, &self.mask_dual, &self.dab_l, &self.dab_ub)
    }
}
