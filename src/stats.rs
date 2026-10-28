//! Report statistics following the international consensus on time in range
//! (Battelino et al., Diabetes Care 2019) as used by LibreView's AGP report.

use chrono::{DateTime, Duration, Local, NaiveDate, Timelike, Utc};

use crate::api::Reading;

/// Consensus thresholds in mg/dL.
pub const VERY_LOW: f32 = 54.0;
pub const VERY_HIGH: f32 = 250.0;

#[derive(Debug, Clone, Default)]
pub struct TimeInRanges {
    pub very_low: f32,
    pub low: f32,
    pub target: f32,
    pub high: f32,
    pub very_high: f32,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct AgpSlot {
    /// minutes since midnight (slot centre)
    pub minute: u32,
    pub p5: f32,
    pub p25: f32,
    pub p50: f32,
    pub p75: f32,
    pub p95: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Light {
    Green,
    Yellow,
    Red,
    None,
}

#[derive(Debug, Clone)]
pub struct PeriodInsight {
    pub label: &'static str,
    pub low_likelihood: Light,
    pub median_vs_goal: Light,
    pub variability_below_median: Light,
}

#[derive(Debug, Clone)]
pub struct DayProfile {
    pub date: NaiveDate,
    /// (fraction of day 0..1, mg/dL)
    pub points: Vec<(f32, f32)>,
}

#[derive(Debug, Clone, Default)]
pub struct Report {
    pub from: Option<DateTime<Utc>>,
    pub to: Option<DateTime<Utc>>,
    pub days: u32,
    pub readings: usize,
    pub sensor_active_pct: f32,
    pub mean: f32,
    pub sd: f32,
    pub cv_pct: f32,
    /// Glucose management indicator (%) = 3.31 + 0.02392 × mean[mg/dL]
    pub gmi_pct: f32,
    pub gmi_mmol_mol: f32,
    /// Estimated HbA1c (ADAG) = (mean + 46.7) / 28.7
    pub ea1c_pct: f32,
    pub ea1c_mmol_mol: f32,
    pub tir: TimeInRanges,
    pub agp: Vec<AgpSlot>,
    pub insights: Vec<PeriodInsight>,
    pub daily: Vec<DayProfile>,
}

pub fn pct_to_mmol_mol(pct: f32) -> f32 {
    (pct - 2.152) * 10.929
}

fn percentile(sorted: &[f32], p: f32) -> f32 {
    if sorted.is_empty() {
        return f32::NAN;
    }
    let rank = p / 100.0 * (sorted.len() - 1) as f32;
    let lo = rank.floor() as usize;
    let hi = rank.ceil() as usize;
    let w = rank - lo as f32;
    sorted[lo] * (1.0 - w) + sorted[hi] * w
}

/// Typical spacing of the readings in seconds (LibreView history: 15 min, LLU graph: 5 min,
/// live polling: 1 min). Used to estimate sensor coverage.
fn typical_interval(readings: &[Reading]) -> i64 {
    let mut gaps: Vec<i64> = readings
        .windows(2)
        .map(|w| (w[1].time - w[0].time).num_seconds())
        .filter(|g| *g > 0 && *g < 3600)
        .collect();
    if gaps.is_empty() {
        return 900;
    }
    gaps.sort_unstable();
    gaps[gaps.len() / 2].clamp(60, 900)
}

/// Resamples to at most one reading per 5 minutes so 1-minute live polling does not outweigh
/// 15-minute imported history in time-weighted statistics.
fn bucket_5min(readings: &[Reading]) -> Vec<Reading> {
    let mut out: Vec<Reading> = Vec::with_capacity(readings.len());
    for r in readings {
        let slot = r.time.timestamp() / 300;
        match out.last() {
            Some(last) if last.time.timestamp() / 300 == slot => {}
            _ => out.push(*r),
        }
    }
    out
}

pub fn build_report(
    raw: &[Reading],
    from: DateTime<Utc>,
    to: DateTime<Utc>,
    target_low: f32,
    target_high: f32,
    median_goal: f32,
) -> Report {
    let mut readings: Vec<Reading> = raw.iter().filter(|r| r.time >= from && r.time <= to).copied().collect();
    readings.sort_by_key(|r| r.time);
    let days = ((to - from).num_hours() as f32 / 24.0).round().max(1.0) as u32;
    let mut report = Report { from: Some(from), to: Some(to), days, ..Default::default() };
    if readings.is_empty() {
        return report;
    }
    let interval = typical_interval(&readings);
    let samples = bucket_5min(&readings);
    report.readings = readings.len();

    // Coverage: distinct sensor slots with data vs. slots in the period.
    let slot = interval.max(300);
    let mut slots: Vec<i64> = readings.iter().map(|r| r.time.timestamp() / slot).collect();
    slots.dedup();
    let total_slots = ((to - from).num_seconds() / slot).max(1);
    report.sensor_active_pct = (slots.len() as f32 / total_slots as f32 * 100.0).min(100.0);

    let values: Vec<f32> = samples.iter().map(|r| r.mg_dl).collect();
    let n = values.len() as f32;
    let mean = values.iter().sum::<f32>() / n;
    let var = values.iter().map(|v| (v - mean).powi(2)).sum::<f32>() / (n - 1.0).max(1.0);
    report.mean = mean;
    report.sd = var.sqrt();
    report.cv_pct = report.sd / mean * 100.0;
    report.gmi_pct = 3.31 + 0.02392 * mean;
    report.gmi_mmol_mol = pct_to_mmol_mol(report.gmi_pct);
    report.ea1c_pct = (mean + 46.7) / 28.7;
    report.ea1c_mmol_mol = pct_to_mmol_mol(report.ea1c_pct);

    let frac = |f: &dyn Fn(f32) -> bool| values.iter().filter(|v| f(**v)).count() as f32 / n * 100.0;
    report.tir = TimeInRanges {
        very_low: frac(&|v| v < VERY_LOW),
        low: frac(&|v| (VERY_LOW..target_low).contains(&v)),
        target: frac(&|v| v >= target_low && v <= target_high),
        high: frac(&|v| v > target_high && v <= VERY_HIGH),
        very_high: frac(&|v| v > VERY_HIGH),
    };

    // AGP: 15 minute slots of the day, each smoothed over a ±30 min window.
    let mut by_slot: Vec<Vec<f32>> = vec![Vec::new(); 96];
    for r in &samples {
        let local = r.time.with_timezone(&Local);
        let s = ((local.hour() * 60 + local.minute()) / 15) as usize;
        by_slot[s].push(r.mg_dl);
    }
    report.agp = (0..96)
        .filter_map(|i| {
            let mut window: Vec<f32> = (-2i32..=2)
                .flat_map(|d| by_slot[((i as i32 + d).rem_euclid(96)) as usize].iter().copied())
                .collect();
            if window.len() < 3 {
                return None;
            }
            window.sort_by(|a, b| a.total_cmp(b));
            Some(AgpSlot {
                minute: i as u32 * 15 + 7,
                p5: percentile(&window, 5.0),
                p25: percentile(&window, 25.0),
                p50: percentile(&window, 50.0),
                p75: percentile(&window, 75.0),
                p95: percentile(&window, 95.0),
            })
        })
        .collect();

    report.insights = insights(&samples, target_low, median_goal);
    report.daily = daily_profiles(&readings, from, to);
    report
}

/// Time-of-day periods as in LibreView's "Glucose Pattern Insights".
const PERIODS: &[(&str, u32, u32)] = &[
    ("Night 00–06", 0, 6),
    ("Morning 06–11", 6, 11),
    ("Midday 11–16", 11, 16),
    ("Evening 16–21", 16, 21),
    ("Late 21–24", 21, 24),
];

fn insights(samples: &[Reading], target_low: f32, median_goal: f32) -> Vec<PeriodInsight> {
    PERIODS
        .iter()
        .map(|(label, from, to)| {
            let mut v: Vec<f32> = samples
                .iter()
                .filter(|r| {
                    let h = r.time.with_timezone(&Local).hour();
                    h >= *from && h < *to
                })
                .map(|r| r.mg_dl)
                .collect();
            if v.len() < 12 {
                return PeriodInsight {
                    label,
                    low_likelihood: Light::None,
                    median_vs_goal: Light::None,
                    variability_below_median: Light::None,
                };
            }
            v.sort_by(|a, b| a.total_cmp(b));
            let p10 = percentile(&v, 10.0);
            let median = percentile(&v, 50.0);
            let low_share = v.iter().filter(|x| **x < target_low).count() as f32 / v.len() as f32;
            PeriodInsight {
                label,
                low_likelihood: if low_share > 0.10 || p10 < VERY_LOW {
                    Light::Red
                } else if low_share > 0.04 {
                    Light::Yellow
                } else {
                    Light::Green
                },
                median_vs_goal: if median <= median_goal {
                    Light::Green
                } else if median <= median_goal + 30.0 {
                    Light::Yellow
                } else {
                    Light::Red
                },
                variability_below_median: {
                    let spread = median - p10;
                    if spread < 30.0 {
                        Light::Green
                    } else if spread < 55.0 {
                        Light::Yellow
                    } else {
                        Light::Red
                    }
                },
            }
        })
        .collect()
}

fn daily_profiles(readings: &[Reading], from: DateTime<Utc>, to: DateTime<Utc>) -> Vec<DayProfile> {
    let first = from.with_timezone(&Local).date_naive();
    let last = to.with_timezone(&Local).date_naive();
    let mut days = Vec::new();
    let mut d = first;
    while d <= last {
        days.push(DayProfile { date: d, points: Vec::new() });
        d += Duration::days(1);
    }
    for r in readings {
        let local = r.time.with_timezone(&Local);
        let idx = (local.date_naive() - first).num_days();
        if let Some(day) = usize::try_from(idx).ok().and_then(|i| days.get_mut(i)) {
            let secs = local.num_seconds_from_midnight() as f32;
            day.points.push((secs / 86400.0, r.mg_dl));
        }
    }
    days
}

/// Formats a duration share of a day, e.g. 4.0 % of 24 h → "58 min".
pub fn share_of_day(pct: f32) -> String {
    let minutes = (pct / 100.0 * 1440.0).round() as u32;
    if minutes >= 60 { format!("{}h {:02}min", minutes / 60, minutes % 60) } else { format!("{minutes} min") }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gmi_and_tir() {
        let start = DateTime::from_timestamp(1_767_225_600, 0).unwrap();
        let readings: Vec<Reading> = (0..288)
            .map(|i| Reading {
                time: start + Duration::minutes(5 * i),
                mg_dl: if i < 144 { 100.0 } else { 200.0 },
                trend: 3,
            })
            .collect();
        let r = build_report(&readings, start, start + Duration::days(1), 70.0, 180.0, 154.0);
        assert!((r.mean - 150.0).abs() < 0.01);
        assert!((r.gmi_pct - 6.898).abs() < 0.01);
        assert!((r.tir.target - 50.0).abs() < 0.5);
        assert!((r.tir.high - 50.0).abs() < 0.5);
        assert!(r.sensor_active_pct > 99.0);
        assert_eq!(r.agp.len(), 96);
    }

    #[test]
    fn share_format() {
        assert_eq!(share_of_day(4.0), "58 min");
        assert_eq!(share_of_day(50.0), "12h 00min");
    }
}
