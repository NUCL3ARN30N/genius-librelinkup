//! The application icon (red drop with a pulse line) and tiny PNG/ICO writers. Kept free of
//! dependencies because `build.rs` includes this file to generate the icon assets.

type Rgba = [u8; 4];

const RED: Rgba = [0xd3, 0x2f, 0x2f, 0xff];
const WHITE: Rgba = [0xff, 0xff, 0xff, 0xff];

pub struct Image {
    pub size: u32,
    pub rgba: Vec<u8>,
}

/// Coverage (0..1) of the drop shape at a point in unit coordinates.
fn in_drop(x: f32, y: f32) -> bool {
    let (cx, cy, r) = (0.5, 0.61, 0.38);
    let tip_y = 0.02;
    if (x - cx).powi(2) + (y - cy).powi(2) <= r * r {
        return true;
    }
    if y < tip_y || y > cy {
        return false;
    }
    // Tangent lines from the tip to the circle give the classic drop silhouette.
    let d = cy - tip_y;
    let half_angle = (r / d).asin();
    let tangent_y = tip_y + d * half_angle.cos().powi(2);
    if y > tangent_y {
        return (x - cx).abs() <= ((r * r) - (y - cy).powi(2)).max(0.0).sqrt();
    }
    (x - cx).abs() <= (y - tip_y) * half_angle.tan()
}

fn blend(dst: &mut [u8], src: Rgba, alpha: f32) {
    let a = alpha * src[3] as f32 / 255.0;
    let da = dst[3] as f32 / 255.0;
    let out_a = a + da * (1.0 - a);
    if out_a <= 0.0 {
        return;
    }
    for i in 0..3 {
        dst[i] = ((src[i] as f32 * a + dst[i] as f32 * da * (1.0 - a)) / out_a).round() as u8;
    }
    dst[3] = (out_a * 255.0).round() as u8;
}

fn draw_drop(img: &mut Image, color: Rgba) {
    let n = img.size as usize;
    const SS: usize = 4;
    for py in 0..n {
        for px in 0..n {
            let mut hits = 0;
            for sy in 0..SS {
                for sx in 0..SS {
                    let x = (px as f32 + (sx as f32 + 0.5) / SS as f32) / n as f32;
                    let y = (py as f32 + (sy as f32 + 0.5) / SS as f32) / n as f32;
                    hits += in_drop(x, y) as usize;
                }
            }
            if hits > 0 {
                let i = (py * n + px) * 4;
                blend(&mut img.rgba[i..i + 4], color, hits as f32 / (SS * SS) as f32);
            }
        }
    }
}

/// Application icon: red drop with a small pulse line.
pub fn app_icon(size: u32) -> Image {
    let mut img = Image { size, rgba: vec![0; (size * size * 4) as usize] };
    draw_drop(&mut img, RED);
    let n = size as f32;
    // zig-zag line
    let pts = [(0.30, 0.68), (0.42, 0.68), (0.50, 0.52), (0.58, 0.80), (0.64, 0.66), (0.72, 0.66)];
    let thick = 0.035;
    for py in 0..size {
        for px in 0..size {
            let (x, y) = ((px as f32 + 0.5) / n, (py as f32 + 0.5) / n);
            let dist = pts
                .windows(2)
                .map(|w| seg_dist((x, y), w[0], w[1]))
                .fold(f32::MAX, f32::min);
            let cov = ((thick - dist) * n + 0.5).clamp(0.0, 1.0);
            if cov > 0.0 {
                let i = ((py * size + px) * 4) as usize;
                blend(&mut img.rgba[i..i + 4], WHITE, cov);
            }
        }
    }
    img
}

fn seg_dist(p: (f32, f32), a: (f32, f32), b: (f32, f32)) -> f32 {
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    let t = (((p.0 - a.0) * dx + (p.1 - a.1) * dy) / (dx * dx + dy * dy)).clamp(0.0, 1.0);
    ((p.0 - a.0 - t * dx).powi(2) + (p.1 - a.1 - t * dy).powi(2)).sqrt()
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xffff_ffffu32;
    for b in data {
        crc ^= *b as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ 0xedb8_8320 } else { crc >> 1 };
        }
    }
    !crc
}

/// Uncompressed (stored deflate) PNG – good enough for a handful of icon files.
pub fn encode_png(img: &Image) -> Vec<u8> {
    let n = img.size as usize;
    let mut raw = Vec::with_capacity(n * (n * 4 + 1));
    for row in img.rgba.chunks(n * 4) {
        raw.push(0);
        raw.extend_from_slice(row);
    }
    let mut z = vec![0x78, 0x01];
    for (i, chunk) in raw.chunks(65535).enumerate() {
        let last = (i + 1) * 65535 >= raw.len();
        z.push(last as u8);
        z.extend_from_slice(&(chunk.len() as u16).to_le_bytes());
        z.extend_from_slice(&(!(chunk.len() as u16)).to_le_bytes());
        z.extend_from_slice(chunk);
    }
    let (mut a, mut b) = (1u32, 0u32);
    for x in &raw {
        a = (a + *x as u32) % 65521;
        b = (b + a) % 65521;
    }
    z.extend_from_slice(&((b << 16) | a).to_be_bytes());

    let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut chunk = |kind: &[u8], data: &[u8]| {
        out.extend_from_slice(&(data.len() as u32).to_be_bytes());
        let mut c = kind.to_vec();
        c.extend_from_slice(data);
        out.extend_from_slice(&c);
        out.extend_from_slice(&crc32(&c).to_be_bytes());
    };
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&(n as u32).to_be_bytes());
    ihdr.extend_from_slice(&(n as u32).to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]);
    chunk(b"IHDR", &ihdr);
    chunk(b"IDAT", &z);
    chunk(b"IEND", &[]);
    out
}

/// ICO with PNG encoded entries (supported since Windows Vista).
pub fn encode_ico(images: &[Image]) -> Vec<u8> {
    let pngs: Vec<Vec<u8>> = images.iter().map(encode_png).collect();
    let mut out = vec![0, 0, 1, 0];
    out.extend_from_slice(&(images.len() as u16).to_le_bytes());
    let mut offset = 6 + 16 * images.len() as u32;
    for (img, png) in images.iter().zip(&pngs) {
        let s = if img.size >= 256 { 0 } else { img.size as u8 };
        out.extend_from_slice(&[s, s, 0, 0, 1, 0, 32, 0]);
        out.extend_from_slice(&(png.len() as u32).to_le_bytes());
        out.extend_from_slice(&offset.to_le_bytes());
        offset += png.len() as u32;
    }
    for png in pngs {
        out.extend_from_slice(&png);
    }
    out
}

/// Writes `icon-256.png` and `app.ico` (run with `--write-icons`, also used by `build.rs`).
pub fn write_assets(dir: &std::path::Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    std::fs::write(dir.join("icon-256.png"), encode_png(&app_icon(256)))?;
    let sizes = [16, 24, 32, 48, 64, 256];
    std::fs::write(dir.join("app.ico"), encode_ico(&sizes.map(app_icon)))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn png_has_valid_signature_and_crc() {
        let png = encode_png(&app_icon(32));
        assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
        assert_eq!(crc32(b"IEND"), 0xae42_6082);
    }

    #[test]
    fn drop_has_tip_and_body() {
        assert!(in_drop(0.5, 0.6));
        assert!(in_drop(0.5, 0.1));
        assert!(!in_drop(0.1, 0.1));
    }
}
