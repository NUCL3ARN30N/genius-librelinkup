//! Turns readings and reports into the plain data the Slint UI renders (paths, labels, rows).
//! Paths use a 1000×400 viewbox, label positions are fractions of the plot area.

use chrono::{DateTime, Datelike, Duration, Local, NaiveDate, Timelike, Utc};
use slint::SharedString;

use crate::api::Reading;
use crate::stats::{self, Light, Report};
use crate::store::{Note, Treatment, TreatmentKind, check_carbs, fmt_units};
use crate::units::{Units, level};
use gllu_core::i18n;
use gllu_core::tr;
use crate::{AxisLabel, DayBar, DayCard, Dot, GraphPoint, Insight, NoteMark, StatItem, TirRow};

const VW: f32 = 1000.0;
const VH: f32 = 400.0;

fn s(v: impl Into<SharedString>) -> SharedString {
    v.into()
}

/// A label/value row; decimals in the value follow the language (decimal comma in German).
pub fn stat(label: &str, value: impl Into<SharedString>, hint: &str) -> StatItem {
    StatItem { label: s(label), value: s(i18n::numbers(&value.into())), hint: s(hint) }
}

/// Upper bound of the y axis in mg/dL, on "nice" steps of the shown unit.
fn y_max(units: Units, max_value: f32, target_high: f32) -> f32 {
    let need = max_value.max(target_high + 40.0) + 10.0;
    if units.mg_dl {
        [250.0, 300.0, 350.0, 400.0, 450.0, 500.0].into_iter().find(|v| *v >= need).unwrap_or(550.0)
    } else {
        [15.0, 18.0, 21.0, 24.0, 27.0, 30.0]
            .into_iter()
            .map(|v| v * crate::store::MMOL_FACTOR)
            .find(|v| *v >= need)
            .unwrap_or(33.0 * crate::store::MMOL_FACTOR)
    }
}

fn y_labels(units: Units, ymax: f32) -> Vec<AxisLabel> {
    let ticks: Vec<f32> = if units.mg_dl {
        (1..=((ymax / 50.0) as i32)).map(|i| i as f32 * 50.0).collect()
    } else {
        let max_mmol = (ymax / crate::store::MMOL_FACTOR).round() as i32;
        (1..=max_mmol / 3).map(|i| i as f32 * 3.0 * crate::store::MMOL_FACTOR).collect()
    };
    ticks
        .into_iter()
        .filter(|v| *v < ymax - 1.0)
        .map(|v| AxisLabel { pos: 1.0 - v / ymax, text: s(units.fmt(v)) })
        .chain(std::iter::once(AxisLabel { pos: 0.0, text: s(units.fmt(ymax)) }))
        .collect()
}

/// Polyline through the readings; gaps longer than `max_gap` start a new segment.
fn polyline(points: impl Iterator<Item = (f32, f32, DateTime<Utc>)>, max_gap: Duration) -> String {
    let mut out = String::new();
    let mut last: Option<DateTime<Utc>> = None;
    for (x, y, t) in points {
        let cmd = match last {
            Some(prev) if t - prev <= max_gap => 'L',
            _ => 'M',
        };
        out.push_str(&format!("{cmd}{:.1} {:.1} ", x * VW, y * VH));
        last = Some(t);
    }
    out
}

/// Time format of the note dialog (also accepted when parsing it back, see [`parse_note_time`]).
pub const NOTE_TIME_FORMAT: &str = "%Y-%m-%d %H:%M";

pub fn note_time_text(t: DateTime<Utc>) -> String {
    t.with_timezone(&Local).format(NOTE_TIME_FORMAT).to_string()
}

/// Parses the note dialog's time: a full local date and time, or only `HH:MM` for today.
pub fn parse_note_time(text: &str) -> Option<DateTime<Utc>> {
    let text = text.trim();
    let local = ["%Y-%m-%d %H:%M", "%Y-%m-%d %H:%M:%S", "%d.%m.%Y %H:%M", "%Y-%m-%dT%H:%M"]
        .iter()
        .find_map(|f| chrono::NaiveDateTime::parse_from_str(text, f).ok())
        .or_else(|| {
            let time = chrono::NaiveTime::parse_from_str(text, "%H:%M").ok()?;
            Some(Local::now().date_naive().and_time(time))
        })?;
    local.and_local_timezone(Local).earliest().map(|t| t.with_timezone(&Utc))
}

/// Index of the entry whose x (sorted ascending) is nearest to `x`, if within `max_dx`.
pub fn nearest_index(len: usize, x_at: impl Fn(usize) -> f32, x: f32, max_dx: f32) -> i32 {
    let (mut lo, mut hi) = (0, len);
    while lo < hi {
        let mid = (lo + hi) / 2;
        if x_at(mid) < x { lo = mid + 1 } else { hi = mid }
    }
    [lo.checked_sub(1), (lo < len).then_some(lo)]
        .into_iter()
        .flatten()
        .map(|i| (i, (x_at(i) - x).abs()))
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .filter(|(_, d)| *d <= max_dx)
        .map(|(i, _)| i as i32)
        .unwrap_or(-1)
}

pub fn note_mark(n: &Note, x: f32, time_format: &str) -> NoteMark {
    let t = n.at().with_timezone(&Local);
    let (treatment, treatment_name, units) = match &n.treatment {
        None => (0, String::new(), String::new()),
        Some(tr) => (
            match tr.kind {
                TreatmentKind::RapidActing => 1,
                TreatmentKind::LongActing => 2,
                TreatmentKind::Custom => 3,
            },
            tr.name.clone(),
            tr.units.map(fmt_units).unwrap_or_default(),
        ),
    };
    let insulin = n.treatment.is_some();
    NoteMark {
        x,
        id: s(n.id.clone()),
        time: s(i18n::date(t.format(time_format))),
        when: s(t.format(NOTE_TIME_FORMAT).to_string()),
        text: s(n.summary()),
        note: s(n.text.clone()),
        treatment,
        treatment_name: s(treatment_name),
        units: s(units),
        carbs: s(n.carbs.map(fmt_units).unwrap_or_default()),
        kind: match (insulin, n.carbs.is_some()) {
            (false, false) => 0,
            (true, false) => 1,
            (false, true) => 2,
            (true, true) => 3,
        },
    }
}

/// Carbs as typed in the note dialog (grams, decimal point or comma); empty is none.
pub fn carbs_from_ui(text: &str) -> anyhow::Result<Option<f32>> {
    let text = text.trim();
    if text.is_empty() {
        return Ok(None);
    }
    let g = text.replace(',', ".").parse::<f32>().map_err(|_| anyhow::anyhow!(tr!("enter the carbs in grams, e.g. 45")))?;
    check_carbs(Some(g))
}

/// "Rapid 12 u · Long 18 u · Afrezza 4 u · Carbs 160 g"; empty for a day without entries.
pub fn totals_text(d: &stats::DayTotals) -> String {
    let mut parts = Vec::new();
    if d.rapid_u > 0.0 {
        parts.push(tr!("Rapid {} u", fmt_units(d.rapid_u)));
    }
    if d.long_u > 0.0 {
        parts.push(tr!("Long {} u", fmt_units(d.long_u)));
    }
    parts.extend(d.custom.iter().map(|(name, u)| tr!("{} {} u", name, fmt_units(*u))));
    if d.carbs_g > 0.0 {
        parts.push(tr!("Carbs {} g", fmt_units(d.carbs_g.round())));
    }
    i18n::numbers(&parts.join(" · "))
}

/// Rounds an average for display: one decimal below 10, whole numbers above.
fn fmt_avg(v: f32) -> String {
    if v < 10.0 { fmt_units((v * 10.0).round() / 10.0) } else { fmt_units(v.round()) }
}

/// The report's "Insulin & carbs" card: averages and one bar pair per day. Both are empty when
/// nothing was recorded in the period.
pub fn insulin_carbs(days: &[stats::DayTotals]) -> (Vec<StatItem>, Vec<DayBar>) {
    let sum = stats::summarize_totals(days);
    if sum.days_with_entries == 0 {
        return (Vec::new(), Vec::new());
    }
    let mut items = Vec::new();
    if sum.avg_rapid_u > 0.0 {
        items.push(stat(tr!("Rapid-acting insulin"), tr!("{} u/day", fmt_avg(sum.avg_rapid_u)), ""));
    }
    if sum.avg_long_u > 0.0 {
        items.push(stat(tr!("Long-acting insulin"), tr!("{} u/day", fmt_avg(sum.avg_long_u)), ""));
    }
    if sum.avg_rapid_u > 0.0 && sum.avg_long_u > 0.0 {
        let basal = sum.avg_long_u / sum.avg_insulin_u() * 100.0;
        items.push(stat(tr!("Total insulin"), tr!("{} u/day", fmt_avg(sum.avg_insulin_u())), &tr!("{} % long-acting", format!("{basal:.0}"))));
    }
    for (name, u) in &sum.avg_custom {
        items.push(stat(name, tr!("{} u/day", fmt_avg(*u)), ""));
    }
    if sum.avg_carbs_g > 0.0 {
        items.push(stat(tr!("Carbohydrates"), tr!("{} g/day", fmt_units(sum.avg_carbs_g.round())), ""));
    }
    items.push(stat(tr!("Days with entries"), tr!("{} of {}", sum.days_with_entries, sum.days), ""));

    let max_u = days.iter().map(stats::DayTotals::insulin_u).fold(0.0, f32::max).max(1.0);
    let max_g = days.iter().map(|d| d.carbs_g).fold(0.0, f32::max).max(1.0);
    let bars = days
        .iter()
        .map(|d| {
            let totals = totals_text(d);
            DayBar {
                label: s(i18n::date(d.date.format("%d %b"))),
                insulin: d.insulin_u() / max_u,
                carbs: d.carbs_g / max_g,
                text: s(format!(
                    "{} · {}",
                    i18n::date(d.date.format("%a %d %b")),
                    if totals.is_empty() { tr!("no entries").to_string() } else { totals }
                )),
            }
        })
        .collect();
    (items, bars)
}

/// The treatment chosen in the note dialog (index as in `Notes.treatments`) with the units as
/// typed (decimal point or comma).
pub fn treatment_from_ui(index: i32, name: &str, units: &str) -> anyhow::Result<Option<Treatment>> {
    let units = units.trim();
    let units = if units.is_empty() {
        None
    } else {
        Some(units.replace(',', ".").parse::<f32>().map_err(|_| anyhow::anyhow!(tr!("enter the units as a number, e.g. 4.5")))?)
    };
    let kind = match index {
        1 => TreatmentKind::RapidActing,
        2 => TreatmentKind::LongActing,
        3 => TreatmentKind::Custom,
        _ if units.is_some() => anyhow::bail!(tr!("choose a treatment for the units")),
        _ => return Ok(None),
    };
    Treatment::new(kind, name, units).map(Some)
}

pub struct GraphModel {
    pub path: String,
    pub dots: Vec<Dot>,
    pub points: Vec<GraphPoint>,
    pub notes: Vec<NoteMark>,
    pub y_labels: Vec<AxisLabel>,
    pub x_labels: Vec<AxisLabel>,
    pub band_top: f32,
    pub band_bottom: f32,
}

/// Dashboard graph for the last `hours`. `style`: 0 line, 1 dots, 2 line + dots. The last dot is
/// always the current reading.
pub fn graph(
    readings: &[Reading],
    notes: &[Note],
    units: Units,
    target_low: f32,
    target_high: f32,
    hours: i64,
    style: u8,
) -> GraphModel {
    let now = Utc::now();
    let label_every = match hours {
        0..=6 => 1,
        7..=12 => 2,
        _ => 4,
    };
    graph_between(readings, notes, units, (target_low, target_high), now - Duration::hours(hours), now, style, label_every, true)
}

/// Graph of `[start, end)` with x labels every `label_every` local hours. `mark_last` adds the
/// last reading as a (larger) dot even in line style.
#[allow(clippy::too_many_arguments)]
fn graph_between(
    readings: &[Reading],
    notes: &[Note],
    units: Units,
    (target_low, target_high): (f32, f32),
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    style: u8,
    label_every: u32,
    mark_last: bool,
) -> GraphModel {
    let now = end;
    let span = (now - start).num_seconds() as f32;
    let visible: Vec<&Reading> = readings.iter().filter(|r| r.time >= start && r.time < end).collect();
    let max_value = visible.iter().map(|r| r.mg_dl).fold(0.0, f32::max);
    let ymax = y_max(units, max_value, target_high);
    let fx = |t: DateTime<Utc>| ((t - start).num_seconds() as f32 / span).clamp(0.0, 1.0);
    let fy = |v: f32| (1.0 - v.min(ymax) / ymax).clamp(0.0, 1.0);

    let path = polyline(
        visible.iter().map(|r| (fx(r.time), fy(r.mg_dl), r.time)),
        Duration::minutes(20),
    );
    let dot = |r: &Reading| Dot { x: fx(r.time), y: fy(r.mg_dl), level: level(r.mg_dl, target_low, target_high) };
    let mut dots: Vec<Dot> = Vec::new();
    if style != 0 {
        // at most one dot per 5 minutes so 1-minute live polling stays readable
        let mut last_slot = i64::MIN;
        let upto = if mark_last { visible.len().saturating_sub(1) } else { visible.len() };
        for r in &visible[..upto] {
            let slot = r.time.timestamp() / 300;
            if slot != last_slot {
                dots.push(dot(r));
                last_slot = slot;
            }
        }
    }
    if mark_last {
        dots.extend(visible.last().map(|r| dot(r)));
    }

    // hover targets; times of other days than the last one get the weekday
    let last_day = (end - Duration::seconds(1)).with_timezone(&Local).date_naive();
    let time_format = |t: DateTime<Utc>| if t.with_timezone(&Local).date_naive() == last_day { "%H:%M" } else { "%a %H:%M" };
    let points = visible
        .iter()
        .map(|r| GraphPoint {
            x: fx(r.time),
            y: fy(r.mg_dl),
            time: s(i18n::date(r.time.with_timezone(&Local).format(time_format(r.time)))),
            value: s(units.fmt_unit(r.mg_dl)),
            level: level(r.mg_dl, target_low, target_high),
            when: s(note_time_text(r.time)),
        })
        .collect();
    let notes = notes
        .iter()
        .filter(|n| n.at() >= start && n.at() < end)
        .map(|n| note_mark(n, fx(n.at()), time_format(n.at())))
        .collect();

    // x labels on full local hours, spaced so they don't crowd
    let local_start = start.with_timezone(&Local);
    let mut t = local_start
        .with_minute(0)
        .and_then(|t| t.with_second(0))
        .unwrap_or(local_start)
        + Duration::hours(1);
    let mut x_labels = Vec::new();
    while t.with_timezone(&Utc) < now {
        if t.hour() % label_every == 0 {
            x_labels.push(AxisLabel { pos: fx(t.with_timezone(&Utc)), text: s(t.format("%H:%M").to_string()) });
        }
        t += Duration::hours(1);
    }

    GraphModel {
        path,
        dots,
        points,
        notes,
        y_labels: y_labels(units, ymax),
        x_labels,
        band_top: fy(target_high),
        band_bottom: fy(target_low),
    }
}

fn light(l: Light) -> i32 {
    match l {
        Light::None => 0,
        Light::Green => 1,
        Light::Yellow => 2,
        Light::Red => 3,
    }
}

pub struct ReportModel {
    pub title: String,
    pub subtitle: String,
    pub stats: Vec<StatItem>,
    pub tir: Vec<TirRow>,
    pub outer: String,
    pub inner: String,
    pub median: String,
    pub y_labels: Vec<AxisLabel>,
    pub x_labels: Vec<AxisLabel>,
    pub band_top: f32,
    pub band_bottom: f32,
    pub insights: Vec<Insight>,
    pub days: Vec<DayCell>,
    pub day_band_top: f32,
    pub day_band_bottom: f32,
    pub ic_stats: Vec<StatItem>,
    pub ic_days: Vec<DayBar>,
}

fn tir_rows(r: &Report, u: Units, target_low: f32, target_high: f32) -> Vec<TirRow> {
    let below = |v: f32| v - if u.mg_dl { 1.0 } else { 0.1 * crate::store::MMOL_FACTOR };
    let tir_row = |label: &str, range: String, pct: f32, lvl: i32| TirRow {
        label: s(label),
        range: s(range),
        pct,
        pct_text: s(format!("{:.0} %", pct)),
        duration: s(stats::share_of_day(pct)),
        level: lvl,
    };
    vec![
        tir_row(tr!("Very high"), format!("> {}", u.fmt_unit(stats::VERY_HIGH)), r.tir.very_high, 5),
        tir_row(
            tr!("High"),
            format!("{} – {}", u.fmt(target_high + 1.0), u.fmt_unit(stats::VERY_HIGH)),
            r.tir.high,
            4,
        ),
        tir_row(tr!("Target range"), format!("{} – {}", u.fmt(target_low), u.fmt_unit(target_high)), r.tir.target, 3),
        tir_row(tr!("Low"), format!("{} – {}", u.fmt(stats::VERY_LOW), u.fmt_unit(below(target_low))), r.tir.low, 2),
        tir_row(tr!("Very low"), format!("< {}", u.fmt_unit(stats::VERY_LOW)), r.tir.very_low, 1),
    ]
}

/// Identifies a local date in the UI (calendar cells, day details).
pub fn day_key(d: NaiveDate) -> i32 {
    d.num_days_from_ce()
}

pub fn day_from_key(key: i32) -> Option<NaiveDate> {
    NaiveDate::from_num_days_from_ce_opt(key)
}

/// A [`DayCard`] as plain data: its models are created on the UI thread ([`DayCell::into_card`]).
#[derive(Default)]
pub struct DayCell {
    pub key: i32,
    pub day: SharedString,
    pub month: SharedString,
    pub path: SharedString,
    pub has_data: bool,
    pub in_range: SharedString,
    pub level: i32,
    pub points: Vec<GraphPoint>,
    pub notes: Vec<NoteMark>,
}

impl DayCell {
    pub fn into_card(self) -> DayCard {
        DayCard {
            key: self.key,
            day: self.day,
            month: self.month,
            path: self.path,
            has_data: self.has_data,
            in_range: self.in_range,
            level: self.level,
            points: slint::ModelRc::new(slint::VecModel::from(self.points)),
            notes: slint::ModelRc::new(slint::VecModel::from(self.notes)),
        }
    }
}

/// Calendar cells (Monday first) from the first to the last day with data, padded to whole weeks.
fn calendar(
    daily: &[stats::DayProfile],
    notes: &[Note],
    units: Units,
    dfy: &dyn Fn(f32) -> f32,
    target_low: f32,
    target_high: f32,
) -> Vec<DayCell> {
    let with_data = |d: &&stats::DayProfile| d.points.len() > 1;
    let (Some(first), Some(last)) = (daily.iter().position(|d| with_data(&d)), daily.iter().rposition(|d| with_data(&d)))
    else {
        return Vec::new();
    };
    let days = &daily[first..=last];
    let pad = |n: usize| (0..n).map(|_| DayCell { key: -1, ..Default::default() });
    let mut cells: Vec<DayCell> = pad(days[0].date.weekday().num_days_from_monday() as usize).collect();
    for (i, d) in days.iter().enumerate() {
        let has_data = d.points.len() > 1;
        let mut path = String::new();
        let mut last_x: Option<f32> = None;
        for (fx, v) in &d.points {
            let cmd = match last_x {
                Some(prev) if fx - prev < 30.0 / 1440.0 => 'L',
                _ => 'M',
            };
            path.push_str(&format!("{cmd}{:.1} {:.1} ", fx * VW, dfy(*v) * VH));
            last_x = Some(*fx);
        }
        // hover targets, at most one per 5 minutes
        let mut points: Vec<GraphPoint> = Vec::new();
        let mut last_slot = -1;
        for (fx, v) in &d.points {
            let slot = (fx * 288.0) as i32;
            if slot != last_slot {
                let minute = (fx * 1440.0) as u32;
                points.push(GraphPoint {
                    x: *fx,
                    y: dfy(*v),
                    time: s(format!("{:02}:{:02}", minute / 60, minute % 60)),
                    value: s(units.fmt(*v)),
                    level: level(*v, target_low, target_high),
                    when: SharedString::default(),
                });
                last_slot = slot;
            }
        }
        let day_notes: Vec<NoteMark> = notes
            .iter()
            .filter(|n| n.at().with_timezone(&Local).date_naive() == d.date)
            .map(|n| {
                let local = n.at().with_timezone(&Local);
                note_mark(n, local.num_seconds_from_midnight() as f32 / 86400.0, "%H:%M")
            })
            .collect();
        let in_range = if d.points.is_empty() {
            0.0
        } else {
            d.points.iter().filter(|(_, v)| (target_low..=target_high).contains(v)).count() as f32 * 100.0
                / d.points.len() as f32
        };
        cells.push(DayCell {
            key: day_key(d.date),
            day: s(d.date.day().to_string()),
            // month name on the first shown day and on every 1st
            month: s(if i == 0 || d.date.day() == 1 { i18n::date(d.date.format("%b")) } else { String::new() }),
            path: s(path),
            has_data,
            in_range: s(if has_data { format!("{in_range:.0} %") } else { String::new() }),
            // goal > 70 % in range; below 50 % is shown like a high
            level: if in_range >= 70.0 { 3 } else if in_range >= 50.0 { 4 } else { 2 },
            points,
            notes: day_notes,
        });
    }
    let rest = (7 - cells.len() % 7) % 7;
    cells.extend(pad(rest));
    cells
}

pub struct DayDetailModel {
    pub title: String,
    pub graph: GraphModel,
    pub stats: Vec<StatItem>,
    pub tir: Vec<TirRow>,
    /// insulin and carbs of the day, see [`totals_text`]
    pub totals: String,
}

/// One local day `[start, end)` for the details dialog.
#[allow(clippy::too_many_arguments)]
pub fn day_detail(
    date: NaiveDate,
    readings: &[Reading],
    notes: &[Note],
    (start, end): (DateTime<Utc>, DateTime<Utc>),
    u: Units,
    target_low: f32,
    target_high: f32,
    style: u8,
) -> DayDetailModel {
    let r = stats::build_report(readings, start, end, target_low, target_high, 154.0);
    let min = readings.iter().map(|r| r.mg_dl).fold(f32::MAX, f32::min);
    let max = readings.iter().map(|r| r.mg_dl).fold(0.0, f32::max);
    let stats = if readings.is_empty() {
        vec![stat(tr!("Readings"), "0", "")]
    } else {
        vec![
            stat(tr!("Average"), u.fmt_unit(r.mean), ""),
            stat(tr!("Lowest"), u.fmt_unit(min), ""),
            stat(tr!("Highest"), u.fmt_unit(max), ""),
            stat(tr!("Standard deviation"), u.fmt_unit(r.sd), ""),
            stat(tr!("Variability (CV)"), format!("{:.1} %", r.cv_pct), ""),
            stat(tr!("% time CGM is active"), format!("{:.0} %", r.sensor_active_pct), ""),
            stat(tr!("Readings"), readings.len().to_string(), ""),
        ]
    };
    DayDetailModel {
        title: i18n::date(date.format("%A, %d %B %Y")),
        graph: graph_between(readings, notes, u, (target_low, target_high), start, end, style, 3, false),
        stats,
        tir: tir_rows(&r, u, target_low, target_high),
        totals: stats::daily_totals(notes, start, end - Duration::seconds(1)).first().map(totals_text).unwrap_or_default(),
    }
}

pub fn report_model(r: &Report, notes: &[Note], units: Units, target_low: f32, target_high: f32) -> ReportModel {
    let fmt_date = |t: Option<DateTime<Utc>>| {
        t.map(|t| i18n::date(t.with_timezone(&Local).format("%d %b %Y"))).unwrap_or_default()
    };
    let subtitle = format!(
        "{} – {} ({}, {})",
        fmt_date(r.from),
        fmt_date(r.to),
        gllu_core::count(r.days as usize, "day"),
        gllu_core::count(r.readings, "reading")
    );
    let u = units;
    let coverage_hint = if r.sensor_active_pct < 70.0 { tr!("less than 70 % – interpret with care") } else { "" };
    let stats = vec![
        stat(tr!("% time CGM is active"), format!("{:.1} %", r.sensor_active_pct), coverage_hint),
        stat(tr!("Average glucose"), u.fmt_unit(r.mean), ""),
        stat(
            tr!("Glucose management indicator (GMI)"),
            format!("{:.1} % · {:.0} mmol/mol", r.gmi_pct, r.gmi_mmol_mol),
            tr!("approximate HbA1c from average CGM glucose"),
        ),
        stat(
            tr!("Estimated HbA1c (eA1c)"),
            format!("{:.1} % · {:.0} mmol/mol", r.ea1c_pct, r.ea1c_mmol_mol),
            tr!("ADAG formula"),
        ),
        stat(tr!("Glucose variability (CV)"), format!("{:.1} %", r.cv_pct), tr!("goal ≤ 36 %")),
        stat(tr!("Standard deviation"), u.fmt_unit(r.sd), ""),
    ];

    let tir = tir_rows(r, u, target_low, target_high);

    // AGP
    let p95_max = r.agp.iter().map(|a| a.p95).fold(0.0, f32::max);
    let ymax = y_max(u, p95_max, target_high).max(if u.mg_dl { 350.0 } else { 21.0 * crate::store::MMOL_FACTOR });
    let fy = |v: f32| (1.0 - v.min(ymax) / ymax) * VH;
    let fx = |m: u32| m as f32 / 1440.0 * VW;
    let band = |hi: &dyn Fn(&stats::AgpSlot) -> f32, lo: &dyn Fn(&stats::AgpSlot) -> f32| {
        if r.agp.len() < 2 {
            return String::new();
        }
        let mut p = String::new();
        for (i, a) in r.agp.iter().enumerate() {
            p.push_str(&format!("{}{:.1} {:.1} ", if i == 0 { 'M' } else { 'L' }, fx(a.minute), fy(hi(a))));
        }
        for a in r.agp.iter().rev() {
            p.push_str(&format!("L{:.1} {:.1} ", fx(a.minute), fy(lo(a))));
        }
        p.push('Z');
        p
    };
    let outer = band(&|a| a.p95, &|a| a.p5);
    let inner = band(&|a| a.p75, &|a| a.p25);
    let median = if r.agp.len() < 2 {
        String::new()
    } else {
        r.agp
            .iter()
            .enumerate()
            .map(|(i, a)| format!("{}{:.1} {:.1}", if i == 0 { 'M' } else { 'L' }, fx(a.minute), fy(a.p50)))
            .collect::<Vec<_>>()
            .join(" ")
    };
    let x_labels = (0..=8)
        .map(|i| AxisLabel {
            pos: i as f32 / 8.0,
            text: s(match i * 3 {
                0 | 24 => "00:00".to_string(),
                h => format!("{h:02}:00"),
            }),
        })
        .collect();

    // daily profiles share one fixed scale
    let day_max = if u.mg_dl { 350.0 } else { 21.0 * crate::store::MMOL_FACTOR };
    let dfy = |v: f32| (1.0 - v.min(day_max) / day_max).clamp(0.0, 1.0);
    let days = calendar(&r.daily, notes, u, &dfy, target_low, target_high);
    let (ic_stats, ic_days) = match (r.from, r.to) {
        (Some(from), Some(to)) => insulin_carbs(&stats::daily_totals(notes, from, to)),
        _ => (Vec::new(), Vec::new()),
    };

    ReportModel {
        title: tr!("Glucose report").into(),
        subtitle,
        stats,
        tir,
        outer,
        inner,
        median,
        y_labels: y_labels(u, ymax),
        x_labels,
        band_top: fy(target_high) / VH,
        band_bottom: fy(target_low) / VH,
        insights: r
            .insights
            .iter()
            .map(|i| Insight {
                label: s(i18n::tr(i.label)),
                low: light(i.low_likelihood),
                median: light(i.median_vs_goal),
                variability: light(i.variability_below_median),
            })
            .collect(),
        days,
        day_band_top: dfy(target_high),
        day_band_bottom: dfy(target_low),
        ic_stats,
        ic_days,
    }
}

/// "just now", "4 min ago", "2 h ago"
pub fn age_text(t: DateTime<Utc>) -> String {
    let mins = (Utc::now() - t).num_minutes();
    match mins {
        i64::MIN..=0 => tr!("just now").into(),
        1..=59 => tr!("{} min ago", mins),
        60..=1439 => tr!("{} h ago", mins / 60),
        _ => i18n::date(t.with_timezone(&Local).format("%d %b %H:%M")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn calendar_starts_on_monday_and_trims_empty_days() {
        let day = |y, m, d, n: usize| stats::DayProfile {
            date: NaiveDate::from_ymd_opt(y, m, d).unwrap(),
            points: (0..n).map(|i| (i as f32 / 100.0, 120.0)).collect(),
        };
        // Tue 27 Oct 2026 has no data and is trimmed; Wed 28 .. Sat 31 with a gap on Thu
        let daily = vec![day(2026, 10, 27, 0), day(2026, 10, 28, 5), day(2026, 10, 29, 0), day(2026, 10, 30, 5)];
        let cells = calendar(&daily, &[], Units { mg_dl: true }, &|v| v / 400.0, 70.0, 180.0);
        assert_eq!(cells.len(), 7);
        assert_eq!(cells.iter().take_while(|c| c.key < 0).count(), 2); // Mon, Tue
        assert_eq!(cells[2].day, "28");
        assert_eq!(cells[2].month, "Oct");
        assert!(cells[2].has_data && !cells[3].has_data && cells[4].has_data);
        assert_eq!(cells[2].in_range, "100 %");
        assert!(cells[5..].iter().all(|c| c.key < 0));
        assert_eq!(day_from_key(cells[2].key), NaiveDate::from_ymd_opt(2026, 10, 28));
        assert!(calendar(&[day(2026, 10, 27, 0)], &[], Units { mg_dl: true }, &|v| v, 70.0, 180.0).is_empty());
    }

    #[test]
    fn nearest_index_respects_distance() {
        let xs = [0.1, 0.2, 0.5, 0.9];
        let at = |i: usize| xs[i];
        assert_eq!(nearest_index(4, at, 0.0, 0.2), 0);
        assert_eq!(nearest_index(4, at, 0.16, 0.2), 1);
        assert_eq!(nearest_index(4, at, 0.36, 0.2), 2);
        assert_eq!(nearest_index(4, at, 1.0, 0.2), 3);
        assert_eq!(nearest_index(4, at, 0.7, 0.1), -1);
        assert_eq!(nearest_index(0, at, 0.5, 1.0), -1);
    }

    #[test]
    fn parses_note_times() {
        let t = parse_note_time("2026-10-29 14:30").unwrap();
        assert_eq!(note_time_text(t), "2026-10-29 14:30");
        assert_eq!(parse_note_time(" 29.10.2026 14:30 "), Some(t));
        let today = parse_note_time("07:05").unwrap().with_timezone(&Local);
        assert_eq!((today.date_naive(), today.hour(), today.minute()), (Local::now().date_naive(), 7, 5));
        assert!(parse_note_time("tomorrow").is_none());
        assert!(parse_note_time("25:00").is_none());
    }

    #[test]
    fn insulin_and_carbs_card() {
        let date = NaiveDate::from_ymd_opt(2026, 10, 26).unwrap();
        let day = |rapid: f32, long: f32, carbs: f32| stats::DayTotals {
            date,
            rapid_u: rapid,
            long_u: long,
            custom: Vec::new(),
            carbs_g: carbs,
            has_entries: rapid + long + carbs > 0.0,
        };
        assert_eq!(insulin_carbs(&[day(0.0, 0.0, 0.0)]).0.len(), 0);
        let (items, bars) = insulin_carbs(&[day(10.0, 20.0, 150.0), day(0.0, 0.0, 0.0), day(5.0, 10.0, 75.0)]);
        let labels: Vec<_> = items.iter().map(|i| (i.label.to_string(), i.value.to_string())).collect();
        assert_eq!(labels[0], ("Rapid-acting insulin".to_string(), "7.5 u/day".to_string()));
        assert_eq!(labels[2], ("Total insulin".to_string(), "23 u/day".to_string()));
        assert_eq!(labels.last().unwrap().1, "2 of 3");
        assert_eq!((bars[0].insulin, bars[0].carbs, bars[2].carbs), (1.0, 1.0, 0.5));
        assert!(bars[1].text.ends_with("no entries"));
        assert_eq!(totals_text(&day(12.0, 18.0, 160.4)), "Rapid 12 u · Long 18 u · Carbs 160 g");
        assert_eq!(carbs_from_ui(" 45,5 ").unwrap(), Some(45.5));
        assert!(carbs_from_ui("lots").is_err() && carbs_from_ui("-3").is_err());
    }

    #[test]
    fn treatment_units_accept_a_decimal_comma() {
        let t = treatment_from_ui(1, "", " 4,5 ").unwrap().unwrap();
        assert_eq!((t.kind, t.units), (TreatmentKind::RapidActing, Some(4.5)));
        assert_eq!(treatment_from_ui(2, "", "4.5").unwrap().unwrap().units, Some(4.5));
        assert_eq!(treatment_from_ui(0, "", "").unwrap(), None);
        assert!(treatment_from_ui(0, "", "3").is_err());
        assert!(treatment_from_ui(1, "", "four").is_err());
        assert!(treatment_from_ui(3, " ", "").is_err());
        assert_eq!(treatment_from_ui(3, "Glucagon", "").unwrap().unwrap().label(), "Glucagon");
    }
}
