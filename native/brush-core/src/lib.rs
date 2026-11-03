//! beleaf brush engine core.
//!
//! Turns pen samples (position, pressure, time) into *dabs*: stamps of the brush tip with a
//! position, size, angle and strength. Drawing the dabs is someone else's job (the GPU renderer),
//! so this crate has no dependencies and runs natively, on Android and in the browser alike.
//!
//! It is a port of beleaf's JavaScript engine: spacing along the stroke, pressure → size and
//! opacity, the stabiliser, "rotation follows the stroke", Clip Studio-style start/end taper
//! (by speed, capped to a share of the stroke) and a sprayed second ("dual") tip.

use std::f32::consts::PI;

/// The second tip that masks the first (Clip Studio's "dual brush"): the final coverage is
/// main coverage × dual coverage, which is what gives textured brushes their broken edges.
#[derive(Clone, Debug)]
pub struct Dual {
    /// size as % of the main tip
    pub size: f32,
    /// spacing as % of its own size
    pub spacing: f32,
    pub spray: bool,
    /// stamps per spray step
    pub count: u32,
    /// spray area as % of the main tip
    pub area: f32,
    pub random_angle: bool,
}

#[derive(Clone, Debug)]
pub struct Brush {
    /// diameter in canvas pixels at full pressure
    pub size: f32,
    /// distance between dabs, % of the current size
    pub spacing: f32,
    /// 0..100, strength of each dab
    pub flow: f32,
    /// base tip angle in degrees
    pub angle: f32,
    /// the tip turns to follow the stroke direction
    pub follow: bool,
    /// stabiliser 0..30
    pub stab: f32,
    /// size at zero pressure, % of full size (100 = pressure doesn't change size)
    pub size_min: Option<f32>,
    /// opacity at zero pressure, % (None = pressure doesn't change opacity)
    pub op_min: Option<f32>,
    /// pressure response curves as (in, out) points from 0..1; empty = straight line
    pub size_curve: Vec<[f32; 2]>,
    pub op_curve: Vec<[f32; 2]>,
    /// tip roundness % (100 = as drawn, lower = squashed)
    pub roundness: f32,
    pub jitter: Jitter,
    /// start / end taper lengths in canvas px (0 = off)
    pub taper_in: f32,
    pub taper_out: f32,
    /// the taper never takes more than this % of the stroke (Clip Studio's ratio); 45 if unset
    pub taper_ratio: Option<f32>,
    /// slow strokes get a shorter taper (Clip Studio's "by speed")
    pub taper_by_speed: bool,
    pub dual: Option<Dual>,
    /// many small particles per dab instead of one stamp
    pub spray: Option<Spray>,
    /// pen tilt → size (Clip Studio "tilt" on brush size)
    pub tilt_size: Option<TiltSize>,
    /// opacity at a flat pen, % (None = tilt doesn't change opacity)
    pub tilt_op: Option<f32>,
    /// a flatter pen squashes the tip, % strength (0 = off)
    pub tilt_round: f32,
    /// the tip turns with the pen's direction (azimuth)
    pub angle_tilt: bool,
    /// random colour changes, per stroke or per dab
    pub color_jitter: Option<ColorJitter>,
}

#[derive(Clone, Debug)]
pub struct Spray {
    /// particle size as % of the brush size
    pub particle: f32,
    pub count: u32,
    pub random_angle: bool,
}

#[derive(Clone, Debug)]
pub struct TiltSize {
    /// size % at the ends of the curve (min at curve 0, max at curve 1)
    pub min: f32,
    pub max: f32,
    pub curve: Vec<[f32; 2]>,
}

#[derive(Clone, Debug, Default)]
pub struct ColorJitter {
    /// hue: 100 = up to ±180°; sat/val in % points
    pub hue: f32,
    pub sat: f32,
    pub val: f32,
    pub per_stroke: bool,
}

pub fn rgb2hsv(c: [f32; 3]) -> [f32; 3] {
    let (r, g, b) = (c[0], c[1], c[2]);
    let mx = r.max(g).max(b);
    let mn = r.min(g).min(b);
    let d = mx - mn;
    let mut h = 0.0;
    if d > 0.0 {
        h = if mx == r { ((g - b) / d + 6.0) % 6.0 } else if mx == g { (b - r) / d + 2.0 } else { (r - g) / d + 4.0 };
    }
    [h * 60.0, if mx > 0.0 { d / mx } else { 0.0 }, mx]
}
pub fn hsv2rgb(h: f32, s: f32, v: f32) -> [f32; 3] {
    let f = |n: f32| {
        let k = (n + h / 60.0) % 6.0;
        v - v * s * k.min(4.0 - k).clamp(0.0, 1.0)
    };
    [f(5.0), f(3.0), f(1.0)]
}

impl Brush {
    /// The "Textured Acrylic 01 2" brush, as imported from Clip Studio into beleaf.
    pub fn textured_acrylic(size: f32) -> Brush {
        Brush {
            size,
            spacing: 16.666_666,
            flow: 100.0,
            angle: 0.0,
            follow: true,
            stab: 10.0,
            size_min: Some(95.0),
            op_min: Some(0.0),
            size_curve: Vec::new(),
            op_curve: Vec::new(),
            roundness: 100.0,
            jitter: Jitter::default(),
            taper_in: 60.0,
            taper_out: 60.0,
            taper_ratio: Some(30.0),
            taper_by_speed: true,
            dual: Some(Dual { size: 119.0, spacing: 37.158_46, spray: true, count: 10, area: 81.0, random_angle: true }),
            ..Brush::plain(size)
        }
    }

    /// A plain round brush with nothing switched on.
    pub fn plain(size: f32) -> Brush {
        Brush {
            size,
            spacing: 8.0,
            flow: 100.0,
            angle: 0.0,
            follow: false,
            stab: 0.0,
            size_min: None,
            op_min: None,
            size_curve: Vec::new(),
            op_curve: Vec::new(),
            roundness: 100.0,
            jitter: Jitter::default(),
            taper_in: 0.0,
            taper_out: 0.0,
            taper_ratio: None,
            taper_by_speed: false,
            dual: None,
            spray: None,
            tilt_size: None,
            tilt_op: None,
            tilt_round: 0.0,
            angle_tilt: false,
            color_jitter: None,
        }
    }
}

/// Random variation per dab, all in % (angle: 100 = up to ±180°).
#[derive(Clone, Debug, Default)]
pub struct Jitter {
    pub size: f32,
    pub opacity: f32,
    pub angle: f32,
    pub round: f32,
    pub scatter: f32,
}

/// beleaf's pressure curve: smooth (monotone-ish Catmull-Rom) through the points.
pub fn eval_curve(pts: &[[f32; 2]], x: f32) -> f32 {
    if pts.len() < 2 {
        return x.clamp(0.0, 1.0);
    }
    let x = x.clamp(0.0, 1.0);
    for i in 1..pts.len() {
        if x <= pts[i][0] {
            let p0 = pts[i.saturating_sub(2)];
            let p1 = pts[i - 1];
            let p2 = pts[i];
            let p3 = pts[(i + 1).min(pts.len() - 1)];
            let t = if p2[0] > p1[0] { (x - p1[0]) / (p2[0] - p1[0]) } else { 0.0 };
            let m1 = (p2[1] - p0[1]) / (p2[0] - p0[0]).max(1e-6) * (p2[0] - p1[0]);
            let m2 = (p3[1] - p1[1]) / (p3[0] - p1[0]).max(1e-6) * (p2[0] - p1[0]);
            let (t2, t3) = (t * t, t * t * t);
            let y = (2.0 * t3 - 3.0 * t2 + 1.0) * p1[1] + (t3 - 2.0 * t2 + t) * m1 + (-2.0 * t3 + 3.0 * t2) * p2[1] + (t3 - t2) * m2;
            return y.clamp(0.0, 1.0);
        }
    }
    pts[pts.len() - 1][1]
}

/// One stamp of a tip.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Dab {
    pub x: f32,
    pub y: f32,
    /// diameter in canvas px (the tip's longest side)
    pub size: f32,
    /// radians
    pub angle: f32,
    /// 0..1
    pub alpha: f32,
    /// true = a stamp of the dual tip (goes into the dual mask)
    pub dual: bool,
    /// distance along the stroke where it was placed
    pub dist: f32,
    /// start-taper factor that was applied when it was placed (so it can be corrected at the end)
    pub ti: f32,
    /// roundness 0..1 (squashes the tip across its height)
    pub round: f32,
    /// colour 0..1 (the stroke colour, or a jittered one)
    pub col: [f32; 3],
}

#[derive(Clone, Copy)]
struct Pt {
    x: f32,
    y: f32,
    p: f32,
    /// pen tilt 0 (flat) .. 1 (upright), if the pen reports it
    tilt: Option<f32>,
}

/// Small, fast, deterministic random numbers (so a stroke redrawn at the end looks the same).
#[derive(Clone)]
struct Rng(u32);
impl Rng {
    fn next(&mut self) -> f32 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.0 = x;
        (x >> 8) as f32 / (1u32 << 24) as f32
    }
}

pub struct Stroke {
    b: Brush,
    dist: f32,
    next_dab: f32,
    next_dual: f32,
    last: Option<Pt>,
    smooth: Option<Pt>,
    heading: Option<f32>,
    anchor: Option<(f32, f32)>,
    speed: Option<f32>,
    t_last: Option<f64>,
    pending: Option<Pt>,
    rng: Rng,
    dabs: Vec<Dab>,
    col: [f32; 3],
    /// last pen direction (azimuth, radians)
    az: Option<f32>,
}


impl Stroke {
    pub fn new(b: Brush, seed: u32) -> Stroke {
        Stroke {
            b,
            dist: 0.0,
            next_dab: 0.0,
            next_dual: 0.0,
            last: None,
            smooth: None,
            heading: None,
            anchor: None,
            speed: None,
            t_last: None,
            pending: None,
            rng: Rng(seed | 1),
            dabs: Vec::new(),
            col: [0.0; 3],
            az: None,
        }
    }

    /// Sets the stroke colour (0..1). Call before the first `add`; per-stroke colour jitter is applied here.
    pub fn set_color(&mut self, c: [f32; 3]) {
        self.col = c;
        if self.b.color_jitter.as_ref().is_some_and(|j| j.per_stroke) {
            self.col = self.jitter_color(c);
        }
    }
    fn jitter_color(&mut self, c: [f32; 3]) -> [f32; 3] {
        let Some(j) = self.b.color_jitter.clone() else { return c };
        if j.hue == 0.0 && j.sat == 0.0 && j.val == 0.0 {
            return c;
        }
        let [h, s, v] = rgb2hsv(c);
        let h = (h + (self.rng.next() * 2.0 - 1.0) * j.hue * 1.8 + 360.0).rem_euclid(360.0);
        let s = (s + (self.rng.next() * 2.0 - 1.0) * j.sat / 100.0).clamp(0.0, 1.0);
        let v = (v + (self.rng.next() * 2.0 - 1.0) * j.val / 100.0).clamp(0.0, 1.0);
        hsv2rgb(h, s, v)
    }

    /// Size multiplier from pressure (and tilt).
    fn size_factor(&self, p: f32, tilt: Option<f32>) -> f32 {
        let mut f = match self.b.size_min {
            Some(m) => (m + (100.0 - m) * eval_curve(&self.b.size_curve, p)) / 100.0,
            None => 1.0,
        };
        if let (Some(ts), Some(t)) = (&self.b.tilt_size, tilt) {
            f *= (ts.min + (ts.max - ts.min) * eval_curve(&ts.curve, t)) / 100.0;
        }
        f
    }
    /// Opacity multiplier from pressure (and tilt).
    fn op_factor(&self, p: f32, tilt: Option<f32>) -> f32 {
        let mut f = match self.b.op_min {
            Some(m) => (m + (100.0 - m) * eval_curve(&self.b.op_curve, p)) / 100.0,
            None => 1.0,
        };
        if let (Some(m), Some(t)) = (self.b.tilt_op, tilt) {
            f *= (m + (100.0 - m) * t) / 100.0;
        }
        f
    }
    /// A taper length, shortened for slow strokes when the brush asks for it.
    fn taper_len(&self, len: f32) -> f32 {
        if len <= 0.0 {
            return 0.0;
        }
        if self.b.taper_by_speed {
            let s = self.speed.unwrap_or(0.3);
            len * (s / 1.1).clamp(0.08, 1.0)
        } else {
            len
        }
    }
    fn taper_in_at(&self, dist: f32) -> f32 {
        let l = self.taper_len(self.b.taper_in);
        if l <= 0.0 || dist >= l {
            1.0
        } else {
            0.15 + 0.85 * (dist / l).sqrt()
        }
    }

    fn emit(&mut self, pt: Pt, out: &mut Vec<Dab>) {
        if self.b.follow && self.heading.is_none() {
            self.pending = Some(pt); // wait until we know which way the stroke goes
            return;
        }
        let ti = self.taper_in_at(self.dist);
        let size = self.b.size * self.size_factor(pt.p, pt.tilt) * ti;
        let alpha = (self.b.flow / 100.0) * self.op_factor(pt.p, pt.tilt);
        let mut ang = self.b.angle;
        if self.b.follow {
            ang += self.heading.unwrap_or(0.0);
        }
        if self.b.angle_tilt {
            if let Some(az) = self.az {
                ang += az * 180.0 / PI - 90.0;
            }
        }
        let mut round = self.b.roundness / 100.0;
        if let Some(t) = pt.tilt {
            if self.b.tilt_round > 0.0 {
                round *= 1.0 - self.b.tilt_round / 100.0 * (1.0 - t) * 0.85;
            }
        }
        let col = if self.b.color_jitter.as_ref().is_some_and(|j| !j.per_stroke) { self.jitter_color(self.col) } else { self.col };
        let j = self.b.jitter.clone();
        let spray = self.b.spray.clone();
        let n = spray.as_ref().map_or(1, |s| s.count.max(1));
        for _ in 0..n {
            let (mut s, mut v, mut a, mut r, mut x, mut y) = (size, alpha, ang, round, pt.x, pt.y);
            if j.size > 0.0 { s *= 1.0 - self.rng.next() * j.size / 100.0; }
            if j.opacity > 0.0 { v *= 1.0 - self.rng.next() * j.opacity / 100.0; }
            if j.angle > 0.0 { a += (self.rng.next() * 2.0 - 1.0) * j.angle * 1.8; }
            if j.round > 0.0 { r *= 1.0 - self.rng.next() * j.round / 100.0; }
            if let Some(sp) = &spray {
                // particles land anywhere inside the brush circle
                let ra = self.rng.next().sqrt() * size / 2.0;
                let th = self.rng.next() * 2.0 * PI;
                x += th.cos() * ra;
                y += th.sin() * ra;
                s = size * sp.particle / 100.0 * if j.size > 0.0 { 1.0 - self.rng.next() * j.size / 100.0 } else { 1.0 };
                if sp.random_angle {
                    a = self.rng.next() * 360.0;
                }
            } else if j.scatter > 0.0 {
                let ra = (self.rng.next() * 2.0 - 1.0) * j.scatter / 100.0 * size;
                let th = self.heading.unwrap_or(0.0) * PI / 180.0 + PI / 2.0;
                x += th.cos() * ra;
                y += th.sin() * ra;
            }
            let d = Dab { x, y, size: s, angle: a * PI / 180.0, alpha: v, dual: false, dist: self.dist, ti, round: r, col };
            self.dabs.push(d);
            out.push(d);
        }
    }

    fn emit_dual(&mut self, pt: Pt, out: &mut Vec<Dab>) {
        let Some(dl) = self.b.dual.clone() else { return };
        let main = self.b.size * self.size_factor(pt.p, pt.tilt);
        let base = main * dl.size / 100.0;
        let n = if dl.spray { dl.count.max(1) } else { 1 };
        for _ in 0..n {
            let (mut x, mut y) = (pt.x, pt.y);
            let a = if dl.random_angle { self.rng.next() * 2.0 * PI } else { 0.0 };
            if dl.spray {
                let r = self.rng.next().sqrt() * main / 2.0 * (dl.area / 100.0);
                let th = self.rng.next() * 2.0 * PI;
                x += th.cos() * r;
                y += th.sin() * r;
            }
            let d = Dab { x, y, size: base, angle: a, alpha: 1.0, dual: true, dist: self.dist, ti: 1.0, round: 1.0, col: self.col };
            self.dabs.push(d);
            out.push(d);
        }
    }

    /// Feed one pen sample (canvas px, pressure 0..1, time in ms). Returns the new dabs to draw.
    pub fn add(&mut self, x: f32, y: f32, p: f32, t_ms: f64) -> Vec<Dab> {
        self.add_pen(x, y, p, None, None, t_ms)
    }

    /// Like `add`, with the pen's tilt (0 flat .. 1 upright) and azimuth (radians) when it reports them.
    pub fn add_pen(&mut self, x: f32, y: f32, p: f32, tilt: Option<f32>, az: Option<f32>, t_ms: f64) -> Vec<Dab> {
        let mut out = Vec::new();
        if az.is_some() {
            self.az = az;
        }
        // stabiliser: follow the pen with a little lag
        let k = if self.b.stab > 0.0 { (self.b.stab / 25.0).powf(0.8).min(0.94) } else { 0.0 };
        let cur = match self.smooth {
            None => Pt { x, y, p, tilt },
            Some(s) => Pt { x: s.x * k + x * (1.0 - k), y: s.y * k + y * (1.0 - k), p: s.p * k * 0.6 + p * (1.0 - k * 0.6), tilt },
        };
        self.smooth = Some(cur);

        let Some(last) = self.last else {
            self.last = Some(cur);
            self.t_last = Some(t_ms);
            self.emit(cur, &mut out);
            self.emit_dual(cur, &mut out);
            let s0 = (self.b.size * self.size_factor(cur.p, tilt)).max(0.5);
            self.next_dab = (s0 * (self.b.spacing / 100.0).max(0.01)).max(0.3);
            if let Some(dl) = &self.b.dual {
                self.next_dual = (s0 * dl.size / 100.0 * (dl.spacing / 100.0).max(0.01)).max(0.3);
            }
            return out;
        };
        let (dx, dy) = (cur.x - last.x, cur.y - last.y);
        let d = (dx * dx + dy * dy).sqrt();
        if d < 0.01 {
            return out;
        }
        // drawing speed in px/ms, smoothed (for "taper by speed")
        if let Some(tl) = self.t_last {
            let inst = d / ((t_ms - tl).max(1.0) as f32);
            self.speed = Some(match self.speed { None => inst, Some(s) => s * 0.85 + inst * 0.15 });
        }
        self.t_last = Some(t_ms);
        // direction from the movement since the last check, so tiny slow circles still get one
        let (ax, ay) = *self.anchor.get_or_insert((last.x, last.y));
        let (hx, hy) = (cur.x - ax, cur.y - ay);
        if (hx * hx + hy * hy).sqrt() > 1.2 {
            let hd = hy.atan2(hx) * 180.0 / PI;
            self.heading = Some(match self.heading {
                None => hd,
                Some(h) => h + (((hd - h + 540.0) % 360.0) - 180.0) * 0.35,
            });
            self.anchor = Some((cur.x, cur.y));
        }
        if self.heading.is_some() {
            if let Some(q) = self.pending.take() {
                self.emit(q, &mut out);
            }
        }
        // main tip: one dab every `spacing` along the path
        let mut t = 0.0;
        loop {
            let pp = last.p + (cur.p - last.p) * t / d;
            let size = (self.b.size * self.size_factor(pp, tilt)).max(0.5);
            let step = (size * (self.b.spacing / 100.0).max(0.01)).max(0.3);
            let need = self.next_dab - self.dist;
            if t + need > d {
                break;
            }
            t += need;
            self.dist += need;
            self.next_dab = self.dist + step;
            let f = t / d;
            self.emit(Pt { x: last.x + dx * f, y: last.y + dy * f, p: last.p + (cur.p - last.p) * f, tilt }, &mut out);
        }
        // dual tip on its own spacing
        if let Some(dl) = self.b.dual.clone() {
            let base = self.dist - t;
            loop {
                let size = (self.b.size * self.size_factor(cur.p, tilt) * dl.size / 100.0).max(0.5);
                let step = (size * (dl.spacing / 100.0).max(0.01)).max(0.3);
                let at = self.next_dual - base;
                if at > d {
                    break;
                }
                let td = at.max(0.0);
                self.next_dual = base + td + step;
                let f = td / d;
                self.emit_dual(Pt { x: last.x + dx * f, y: last.y + dy * f, p: last.p + (cur.p - last.p) * f, tilt }, &mut out);
            }
        }
        self.dist = self.dist - t + d;
        self.last = Some(cur);
        out
    }

    /// The pen lifted: returns *every* dab of the stroke with the final taper applied, so the
    /// renderer can redraw the stroke once (start taper fixed for short strokes, end taper added).
    pub fn finish(&mut self) -> Vec<Dab> {
        if let Some(q) = self.pending.take() {
            if self.heading.is_none() {
                self.heading = Some(0.0);
            }
            let mut tmp = Vec::new();
            self.emit(q, &mut tmp);
        }
        let total = self.dist;
        let cap = total * self.b.taper_ratio.unwrap_or(45.0) / 100.0;
        let out = if self.b.taper_out > 0.0 { self.taper_len(self.b.taper_out).min(cap) } else { 0.0 };
        let in_fix = if self.b.taper_in > 0.0 { self.taper_len(self.b.taper_in).min(cap) } else { 0.0 };
        self.dabs
            .iter()
            .map(|d| {
                if d.dual {
                    return *d;
                }
                let rem = total - d.dist;
                let mut f = if out > 0.0 && rem < out { 0.1 + 0.9 * (rem / out).max(0.0).sqrt() } else { 1.0 };
                if in_fix > 0.0 && d.ti < 1.0 {
                    let want = if d.dist >= in_fix { 1.0 } else { 0.15 + 0.85 * (d.dist / in_fix).sqrt() };
                    f *= want / d.ti;
                }
                Dab { size: d.size * f, ..*d }
            })
            .collect()
    }

    /// The stroke colour after per-stroke jitter.
    pub fn color(&self) -> [f32; 3] {
        self.col
    }

    pub fn length(&self) -> f32 {
        self.dist
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn circle(b: Brush, r: f32, loops: f32, pressure: f32, ms_per_step: f64) -> (Vec<Dab>, Vec<Dab>) {
        let mut s = Stroke::new(b, 7);
        let mut live = Vec::new();
        let steps = (loops * 48.0) as usize;
        for i in 0..=steps {
            let a = i as f32 / 48.0 * 2.0 * PI;
            live.extend(s.add(100.0 + r * a.cos(), 100.0 + r * a.sin(), pressure, i as f64 * ms_per_step));
        }
        let fin = s.finish();
        (live, fin)
    }

    #[test]
    fn tiny_slow_circles_paint_straight_away() {
        // the bug from the JS engine: a 3 px circle drawn slowly with a direction-following brush painted nothing
        let (live, fin) = circle(Brush::textured_acrylic(24.0), 3.0, 1.0, 0.4, 16.0);
        let main_live = live.iter().filter(|d| !d.dual).count();
        assert!(main_live >= 3, "only {main_live} main dabs while drawing");
        let big = fin.iter().filter(|d| !d.dual && d.size > 24.0 * 0.5).count();
        assert!(big >= 3, "after lifting, dabs are still tiny: {big}");
    }

    #[test]
    fn long_fast_strokes_taper_both_ends() {
        let mut s = Stroke::new(Brush::textured_acrylic(24.0), 3);
        for i in 0..=200 {
            s.add(i as f32 * 3.0, 50.0, 0.8, i as f64 * 1.0); // 3 px/ms: fast
        }
        let fin: Vec<Dab> = s.finish().into_iter().filter(|d| !d.dual).collect();
        let first = fin.first().unwrap().size;
        let mid = fin[fin.len() / 2].size;
        let last = fin.last().unwrap().size;
        assert!(first < mid * 0.5, "start {first} vs middle {mid}");
        assert!(last < mid * 0.5, "end {last} vs middle {mid}");
    }

    #[test]
    fn slow_strokes_keep_their_ends_thick() {
        let mut s = Stroke::new(Brush::textured_acrylic(24.0), 3);
        for i in 0..=200 {
            s.add(i as f32 * 1.0, 50.0, 0.8, i as f64 * 20.0); // 0.05 px/ms: slow
        }
        let fin: Vec<Dab> = s.finish().into_iter().filter(|d| !d.dual).collect();
        let mid = fin[fin.len() / 2].size;
        let last = fin.last().unwrap().size;
        assert!(last > mid * 0.3, "slow stroke end tapered too hard: {last} vs {mid}");
    }

    #[test]
    fn pressure_controls_opacity() {
        let (_, soft) = circle(Brush::textured_acrylic(24.0), 30.0, 1.0, 0.2, 4.0);
        let (_, hard) = circle(Brush::textured_acrylic(24.0), 30.0, 1.0, 0.9, 4.0);
        let avg = |v: &Vec<Dab>| { let m: Vec<&Dab> = v.iter().filter(|d| !d.dual).collect(); m.iter().map(|d| d.alpha).sum::<f32>() / m.len() as f32 };
        assert!(avg(&hard) > avg(&soft) * 2.0);
    }

    #[test]
    fn curves_match_beleaf() {
        assert!((eval_curve(&[], 0.3) - 0.3).abs() < 1e-6);
        assert!((eval_curve(&[[0.0, 0.0], [1.0, 1.0]], 0.5) - 0.5).abs() < 1e-3);
        let soft = eval_curve(&[[0.0, 0.0], [0.35, 0.15], [1.0, 1.0]], 0.35);
        assert!((soft - 0.15).abs() < 1e-3, "{soft}");
    }

    #[test]
    fn jitter_varies_size() {
        let mut b = Brush::textured_acrylic(24.0);
        b.jitter.size = 60.0;
        b.dual = None;
        let (_, fin) = circle(b, 40.0, 1.0, 0.9, 4.0);
        let sizes: Vec<f32> = fin.iter().filter(|d| d.dist > 80.0 && d.dist < 160.0).map(|d| d.size).collect();
        let (mn, mx) = sizes.iter().fold((f32::MAX, 0.0f32), |(a, b), &s| (a.min(s), b.max(s)));
        assert!(mx > mn * 1.3, "{mn} {mx}");
    }

    #[test]
    fn dual_tip_sprays_around_the_path() {
        let (live, _) = circle(Brush::textured_acrylic(24.0), 40.0, 1.0, 0.7, 4.0);
        let dual = live.iter().filter(|d| d.dual).count();
        assert!(dual > 20, "{dual} dual stamps");
    }

    fn line(b: Brush, tilt: Option<f32>, az: Option<f32>) -> Vec<Dab> {
        let mut s = Stroke::new(b, 11);
        s.set_color([1.0, 0.0, 0.0]);
        for i in 0..=60 {
            s.add_pen(i as f32 * 3.0, 50.0, 1.0, tilt, az, i as f64 * 8.0);
        }
        s.finish()
    }

    #[test]
    fn spray_makes_small_particles_inside_the_brush() {
        let mut b = Brush::plain(40.0);
        b.spray = Some(Spray { particle: 10.0, count: 6, random_angle: true });
        let d = line(b, None, None);
        assert!(d.len() > 6 * 20);
        assert!(d.iter().all(|d| d.size <= 4.01 && (d.y - 50.0).abs() <= 20.01));
    }

    #[test]
    fn tilt_changes_size_opacity_and_roundness() {
        let mut b = Brush::plain(20.0);
        b.tilt_size = Some(TiltSize { min: 100.0, max: 200.0, curve: vec![[0.0, 1.0], [1.0, 0.0]] });
        b.tilt_op = Some(30.0);
        b.tilt_round = 100.0;
        let flat = line(b.clone(), Some(0.0), None);
        let up = line(b, Some(1.0), None);
        assert!((flat[3].size - 40.0).abs() < 0.5 && (up[3].size - 20.0).abs() < 0.5);
        assert!((flat[3].alpha - 0.3).abs() < 0.01 && (up[3].alpha - 1.0).abs() < 0.01);
        assert!((flat[3].round - 0.15).abs() < 0.01 && (up[3].round - 1.0).abs() < 0.01);
    }

    #[test]
    fn angle_follows_the_pen_direction() {
        let mut b = Brush::plain(20.0);
        b.angle_tilt = true;
        let d = line(b, Some(0.5), Some(PI));
        assert!((d[2].angle - PI / 2.0).abs() < 0.01);
    }

    #[test]
    fn colour_jitter_per_dab_and_per_stroke() {
        let mut b = Brush::plain(20.0);
        b.color_jitter = Some(ColorJitter { hue: 20.0, sat: 0.0, val: 0.0, per_stroke: false });
        let d = line(b.clone(), None, None);
        let differ = d.windows(2).filter(|w| w[0].col != w[1].col).count();
        assert!(differ > d.len() / 2);
        b.color_jitter.as_mut().unwrap().per_stroke = true;
        let d = line(b, None, None);
        assert!(d.windows(2).all(|w| w[0].col == w[1].col) && d[0].col != [1.0, 0.0, 0.0]);
    }

    #[test]
    fn hsv_round_trips() {
        for c in [[1.0, 0.0, 0.0], [0.2, 0.6, 0.9], [0.5, 0.5, 0.5]] {
            let [h, s, v] = rgb2hsv(c);
            let back = hsv2rgb(h, s, v);
            for i in 0..3 { assert!((back[i] - c[i]).abs() < 1e-4, "{c:?} -> {back:?}"); }
        }
    }
}
