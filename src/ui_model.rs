//! Turns readings and reports into the plain data the Slint UI renders (paths, labels, rows).
//! Paths use a 1000×400 viewbox, label positions are fractions of the plot area.

use chrono::{DateTime, Datelike, Duration, Local, NaiveDate, Timelike, Utc};
use slint::SharedString;

use crate::api::Reading;
use crate::stats::{self, Light, Report};
use crate::store::Note;
use crate::units::{Units, level};
use crate::{AxisLabel, DayCard, Dot, GraphPoint, Insight, NoteMark, StatItem, TirRow};

const VW: f32 = 1000.0;
const VH: f32 = 400.0;

fn s(v: impl Into<SharedString>) -> SharedString {
    v.into()
}

pub fn stat(label: &str, value: impl Into<SharedString>, hint: &str) -> StatItem {
    StatItem { label: s(label), value: value.into(), hint: s(hint) }
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
    NoteMark {
        x,
        id: s(n.id.clone()),
        time: s(t.format(time_format).to_string()),
        when: s(t.format(NOTE_TIME_FORMAT).to_string()),
        text: s(n.text.clone()),
    }
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
            time: s(r.time.with_timezone(&Local).format(time_format(r.time)).to_string()),
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
        tir_row("Very high", format!("> {}", u.fmt_unit(stats::VERY_HIGH)), r.tir.very_high, 5),
        tir_row(
            "High",
            format!("{} – {}", u.fmt(target_high + 1.0), u.fmt_unit(stats::VERY_HIGH)),
            r.tir.high,
            4,
        ),
        tir_row("Target range", format!("{} – {}", u.fmt(target_low), u.fmt_unit(target_high)), r.tir.target, 3),
        tir_row("Low", format!("{} – {}", u.fmt(stats::VERY_LOW), u.fmt_unit(below(target_low))), r.tir.low, 2),
        tir_row("Very low", format!("< {}", u.fmt_unit(stats::VERY_LOW)), r.tir.very_low, 1),
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
            month: s(if i == 0 || d.date.day() == 1 { d.date.format("%b").to_string() } else { String::new() }),
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
        vec![stat("Readings", "0", "")]
    } else {
        vec![
            stat("Average", u.fmt_unit(r.mean), ""),
            stat("Lowest", u.fmt_unit(min), ""),
            stat("Highest", u.fmt_unit(max), ""),
            stat("Standard deviation", u.fmt_unit(r.sd), ""),
            stat("Variability (CV)", format!("{:.1} %", r.cv_pct), ""),
            stat("% time CGM is active", format!("{:.0} %", r.sensor_active_pct), ""),
            stat("Readings", readings.len().to_string(), ""),
        ]
    };
    DayDetailModel {
        title: date.format("%A, %d %B %Y").to_string(),
        graph: graph_between(readings, notes, u, (target_low, target_high), start, end, style, 3, false),
        stats,
        tir: tir_rows(&r, u, target_low, target_high),
    }
}

pub fn report_model(r: &Report, notes: &[Note], units: Units, target_low: f32, target_high: f32) -> ReportModel {
    let fmt_date = |t: Option<DateTime<Utc>>| {
        t.map(|t| t.with_timezone(&Local).format("%d %b %Y").to_string()).unwrap_or_default()
    };
    let subtitle = format!("{} – {} ({} days, {} readings)", fmt_date(r.from), fmt_date(r.to), r.days, r.readings);
    let u = units;
    let coverage_hint = if r.sensor_active_pct < 70.0 { "less than 70 % – interpret with care" } else { "" };
    let stats = vec![
        stat("% time CGM is active", format!("{:.1} %", r.sensor_active_pct), coverage_hint),
        stat("Average glucose", u.fmt_unit(r.mean), ""),
        stat(
            "Glucose management indicator (GMI)",
            format!("{:.1} % · {:.0} mmol/mol", r.gmi_pct, r.gmi_mmol_mol),
            "approximate HbA1c from average CGM glucose",
        ),
        stat(
            "Estimated HbA1c (eA1c)",
            format!("{:.1} % · {:.0} mmol/mol", r.ea1c_pct, r.ea1c_mmol_mol),
            "ADAG formula",
        ),
        stat("Glucose variability (CV)", format!("{:.1} %", r.cv_pct), "goal ≤ 36 %"),
        stat("Standard deviation", u.fmt_unit(r.sd), ""),
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

    ReportModel {
        title: "Glucose report".into(),
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
                label: s(i.label),
                low: light(i.low_likelihood),
                median: light(i.median_vs_goal),
                variability: light(i.variability_below_median),
            })
            .collect(),
        days,
        day_band_top: dfy(target_high),
        day_band_bottom: dfy(target_low),
    }
}

/// "just now", "4 min ago", "2 h ago"
pub fn age_text(t: DateTime<Utc>) -> String {
    let mins = (Utc::now() - t).num_minutes();
    match mins {
        i64::MIN..=0 => "just now".into(),
        1..=59 => format!("{mins} min ago"),
        60..=1439 => format!("{} h ago", mins / 60),
        _ => t.with_timezone(&Local).format("%d %b %H:%M").to_string(),
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
}
