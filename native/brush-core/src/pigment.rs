//! Paint-like colour mixing (Kubelka–Munk), a port of beleaf's `PIG` (pigment.js).
//!
//! Each colour becomes a light-reflection curve across the rainbow (16 bands, 400–700 nm). Mixing
//! paints averages how much each band gets absorbed, so yellow + blue reflect only the green band
//! they share, like real paint. Colours here are sRGB 0..1.

use std::sync::OnceLock;

pub const N: usize = 16;

struct Tables {
    bar: [[f64; 3]; N],
    base: [[f64; N]; 7], // W C M Y R G B
}

fn g(l: f64, m: f64, s1: f64, s2: f64) -> f64 {
    let t = (l - m) / if l < m { s1 } else { s2 };
    (-0.5 * t * t).exp()
}

fn recon(bar: &[[f64; 3]; N], r: &[f64; N]) -> [f64; 3] {
    let mut o = [0.0; 3];
    for k in 0..N {
        for c in 0..3 {
            o[c] += r[k] * bar[k][c];
        }
    }
    o
}

fn solve3(a: [[f64; 3]; 3], b: [f64; 3]) -> [f64; 3] {
    let [[a0, b0, c0], [d, e, f], [g2, h, i]] = a;
    let mut det = a0 * (e * i - f * h) - b0 * (d * i - f * g2) + c0 * (d * h - e * g2);
    if det == 0.0 {
        det = 1e-12;
    }
    [
        (b[0] * (e * i - f * h) - b0 * (b[1] * i - f * b[2]) + c0 * (b[1] * h - e * b[2])) / det,
        (a0 * (b[1] * i - f * b[2]) - b[0] * (d * i - f * g2) + c0 * (d * b[2] - b[1] * g2)) / det,
        (a0 * (e * b[2] - b[1] * h) - b0 * (d * b[2] - b[1] * g2) + b[0] * (d * h - e * g2)) / det,
    ]
}

/// Find a smooth reflectance curve (sigmoid of a parabola) whose colour matches the target.
fn fit(bar: &[[f64; 3]; N], ln: &[f64; N], t: [f64; 3]) -> [f64; N] {
    let t = t.map(|v| v.clamp(0.003, 0.997));
    let mut c = [0.0f64; 3];
    let mut r = [0.0f64; N];
    for _ in 0..40 {
        for k in 0..N {
            r[k] = 1.0 / (1.0 + (-(c[0] + c[1] * ln[k] + c[2] * ln[k] * ln[k])).exp());
        }
        let p = recon(bar, &r);
        let e = [t[0] - p[0], t[1] - p[1], t[2] - p[2]];
        if e[0].abs() + e[1].abs() + e[2].abs() < 1e-5 {
            break;
        }
        let mut jac = [[0.0f64; 3]; 3];
        for ch in 0..3 {
            for j in 0..3 {
                let mut s = 0.0;
                for k in 0..N {
                    let basis = match j {
                        0 => 1.0,
                        1 => ln[k],
                        _ => ln[k] * ln[k],
                    };
                    s += r[k] * (1.0 - r[k]) * basis * bar[k][ch];
                }
                jac[ch][j] = s;
            }
        }
        let mut a = [[0.0f64; 3]; 3];
        let mut bv = [0.0f64; 3];
        for i in 0..3 {
            for j in 0..3 {
                for ch in 0..3 {
                    a[i][j] += jac[ch][i] * jac[ch][j];
                }
            }
            a[i][i] += 1e-4 + a[i][i] * 0.02;
            for ch in 0..3 {
                bv[i] += jac[ch][i] * e[ch];
            }
        }
        let d = solve3(a, bv);
        for j in 0..3 {
            c[j] = (c[j] + d[j].clamp(-6.0, 6.0)).clamp(-60.0, 60.0);
        }
    }
    r
}

fn tables() -> &'static Tables {
    static T: OnceLock<Tables> = OnceLock::new();
    T.get_or_init(|| {
        let mut bar = [[0.0f64; 3]; N];
        let mut ln = [0.0f64; N];
        let m = [[3.2406, -1.5372, -0.4986], [-0.9689, 1.8758, 0.0415], [0.0557, -0.204, 1.057]];
        for k in 0..N {
            let l = 400.0 + k as f64 * 20.0;
            ln[k] = (l - 550.0) / 150.0;
            // CIE 1931 colour-matching functions, multi-lobe fit (Wyman, Sloan & Shirley 2013)
            let x = 1.056 * g(l, 599.8, 37.9, 31.0) + 0.362 * g(l, 442.0, 16.0, 26.7) - 0.065 * g(l, 501.1, 20.4, 26.2);
            let y = 0.821 * g(l, 568.8, 46.9, 40.5) + 0.286 * g(l, 530.9, 16.3, 31.1);
            let z = 1.217 * g(l, 437.0, 11.8, 36.0) + 0.681 * g(l, 459.0, 26.0, 13.8);
            for c in 0..3 {
                bar[k][c] = m[c][0] * x + m[c][1] * y + m[c][2] * z;
            }
        }
        // a perfect white reflector comes out as white
        for c in 0..3 {
            let s: f64 = (0..N).map(|k| bar[k][c]).sum();
            for k in 0..N {
                bar[k][c] /= s;
            }
        }
        let base = [
            [1.0; N],
            fit(&bar, &ln, [0.0, 1.0, 1.0]),
            fit(&bar, &ln, [1.0, 0.0, 1.0]),
            fit(&bar, &ln, [1.0, 1.0, 0.0]),
            fit(&bar, &ln, [1.0, 0.0, 0.0]),
            fit(&bar, &ln, [0.0, 1.0, 0.0]),
            fit(&bar, &ln, [0.0, 0.0, 1.0]),
        ];
        Tables { bar, base }
    })
}

fn to_lin(v: f32) -> f64 {
    let v = v as f64;
    if v <= 0.04045 {
        v / 12.92
    } else {
        ((v + 0.055) / 1.055).powf(2.4)
    }
}
fn to_s(v: f64) -> f32 {
    let v = v.clamp(0.0, 1.0);
    (if v <= 0.0031308 { v * 12.92 } else { 1.055 * v.powf(1.0 / 2.4) - 0.055 }) as f32
}

/// Seven base paints (white, cyan, magenta, yellow, red, green, blue); any colour is a blend of a
/// few of them, so similar colours always get similar curves (after Smits 1999).
fn spectrum(t: &Tables, r: f64, g: f64, b: f64) -> [f64; N] {
    let mut s = [0.0; N];
    let mut add = |k: usize, w: f64| {
        if w > 0.0 {
            for i in 0..N {
                s[i] += w * t.base[k][i];
            }
        }
    };
    const W: usize = 0;
    const C: usize = 1;
    const M: usize = 2;
    const Y: usize = 3;
    const R: usize = 4;
    const G: usize = 5;
    const B: usize = 6;
    if r <= g && r <= b {
        add(W, r);
        if g <= b {
            add(C, g - r);
            add(B, b - g);
        } else {
            add(C, b - r);
            add(G, g - b);
        }
    } else if g <= r && g <= b {
        add(W, g);
        if r <= b {
            add(M, r - g);
            add(B, b - r);
        } else {
            add(M, b - g);
            add(R, r - b);
        }
    } else {
        add(W, b);
        if r <= g {
            add(Y, r - b);
            add(G, g - r);
        } else {
            add(Y, g - b);
            add(R, r - g);
        }
    }
    s
}

/// A colour prepared for mixing: its absorption curve, the small error the curve leaves, and its brightness.
#[derive(Clone, Copy)]
pub struct Prepared {
    ks: [f64; N],
    err: [f64; 3],
    y: f64,
}

pub fn prep(rgb: [f32; 3]) -> Prepared {
    let t = tables();
    let lin = [to_lin(rgb[0]), to_lin(rgb[1]), to_lin(rgb[2])];
    let s = spectrum(t, lin[0], lin[1], lin[2]);
    let mut ks = [0.0; N];
    let mut r = [0.0; N];
    for k in 0..N {
        let rr = s[k].clamp(0.02, 0.999);
        ks[k] = (1.0 - rr) * (1.0 - rr) / (2.0 * rr);
        r[k] = 1.0 + ks[k] - (ks[k] * ks[k] + 2.0 * ks[k]).sqrt();
    }
    let rc = recon(&t.bar, &r);
    Prepared { ks, err: [lin[0] - rc[0], lin[1] - rc[1], lin[2] - rc[2]], y: 0.2126 * lin[0] + 0.7152 * lin[1] + 0.0722 * lin[2] }
}

/// Mix two prepared colours, `t` = share of the second (0..1). Unmixed colours come back exactly.
pub fn mix_p(a: &Prepared, b: &Prepared, t: f32) -> [f32; 3] {
    let tb = tables();
    let t = t as f64;
    // lighter paints tint less strongly per drop, darker ones more: weight by share² × brightness
    let fa = (1.0 - t) * (1.0 - t) * (a.y + 0.02);
    let fb = t * t * (b.y + 0.02);
    let u = if fa + fb != 0.0 { fb / (fa + fb) } else { t };
    let mut o = [0.0f64; 3];
    for k in 0..N {
        let q = a.ks[k] * (1.0 - u) + b.ks[k] * u;
        let r = 1.0 + q - (q * q + 2.0 * q).sqrt();
        for c in 0..3 {
            o[c] += r * tb.bar[k][c];
        }
    }
    [0, 1, 2].map(|c| to_s(o[c] + a.err[c] * (1.0 - u) + b.err[c] * u))
}

pub fn mix(a: [f32; 3], b: [f32; 3], t: f32) -> [f32; 3] {
    mix_p(&prep(a), &prep(b), t)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn yellow_and_blue_make_green() {
        let g = mix([1.0, 0.9, 0.0], [0.0, 0.2, 0.9], 0.5);
        assert!(g[1] > g[0] && g[1] > g[2], "{g:?}");
    }

    #[test]
    fn unmixed_colours_come_back() {
        for c in [[0.8, 0.3, 0.1], [0.1, 0.5, 0.9], [1.0, 1.0, 1.0]] {
            for (t, want) in [(0.0, c), (1.0, c)] {
                let m = mix(c, c, t);
                for i in 0..3 {
                    assert!((m[i] - want[i]).abs() < 0.01, "{c:?} {t} {m:?}");
                }
            }
            let m = mix(c, [0.0, 0.0, 0.0], 0.0);
            for i in 0..3 {
                assert!((m[i] - c[i]).abs() < 0.01, "{c:?} {m:?}");
            }
        }
    }
}
