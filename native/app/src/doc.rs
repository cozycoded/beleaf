//! .beleaf project files, the same format the web version saves: "BLFPROJ1", one byte (1 = deflated),
//! then JSON { app, kind: "project", v, rec, blobs } where each layer's pixels are a PNG in base64url.
//! Layers in rec.layers go bottom to top; folders are kept flat here for now.

use serde_json::{json, Value};

pub struct LayerData {
    pub name: String,
    pub visible: bool,
    pub opacity: f32,
    /// straight RGBA, w*h*4 (empty = transparent)
    pub rgba: Vec<u8>,
}
pub struct DocData {
    pub name: String,
    pub w: u32,
    pub h: u32,
    pub paper: [f32; 3],
    pub active: usize,
    pub layers: Vec<LayerData>,
}

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
fn b64u(data: &[u8]) -> String {
    let mut s = String::with_capacity(data.len() * 4 / 3 + 4);
    for c in data.chunks(3) {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        s.push(B64[(n >> 18 & 63) as usize] as char);
        s.push(B64[(n >> 12 & 63) as usize] as char);
        if c.len() > 1 {
            s.push(B64[(n >> 6 & 63) as usize] as char);
        }
        if c.len() > 2 {
            s.push(B64[(n & 63) as usize] as char);
        }
    }
    s
}
fn unb64(s: &str) -> Vec<u8> {
    let mut map = [255u8; 256];
    for (i, c) in B64.iter().enumerate() {
        map[*c as usize] = i as u8;
    }
    map[b'+' as usize] = 62;
    map[b'/' as usize] = 63;
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let (mut acc, mut bits) = (0u32, 0u32);
    for b in s.bytes() {
        let v = map[b as usize];
        if v == 255 {
            continue;
        }
        acc = acc << 6 | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits & 255) as u8);
        }
    }
    out
}

pub fn png_encode(w: u32, h: u32, rgba: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    {
        let mut e = png::Encoder::new(&mut out, w, h);
        e.set_color(png::ColorType::Rgba);
        e.set_depth(png::BitDepth::Eight);
        e.set_compression(png::Compression::Fast);
        if let Ok(mut wr) = e.write_header() {
            let _ = wr.write_image_data(rgba);
        }
    }
    out
}
fn png_decode(bytes: &[u8], w: u32, h: u32) -> Option<Vec<u8>> {
    let mut dec = png::Decoder::new(bytes);
    dec.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut r = dec.read_info().ok()?;
    let mut buf = vec![0u8; r.output_buffer_size()];
    let info = r.next_frame(&mut buf).ok()?;
    let (pw, ph) = (info.width, info.height);
    let ch = match info.color_type {
        png::ColorType::Rgba => 4,
        png::ColorType::Rgb => 3,
        png::ColorType::GrayscaleAlpha => 2,
        _ => 1,
    };
    let mut out = vec![0u8; (w * h * 4) as usize];
    for y in 0..ph.min(h) {
        for x in 0..pw.min(w) {
            let s = ((y * pw + x) * ch) as usize;
            let d = ((y * w + x) * 4) as usize;
            let px = match ch {
                4 => [buf[s], buf[s + 1], buf[s + 2], buf[s + 3]],
                3 => [buf[s], buf[s + 1], buf[s + 2], 255],
                2 => [buf[s], buf[s], buf[s], buf[s + 1]],
                _ => [buf[s], buf[s], buf[s], 255],
            };
            out[d..d + 4].copy_from_slice(&px);
        }
    }
    Some(out)
}

fn hex(c: [f32; 3]) -> String {
    format!("#{:02X}{:02X}{:02X}", (c[0] * 255.0).round() as u8, (c[1] * 255.0).round() as u8, (c[2] * 255.0).round() as u8)
}
fn unhex(s: &str) -> Option<[f32; 3]> {
    let s = s.trim_start_matches('#');
    if s.len() < 6 {
        return None;
    }
    let p = |i: usize| u8::from_str_radix(&s[i..i + 2], 16).ok().map(|v| v as f32 / 255.0);
    Some([p(0)?, p(2)?, p(4)?])
}

fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

pub fn save(d: &DocData) -> Vec<u8> {
    let mut blobs = serde_json::Map::new();
    let mut layers = Vec::new();
    for (i, l) in d.layers.iter().enumerate() {
        let empty = l.rgba.is_empty() || l.rgba.chunks_exact(4).all(|p| p[3] == 0);
        let key = format!("f{i}");
        if !empty {
            blobs.insert(key.clone(), Value::String(b64u(&png_encode(d.w, d.h, &l.rgba))));
        }
        layers.push(json!({
            "id": i + 1, "name": l.name, "kind": "raster", "visible": l.visible, "opacity": l.opacity, "blend": "source-over",
            "parent": Value::Null, "color": Value::Null, "open": true, "paper": false, "clip": false, "lockAlpha": false, "locked": false, "ref": false, "draft": false,
            "s": if empty { Value::Null } else { Value::String(key) }
        }));
    }
    let n = d.layers.len();
    let rec = json!({
        "pid": format!("p{:x}", now_ms()), "name": d.name, "w": d.w, "h": d.h, "sheet": Value::Null,
        "paper": { "on": true, "col": hex(d.paper) },
        "anim": { "n": 1, "cur": 0, "holds": [1], "fps": 12, "onion": true, "loop": true },
        "active": d.active.min(n.saturating_sub(1)), "layers": layers, "counters": [n + 1, 1, 1], "updated": now_ms(), "thumb": Value::Null, "v": 1
    });
    let body = json!({ "app": "beLEAF", "kind": "project", "v": 1, "rec": rec, "blobs": blobs });
    let raw = serde_json::to_vec(&body).unwrap_or_default();
    let z = miniz_oxide::deflate::compress_to_vec(&raw, 6);
    let mut out = b"BLFPROJ1".to_vec();
    out.push(1);
    out.extend_from_slice(&z);
    out
}

pub fn load(bytes: &[u8], max_side: u32) -> Result<DocData, String> {
    if bytes.len() < 10 || &bytes[0..8] != b"BLFPROJ1" {
        return Err("This isn’t a beleaf project file".into());
    }
    let raw = if bytes[8] == 1 { miniz_oxide::inflate::decompress_to_vec(&bytes[9..]).map_err(|_| "The file is damaged".to_string())? } else { bytes[9..].to_vec() };
    let o: Value = serde_json::from_slice(&raw).map_err(|_| "The file is damaged".to_string())?;
    let rec = &o["rec"];
    let w = rec["w"].as_u64().unwrap_or(1024) as u32;
    let h = rec["h"].as_u64().unwrap_or(1024) as u32;
    if w > max_side || h > max_side {
        return Err(format!("This canvas is {w} × {h}; the biggest this device can hold is {max_side} px a side"));
    }
    let blobs = &o["blobs"];
    let mut layers = Vec::new();
    for m in rec["layers"].as_array().cloned().unwrap_or_default() {
        let kind = m["kind"].as_str().unwrap_or("raster");
        if kind == "folder" {
            continue;
        }
        let key = if kind == "anim" { m["cels"].as_array().and_then(|c| c.first()).and_then(|v| v.as_str()).map(|s| s.to_string()) } else { m["s"].as_str().map(|s| s.to_string()) };
        let rgba = key.and_then(|k| blobs[&k].as_str().map(unb64)).and_then(|png| png_decode(&png, w, h)).unwrap_or_default();
        layers.push(LayerData { name: m["name"].as_str().unwrap_or("Layer").to_string(), visible: m["visible"].as_bool().unwrap_or(true), opacity: m["opacity"].as_f64().unwrap_or(1.0) as f32, rgba });
    }
    if layers.is_empty() {
        layers.push(LayerData { name: "Layer 1".into(), visible: true, opacity: 1.0, rgba: Vec::new() });
    }
    let paper = rec["paper"]["col"].as_str().and_then(unhex).unwrap_or([1.0, 1.0, 1.0]);
    let active = rec["active"].as_u64().unwrap_or(0) as usize;
    Ok(DocData { name: rec["name"].as_str().unwrap_or("").to_string(), w, h, paper, active: active.min(layers.len() - 1), layers })
}

/// Flatten the layers over the paper into one straight-RGBA picture (for PNG export).
pub fn flatten(d: &DocData) -> Vec<u8> {
    let n = (d.w * d.h) as usize;
    let mut acc = vec![0f32; n * 4];
    for i in 0..n {
        acc[i * 4] = d.paper[0];
        acc[i * 4 + 1] = d.paper[1];
        acc[i * 4 + 2] = d.paper[2];
        acc[i * 4 + 3] = 1.0;
    }
    for l in &d.layers {
        if !l.visible || l.rgba.is_empty() {
            continue;
        }
        for i in 0..n {
            let a = l.rgba[i * 4 + 3] as f32 / 255.0 * l.opacity;
            if a <= 0.0 {
                continue;
            }
            for c in 0..3 {
                acc[i * 4 + c] = l.rgba[i * 4 + c] as f32 / 255.0 * a + acc[i * 4 + c] * (1.0 - a);
            }
        }
    }
    acc.iter().map(|v| (v.clamp(0.0, 1.0) * 255.0).round() as u8).collect()
}
