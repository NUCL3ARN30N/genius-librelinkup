//! Tray icon rendering: an SVG built from the [`TrayStyle`] is rasterised with resvg using real
//! system fonts, at the exact pixel size the tray wants (crisp instead of a scaled bitmap).

use std::cell::RefCell;

use resvg::{tiny_skia, usvg};

use crate::config::TrayStyle;

pub type Rgba = [u8; 4];

thread_local! {
    static OPTIONS: RefCell<Option<usvg::Options<'static>>> = const { RefCell::new(None) };
}

fn with_options<R>(f: impl FnOnce(&usvg::Options<'static>) -> R) -> R {
    OPTIONS.with(|o| {
        let mut o = o.borrow_mut();
        let opt = o.get_or_insert_with(|| {
            let mut opt = usvg::Options::default();
            opt.fontdb_mut().load_system_fonts();
            opt
        });
        f(opt)
    })
}

pub fn hex(c: Rgba) -> String {
    format!("#{:02x}{:02x}{:02x}", c[0], c[1], c[2])
}

/// Relative luminance, used to pick white or black text automatically.
fn is_light(c: Rgba) -> bool {
    let lin = |v: u8| {
        let v = v as f32 / 255.0;
        if v <= 0.04045 { v / 12.92 } else { ((v + 0.055) / 1.055).powf(2.4) }
    };
    0.2126 * lin(c[0]) + 0.7152 * lin(c[1]) + 0.0722 * lin(c[2]) > 0.45
}

fn font_family(font: u8) -> (&'static str, &'static str) {
    match font {
        1 => ("'Segoe UI', Roboto, 'Noto Sans', 'DejaVu Sans', Ubuntu, Cantarell, sans-serif", "normal"),
        2 => ("Consolas, 'Cascadia Mono', 'DejaVu Sans Mono', 'Ubuntu Mono', 'Noto Sans Mono', monospace", "normal"),
        _ => (
            "Bahnschrift, 'Arial Narrow', 'Roboto Condensed', 'DejaVu Sans Condensed', 'Ubuntu Condensed', \
             'Noto Sans', sans-serif",
            "condensed",
        ),
    }
}

/// Approximate advance of the bold digits in em, used to decide when to squeeze the text.
fn estimated_width(text: &str, font: u8) -> f32 {
    let narrow = if font == 0 { 0.82 } else { 1.0 };
    text.chars()
        .map(|c| match c {
            '.' | ',' => 0.28,
            '-' => 0.4,
            _ => 0.58,
        })
        .sum::<f32>()
        * narrow
}

/// Solid trend arrow (1 = falling fast … 5 = rising fast) centred at (cx, cy), fitting a
/// `size`×`size` box. Filled shapes survive 16 px far better than stroked lines; below ~10 px
/// the stem would only blur, so it becomes a plain triangle.
fn arrow(trend: u8, cx: f32, cy: f32, size: f32, color: &str) -> String {
    let angle: f32 = match trend {
        1 => 90.0,
        2 => 45.0,
        3 => 0.0,
        4 => -45.0,
        5 => -90.0,
        _ => return String::new(),
    };
    // a rotated shape needs more room, shrink diagonals so they stay inside the box
    let h = size / 2.0 * if angle % 90.0 == 0.0 { 1.0 } else { 0.82 };
    let d = if size < 10.0 {
        format!("M{:.2} {:.2} L{h:.2} 0 L{:.2} {:.2} Z", -h * 0.7, -h, -h * 0.7, h)
    } else {
        let s = h * 0.36; // half stem thickness
        let neck = h * 0.05;
        format!(
            "M{:.2} {:.2} L{neck:.2} {:.2} L{neck:.2} {:.2} L{h:.2} 0 L{neck:.2} {h:.2} L{neck:.2} {s:.2} L{:.2} {s:.2} Z",
            -h, -s, -s, -h, -h
        )
    };
    format!(r#"<path transform="translate({cx:.2} {cy:.2}) rotate({angle})" fill="{color}" d="{d}"/>"#)
}

/// Arrow for tray-sized icons: axis-aligned shapes on whole pixels (▲ ▼ ▶, and right-angle
/// corners ◥ ◢ for the diagonals) stay distinguishable where a rotated arrow turns into a
/// blob. From 7 px of width there is room for a stem as well.
fn pixel_arrow(trend: u8, left: f32, cy: f32, width: f32, color: &str) -> String {
    let w = width.floor().max(3.0);
    let x0 = left.round();
    let x1 = x0 + w;
    let stem = w >= 7.0;
    // pixel triangles need an odd base and half that height (5 → 3 → 1 px rows)
    let odd = |v: f32| if v as i32 % 2 == 0 { v + 1.0 } else { v };
    let thick = odd((w / 3.0).round()).max(1.0);
    let path = |d: String| format!(r#"<path fill="{color}" d="{d}" shape-rendering="crispEdges"/>"#);
    match trend {
        1 | 5 => {
            let base = odd(w);
            let rise = (base + 1.0) / 2.0;
            let shaft = if stem { rise } else { 0.0 };
            let bx0 = x0 + ((w - base) / 2.0).floor();
            let (bx1, bxc) = (bx0 + base, bx0 + base / 2.0);
            let top = (cy - (rise + shaft) / 2.0).round();
            let (sx0, sx1) = (bxc - thick / 2.0, bxc + thick / 2.0);
            if trend == 1 {
                let j = top + shaft;
                path(format!(
                    "M{sx0} {top} L{sx1} {top} L{sx1} {j} L{bx1} {j} L{bxc} {} L{bx0} {j} L{sx0} {j} Z",
                    j + rise
                ))
            } else {
                let j = top + rise;
                path(format!(
                    "M{bxc} {top} L{bx1} {j} L{sx1} {j} L{sx1} {} L{sx0} {} L{sx0} {j} L{bx0} {j} Z",
                    j + shaft,
                    j + shaft
                ))
            }
        }
        3 => {
            let base = odd(w.min(if stem { w } else { w * 2.0 - 1.0 }));
            let run = (base + 1.0) / 2.0;
            let shaft = if stem { w - run } else { 0.0 };
            let ax = x0 + ((w - run - shaft) / 2.0).floor();
            let ay0 = (cy - base / 2.0).round();
            let (j, mid) = (ax + shaft, ay0 + base / 2.0);
            let (sy0, sy1) = (mid - thick / 2.0, mid + thick / 2.0);
            path(format!(
                "M{ax} {sy0} L{j} {sy0} L{j} {ay0} L{} {mid} L{j} {} L{j} {sy1} L{ax} {sy1} Z",
                j + run,
                ay0 + base
            ))
        }
        2 | 4 => {
            // square box, corner at the top right (rising) or bottom right (falling)
            let y0 = (cy - w / 2.0).round();
            let y1 = y0 + w;
            let leg = if stem { (w * 0.62).ceil() } else { w };
            let (corner, head, line) = if trend == 4 {
                (
                    format!("M{} {y0} L{x1} {y0} L{x1} {} Z", x1 - leg, y0 + leg),
                    (x1 - leg / 2.0, y0 + leg / 2.0),
                    (x0 + thick / 2.0, y1 - thick / 2.0),
                )
            } else {
                (
                    format!("M{x1} {} L{x1} {y1} L{} {y1} Z", y1 - leg, x1 - leg),
                    (x1 - leg / 2.0, y1 - leg / 2.0),
                    (x0 + thick / 2.0, y0 + thick / 2.0),
                )
            };
            let mut out = path(corner);
            if stem {
                out.push_str(&format!(
                    r#"<line x1="{:.2}" y1="{:.2}" x2="{:.2}" y2="{:.2}" stroke="{color}" stroke-width="{thick}"/>"#,
                    line.0, line.1, head.0, head.1
                ));
            }
            out
        }
        _ => String::new(),
    }
}

/// Foreground (text/arrow) colour for the background `color`.
fn foreground(color: Rgba, style: &TrayStyle) -> String {
    match style.text_color {
        1 => "#ffffff".to_string(),
        2 => "#000000".to_string(),
        _ if style.shape != 0 => if is_light(color) { "#000000".into() } else { "#ffffff".into() },
        _ => hex(color),
    }
}

/// SVG header plus the background shape.
fn open_svg(color: Rgba, style: &TrayStyle, n: f32) -> String {
    let bg = hex(color);
    let mut out = format!(r#"<svg xmlns="http://www.w3.org/2000/svg" width="{n}" height="{n}" viewBox="0 0 {n} {n}">"#);
    match style.shape {
        1 => out.push_str(&format!(
            // drop: tip at the top, round body
            r#"<path fill="{bg}" d="M{c} {t} C{c} {t} {r} {m} {r} {b1} A{rad} {rad} 0 0 1 {l} {b1} C{l} {m} {c} {t} {c} {t} Z"/>"#,
            c = n / 2.0,
            t = n * 0.0,
            r = n * 0.94,
            l = n * 0.06,
            m = n * 0.42,
            b1 = n * 0.56,
            rad = n * 0.44,
        )),
        2 => out.push_str(&format!(r#"<circle fill="{bg}" cx="{c}" cy="{c}" r="{c}"/>"#, c = n / 2.0)),
        3 => out.push_str(&format!(
            r#"<rect fill="{bg}" x="0" y="0" width="{n}" height="{n}" rx="{r}"/>"#,
            r = n * 0.22
        )),
        _ => {}
    }
    out
}

/// Vertical centre of the shape's body (the drop is heavier at the bottom).
fn content_center(style: &TrayStyle, n: f32) -> f32 {
    if style.shape == 1 { n * 0.6 } else { n / 2.0 }
}

fn find_text(group: &usvg::Group) -> Option<&usvg::Text> {
    group.children().iter().find_map(|node| match node {
        usvg::Node::Text(t) => Some(&**t),
        usvg::Node::Group(g) => find_text(g),
        _ => None,
    })
}

/// Tight bounds (x, y, width, height) of the glyphs of a `<text>` element, laid out with the
/// system fonts. `None` when no font could render it.
fn measure_text(element: &str, n: f32) -> Option<(f32, f32, f32, f32)> {
    let svg = format!(r#"<svg xmlns="http://www.w3.org/2000/svg" width="{n}" height="{n}">{element}</svg>"#);
    with_options(|opt| {
        let tree = usvg::Tree::from_str(&svg, opt).ok()?;
        let b = find_text(tree.root())?.flattened().abs_bounding_box();
        (b.width() > 0.0 && b.height() > 0.0).then(|| (b.x(), b.y(), b.width(), b.height()))
    })
}

/// Builds the SVG for one icon. `n` is the pixel size.
pub fn svg(text: &str, trend: u8, color: Rgba, style: &TrayStyle, n: u32) -> String {
    let n = n as f32;
    let has_shape = style.shape != 0;
    let fg = foreground(color, style);
    let mut out = open_svg(color, style, n);

    let show_arrow = style.arrow && !style.arrow_separate && (1..=5).contains(&trend);
    // keep content inside the shape: circles and drops are narrower towards the edges
    let pad = match style.shape {
        0 => 0.0,
        2 => n * 0.1,
        1 => n * 0.08,
        _ => n * 0.07,
    };
    let arrow_w = if show_arrow { n * 0.3 } else { 0.0 };
    let gap = if show_arrow { (n * 0.04).max(1.0) } else { 0.0 };
    let avail = n - 2.0 * pad - arrow_w - gap;
    let (family, stretch) = font_family(style.font);
    // big enough to fill the height; squeeze horizontally when the value is wide
    let mut fs = n * if has_shape { 0.78 } else { 0.98 };
    if style.shape == 1 {
        fs *= 0.86;
    }
    let cy = content_center(style, n);
    let escaped: String = text.chars().filter(|c| c.is_ascii_alphanumeric() || ".,-".contains(*c)).collect();
    let attrs = format!(r#"font-family="{family}" font-weight="700" font-stretch="{stretch}" font-size="{fs:.2}" fill="{fg}""#);
    let max_h = n - 2.0 * pad;
    match measure_text(&format!(r#"<text x="0" y="0" {attrs}>{escaped}</text>"#), n) {
        // centre the actual glyphs (font metrics differ a lot between systems), with the
        // top-left corner on a whole pixel so the edges stay crisp
        Some((bx, by, bw, bh)) => {
            let sy = (max_h / bh).min(1.0);
            let sx = (avail / bw).min(sy);
            let left = (pad + (avail - bw * sx) / 2.0).round();
            let top = (cy - bh * sy / 2.0).round();
            out.push_str(&format!(
                r#"<text x="0" y="0" transform="matrix({sx:.4} 0 0 {sy:.4} {:.2} {:.2})" {attrs}>{escaped}</text>"#,
                left - bx * sx,
                top - by * sy
            ));
        }
        // no usable font: place by estimated metrics
        None => {
            let est = estimated_width(text, style.font) * fs;
            let squeeze = if est > avail {
                format!(r#" textLength="{avail:.2}" lengthAdjust="spacingAndGlyphs""#)
            } else {
                String::new()
            };
            let x = pad + (avail - est.min(avail)) / 2.0;
            let baseline = if style.shape == 1 { n * 0.6 + fs * 0.33 } else { n / 2.0 + fs * 0.35 };
            out.push_str(&format!(r#"<text x="{x:.2}" y="{baseline:.2}" {attrs}{squeeze}>{escaped}</text>"#));
        }
    }
    if show_arrow {
        if n < 40.0 {
            out.push_str(&pixel_arrow(trend, n - pad - arrow_w, cy + if style.shape == 1 { n * 0.02 } else { 0.0 }, arrow_w, &fg));
        } else {
            out.push_str(&arrow(trend, n - pad - arrow_w / 2.0, cy, arrow_w, &fg));
        }
    }
    out.push_str("</svg>");
    out
}

/// The trend arrow on its own (for the separate tray icon), as large as the shape allows. An
/// unknown trend shows a short bar.
pub fn svg_arrow(trend: u8, color: Rgba, style: &TrayStyle, n: u32) -> String {
    let n = n as f32;
    let fg = foreground(color, style);
    let mut out = open_svg(color, style, n);
    // side of the box the arrow fits in: the inscribed square of round shapes is smaller
    let w = n * match style.shape {
        0 => 0.94,
        1 => 0.5,
        2 => 0.62,
        _ => 0.7,
    };
    let (cx, cy) = (n / 2.0, content_center(style, n));
    if (1..=5).contains(&trend) {
        // axis-aligned arrows on whole pixels; once there is room for a stem a diagonal reads
        // better as one solid rotated arrow than as the corner + stem used next to the number
        if n < 40.0 && (matches!(trend, 1 | 3 | 5) || w < 10.0) {
            out.push_str(&pixel_arrow(trend, (cx - w / 2.0).floor(), cy, w, &fg));
        } else {
            out.push_str(&arrow(trend, cx, cy, w, &fg));
        }
    } else {
        let (bw, bh) = ((w * 0.5).round().max(2.0), (w * 0.16).round().max(1.0));
        out.push_str(&format!(
            r#"<rect fill="{fg}" x="{}" y="{}" width="{bw}" height="{bh}" shape-rendering="crispEdges"/>"#,
            (cx - bw / 2.0).round(),
            (cy - bh / 2.0).round()
        ));
    }
    out.push_str("</svg>");
    out
}

/// Renders to straight (non-premultiplied) RGBA.
pub fn render(text: &str, trend: u8, color: Rgba, style: &TrayStyle, size: u32) -> Vec<u8> {
    rasterize(&svg(text, trend, color, style, size), color, size)
}

/// Renders the separate trend-arrow icon.
pub fn render_arrow(trend: u8, color: Rgba, style: &TrayStyle, size: u32) -> Vec<u8> {
    rasterize(&svg_arrow(trend, color, style, size), color, size)
}

fn rasterize(svg: &str, color: Rgba, size: u32) -> Vec<u8> {
    let mut pixmap = tiny_skia::Pixmap::new(size, size).expect("pixmap");
    let ok = with_options(|opt| match usvg::Tree::from_str(svg, opt) {
        Ok(tree) => {
            resvg::render(&tree, tiny_skia::Transform::default(), &mut pixmap.as_mut());
            true
        }
        Err(e) => {
            tracing::warn!("tray svg failed: {e}");
            false
        }
    });
    if !ok {
        pixmap.fill(tiny_skia::Color::from_rgba8(color[0], color[1], color[2], 255));
    }
    pixmap
        .pixels()
        .iter()
        .flat_map(|p| {
            let c = p.demultiply();
            [c.red(), c.green(), c.blue(), c.alpha()]
        })
        .collect()
}

/// Pixel size of a notification-area icon for the current display scaling.
#[cfg(windows)]
pub fn tray_size() -> u32 {
    unsafe extern "system" {
        fn GetDpiForSystem() -> u32;
        fn GetSystemMetricsForDpi(index: i32, dpi: u32) -> i32;
    }
    const SM_CXSMICON: i32 = 49;
    let px = unsafe { GetSystemMetricsForDpi(SM_CXSMICON, GetDpiForSystem()) };
    if px > 0 { px as u32 } else { 16 }
}

#[cfg(not(windows))]
pub fn tray_size() -> u32 {
    // StatusNotifierItem hosts scale the pixmap themselves; 48 px keeps it sharp on HiDPI panels.
    48
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn svg_parses_for_all_styles() {
        let opt = usvg::Options::default();
        for shape in 0..4 {
            for font in 0..3 {
                for arrow_separate in [false, true] {
                    let style = TrayStyle { shape, font, arrow: true, text_color: 0, arrow_separate };
                    for (text, trend) in [("6.2", 4), ("112", 1), ("LO", 0), ("-", 3)] {
                        for size in [16, 24, 48] {
                            let s = svg(text, trend, [0x2e, 0x7d, 0x32, 0xff], &style, size);
                            usvg::Tree::from_str(&s, &opt).unwrap_or_else(|e| panic!("{e}: {s}"));
                            let s = svg_arrow(trend, [0x2e, 0x7d, 0x32, 0xff], &style, size);
                            usvg::Tree::from_str(&s, &opt).unwrap_or_else(|e| panic!("{e}: {s}"));
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn renders_pixels() {
        let style = TrayStyle::default();
        let px = render("6.2", 4, [0x2e, 0x7d, 0x32, 0xff], &style, 32);
        assert_eq!(px.len(), 32 * 32 * 4);
        assert!(px.chunks(4).any(|p| p[3] > 0));
        let px = render_arrow(5, [0x2e, 0x7d, 0x32, 0xff], &style, 16);
        assert_eq!(px.len(), 16 * 16 * 4);
    }

    /// Centre of the bounding box of the pixels that clearly differ from the background.
    fn ink_center(px: &[u8], size: u32, bg: Rgba) -> Option<(f32, f32)> {
        let mut bounds: Option<(u32, u32, u32, u32)> = None;
        for y in 0..size {
            for x in 0..size {
                let p = &px[((y * size + x) * 4) as usize..][..4];
                let diff: i32 = (0..3).map(|c| (p[c] as i32 - bg[c] as i32).abs()).sum();
                if p[3] > 200 && diff > 200 {
                    let (x0, x1, y0, y1) = bounds.unwrap_or((x, x, y, y));
                    bounds = Some((x0.min(x), x1.max(x), y0.min(y), y1.max(y)));
                }
            }
        }
        bounds.map(|(x0, x1, y0, y1)| ((x0 + x1) as f32 / 2.0, (y0 + y1) as f32 / 2.0))
    }

    #[test]
    fn text_is_centred_when_a_font_is_available() {
        let style = TrayStyle { shape: 3, arrow: false, ..TrayStyle::default() };
        let bg = [0x2e, 0x7d, 0x32, 0xff];
        for size in [16u32, 24, 32] {
            for text in ["6.2", "112", "10.4"] {
                let s = svg(text, 0, bg, &style, size);
                if !s.contains("matrix(") {
                    return; // no system fonts in this environment
                }
                let px = render(text, 0, bg, &style, size);
                let (cx, cy) = ink_center(&px, size, bg).expect("text pixels");
                let mid = (size as f32 - 1.0) / 2.0;
                assert!((cx - mid).abs() <= 1.0 && (cy - mid).abs() <= 1.0, "{text} at {size}px: centre {cx},{cy}");
            }
        }
    }
}
