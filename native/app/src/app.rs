//! beleaf's drawing app, independent of the platform: the document, tools, undo, finger gestures and the
//! on-screen controls. lib.rs feeds it pen and finger samples and gives it a frame to draw into.

use crate::doc::{self, DocData, LayerData};
use crate::gfx::{Gfx, Layer, Snap, View};
use brush_core::{Brush, Dab, Stroke};
use egui::{Color32, Pos2, Rect, Vec2};
use std::collections::HashMap;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Tool {
    Finger,
    Pen,
    PenEraser,
    Mouse,
}

#[derive(Clone, Copy, Debug)]
pub struct Ptr {
    pub id: i32,
    pub x: f32,
    pub y: f32,
    /// 0..1
    pub pressure: f32,
    /// 0 flat .. 1 upright
    pub tilt: Option<f32>,
    /// pen direction, radians
    pub az: Option<f32>,
    pub tool: Tool,
    pub t_ms: f64,
    /// the S Pen's side button is held
    pub button: bool,
}

pub enum PEv {
    Down(Ptr),
    /// every sample since the last event (oldest first), for one pointer
    Move(Vec<Ptr>),
    Up(Ptr),
    Cancel,
    Hover(Ptr),
    HoverEnd,
}

/* ---------------- brushes ---------------- */

pub struct Preset {
    pub name: &'static str,
    pub tip: usize,
    pub dual_tip: usize,
    pub size: f32,
    pub opacity: f32,
    pub erase: bool,
    make: fn(f32) -> Brush,
}
// tip images: 0 crisp round, 1 firm round, 2 soft round, 3 acrylic main, 4 acrylic dual
pub fn tip_images() -> Vec<Vec<u8>> {
    vec![
        crate::gfx::round_tip(1.0),
        crate::gfx::round_tip(0.6),
        crate::gfx::round_tip(0.0),
        include_bytes!("../../poc/assets/tip_main.bin").to_vec(),
        include_bytes!("../../poc/assets/tip_dual.bin").to_vec(),
    ]
}
fn presets() -> Vec<Preset> {
    vec![
        Preset { name: "G-pen", tip: 0, dual_tip: 0, size: 8.0, opacity: 1.0, erase: false, make: |s| Brush { size_min: Some(12.0), spacing: 5.0, stab: 4.0, taper_in: 8.0, taper_out: 12.0, taper_ratio: Some(30.0), ..Brush::plain(s) } },
        Preset { name: "Pencil", tip: 1, dual_tip: 0, size: 5.0, opacity: 1.0, erase: false, make: |s| Brush { size_min: Some(55.0), op_min: Some(25.0), spacing: 6.0, ..Brush::plain(s) } },
        Preset { name: "Marker", tip: 0, dual_tip: 0, size: 24.0, opacity: 0.8, erase: false, make: |s| Brush { spacing: 6.0, ..Brush::plain(s) } },
        Preset { name: "Soft brush", tip: 2, dual_tip: 0, size: 80.0, opacity: 1.0, erase: false, make: |s| Brush { op_min: Some(0.0), spacing: 8.0, flow: 60.0, ..Brush::plain(s) } },
        Preset { name: "Textured acrylic", tip: 3, dual_tip: 4, size: 40.0, opacity: 1.0, erase: false, make: Brush::textured_acrylic },
        Preset { name: "Eraser", tip: 1, dual_tip: 0, size: 30.0, opacity: 1.0, erase: true, make: |s| Brush { spacing: 6.0, ..Brush::plain(s) } },
    ]
}

/* ---------------- undo ---------------- */

enum Step {
    Paint(Snap),
    AddLayer { idx: usize, layer: Option<Layer> },
    DelLayer { idx: usize, layer: Option<Layer> },
    Move { from: usize, to: usize },
}
fn step_bytes(s: &Step) -> u64 {
    match s {
        Step::Paint(p) => p.w as u64 * p.h as u64 * 8,
        Step::AddLayer { layer: Some(_), .. } | Step::DelLayer { layer: Some(_), .. } => 4 * 1024 * 1024,
        _ => 0,
    }
}

struct Live {
    ptr: i32,
    stroke: Stroke,
    all: Vec<Dab>,
    tip: usize,
    dual: usize,
    t0: f64,
    finger: bool,
}

enum Gesture {
    Pan { id: i32, sx: f32, sy: f32, v0: View },
    Pinch { a: i32, b: i32, pa: (f32, f32), pb: (f32, f32), v0: View },
}

pub struct Files {
    pub internal: std::path::PathBuf,
    pub shared: std::path::PathBuf,
}

pub struct App {
    pub gfx: Option<Gfx>,
    pub ctx: egui::Context,
    pub renderer: Option<egui_wgpu::Renderer>,
    pub screen: (u32, u32),
    pub ppp: f32,
    pub view: View,
    view_fitted: bool,
    pub active: usize,
    presets: Vec<Preset>,
    brush: usize,
    last_paint: usize,
    pub color: [f32; 3],
    recent: Vec<[f32; 3]>,
    live: Option<Live>,
    pending: Vec<Dab>,
    gesture: Option<Gesture>,
    fingers: HashMap<i32, (f32, f32)>,
    tap: Option<(f64, usize, bool)>,
    undo: Vec<Step>,
    redo: Vec<Step>,
    pub pen_seen: bool,
    finger_draws: bool,
    pub dirty: bool,
    pub changed: bool,
    pub last_change: f64,
    pub doc_name: String,
    hover: Option<(f32, f32)>,
    // on-screen controls
    ui_rects: Vec<Rect>,
    ui_ptr: Option<i32>,
    egui_events: Vec<egui::Event>,
    show_layers: bool,
    show_color: bool,
    show_menu: bool,
    show_open: bool,
    show_new: bool,
    new_size: (u32, u32),
    toast: Option<(String, f64)>,
    open_list: Vec<(std::path::PathBuf, bool)>,
    browse_dir: std::path::PathBuf,
    /// beleaf may read every file on the phone (Android's "All files access")
    pub files_ok: bool,
    /// ask the platform to check / to show the permission page
    pub check_files: bool,
    pub ask_files: bool,
    pub files: Files,
    pub want_save: bool,
    pub surface_format: wgpu::TextureFormat,
    finger_start: HashMap<i32, (f32, f32)>,
    /// messages from saving/exporting threads, shown when they finish
    notes: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    /// the part of the screen not covered by the phone's own bars (left, top, right, bottom px)
    pub safe: [f32; 4],
}

fn now_ms() -> f64 {
    use std::sync::OnceLock;
    static T0: OnceLock<std::time::Instant> = OnceLock::new();
    T0.get_or_init(std::time::Instant::now).elapsed().as_secs_f64() * 1000.0
}
fn c32(c: [f32; 3]) -> Color32 {
    Color32::from_rgb((c[0] * 255.0) as u8, (c[1] * 255.0) as u8, (c[2] * 255.0) as u8)
}

impl App {
    pub fn new(files: Files) -> App {
        let ctx = egui::Context::default();
        let mut v = egui::Visuals::dark();
        v.panel_fill = Color32::from_rgb(0x14, 0x14, 0x16);
        v.window_fill = Color32::from_rgb(0x1C, 0x1C, 0x20);
        v.extreme_bg_color = Color32::from_rgb(0x10, 0x10, 0x12);
        v.selection.bg_fill = Color32::from_rgb(0x4C, 0x9A, 0xFF);
        v.window_rounding = egui::Rounding::same(14.0);
        v.widgets.inactive.rounding = egui::Rounding::same(10.0);
        v.widgets.hovered.rounding = egui::Rounding::same(10.0);
        v.widgets.active.rounding = egui::Rounding::same(10.0);
        ctx.set_visuals(v);
        ctx.style_mut(|s| {
            s.spacing.interact_size = Vec2::new(44.0, 38.0);
            s.spacing.button_padding = Vec2::new(12.0, 8.0);
            s.spacing.item_spacing = Vec2::new(8.0, 8.0);
            s.spacing.slider_width = 160.0;
            for (k, f) in s.text_styles.iter_mut() {
                f.size = match k {
                    egui::TextStyle::Heading => 20.0,
                    egui::TextStyle::Small => 12.0,
                    _ => 16.0,
                };
            }
        });
        let presets = presets();
        App {
            gfx: None,
            ctx,
            renderer: None,
            screen: (1, 1),
            ppp: 2.625,
            view: View { tx: 0.0, ty: 0.0, s: 1.0, r: 0.0 },
            view_fitted: false,
            active: 0,
            presets,
            brush: 0,
            last_paint: 0,
            color: [0.16, 0.12, 0.14],
            recent: vec![[0.16, 0.12, 0.14], [0.95, 0.65, 0.35], [0.88, 0.4, 0.5], [0.3, 0.55, 0.95], [0.35, 0.78, 0.6], [1.0, 1.0, 1.0]],
            live: None,
            pending: Vec::new(),
            gesture: None,
            fingers: HashMap::new(),
            tap: None,
            undo: Vec::new(),
            redo: Vec::new(),
            pen_seen: false,
            finger_draws: false,
            dirty: true,
            changed: false,
            last_change: 0.0,
            doc_name: String::new(),
            hover: None,
            ui_rects: Vec::new(),
            ui_ptr: None,
            egui_events: Vec::new(),
            show_layers: false,
            show_color: false,
            show_menu: false,
            show_open: false,
            show_new: false,
            new_size: (2048, 2048),
            toast: None,
            open_list: Vec::new(),
            browse_dir: std::path::PathBuf::from("/storage/emulated/0/Download"),
            files_ok: false,
            check_files: true,
            ask_files: false,
            files,
            want_save: false,
            surface_format: wgpu::TextureFormat::Rgba8Unorm,
            finger_start: HashMap::new(),
            notes: Default::default(),
            safe: [0.0, 0.0, 0.0, 0.0],
        }
    }

    pub fn say(&mut self, s: impl Into<String>) {
        self.toast = Some((s.into(), now_ms() + 2600.0));
        self.dirty = true;
    }

    /* ---------------- the document ---------------- */

    /// Start from a document (a saved one, or a new empty canvas).
    pub fn load_doc(&mut self, d: DocData) {
        let Some(old) = self.gfx.take() else { return };
        // a different size needs new canvas textures: rebuild the renderer around the same device
        self.gfx = Some(if old.cw != d.w || old.ch != d.h { old.rebuilt(self.surface_format, d.w, d.h, &tip_images()) } else { old });
        let g = self.gfx.as_mut().unwrap();
        g.layers.clear();
        g.paper = d.paper;
        for (i, l) in d.layers.iter().enumerate() {
            let mut layer = g.make_layer(&l.name);
            layer.visible = l.visible;
            layer.opacity = l.opacity;
            g.layers.push(layer);
            if !l.rgba.is_empty() && l.rgba.len() == (g.cw * g.ch * 4) as usize {
                g.upload_layer(i, &l.rgba);
            }
        }
        self.active = d.active.min(g.layers.len().saturating_sub(1));
        self.doc_name = d.name;
        self.undo.clear();
        self.redo.clear();
        self.view_fitted = false;
        self.dirty = true;
    }
    pub fn new_doc(&mut self, w: u32, h: u32) {
        let d = DocData { name: String::new(), w, h, paper: [1.0, 1.0, 1.0], active: 0, layers: vec![LayerData { name: "Layer 1".into(), visible: true, opacity: 1.0, rgba: Vec::new() }] };
        self.load_doc(d);
        self.changed = true;
    }
    /// Everything needed to save (reads every layer back from the graphics card).
    pub fn snapshot(&self) -> Option<DocData> {
        let g = self.gfx.as_ref()?;
        let layers = (0..g.layers.len()).map(|i| LayerData { name: g.layers[i].name.clone(), visible: g.layers[i].visible, opacity: g.layers[i].opacity, rgba: g.read_layer(i) }).collect();
        Some(DocData { name: self.doc_name.clone(), w: g.cw, h: g.ch, paper: g.paper, active: self.active, layers })
    }

    pub fn fit_view(&mut self) {
        let Some(g) = self.gfx.as_ref() else { return };
        let sf = self.safe_rect();
        let top = sf[1] + 70.0 * self.ppp;
        let bot = sf[3] - 80.0 * self.ppp;
        let avail = (sf[2] - sf[0] - 24.0 * self.ppp, bot - top);
        let s = (avail.0 / g.cw as f32).min(avail.1 / g.ch as f32).max(0.01);
        self.view = View { s, r: 0.0, tx: (sf[0] + sf[2] - g.cw as f32 * s) / 2.0, ty: top + (avail.1 - g.ch as f32 * s) / 2.0 };
        self.view_fitted = true;
        self.dirty = true;
    }

    /// The usable part of the screen in px (the whole screen until the phone tells us its bars).
    /// The platform checked "All files access": refresh the open window's list if it changed.
    pub fn set_files_ok(&mut self, ok: bool) {
        if ok != self.files_ok {
            self.files_ok = ok;
            if self.show_open {
                self.open_list = list_dir(&self.browse_dir);
            }
            self.dirty = true;
        }
    }

    pub fn safe_rect(&self) -> [f32; 4] {
        let (w, h) = (self.screen.0 as f32, self.screen.1 as f32);
        let s = self.safe;
        if s[2] > s[0] + 50.0 && s[3] > s[1] + 50.0 { [s[0].max(0.0), s[1].max(0.0), s[2].min(w), s[3].min(h)] } else { [0.0, 0.0, w, h] }
    }

    /* ---------------- undo ---------------- */

    fn push_step(&mut self, s: Step) {
        self.undo.push(s);
        self.redo.clear();
        // keep the history within about 400 MB of graphics memory and 200 steps
        let mut total: u64 = self.undo.iter().map(step_bytes).sum();
        while self.undo.len() > 1 && (self.undo.len() > 200 || total > 400 * 1024 * 1024) {
            let s = self.undo.remove(0);
            total -= step_bytes(&s);
        }
        self.changed = true;
        self.last_change = now_ms();
    }
    pub fn undo(&mut self) {
        if self.live.is_some() {
            self.cancel_stroke();
            return;
        }
        let Some(s) = self.undo.pop() else { return self.say("Nothing to undo") };
        let s = self.apply(s, true);
        self.redo.push(s);
        self.changed = true;
        self.dirty = true;
    }
    pub fn redo(&mut self) {
        let Some(s) = self.redo.pop() else { return self.say("Nothing to redo") };
        let s = self.apply(s, false);
        self.undo.push(s);
        self.changed = true;
        self.dirty = true;
    }
    fn apply(&mut self, s: Step, back: bool) -> Step {
        let g = self.gfx.as_mut().unwrap();
        match s {
            Step::Paint(p) => {
                g.restore(&p, back);
                if let Some(i) = g.index_of(p.layer) {
                    self.active = i;
                }
                Step::Paint(p)
            }
            Step::AddLayer { idx, layer } => {
                // undo = take it out again, redo = put it back
                if back {
                    let l = g.layers.remove(idx);
                    self.active = self.active.min(g.layers.len().saturating_sub(1));
                    Step::AddLayer { idx, layer: Some(l) }
                } else {
                    if let Some(l) = layer {
                        g.layers.insert(idx.min(g.layers.len()), l);
                    }
                    self.active = idx.min(g.layers.len() - 1);
                    Step::AddLayer { idx, layer: None }
                }
            }
            Step::DelLayer { idx, layer } => {
                if back {
                    if let Some(l) = layer {
                        g.layers.insert(idx.min(g.layers.len()), l);
                    }
                    self.active = idx.min(g.layers.len() - 1);
                    Step::DelLayer { idx, layer: None }
                } else {
                    let l = g.layers.remove(idx);
                    self.active = self.active.min(g.layers.len().saturating_sub(1));
                    Step::DelLayer { idx, layer: Some(l) }
                }
            }
            Step::Move { from, to } => {
                let (a, b) = if back { (to, from) } else { (from, to) };
                let l = g.layers.remove(a);
                g.layers.insert(b, l);
                self.active = b;
                Step::Move { from, to }
            }
        }
    }

    fn add_layer(&mut self) {
        let Some(g) = self.gfx.as_mut() else { return };
        if g.layers.len() >= 40 {
            return self.say("That's the most layers for now (40)");
        }
        let n = g.layers.len() + 1;
        let l = g.make_layer(&format!("Layer {n}"));
        let idx = (self.active + 1).min(g.layers.len());
        g.layers.insert(idx, l);
        self.active = idx;
        self.push_step(Step::AddLayer { idx, layer: None });
        self.dirty = true;
    }
    fn delete_layer(&mut self) {
        let Some(g) = self.gfx.as_mut() else { return };
        if g.layers.len() <= 1 {
            return self.say("A canvas needs at least one layer");
        }
        let idx = self.active;
        let l = g.layers.remove(idx);
        self.active = idx.min(g.layers.len() - 1);
        self.push_step(Step::DelLayer { idx, layer: Some(l) });
        self.dirty = true;
    }
    fn move_layer(&mut self, up: bool) {
        let Some(g) = self.gfx.as_mut() else { return };
        let from = self.active;
        let to = if up { from + 1 } else { from.wrapping_sub(1) };
        if to >= g.layers.len() {
            return;
        }
        let l = g.layers.remove(from);
        g.layers.insert(to, l);
        self.active = to;
        self.push_step(Step::Move { from, to });
        self.dirty = true;
    }
    fn clear_active(&mut self) {
        let Some(g) = self.gfx.as_mut() else { return };
        let snap = {
            let mut s = g.snap_before(self.active, 0, 0, g.cw, g.ch);
            g.clear_layer(self.active);
            g.snap_after(&mut s);
            s
        };
        self.push_step(Step::Paint(snap));
        self.dirty = true;
    }

    /* ---------------- drawing ---------------- */

    fn begin_stroke(&mut self, p: &Ptr) {
        let Some(g) = self.gfx.as_mut() else { return };
        if !g.layers[self.active].visible {
            self.say("This layer is hidden. Show it in Layers to draw on it.");
            return;
        }
        let pen_erase = p.tool == Tool::PenEraser || (p.tool == Tool::Pen && p.button);
        let pr = &self.presets[if pen_erase { self.presets.len() - 1 } else { self.brush }];
        let mut b = (pr.make)(pr.size);
        if p.tool == Tool::Finger || p.tool == Tool::Mouse {
            // no pressure from a finger: draw at a steady medium pressure
            b.size_min = b.size_min.map(|m| m.max(70.0));
        }
        let seed = (now_ms() as u32).wrapping_mul(2654435761);
        let mut st = Stroke::new(b, seed);
        st.set_color(self.color);
        g.begin_stroke(self.color, pr.opacity, pr.dual_tip != pr.tip && pr.name == "Textured acrylic", pr.erase || pen_erase);
        let (tip, dual) = (pr.tip, pr.dual_tip);
        self.live = Some(Live { ptr: p.id, stroke: st, all: Vec::new(), tip, dual, t0: now_ms(), finger: p.tool == Tool::Finger });
        self.add_samples(&[*p]);
    }
    fn add_samples(&mut self, ps: &[Ptr]) {
        let v = self.view;
        let Some(l) = self.live.as_mut() else { return };
        for p in ps {
            let (x, y) = v.to_canvas(p.x, p.y);
            let pr = if l.finger || p.tool == Tool::Mouse { 0.6 } else { p.pressure.clamp(0.0, 1.0) };
            let d = l.stroke.add_pen(x, y, pr, p.tilt, p.az, p.t_ms);
            l.all.extend_from_slice(&d);
            self.pending.extend_from_slice(&d);
        }
        self.dirty = true;
    }
    fn end_stroke(&mut self) {
        let Some(mut l) = self.live.take() else { return };
        let all = l.stroke.finish();
        self.pending.clear();
        let Some(g) = self.gfx.as_mut() else { return };
        if all.is_empty() {
            g.cancel_stroke();
            return;
        }
        // the changed rectangle, for undo
        let (mut x0, mut y0, mut x1, mut y1) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
        for d in &all {
            let r = d.size * 0.75 + 2.0;
            x0 = x0.min(d.x - r);
            y0 = y0.min(d.y - r);
            x1 = x1.max(d.x + r);
            y1 = y1.max(d.y + r);
        }
        let (cw, ch) = (g.cw as f32, g.ch as f32);
        let (x0, y0, x1, y1) = (x0.clamp(0.0, cw) as u32, y0.clamp(0.0, ch) as u32, x1.clamp(0.0, cw).ceil() as u32, y1.clamp(0.0, ch).ceil() as u32);
        if x1 <= x0 || y1 <= y0 {
            g.cancel_stroke();
            return;
        }
        let mut snap = g.snap_before(self.active, x0, y0, x1 - x0, y1 - y0);
        g.commit_stroke(self.active, &all, l.tip, l.dual);
        g.snap_after(&mut snap);
        let _ = &mut l;
        self.push_step(Step::Paint(snap));
        self.dirty = true;
    }
    fn cancel_stroke(&mut self) {
        self.live = None;
        self.pending.clear();
        if let Some(g) = self.gfx.as_mut() {
            g.cancel_stroke();
        }
        self.dirty = true;
    }

    /* ---------------- input ---------------- */

    fn over_ui(&self, x: f32, y: f32) -> bool {
        let p = Pos2::new(x / self.ppp, y / self.ppp);
        self.ui_rects.iter().any(|r| r.expand(2.0).contains(p))
    }
    fn to_egui(&mut self, p: &Ptr, ev: u8) {
        let pos = Pos2::new(p.x / self.ppp, p.y / self.ppp);
        match ev {
            0 => {
                self.egui_events.push(egui::Event::PointerMoved(pos));
                self.egui_events.push(egui::Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed: true, modifiers: Default::default() });
            }
            1 => self.egui_events.push(egui::Event::PointerMoved(pos)),
            _ => {
                self.egui_events.push(egui::Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed: false, modifiers: Default::default() });
                self.egui_events.push(egui::Event::PointerGone);
            }
        }
        self.dirty = true;
    }

    pub fn input(&mut self, ev: PEv) {
        match ev {
            PEv::Hover(p) => {
                if p.tool == Tool::Pen || p.tool == Tool::PenEraser {
                    self.pen_seen = true;
                }
                if self.over_ui(p.x, p.y) {
                    self.hover = None;
                    self.to_egui(&p, 1);
                } else {
                    self.hover = Some((p.x, p.y));
                }
                self.dirty = true;
            }
            PEv::HoverEnd => {
                self.hover = None;
                self.dirty = true;
            }
            PEv::Down(p) => {
                if p.tool == Tool::Pen || p.tool == Tool::PenEraser {
                    self.pen_seen = true;
                }
                self.hover = None;
                // on the controls: hand it to them
                if self.ui_ptr.is_none() && self.over_ui(p.x, p.y) && self.live.is_none() && self.gesture.is_none() {
                    self.ui_ptr = Some(p.id);
                    self.to_egui(&p, 0);
                    return;
                }
                if p.tool == Tool::Finger {
                    self.fingers.insert(p.id, (p.x, p.y));
                    self.finger_start.insert(p.id, (p.x, p.y));
                    let n = self.fingers.len();
                    if let Some(t) = self.tap.as_mut() {
                        t.1 = t.1.max(n);
                    } else {
                        self.tap = Some((now_ms(), 1, false));
                    }
                    if n >= 2 {
                        // a second finger: it's a gesture, not a line (drop a stroke the first finger just began)
                        if self.live.as_ref().map(|l| l.finger).unwrap_or(false) {
                            self.cancel_stroke();
                        }
                        let ids: Vec<i32> = self.fingers.keys().copied().take(2).collect();
                        let (a, b) = (ids[0], ids[1]);
                        self.gesture = Some(Gesture::Pinch { a, b, pa: self.fingers[&a], pb: self.fingers[&b], v0: self.view });
                        return;
                    }
                    if self.pen_seen && !self.finger_draws {
                        self.gesture = Some(Gesture::Pan { id: p.id, sx: p.x, sy: p.y, v0: self.view });
                        return;
                    }
                }
                if self.live.is_none() {
                    self.begin_stroke(&p);
                }
            }
            PEv::Move(ps) => {
                let Some(last) = ps.last().copied() else { return };
                if self.ui_ptr == Some(last.id) {
                    self.to_egui(&last, 1);
                    return;
                }
                if last.tool == Tool::Finger && self.fingers.contains_key(&last.id) {
                    if let (Some(f), Some(t)) = (self.finger_start.get(&last.id).copied(), self.tap.as_mut()) {
                        if (last.x - f.0).hypot(last.y - f.1) > 10.0 * self.ppp {
                            t.2 = true;
                        }
                    }
                    self.fingers.insert(last.id, (last.x, last.y));
                }
                match &self.gesture {
                    Some(Gesture::Pan { id, sx, sy, v0 }) if *id == last.id => {
                        self.view = View { tx: v0.tx + last.x - sx, ty: v0.ty + last.y - sy, ..*v0 };
                        self.dirty = true;
                        return;
                    }
                    Some(Gesture::Pinch { a, b, pa, pb, v0 }) => {
                        let (Some(ca), Some(cb)) = (self.fingers.get(a).copied(), self.fingers.get(b).copied()) else { return };
                        let d0 = ((pb.0 - pa.0).hypot(pb.1 - pa.1)).max(1.0);
                        let d1 = ((cb.0 - ca.0).hypot(cb.1 - ca.1)).max(1.0);
                        let s = (v0.s * d1 / d0).clamp(0.02, 40.0);
                        let mut r = v0.r + (cb.1 - ca.1).atan2(cb.0 - ca.0) - (pb.1 - pa.1).atan2(pb.0 - pa.0);
                        // a little twist doesn't turn the canvas, and it clicks back to straight
                        let deg = r.to_degrees().rem_euclid(360.0);
                        let near = (deg / 90.0).round() * 90.0;
                        if (deg - near).abs() < 6.0 {
                            r = near.to_radians();
                        }
                        let mid0 = ((pa.0 + pb.0) / 2.0, (pa.1 + pb.1) / 2.0);
                        let mid1 = ((ca.0 + cb.0) / 2.0, (ca.1 + cb.1) / 2.0);
                        let anchor = v0.to_canvas(mid0.0, mid0.1);
                        let mut v = View { s, r, tx: 0.0, ty: 0.0 };
                        let (px, py) = v.to_screen(anchor.0, anchor.1);
                        v.tx = mid1.0 - px;
                        v.ty = mid1.1 - py;
                        self.view = v;
                        self.dirty = true;
                        return;
                    }
                    _ => {}
                }
                if self.live.as_ref().map(|l| l.ptr == last.id).unwrap_or(false) {
                    self.add_samples(&ps);
                }
            }
            PEv::Up(p) => {
                if self.ui_ptr == Some(p.id) {
                    self.ui_ptr = None;
                    self.to_egui(&p, 2);
                    return;
                }
                if p.tool == Tool::Finger {
                    self.fingers.remove(&p.id);
                    self.finger_start.remove(&p.id);
                    if self.fingers.is_empty() {
                        // quick taps with two or three fingers: undo / redo
                        if let Some((t0, n, moved)) = self.tap.take() {
                            if !moved && now_ms() - t0 < 320.0 && self.live.as_ref().map(|l| now_ms() - l.t0 < 320.0).unwrap_or(true) {
                                if n == 2 {
                                    if let Some(Gesture::Pinch { v0, .. }) = &self.gesture {
                                        self.view = *v0;
                                    }
                                    self.gesture = None;
                                    self.undo();
                                    self.say("Undo");
                                    return;
                                } else if n == 3 {
                                    self.gesture = None;
                                    self.redo();
                                    self.say("Redo");
                                    return;
                                }
                            }
                        }
                    }
                    match &self.gesture {
                        Some(Gesture::Pinch { a, b, .. }) if *a == p.id || *b == p.id => {
                            self.gesture = None;
                            return;
                        }
                        Some(Gesture::Pan { id, .. }) if *id == p.id => {
                            self.gesture = None;
                            return;
                        }
                        _ => {}
                    }
                }
                if self.live.as_ref().map(|l| l.ptr == p.id).unwrap_or(false) {
                    self.add_samples(&[p]);
                    self.end_stroke();
                }
            }
            PEv::Cancel => {
                self.cancel_stroke();
                self.gesture = None;
                self.fingers.clear();
                self.tap = None;
                if self.ui_ptr.take().is_some() {
                    self.egui_events.push(egui::Event::PointerGone);
                }
            }
        }
    }

    /* ---------------- each frame ---------------- */

    /// Draw a frame into the screen texture.
    pub fn frame(&mut self, target: &wgpu::TextureView) -> Vec<wgpu::CommandBuffer> {
        if !self.view_fitted {
            self.fit_view();
        }
        let ppp = self.ppp;
        let (sw, sh) = self.screen;
        // the controls
        let sf = self.safe_rect();
        let mut raw = egui::RawInput { screen_rect: Some(Rect::from_min_max(Pos2::new(sf[0] / ppp, sf[1] / ppp), Pos2::new(sf[2] / ppp, sf[3] / ppp))), time: Some(now_ms() / 1000.0), ..Default::default() };
        raw.viewports.entry(egui::ViewportId::ROOT).or_default().native_pixels_per_point = Some(ppp);
        raw.events = std::mem::take(&mut self.egui_events);
        let ctx = self.ctx.clone();
        let out = ctx.run(raw, |ctx| self.ui(ctx));
        let jobs = ctx.tessellate(out.shapes, out.pixels_per_point);
        if out.viewport_output.values().any(|v| v.repaint_delay.as_secs_f32() < 0.05) {
            self.dirty = true;
        }

        let g = self.gfx.as_ref().unwrap();
        let mut enc = g.device.create_command_encoder(&Default::default());
        if !self.pending.is_empty() {
            if let Some(l) = &self.live {
                g.draw_dabs(&mut enc, &self.pending, l.tip, l.dual);
            }
            self.pending.clear();
        }
        g.render(&mut enc, target, sw as f32, sh as f32, &self.view, self.active, [0.06, 0.06, 0.07]);

        let r = self.renderer.as_mut().unwrap();
        for (id, delta) in &out.textures_delta.set {
            r.update_texture(&g.device, &g.queue, *id, delta);
        }
        let sd = egui_wgpu::ScreenDescriptor { size_in_pixels: [sw, sh], pixels_per_point: ppp };
        let mut cmds = r.update_buffers(&g.device, &g.queue, &mut enc, &jobs, &sd);
        {
            let rp = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("controls"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment { view: target, resolve_target: None, ops: wgpu::Operations { load: wgpu::LoadOp::Load, store: wgpu::StoreOp::Store } })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            r.render(&mut rp.forget_lifetime(), &jobs, &sd);
        }
        for id in &out.textures_delta.free {
            r.free_texture(id);
        }
        cmds.push(enc.finish());
        cmds
    }

    fn ui(&mut self, ctx: &egui::Context) {
        let mut rects = Vec::new();
        let ppp = self.ppp;
        // ---- top bar ----
        let top = egui::TopBottomPanel::top("top").exact_height(52.0).show(ctx, |ui| {
            ui.horizontal_centered(|ui| {
                if ui.button("☰").on_hover_text("Menu").clicked() {
                    self.show_menu = !self.show_menu;
                }
                ui.label(egui::RichText::new("beleaf").strong().color(Color32::from_rgb(0x6C, 0xC2, 0x8A)));
                if ui.add_enabled(!self.undo.is_empty(), egui::Button::new("Undo")).clicked() {
                    self.undo();
                }
                if ui.add_enabled(!self.redo.is_empty(), egui::Button::new("Redo")).clicked() {
                    self.redo();
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("Layers").clicked() {
                        self.show_layers = !self.show_layers;
                    }
                    let (resp, p) = ui.allocate_painter(Vec2::new(40.0, 40.0), egui::Sense::click());
                    p.circle_filled(resp.rect.center(), 15.0, c32(self.color));
                    p.circle_stroke(resp.rect.center(), 15.0, egui::Stroke::new(2.5, Color32::WHITE));
                    if resp.clicked() {
                        self.show_color = !self.show_color;
                    }
                });
            });
        });
        rects.push(top.response.rect);
        // ---- bottom bar: brushes ----
        let bot = egui::TopBottomPanel::bottom("bottom").exact_height(64.0).show(ctx, |ui| {
            egui::ScrollArea::horizontal().show(ui, |ui| {
                ui.horizontal_centered(|ui| {
                    for i in 0..self.presets.len() {
                        let on = self.brush == i;
                        let txt = egui::RichText::new(self.presets[i].name).size(15.0);
                        if ui.add(egui::SelectableLabel::new(on, txt)).clicked() {
                            if !self.presets[i].erase {
                                self.last_paint = i;
                            }
                            self.brush = i;
                        }
                    }
                });
            });
        });
        rects.push(bot.response.rect);
        // ---- size and opacity on the left ----
        let sr = ctx.screen_rect();
        let left = egui::Area::new(egui::Id::new("sliders")).fixed_pos(Pos2::new(6.0, sr.center().y - 150.0)).show(ctx, |ui| {
            egui::Frame::window(ui.style()).inner_margin(8.0).show(ui, |ui| {
                ui.spacing_mut().slider_width = 130.0;
                let pr = &mut self.presets[self.brush];
                ui.label(egui::RichText::new("Size").small());
                ui.add(egui::Slider::new(&mut pr.size, 1.0..=400.0).vertical().logarithmic(true).show_value(false));
                ui.label(egui::RichText::new(format!("{:.0}", pr.size)).small());
                ui.add_space(6.0);
                ui.label(egui::RichText::new("Opacity").small());
                let mut op = pr.opacity * 100.0;
                if ui.add(egui::Slider::new(&mut op, 1.0..=100.0).vertical().show_value(false)).changed() {
                    pr.opacity = op / 100.0;
                }
                ui.label(egui::RichText::new(format!("{:.0}%", op)).small());
            });
        });
        rects.push(left.response.rect);

        // ---- colour ----
        if self.show_color {
            let mut open = true;
            let r = egui::Window::new("Colour").open(&mut open).resizable(false).default_pos(Pos2::new(sr.width() - 300.0, 70.0)).show(ctx, |ui| {
                let mut c = c32(self.color);
                if egui::color_picker::color_picker_color32(ui, &mut c, egui::color_picker::Alpha::Opaque) {
                    self.color = [c.r() as f32 / 255.0, c.g() as f32 / 255.0, c.b() as f32 / 255.0];
                }
                ui.horizontal_wrapped(|ui| {
                    for rc in self.recent.clone() {
                        let (resp, p) = ui.allocate_painter(Vec2::new(34.0, 34.0), egui::Sense::click());
                        p.circle_filled(resp.rect.center(), 14.0, c32(rc));
                        p.circle_stroke(resp.rect.center(), 14.0, egui::Stroke::new(1.0, Color32::GRAY));
                        if resp.clicked() {
                            self.color = rc;
                        }
                    }
                });
            });
            if let Some(r) = r {
                rects.push(r.response.rect);
            }
            if !open {
                self.show_color = false;
                self.remember_color();
            }
        }
        // ---- layers ----
        if self.show_layers {
            let mut open = true;
            let mut act: Option<u8> = None;
            let r = egui::Window::new("Layers").open(&mut open).default_pos(Pos2::new(sr.width() - 290.0, 70.0)).default_width(270.0).show(ctx, |ui| {
                ui.horizontal_wrapped(|ui| {
                    if ui.button("+ Layer").clicked() {
                        act = Some(0);
                    }
                    if ui.button("Up").clicked() {
                        act = Some(1);
                    }
                    if ui.button("Down").clicked() {
                        act = Some(2);
                    }
                    if ui.button("Clear").clicked() {
                        act = Some(3);
                    }
                    if ui.button("Delete").clicked() {
                        act = Some(4);
                    }
                });
                ui.separator();
                let g = self.gfx.as_mut().unwrap();
                egui::ScrollArea::vertical().max_height(sr.height() * 0.45).show(ui, |ui| {
                    for i in (0..g.layers.len()).rev() {
                        let l = &mut g.layers[i];
                        ui.horizontal(|ui| {
                            if ui.selectable_label(l.visible, if l.visible { "👁" } else { "–" }).clicked() {
                                l.visible = !l.visible;
                                self.changed = true;
                            }
                            if ui.selectable_label(self.active == i, l.name.as_str()).clicked() {
                                self.active = i;
                            }
                        });
                        if self.active == i {
                            let mut op = l.opacity * 100.0;
                            if ui.add(egui::Slider::new(&mut op, 0.0..=100.0).text("opacity")).changed() {
                                l.opacity = op / 100.0;
                                self.changed = true;
                            }
                        }
                    }
                });
            });
            if let Some(r) = r {
                rects.push(r.response.rect);
            }
            if !open {
                self.show_layers = false;
            }
            match act {
                Some(0) => self.add_layer(),
                Some(1) => self.move_layer(true),
                Some(2) => self.move_layer(false),
                Some(3) => self.clear_active(),
                Some(4) => self.delete_layer(),
                _ => {}
            }
        }
        // ---- menu ----
        if self.show_menu {
            let mut open = true;
            let mut act: Option<u8> = None;
            let r = egui::Window::new("Menu").open(&mut open).default_pos(Pos2::new(8.0, 60.0)).default_width(260.0).show(ctx, |ui| {
                if ui.button("New canvas…").clicked() {
                    act = Some(0);
                }
                if ui.button("Save to Downloads/beleaf").clicked() {
                    act = Some(1);
                }
                if ui.button("Open from Downloads/beleaf…").clicked() {
                    act = Some(2);
                }
                if ui.button("Export PNG to Downloads/beleaf").clicked() {
                    act = Some(3);
                }
                ui.separator();
                if ui.button("Fit canvas to screen").clicked() {
                    act = Some(4);
                }
                let mut fd = self.finger_draws;
                if ui.checkbox(&mut fd, "Fingers draw too (off: fingers move the canvas once a pen is used)").changed() {
                    self.finger_draws = fd;
                }
                ui.separator();
                ui.label(egui::RichText::new("Two fingers tap: undo · three fingers: redo · pinch: zoom and turn · S Pen button held: erase").small());
                ui.label(egui::RichText::new(format!("beleaf native {} · test build", env!("CARGO_PKG_VERSION"))).small().weak());
            });
            if let Some(r) = r {
                rects.push(r.response.rect);
            }
            if !open {
                self.show_menu = false;
            }
            match act {
                Some(0) => self.show_new = true,
                Some(1) => {
                    self.want_save = true;
                    self.show_menu = false;
                }
                Some(2) => {
                    self.check_files = true;
                    self.open_list = list_dir(&self.browse_dir);
                    self.show_open = true;
                    self.show_menu = false;
                }
                Some(3) => {
                    self.export_png();
                    self.show_menu = false;
                }
                Some(4) => {
                    self.fit_view();
                    self.show_menu = false;
                }
                _ => {}
            }
        }
        if self.show_new {
            let mut open = true;
            let mut go = None;
            let r = egui::Window::new("New canvas").open(&mut open).collapsible(false).show(ctx, |ui| {
                for (label, w, h) in [("Square 2048", 2048, 2048), ("Emote 1000", 1000, 1000), ("Emote 500", 500, 500), ("Phone 1440 × 3200", 1440, 3200), ("Card 2010 × 2814", 2010, 2814), ("A4 at 200 dpi", 1654, 2339)] {
                    if ui.button(label).clicked() {
                        go = Some((w, h));
                    }
                }
                ui.label(egui::RichText::new("Your current drawing is saved first (autosave), but undo starts fresh.").small().weak());
            });
            if let Some(r) = r {
                rects.push(r.response.rect);
            }
            if let Some((w, h)) = go {
                self.want_save = true;
                self.new_size = (w, h);
                self.new_doc(w, h);
                self.show_new = false;
            } else if !open {
                self.show_new = false;
            }
        }
        if self.show_open {
            let mut open = true;
            let mut pick = None;
            let mut go: Option<std::path::PathBuf> = None;
            let r = egui::Window::new("Open a .beleaf file").open(&mut open).collapsible(false).default_width(sr.width() - 24.0).default_pos(Pos2::new(sr.left() + 12.0, sr.top() + 60.0)).show(ctx, |ui| {
                if !self.files_ok {
                    ui.label("To open files you saved from the browser or got from someone else, beleaf needs Android's “All files access”. Files this app saved itself open either way.");
                    if ui.button("Allow beleaf to open your files").clicked() {
                        self.ask_files = true;
                    }
                    ui.separator();
                }
                ui.horizontal_wrapped(|ui| {
                    for (label, p) in [("Download", "/storage/emulated/0/Download"), ("beleaf saves", "/storage/emulated/0/Download/beleaf"), ("Documents", "/storage/emulated/0/Documents"), ("Phone", "/storage/emulated/0")] {
                        if ui.selectable_label(self.browse_dir == std::path::Path::new(p), label).clicked() {
                            go = Some(p.into());
                        }
                    }
                });
                ui.horizontal(|ui| {
                    if ui.add_enabled(self.browse_dir.parent().is_some() && self.browse_dir != std::path::Path::new("/storage/emulated/0"), egui::Button::new("⬆ Up")).clicked() {
                        go = self.browse_dir.parent().map(|p| p.to_path_buf());
                    }
                    ui.label(egui::RichText::new(self.browse_dir.display().to_string()).small().weak());
                });
                ui.separator();
                if self.open_list.is_empty() {
                    ui.label(if self.files_ok { "No folders or .beleaf files here." } else { "Nothing beleaf can see here yet." });
                }
                egui::ScrollArea::vertical().max_height(sr.height() * 0.5).show(ui, |ui| {
                    for (p, dir) in &self.open_list {
                        let n = p.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
                        let label = if *dir { format!("📁  {n}") } else { format!("🍃  {n}") };
                        if ui.add_sized([ui.available_width(), 40.0], egui::Button::new(label).wrap_mode(egui::TextWrapMode::Truncate)).clicked() {
                            if *dir {
                                go = Some(p.clone());
                            } else {
                                pick = Some(p.clone());
                            }
                        }
                    }
                });
            });
            if let Some(d) = go {
                self.browse_dir = d;
                self.open_list = list_dir(&self.browse_dir);
            }
            if let Some(r) = r {
                rects.push(r.response.rect);
            }
            if let Some(p) = pick {
                self.show_open = false;
                self.open_path(&p);
            } else if !open {
                self.show_open = false;
            }
        }
        // messages from saving
        let got: Vec<String> = self.notes.lock().map(|mut n| n.drain(..).collect()).unwrap_or_default();
        if let Some(m) = got.last() {
            self.toast = Some((m.clone(), now_ms() + 3500.0));
        }
        if self.toast.is_some() {
            ctx.request_repaint_after(std::time::Duration::from_millis(250));
        }
        // ---- toast ----
        if let Some((t, until)) = self.toast.clone() {
            if now_ms() < until {
                egui::Area::new(egui::Id::new("toast")).anchor(egui::Align2::CENTER_BOTTOM, Vec2::new(0.0, -80.0)).interactable(false).show(ctx, |ui| {
                    egui::Frame::window(ui.style()).show(ui, |ui| {
                        ui.label(t);
                    });
                });
                ctx.request_repaint_after(std::time::Duration::from_millis(200));
            } else {
                self.toast = None;
            }
        }
        // ---- brush outline under a hovering pen ----
        if let Some((x, y)) = self.hover {
            let pr = &self.presets[self.brush];
            let r = (pr.size * self.view.s / 2.0 / ppp).max(2.0);
            let p = ctx.layer_painter(egui::LayerId::new(egui::Order::Foreground, egui::Id::new("cursor")));
            p.circle_stroke(Pos2::new(x / ppp, y / ppp), r, egui::Stroke::new(1.5, Color32::from_black_alpha(160)));
            p.circle_stroke(Pos2::new(x / ppp, y / ppp), r + 1.0f32, egui::Stroke::new(1.0, Color32::from_white_alpha(200)));
        }
        self.ui_rects = rects;
    }

    /// The phone's back button: close the top window; nothing else (so a stray swipe never closes the app).
    pub fn back(&mut self) {
        if self.show_new { self.show_new = false; } else if self.show_open { self.show_open = false; } else if self.show_menu { self.show_menu = false; } else if self.show_color { self.show_color = false; self.remember_color(); } else if self.show_layers { self.show_layers = false; } else { self.say("Use ☰ › Save to keep a copy. beleaf also saves by itself when you leave."); }
        self.dirty = true;
    }

    fn remember_color(&mut self) {
        let c = self.color;
        self.recent.retain(|r| (r[0] - c[0]).abs() + (r[1] - c[1]).abs() + (r[2] - c[2]).abs() > 0.02);
        self.recent.insert(0, c);
        self.recent.truncate(12);
    }

    /* ---------------- files ---------------- */

    pub fn autosave_path(&self) -> std::path::PathBuf {
        self.files.internal.join("autosave.beleaf")
    }
    /// Write the drawing to a file (the pixels are read here; packing and writing happen on another thread).
    pub fn save_to(&mut self, path: std::path::PathBuf, quiet: bool) {
        let Some(d) = self.snapshot() else { return };
        self.changed = false;
        let notes = self.notes.clone();
        if !quiet {
            self.say("Saving…");
        }
        std::thread::spawn(move || {
            if let Some(dir) = path.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            let bytes = doc::save(&d);
            let tmp = path.with_extension("tmp");
            let ok = std::fs::write(&tmp, &bytes).and_then(|_| std::fs::rename(&tmp, &path));
            let m = match ok {
                Ok(_) => format!("Saved: {}", path.display()),
                Err(e) => {
                    log::error!("save failed: {e}");
                    format!("Couldn't save there ({e})")
                }
            };
            if !quiet || m.starts_with("Couldn") {
                if let Ok(mut n) = notes.lock() {
                    n.push(m);
                }
            }
        });
    }
    pub fn save_shared(&mut self) {
        let name = if self.doc_name.is_empty() { format!("beleaf-{}", now_secs()) } else { sanitize(&self.doc_name) };
        let p = self.files.shared.join(format!("{name}.beleaf"));
        self.save_to(p, false);
    }
    fn export_png(&mut self) {
        let Some(d) = self.snapshot() else { return };
        let name = if self.doc_name.is_empty() { format!("beleaf-{}", now_secs()) } else { sanitize(&self.doc_name) };
        let p = self.files.shared.join(format!("{name}.png"));
        let notes = self.notes.clone();
        self.say("Exporting…");
        std::thread::spawn(move || {
            let _ = std::fs::create_dir_all(p.parent().unwrap());
            let px = doc::flatten(&d);
            let rgba: Vec<u8> = px.chunks_exact(4).flat_map(|c| [c[0], c[1], c[2], 255]).collect();
            let m = match std::fs::write(&p, doc::png_encode(d.w, d.h, &rgba)) {
                Ok(_) => format!("Exported: {}", p.display()),
                Err(e) => format!("Couldn't export there ({e})"),
            };
            if let Ok(mut n) = notes.lock() {
                n.push(m);
            }
        });
    }
    pub fn open_path(&mut self, p: &std::path::Path) {
        let max = self.gfx.as_ref().map(|g| g.device.limits().max_texture_dimension_2d).unwrap_or(4096);
        match std::fs::read(p).map_err(|e| format!("Couldn't read it ({e})")).and_then(|b| doc::load(&b, max)) {
            Ok(mut d) => {
                if d.name.is_empty() {
                    d.name = p.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
                }
                self.load_doc(d);
                self.say("Opened");
            }
            Err(e) => self.say(e),
        }
    }
}

fn now_secs() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}
fn sanitize(s: &str) -> String {
    let t: String = s.chars().map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '-' }).collect();
    if t.is_empty() { "beleaf".into() } else { t }
}
/// Folders (not hidden ones) first, then .beleaf files, newest first.
fn list_dir(dir: &std::path::Path) -> Vec<(std::path::PathBuf, bool)> {
    let mut dirs = Vec::new();
    let mut files = Vec::new();
    if let Ok(r) = std::fs::read_dir(dir) {
        for e in r.flatten() {
            let p = e.path();
            let name = p.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
            if name.starts_with('.') {
                continue;
            }
            if p.is_dir() {
                dirs.push(p);
            } else if p.extension().map(|x| x.eq_ignore_ascii_case("beleaf")).unwrap_or(false) {
                let t = e.metadata().and_then(|m| m.modified()).ok();
                files.push((p, t));
            }
        }
    }
    dirs.sort_by_key(|p| p.file_name().map(|s| s.to_string_lossy().to_lowercase()));
    files.sort_by(|a, b| b.1.cmp(&a.1));
    dirs.into_iter().map(|p| (p, true)).chain(files.into_iter().map(|(p, _)| (p, false))).collect()
}
