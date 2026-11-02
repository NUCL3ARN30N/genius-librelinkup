//! Printable A4 report (PDF), drawn directly with `pdf-writer`. Text uses the embedded Noto Sans
//! (subset per document) so the output is the same on Windows, Linux and the font-less server
//! image. Pages: summary, AGP with pattern insights, daily profiles (7 per page), notes.

use std::collections::BTreeMap;
use std::io::Write;

use anyhow::{Context, Result, anyhow};
use chrono::{Local, NaiveDate, Timelike};
use pdf_writer::types::{CidFontType, FontFlags, SystemInfo, UnicodeCmap};
use pdf_writer::{Content, Filter, Finish, Name, Pdf, Rect, Ref, Str, TextStr};

use crate::stats::{self, DayTotals, Light, Report};
use crate::store::{MMOL_FACTOR, Note, fmt_units};

static REGULAR: &[u8] = include_bytes!("../assets/fonts/NotoSans-Regular.ttf");
static BOLD: &[u8] = include_bytes!("../assets/fonts/NotoSans-Bold.ttf");

/// A4 in points, and the page margin.
const PW: f32 = 595.0;
const PH: f32 = 842.0;
const M: f32 = 40.0;

pub struct PdfOptions {
    /// e.g. "Glucose report"
    pub title: String,
    /// patient name, may be empty
    pub patient: String,
    /// e.g. "20 Oct 2026 – 02 Nov 2026"
    pub period: String,
    /// local time the report was made, e.g. "2026-11-02 15:40"
    pub generated: String,
    /// show mmol/L instead of mg/dL
    pub mmol: bool,
    /// target range in mg/dL
    pub target_low: f32,
    pub target_high: f32,
    /// "en" or "de"
    pub lang: String,
}

type Rgb = (f32, f32, f32);

const fn hex(v: u32) -> Rgb {
    (((v >> 16) & 0xff) as f32 / 255.0, ((v >> 8) & 0xff) as f32 / 255.0, (v & 0xff) as f32 / 255.0)
}

/// `c` blended with white (`amount` 0 = white, 1 = `c`) – PDF without transparency groups.
fn tint(c: Rgb, amount: f32) -> Rgb {
    (1.0 - (1.0 - c.0) * amount, 1.0 - (1.0 - c.1) * amount, 1.0 - (1.0 - c.2) * amount)
}

const INK: Rgb = hex(0x191c20);
const SOFT: Rgb = hex(0x5b5f67);
const GRID: Rgb = hex(0xdfe1e6);
const ACCENT: Rgb = hex(0x1d5fa8);
const INSULIN: Rgb = hex(0x1e88e5);
const CARBS: Rgb = hex(0xef8f00);
const OTHER: Rgb = hex(0x6b6f78);
/// very low, low, target, high, very high (LibreView colours)
const LEVELS: [Rgb; 5] = [hex(0x8e0e0e), hex(0xd32f2f), hex(0x2e7d32), hex(0xf9a825), hex(0xef6c00)];

// ------------------------------------------------------------------ fonts

/// One embedded font: glyphs are renumbered as they are used and the font is subset at the end.
struct Font {
    face: ttf_parser::Face<'static>,
    data: &'static [u8],
    remap: subsetter::GlyphRemapper,
    /// old glyph id per new glyph id
    olds: Vec<u16>,
    /// new glyph id → character, for copy & paste (ToUnicode)
    chars: BTreeMap<u16, char>,
    name: &'static str,
}

impl Font {
    fn new(data: &'static [u8], name: &'static str) -> Result<Font> {
        let face = ttf_parser::Face::parse(data, 0).map_err(|e| anyhow!("font {name}: {e}"))?;
        Ok(Font { face, data, remap: subsetter::GlyphRemapper::new(), olds: vec![0], chars: BTreeMap::new(), name })
    }

    fn glyph(&self, c: char) -> u16 {
        self.face.glyph_index(c).or_else(|| self.face.glyph_index('?')).map(|g| g.0).unwrap_or(0)
    }

    fn advance(&self, gid: u16) -> f32 {
        self.face.glyph_hor_advance(ttf_parser::GlyphId(gid)).unwrap_or(0) as f32 / self.face.units_per_em() as f32
    }

    fn width(&self, text: &str, size: f32) -> f32 {
        text.chars().map(|c| self.advance(self.glyph(c))).sum::<f32>() * size
    }

    /// Two-byte glyph ids (Identity-H) of the subset font.
    fn encode(&mut self, text: &str) -> Vec<u8> {
        let mut out = Vec::with_capacity(text.len() * 2);
        for c in text.chars() {
            let old = self.glyph(c);
            let new = match self.remap.get(old) {
                Some(n) => n,
                None => {
                    let n = self.remap.remap(old);
                    self.olds.push(old);
                    n
                }
            };
            self.chars.entry(new).or_insert(c);
            out.extend_from_slice(&new.to_be_bytes());
        }
        out
    }

    /// Writes the Type0 font with its CID font, descriptor, subset font file and ToUnicode map.
    fn embed(&self, pdf: &mut Pdf, next: &mut impl FnMut() -> Ref, font_ref: Ref) -> Result<()> {
        let (cid, desc, file, cmap) = (next(), next(), next(), next());
        // subset fonts are tagged with six capital letters
        let tag: String = (0..6).map(|i| (b'A' + ((self.olds.len() * 7 + i * 3 + self.name.len()) % 26) as u8) as char).collect();
        let base = format!("{tag}+{}", self.name);
        let system = SystemInfo { registry: Str(b"Adobe"), ordering: Str(b"Identity"), supplement: 0 };

        pdf.type0_font(font_ref)
            .base_font(Name(base.as_bytes()))
            .encoding_predefined(Name(b"Identity-H"))
            .descendant_font(cid)
            .to_unicode(cmap);
        let mut cid_font = pdf.cid_font(cid);
        cid_font
            .subtype(CidFontType::Type2)
            .base_font(Name(base.as_bytes()))
            .system_info(system)
            .font_descriptor(desc)
            .default_width(0.0)
            .cid_to_gid_map_predefined(Name(b"Identity"));
        let widths: Vec<f32> = self.olds.iter().map(|g| self.advance(*g) * 1000.0).collect();
        cid_font.widths().consecutive(0, widths);
        cid_font.finish();

        let upem = self.face.units_per_em() as f32;
        let scale = |v: i16| v as f32 / upem * 1000.0;
        let bb = self.face.global_bounding_box();
        pdf.font_descriptor(desc)
            .name(Name(base.as_bytes()))
            .flags(FontFlags::NON_SYMBOLIC)
            .bbox(Rect::new(scale(bb.x_min), scale(bb.y_min), scale(bb.x_max), scale(bb.y_max)))
            .italic_angle(0.0)
            .ascent(scale(self.face.ascender()))
            .descent(scale(self.face.descender()))
            .cap_height(scale(self.face.capital_height().unwrap_or(self.face.ascender())))
            .stem_v(if self.name.contains("Bold") { 140.0 } else { 80.0 })
            .font_file2(file);

        let subset = subsetter::subset(self.data, 0, &self.remap).map_err(|e| anyhow!("subsetting {}: {e}", self.name))?;
        pdf.stream(file, &deflate(&subset)?).filter(Filter::FlateDecode).pair(Name(b"Length1"), subset.len() as i32);

        let mut map = UnicodeCmap::new(Name(b"Custom"), system);
        for (gid, c) in &self.chars {
            map.pair(*gid, *c);
        }
        pdf.cmap(cmap, &map.finish());
        Ok(())
    }
}

fn deflate(data: &[u8]) -> Result<Vec<u8>> {
    let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::new(6));
    z.write_all(data)?;
    Ok(z.finish()?)
}

// ------------------------------------------------------------------ drawing

/// Pages under construction. Coordinates are measured from the top-left corner in points.
struct Doc {
    fonts: [Font; 2],
    pages: Vec<Content>,
}

#[derive(Clone, Copy, PartialEq)]
enum Align {
    Left,
    Right,
    Center,
}

impl Doc {
    fn page(&mut self) -> &mut Content {
        if self.pages.is_empty() {
            self.new_page();
        }
        self.pages.last_mut().unwrap()
    }

    fn new_page(&mut self) {
        self.pages.push(Content::new());
    }

    fn width(&self, text: &str, size: f32, bold: bool) -> f32 {
        self.fonts[bold as usize].width(text, size)
    }

    /// Text with its baseline at `y`.
    #[allow(clippy::too_many_arguments)]
    fn text(&mut self, x: f32, y: f32, size: f32, bold: bool, color: Rgb, align: Align, text: &str) {
        let w = self.width(text, size, bold);
        let x = match align {
            Align::Left => x,
            Align::Right => x - w,
            Align::Center => x - w / 2.0,
        };
        let bytes = self.fonts[bold as usize].encode(text);
        let c = self.page();
        c.set_fill_rgb(color.0, color.1, color.2);
        c.begin_text();
        c.set_font(Name(if bold { b"F2" } else { b"F1" }), size);
        c.set_text_matrix([1.0, 0.0, 0.0, 1.0, x, PH - y]);
        c.show(Str(&bytes));
        c.end_text();
    }

    /// Splits `text` into lines no wider than `width`.
    fn wrap(&self, text: &str, width: f32, size: f32, bold: bool) -> Vec<String> {
        let mut lines = Vec::new();
        let mut line = String::new();
        for word in text.split_whitespace() {
            let candidate = if line.is_empty() { word.to_string() } else { format!("{line} {word}") };
            if !line.is_empty() && self.width(&candidate, size, bold) > width {
                lines.push(std::mem::replace(&mut line, word.to_string()));
            } else {
                line = candidate;
            }
        }
        if !line.is_empty() {
            lines.push(line);
        }
        lines
    }

    /// Wrapped text starting with the baseline at `y`; returns the baseline after the last line.
    fn paragraph(&mut self, x: f32, y: f32, width: f32, size: f32, color: Rgb, text: &str) -> f32 {
        let mut y = y;
        for line in self.wrap(text, width, size, false) {
            self.text(x, y, size, false, color, Align::Left, &line);
            y += size * 1.35;
        }
        y
    }

    fn rect(&mut self, x: f32, y: f32, w: f32, h: f32, color: Rgb) {
        let c = self.page();
        c.set_fill_rgb(color.0, color.1, color.2);
        c.rect(x, PH - y - h, w, h);
        c.fill_nonzero();
    }

    fn line(&mut self, x1: f32, y1: f32, x2: f32, y2: f32, width: f32, color: Rgb) {
        let c = self.page();
        c.set_stroke_rgb(color.0, color.1, color.2);
        c.set_line_width(width);
        c.move_to(x1, PH - y1);
        c.line_to(x2, PH - y2);
        c.stroke();
    }

    /// Polyline through `points`; `None` breaks the line.
    fn polyline(&mut self, points: &[Option<(f32, f32)>], width: f32, color: Rgb) {
        let c = self.page();
        c.set_stroke_rgb(color.0, color.1, color.2);
        c.set_line_width(width);
        c.set_line_join(pdf_writer::types::LineJoinStyle::RoundJoin);
        let mut open = false;
        for p in points {
            match (p, open) {
                (Some((x, y)), false) => {
                    c.move_to(*x, PH - y);
                    open = true;
                }
                (Some((x, y)), true) => {
                    c.line_to(*x, PH - y);
                }
                (None, _) => open = false,
            }
        }
        c.stroke();
    }

    fn polygon(&mut self, points: &[(f32, f32)], color: Rgb) {
        let Some(((x0, y0), rest)) = points.split_first() else { return };
        let c = self.page();
        c.set_fill_rgb(color.0, color.1, color.2);
        c.move_to(*x0, PH - y0);
        for (x, y) in rest {
            c.line_to(*x, PH - y);
        }
        c.close_path();
        c.fill_nonzero();
    }

    fn circle(&mut self, cx: f32, cy: f32, r: f32, color: Rgb) {
        // four Bézier quarter arcs
        let k = 0.552_284_8 * r;
        let (cx, cy) = (cx, PH - cy);
        let c = self.page();
        c.set_fill_rgb(color.0, color.1, color.2);
        c.move_to(cx + r, cy);
        c.cubic_to(cx + r, cy + k, cx + k, cy + r, cx, cy + r);
        c.cubic_to(cx - k, cy + r, cx - r, cy + k, cx - r, cy);
        c.cubic_to(cx - r, cy - k, cx - k, cy - r, cx, cy - r);
        c.cubic_to(cx + k, cy - r, cx + r, cy - k, cx + r, cy);
        c.fill_nonzero();
    }

    fn heading(&mut self, x: f32, y: f32, text: &str) {
        self.text(x, y, 12.5, true, INK, Align::Left, text);
    }
}

// ------------------------------------------------------------------ report

struct Fmt {
    mmol: bool,
}

impl Fmt {
    fn unit(&self) -> &'static str {
        if self.mmol { "mmol/L" } else { "mg/dL" }
    }
    fn num(&self, mg: f32) -> String {
        if self.mmol { format!("{:.1}", mg / MMOL_FACTOR) } else { format!("{mg:.0}") }
    }
    fn val(&self, mg: f32) -> String {
        format!("{} {}", self.num(mg), self.unit())
    }
    /// The next lower displayed value (for "3.9 – 10.0" style range ends).
    fn below(&self, mg: f32) -> f32 {
        mg - if self.mmol { 0.1 * MMOL_FACTOR } else { 1.0 }
    }
}

fn light_color(l: Light) -> Rgb {
    match l {
        Light::Green => hex(0x43a047),
        Light::Yellow => hex(0xf9c623),
        Light::Red => hex(0xe53935),
        Light::None => hex(0xc4c6cf),
    }
}

/// Builds the PDF. `totals` are the daily insulin & carbs totals of the report period
/// ([`stats::daily_totals`]); `notes` are listed on the last pages.
pub fn render(report: &Report, notes: &[Note], totals: &[DayTotals], opts: &PdfOptions) -> Result<Vec<u8>> {
    let mut doc = Doc { fonts: [Font::new(REGULAR, "NotoSans-Regular")?, Font::new(BOLD, "NotoSans-Bold")?], pages: Vec::new() };
    let f = Fmt { mmol: opts.mmol };
    summary_page(&mut doc, report, totals, opts, &f);
    agp_page(&mut doc, report, opts, &f);
    daily_pages(&mut doc, report, notes, totals, opts, &f);
    notes_pages(&mut doc, notes);
    let count = doc.pages.len();
    for i in 0..count {
        doc.pages.swap(i, count - 1);
        let y = PH - 22.0;
        doc.line(M, y - 12.0, PW - M, y - 12.0, 0.5, GRID);
        doc.text(M, y, 8.0, false, SOFT, Align::Left, "Genius LibreLinkUp · not a medical device – do not base treatment decisions on this report");
        doc.text(PW - M, y, 8.0, false, SOFT, Align::Right, &format!("Page {} of {count}", i + 1));
        doc.pages.swap(i, count - 1);
    }
    finish(doc, opts)
}

fn finish(doc: Doc, opts: &PdfOptions) -> Result<Vec<u8>> {
    let mut pdf = Pdf::new();
    let mut id = 0;
    let mut next = || {
        id += 1;
        Ref::new(id)
    };
    let (catalog, tree, info) = (next(), next(), next());
    let fonts = [next(), next()];
    let page_ids: Vec<(Ref, Ref)> = doc.pages.iter().map(|_| (next(), next())).collect();
    pdf.catalog(catalog).pages(tree);
    pdf.pages(tree).kids(page_ids.iter().map(|(p, _)| *p)).count(page_ids.len() as i32);
    for ((page_id, content_id), content) in page_ids.iter().zip(doc.pages) {
        let mut page = pdf.page(*page_id);
        page.media_box(Rect::new(0.0, 0.0, PW, PH)).parent(tree).contents(*content_id);
        page.resources().fonts().pair(Name(b"F1"), fonts[0]).pair(Name(b"F2"), fonts[1]);
        page.finish();
        pdf.stream(*content_id, &deflate(&content.finish())?).filter(Filter::FlateDecode);
    }
    let title = if opts.patient.is_empty() { opts.title.clone() } else { format!("{} – {}", opts.title, opts.patient) };
    pdf.document_info(info).title(TextStr(&title)).producer(TextStr("Genius LibreLinkUp")).creator(TextStr("Genius LibreLinkUp"));
    for (font, font_ref) in doc.fonts.iter().zip(fonts) {
        font.embed(&mut pdf, &mut next, font_ref).context("embedding the font")?;
    }
    Ok(pdf.finish())
}

fn header(doc: &mut Doc, opts: &PdfOptions, subtitle: &str) -> f32 {
    doc.text(M, 58.0, 20.0, true, INK, Align::Left, &opts.title);
    doc.text(PW - M, 52.0, 9.0, false, SOFT, Align::Right, "Genius LibreLinkUp");
    doc.text(PW - M, 64.0, 8.0, false, SOFT, Align::Right, &format!("Generated {}", opts.generated));
    let mut line = vec![opts.period.clone()];
    if !opts.patient.is_empty() {
        line.insert(0, opts.patient.clone());
    }
    if !subtitle.is_empty() {
        line.push(subtitle.to_string());
    }
    doc.text(M, 76.0, 10.0, false, SOFT, Align::Left, &line.join("  ·  "));
    doc.line(M, 88.0, PW - M, 88.0, 0.8, GRID);
    110.0
}

fn summary_page(doc: &mut Doc, r: &Report, totals: &[DayTotals], opts: &PdfOptions, f: &Fmt) {
    doc.new_page();
    let mut y = header(doc, opts, &format!("{} days, {} readings", r.days, r.readings));

    // glucose statistics (left) and time in ranges (right)
    let (lx, lw) = (M, 245.0);
    doc.heading(lx, y, "Glucose statistics");
    let coverage = if r.sensor_active_pct < 70.0 { "less than 70 % – interpret with care" } else { "" };
    let rows: Vec<(&str, String, &str)> = vec![
        ("% time CGM is active", format!("{:.1} %", r.sensor_active_pct), coverage),
        ("Average glucose", f.val(r.mean), ""),
        ("GMI", format!("{:.1} % · {:.0} mmol/mol", r.gmi_pct, r.gmi_mmol_mol), "glucose management indicator, approximate HbA1c"),
        ("Estimated HbA1c (eA1c)", format!("{:.1} % · {:.0} mmol/mol", r.ea1c_pct, r.ea1c_mmol_mol), "ADAG formula"),
        ("Glucose variability (CV)", format!("{:.1} %", r.cv_pct), "goal: 36 % or lower"),
        ("Standard deviation", f.val(r.sd), ""),
    ];
    let mut ly = y + 24.0;
    for (label, value, hint) in rows {
        doc.text(lx, ly, 9.5, false, INK, Align::Left, label);
        doc.text(lx + lw, ly, 10.5, true, INK, Align::Right, &value);
        if !hint.is_empty() {
            doc.text(lx, ly + 11.0, 7.5, false, SOFT, Align::Left, hint);
            ly += 9.0;
        }
        ly += 24.0;
    }

    let rx = 315.0;
    doc.heading(rx, y, "Time in ranges");
    let (lo, hi) = (opts.target_low, opts.target_high);
    let tir = [
        ("Very high", format!("> {}", f.val(stats::VERY_HIGH)), r.tir.very_high, LEVELS[4]),
        ("High", format!("{} – {}", f.num(hi + 1.0), f.val(stats::VERY_HIGH)), r.tir.high, LEVELS[3]),
        ("Target range", format!("{} – {}", f.num(lo), f.val(hi)), r.tir.target, LEVELS[2]),
        ("Low", format!("{} – {}", f.num(stats::VERY_LOW), f.val(f.below(lo))), r.tir.low, LEVELS[1]),
        ("Very low", format!("< {}", f.val(stats::VERY_LOW)), r.tir.very_low, LEVELS[0]),
    ];
    let (bar_y, bar_h) = (y + 14.0, 190.0);
    let mut by = bar_y;
    let total: f32 = tir.iter().map(|t| t.2).sum::<f32>().max(0.001);
    for (_, _, pct, color) in &tir {
        let h = pct / total * bar_h;
        if h > 0.0 {
            doc.rect(rx, by, 24.0, h.max(1.0), *color);
        }
        by += h;
    }
    for (i, (label, range, pct, color)) in tir.iter().enumerate() {
        let ty = bar_y + 14.0 + i as f32 * 38.0;
        doc.circle(rx + 40.0, ty - 3.5, 4.0, *color);
        doc.text(rx + 50.0, ty, 10.0, true, INK, Align::Left, label);
        doc.text(rx + 50.0, ty + 12.0, 8.0, false, SOFT, Align::Left, range);
        doc.text(PW - M, ty, 12.0, true, INK, Align::Right, &format!("{pct:.0} %"));
        doc.text(PW - M, ty + 12.0, 8.0, false, SOFT, Align::Right, &stats::share_of_day(*pct));
    }
    y = ly.max(bar_y + bar_h) + 8.0;
    y = doc.paragraph(M, y, PW - 2.0 * M, 8.0, SOFT, "Goals for most adults with type 1 or type 2 diabetes: target range > 70 %, low < 4 %, very low < 1 %, high < 25 %, very high < 5 % (international consensus on time in range).");
    y += 14.0;

    // insulin & carbs
    doc.heading(M, y, "Insulin & carbs");
    y += 8.0;
    let sum = stats::summarize_totals(totals);
    if sum.days_with_entries == 0 {
        doc.paragraph(M, y + 14.0, PW - 2.0 * M, 9.0, SOFT, "No insulin or carbs were recorded in the notes of this period.");
        return;
    }
    let mut items: Vec<(String, String)> = Vec::new();
    let per_day = |v: f32, unit: &str| format!("{} {unit}/day", fmt_units(if v < 10.0 { (v * 10.0).round() / 10.0 } else { v.round() }));
    if sum.avg_rapid_u > 0.0 {
        items.push(("Rapid-acting insulin".into(), per_day(sum.avg_rapid_u, "u")));
    }
    if sum.avg_long_u > 0.0 {
        items.push(("Long-acting insulin".into(), per_day(sum.avg_long_u, "u")));
    }
    if sum.avg_rapid_u > 0.0 && sum.avg_long_u > 0.0 {
        items.push(("Total insulin".into(), per_day(sum.avg_insulin_u(), "u")));
    }
    for (name, u) in &sum.avg_custom {
        items.push((name.clone(), per_day(*u, "u")));
    }
    if sum.avg_carbs_g > 0.0 {
        items.push(("Carbohydrates".into(), per_day(sum.avg_carbs_g.round(), "g")));
    }
    items.push(("Days with entries".into(), format!("{} of {}", sum.days_with_entries, sum.days)));
    let mut iy = y + 18.0;
    for (label, value) in &items {
        doc.text(M, iy, 9.5, false, INK, Align::Left, label);
        doc.text(M + 200.0, iy, 10.0, true, INK, Align::Right, value);
        iy += 18.0;
    }
    // bar chart per day
    let (cx, cw, ch) = (M + 225.0, PW - 2.0 * M - 225.0, 110.0);
    let cy = y + 6.0;
    let max_u = totals.iter().map(DayTotals::insulin_u).fold(0.0, f32::max).max(1.0);
    let max_g = totals.iter().map(|d| d.carbs_g).fold(0.0, f32::max).max(1.0);
    let slot = cw / totals.len().max(1) as f32;
    let bar = ((slot - 2.0) / 2.0).clamp(0.6, 8.0);
    doc.line(cx, cy + ch, cx + cw, cy + ch, 0.6, GRID);
    for (i, d) in totals.iter().enumerate() {
        let x = cx + slot * i as f32 + (slot - 2.0 * bar - 0.5) / 2.0;
        let hu = d.insulin_u() / max_u * ch;
        let hg = d.carbs_g / max_g * ch;
        if hu > 0.0 {
            doc.rect(x, cy + ch - hu, bar, hu, INSULIN);
        }
        if hg > 0.0 {
            doc.rect(x + bar + 0.5, cy + ch - hg, bar, hg, CARBS);
        }
        if i % if totals.len() > 16 { 7 } else { 2 } == 0 {
            doc.text(cx + slot * (i as f32 + 0.5), cy + ch + 10.0, 7.0, false, SOFT, Align::Center, &d.date.format("%d %b").to_string());
        }
    }
    let ly = cy + ch + 24.0;
    doc.rect(cx, ly - 6.5, 7.0, 7.0, INSULIN);
    doc.text(cx + 10.0, ly, 8.0, false, SOFT, Align::Left, &format!("Insulin (max {} u)", fmt_units(max_u.round())));
    let lx2 = cx + 20.0 + doc.width(&format!("Insulin (max {} u)", fmt_units(max_u.round())), 8.0, false);
    doc.rect(lx2, ly - 6.5, 7.0, 7.0, CARBS);
    doc.text(lx2 + 10.0, ly, 8.0, false, SOFT, Align::Left, &format!("Carbs (max {} g)", fmt_units(max_g.round())));
    doc.paragraph(M, iy.max(ly + 18.0), PW - 2.0 * M, 8.0, SOFT, "Averages count only the days with entries. The totals come from the notes and are only as complete as they are.");
}

/// Upper end of a glucose axis in mg/dL on "nice" steps of the shown unit.
fn axis_max(f: &Fmt, need: f32) -> f32 {
    if f.mmol {
        [15.0, 18.0, 21.0, 24.0, 27.0, 30.0].into_iter().map(|v| v * MMOL_FACTOR).find(|v| *v >= need).unwrap_or(33.0 * MMOL_FACTOR)
    } else {
        [250.0, 300.0, 350.0, 400.0, 450.0, 500.0].into_iter().find(|v| *v >= need).unwrap_or(550.0)
    }
}

/// Axis ticks in mg/dL (every 50 mg/dL or 3 mmol/L) below `ymax`.
fn ticks(f: &Fmt, ymax: f32) -> Vec<f32> {
    let step = if f.mmol { 3.0 * MMOL_FACTOR } else { 50.0 };
    (1..).map(|i| i as f32 * step).take_while(|v| *v < ymax - 1.0).collect()
}

/// Frame of a glucose chart: target band, grid, y labels. Returns the y mapping.
#[allow(clippy::too_many_arguments)]
/// `label_every`: label every n-th grid line (0: none).
fn glucose_frame(doc: &mut Doc, f: &Fmt, x: f32, y: f32, w: f32, h: f32, ymax: f32, lo: f32, hi: f32, label_every: usize) -> impl Fn(f32) -> f32 + use<> {
    let fy = move |v: f32| y + h * (1.0 - v.min(ymax) / ymax);
    doc.rect(x, fy(hi), w, fy(lo) - fy(hi), tint(LEVELS[2], 0.14));
    for (i, v) in ticks(f, ymax).into_iter().enumerate() {
        doc.line(x, fy(v), x + w, fy(v), 0.4, GRID);
        if label_every > 0 && (i + 1) % label_every == 0 {
            doc.text(x - 5.0, fy(v) + 2.5, 7.0, false, SOFT, Align::Right, &f.num(v));
        }
    }
    doc.line(x, fy(hi), x + w, fy(hi), 0.6, tint(LEVELS[2], 0.6));
    doc.line(x, fy(lo), x + w, fy(lo), 0.6, tint(LEVELS[2], 0.6));
    doc.line(x, y + h, x + w, y + h, 0.6, GRID);
    fy
}

fn agp_page(doc: &mut Doc, r: &Report, opts: &PdfOptions, f: &Fmt) {
    doc.new_page();
    let mut y = header(doc, opts, "");
    doc.heading(M, y, "Ambulatory glucose profile (AGP)");
    y = doc.paragraph(M, y + 14.0, PW - 2.0 * M, 8.5, SOFT, "Median (line) with the 25–75 % (dark) and 5–95 % (light) percentiles of all days, as if they happened in a single day.");
    let (x, w, h) = (M + 30.0, PW - 2.0 * M - 30.0, 300.0);
    let top = y + 4.0;
    let p95 = r.agp.iter().map(|a| a.p95).fold(0.0, f32::max);
    let ymax = axis_max(f, p95.max(opts.target_high + 40.0) + 10.0).max(if f.mmol { 21.0 * MMOL_FACTOR } else { 350.0 });
    let fy = glucose_frame(doc, f, x, top, w, h, ymax, opts.target_low, opts.target_high, 1);
    doc.text(x - 5.0, top + 3.0, 7.0, false, SOFT, Align::Right, f.unit());
    let fx = |minute: u32| x + w * minute as f32 / 1440.0;
    for i in 0..=8 {
        let hx = x + w * i as f32 / 8.0;
        if i > 0 && i < 8 {
            doc.line(hx, top, hx, top + h, 0.4, GRID);
        }
        doc.text(hx, top + h + 11.0, 7.5, false, SOFT, Align::Center, &format!("{:02}:00", (i * 3) % 24));
    }
    if r.agp.len() >= 2 {
        let band = |hi: fn(&stats::AgpSlot) -> f32, lo: fn(&stats::AgpSlot) -> f32| {
            let mut pts: Vec<(f32, f32)> = r.agp.iter().map(|a| (fx(a.minute), fy(hi(a)))).collect();
            pts.extend(r.agp.iter().rev().map(|a| (fx(a.minute), fy(lo(a)))));
            pts
        };
        doc.polygon(&band(|a| a.p95, |a| a.p5), tint(ACCENT, 0.25));
        doc.polygon(&band(|a| a.p75, |a| a.p25), tint(ACCENT, 0.55));
        let median: Vec<Option<(f32, f32)>> = r.agp.iter().map(|a| Some((fx(a.minute), fy(a.p50)))).collect();
        doc.polyline(&median, 1.8, hex(0x0b2643));
    }
    y = top + h + 40.0;

    doc.heading(M, y, "Glucose pattern insights");
    y += 22.0;
    let label_w = 140.0;
    let col = (PW - 2.0 * M - label_w) / r.insights.len().max(1) as f32;
    for (i, ins) in r.insights.iter().enumerate() {
        doc.text(M + label_w + col * (i as f32 + 0.5), y, 8.5, false, SOFT, Align::Center, ins.label);
    }
    type Row = (&'static str, fn(&stats::PeriodInsight) -> Light);
    let rows: [Row; 3] = [
        ("Likelihood of low", |i| i.low_likelihood),
        ("Median vs. goal", |i| i.median_vs_goal),
        ("Variability below median", |i| i.variability_below_median),
    ];
    for (ri, (label, get)) in rows.iter().enumerate() {
        let ry = y + 14.0 + ri as f32 * 30.0;
        if ri % 2 == 0 {
            doc.rect(M, ry, PW - 2.0 * M, 30.0, hex(0xf3f4f7));
        }
        doc.text(M + 8.0, ry + 18.0, 9.0, true, INK, Align::Left, label);
        for (i, ins) in r.insights.iter().enumerate() {
            let light = get(ins);
            let cx = M + label_w + col * (i as f32 + 0.5);
            doc.circle(cx, ry + 15.0, 9.0, light_color(light));
            let mark = match light {
                Light::Green => "OK",
                Light::None => "–",
                _ => "!",
            };
            doc.text(cx, ry + 18.0, 7.5, true, if light == Light::None { SOFT } else { hex(0xffffff) }, Align::Center, mark);
        }
    }
    y += 14.0 + 3.0 * 30.0 + 18.0;
    doc.paragraph(M, y, PW - 2.0 * M, 8.0, SOFT, "Green: no pattern found, yellow: moderate, red: high. A simplified, informational approximation of LibreView's pattern insights – not medical advice.");
}

/// Notes of one local day, oldest first.
fn day_notes<'a>(notes: &'a [Note], date: NaiveDate) -> impl Iterator<Item = &'a Note> + 'a {
    notes.iter().filter(move |n| !n.deleted && n.at().with_timezone(&Local).date_naive() == date)
}

fn note_color(n: &Note) -> Rgb {
    match (n.treatment.is_some(), n.carbs.is_some()) {
        (true, false) => INSULIN,
        (_, true) => CARBS,
        _ => OTHER,
    }
}

fn totals_line(d: &DayTotals) -> String {
    let mut parts = Vec::new();
    if d.rapid_u > 0.0 {
        parts.push(format!("Rapid {} u", fmt_units(d.rapid_u)));
    }
    if d.long_u > 0.0 {
        parts.push(format!("Long {} u", fmt_units(d.long_u)));
    }
    parts.extend(d.custom.iter().map(|(name, u)| format!("{name} {} u", fmt_units(*u))));
    if d.carbs_g > 0.0 {
        parts.push(format!("Carbs {} g", fmt_units(d.carbs_g.round())));
    }
    parts.join(" · ")
}

fn daily_pages(doc: &mut Doc, r: &Report, notes: &[Note], totals: &[DayTotals], opts: &PdfOptions, f: &Fmt) {
    let totals_of = |d: NaiveDate| totals.iter().find(|t| t.date == d);
    let days: Vec<&stats::DayProfile> = r
        .daily
        .iter()
        .filter(|d| d.points.len() > 1 || totals_of(d.date).is_some_and(|t| t.has_entries))
        .collect();
    let (lo, hi) = (opts.target_low, opts.target_high);
    let ymax = if f.mmol { 21.0 * MMOL_FACTOR } else { 350.0 };
    for (page, chunk) in days.chunks(7).enumerate() {
        doc.new_page();
        let pages = days.len().div_ceil(7);
        let mut y = header(doc, opts, "");
        doc.heading(M, y, &if pages > 1 { format!("Daily glucose profiles ({} of {pages})", page + 1) } else { "Daily glucose profiles".to_string() });
        y += 18.0;
        let (x, w, h) = (M + 120.0, PW - 2.0 * M - 120.0, 62.0);
        for d in chunk {
            let top = y;
            // date, share in range
            doc.text(M, top + 12.0, 10.5, true, INK, Align::Left, &d.date.format("%a %d %b").to_string());
            if d.points.len() > 1 {
                let in_range = d.points.iter().filter(|(_, v)| (lo..=hi).contains(v)).count() as f32 * 100.0 / d.points.len() as f32;
                let color = if in_range >= 70.0 { LEVELS[2] } else if in_range >= 50.0 { hex(0xb07800) } else { LEVELS[1] };
                doc.text(M, top + 26.0, 9.5, true, color, Align::Left, &format!("{in_range:.0} % in range"));
                let mean = d.points.iter().map(|p| p.1).sum::<f32>() / d.points.len() as f32;
                doc.text(M, top + 38.0, 8.0, false, SOFT, Align::Left, &format!("avg {}", f.val(mean)));
            } else {
                doc.text(M, top + 26.0, 8.5, false, SOFT, Align::Left, "no readings");
            }
            let fy = glucose_frame(doc, f, x, top, w, h, ymax, lo, hi, 2);
            for hh in [6, 12, 18] {
                let hx = x + w * hh as f32 / 24.0;
                doc.line(hx, top, hx, top + h, 0.4, GRID);
            }
            // the course of the day; gaps over 30 minutes break the line
            let mut pts: Vec<Option<(f32, f32)>> = Vec::with_capacity(d.points.len());
            let mut last: Option<f32> = None;
            for (fx, v) in &d.points {
                if last.is_some_and(|l| fx - l > 30.0 / 1440.0) {
                    pts.push(None);
                }
                pts.push(Some((x + w * fx, fy(*v))));
                last = Some(*fx);
            }
            doc.polyline(&pts, 0.9, ACCENT);
            // note markers below the chart
            for n in day_notes(notes, d.date) {
                let local = n.at().with_timezone(&Local);
                let nx = x + w * (local.num_seconds_from_midnight() as f32 / 86400.0);
                doc.polygon(&[(nx, top + h + 1.0), (nx - 3.0, top + h + 6.0), (nx + 3.0, top + h + 6.0)], note_color(n));
            }
            for (i, label) in ["00", "06", "12", "18", "24"].iter().enumerate() {
                doc.text(x + w * i as f32 / 4.0, top + h + 14.0, 6.5, false, SOFT, Align::Center, label);
            }
            if let Some(t) = totals_of(d.date).filter(|t| t.has_entries) {
                for (i, line) in doc.wrap(&totals_line(t), 92.0, 7.5, false).into_iter().take(3).enumerate() {
                    doc.text(M, top + 50.0 + i as f32 * 9.0, 7.5, false, INK, Align::Left, &line);
                }
            }
            y += h + 32.0;
        }
        doc.paragraph(M, y, PW - 2.0 * M, 7.5, SOFT, &format!(
            "Fixed scale 0–{} {}. Markers under a day: blue insulin, orange carbs, grey other notes.",
            f.num(ymax),
            f.unit()
        ));
    }
}

fn notes_pages(doc: &mut Doc, notes: &[Note]) {
    let notes: Vec<&Note> = notes.iter().filter(|n| !n.deleted).collect();
    if notes.is_empty() {
        return;
    }
    let bottom = PH - 60.0;
    let mut y = bottom;
    let mut day: Option<NaiveDate> = None;
    for n in notes {
        let local = n.at().with_timezone(&Local);
        let lines = doc.wrap(&n.summary(), PW - 2.0 * M - 70.0, 9.0, false);
        let new_day = day != Some(local.date_naive());
        let need = lines.len() as f32 * 12.0 + if new_day { 26.0 } else { 4.0 };
        if y + need > bottom {
            doc.new_page();
            doc.text(M, 58.0, 20.0, true, INK, Align::Left, "Notes");
            doc.line(M, 72.0, PW - M, 72.0, 0.8, GRID);
            y = 96.0;
            day = None;
        }
        if day != Some(local.date_naive()) {
            day = Some(local.date_naive());
            doc.text(M, y, 10.0, true, INK, Align::Left, &local.format("%A, %d %B %Y").to_string());
            y += 16.0;
        }
        doc.circle(M + 3.0, y - 3.0, 2.5, note_color(n));
        doc.text(M + 10.0, y, 9.0, false, SOFT, Align::Left, &local.format("%H:%M").to_string());
        for line in lines {
            doc.text(M + 70.0, y, 9.0, false, INK, Align::Left, &line);
            y += 12.0;
        }
        y += 4.0;
    }
}

/// Number of pages in a PDF made by [`render`] (for tests and the preview command).
pub fn page_count(pdf: &[u8]) -> usize {
    pdf.windows(11).filter(|w| *w == b"/Type /Page").count() - pdf.windows(12).filter(|w| *w == b"/Type /Pages").count()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration, Utc};

    fn demo_pdf(days: i64) -> Vec<u8> {
        let now = Utc::now();
        let from = now - Duration::days(days);
        let readings = crate::demo::readings(from, now, 900);
        let report = stats::build_report(&readings, from, now, 70.0, 180.0, 154.0);
        let notes: Vec<Note> = crate::demo::notes(from, now)
            .into_iter()
            .enumerate()
            .map(|(i, (t, text, treatment, carbs))| Note {
                id: i.to_string(),
                time: t.timestamp(),
                text,
                updated: 0,
                deleted: false,
                treatment,
                carbs,
                extra: Default::default(),
            })
            .collect();
        let totals = stats::daily_totals(&notes, from, now);
        let opts = PdfOptions {
            title: "Glucose report".into(),
            patient: "Demo Patient".into(),
            period: format!("{} – {}", from.format("%d %b %Y"), now.format("%d %b %Y")),
            generated: now.format("%Y-%m-%d %H:%M").to_string(),
            mmol: true,
            target_low: 70.0,
            target_high: 180.0,
            lang: "en".into(),
        };
        render(&report, &notes, &totals, &opts).unwrap()
    }

    #[test]
    fn renders_14_and_90_days() {
        let short = demo_pdf(14);
        assert!(short.starts_with(b"%PDF-"));
        // summary, AGP, 15 days of profiles on 3 pages, notes
        let pages = page_count(&short);
        assert!((6..=8).contains(&pages), "{pages}");
        let long = demo_pdf(90);
        assert!(page_count(&long) > pages + 10);
        // the fonts are subset: far smaller than the two embedded files
        assert!(short.len() < 400_000, "{}", short.len());
        if let Ok(dir) = std::env::var("GLLU_PDF_OUT") {
            std::fs::write(std::path::Path::new(&dir).join("report-14.pdf"), &short).unwrap();
            std::fs::write(std::path::Path::new(&dir).join("report-90.pdf"), &long).unwrap();
        }
    }
}
