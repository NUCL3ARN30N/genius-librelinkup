//! Decides when to show glucose notifications and shows them as desktop notifications.
//!
//! Kinds, most important first: no data, urgent low (< 54 mg/dL), low, predicted low, falling
//! fast, high, rising fast. One alert is active at a time; low, high and no data repeat every
//! `notify_repeat_minutes`, the trend and prediction alerts are shown once per episode. An
//! episode ends when no condition holds any more. During quiet hours and while snoozed only
//! urgent lows notify; what quiet hours suppress is not shown afterwards either.

use std::collections::HashSet;

use chrono::{DateTime, Duration, Local, NaiveTime, Utc};

use crate::api::Reading;
use crate::config::Settings;
use crate::stats::VERY_LOW;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AlertKind {
    Stale,
    UrgentLow,
    Low,
    PredictedLow,
    FallingFast,
    High,
    RisingFast,
}

impl AlertKind {
    fn repeats(self) -> bool {
        matches!(self, AlertKind::Stale | AlertKind::UrgentLow | AlertKind::Low | AlertKind::High)
    }
}

/// What a fired alert is about.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Alert {
    pub kind: AlertKind,
    /// mg/dL per minute over the last 15 minutes, if it could be computed
    pub slope: Option<f32>,
    /// predicted value (mg/dL) `predict_minutes` after the latest reading, for predicted lows
    pub predicted: Option<f32>,
}

#[derive(Default)]
pub struct AlertState {
    active: Option<AlertKind>,
    last_shown: Option<DateTime<Utc>>,
    /// kinds shown (or suppressed by quiet hours) in the current episode
    shown: HashSet<AlertKind>,
    /// quiet hours suppressed something in this episode: nothing but urgent lows until it ends
    muted: bool,
    snoozed_until: Option<DateTime<Utc>>,
}

/// Least-squares line through the readings of the last `minutes` before the latest one:
/// (slope in mg/dL per minute, fitted value at the latest reading). Needs at least 3 readings
/// spanning half the window and no gap over 10 minutes.
pub fn fit(readings: &[Reading], minutes: i64) -> Option<(f32, f32)> {
    let last = readings.last()?;
    let window: Vec<&Reading> = readings.iter().filter(|r| r.time >= last.time - Duration::minutes(minutes) && r.time <= last.time).collect();
    if window.len() < 3 || (last.time - window[0].time) < Duration::minutes(minutes / 2) {
        return None;
    }
    if window.windows(2).any(|w| w[1].time - w[0].time > Duration::minutes(10)) {
        return None;
    }
    let xs: Vec<f32> = window.iter().map(|r| (r.time - last.time).num_seconds() as f32 / 60.0).collect();
    let n = xs.len() as f32;
    let (mx, my) = (xs.iter().sum::<f32>() / n, window.iter().map(|r| r.mg_dl).sum::<f32>() / n);
    let sxx: f32 = xs.iter().map(|x| (x - mx).powi(2)).sum();
    if sxx <= f32::EPSILON {
        return None;
    }
    let sxy: f32 = xs.iter().zip(&window).map(|(x, r)| (x - mx) * (r.mg_dl - my)).sum();
    let slope = sxy / sxx;
    // x = 0 is the latest reading
    Some((slope, my - slope * mx))
}

/// Minutes since midnight of "HH:MM".
pub fn parse_hhmm(s: &str) -> Option<u32> {
    let t = NaiveTime::parse_from_str(s.trim(), "%H:%M").ok()?;
    Some(t.signed_duration_since(NaiveTime::MIN).num_minutes() as u32)
}

/// Whether `minute` (since local midnight) lies in `[from, to)`; the window may cross midnight.
pub fn in_window(minute: u32, from: u32, to: u32) -> bool {
    if from <= to { (from..to).contains(&minute) } else { minute >= from || minute < to }
}

impl AlertState {
    pub fn snooze(&mut self, until: Option<DateTime<Utc>>) {
        self.snoozed_until = until;
    }

    /// The end of an active snooze.
    pub fn snoozed_until(&self, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
        self.snoozed_until.filter(|t| *t > now)
    }

    fn quiet(s: &Settings, now: DateTime<Utc>) -> bool {
        let (Some(from), Some(to)) = (parse_hhmm(&s.quiet_from), parse_hhmm(&s.quiet_to)) else { return false };
        let local = now.with_timezone(&Local);
        s.quiet_hours && from != to && in_window(local.time().signed_duration_since(NaiveTime::MIN).num_minutes() as u32, from, to)
    }

    /// The alert to show now, if any. `recent` are the readings of the last half hour or more,
    /// sorted, ending with the latest.
    pub fn evaluate(&mut self, s: &Settings, recent: &[Reading], now: DateTime<Utc>) -> Option<Alert> {
        self.evaluate_with(s, recent, now, Self::quiet(s, now))
    }

    fn evaluate_with(&mut self, s: &Settings, recent: &[Reading], now: DateTime<Utc>, quiet: bool) -> Option<Alert> {
        let latest = recent.last()?;
        let slope = fit(recent, 15).map(|f| f.0);
        let predicted = fit(recent, 20).map(|(slope, at)| at + slope * s.predict_minutes as f32);
        let v = latest.mg_dl;
        let fast = |sign: f32, trend: u8| latest.trend == trend || slope.is_some_and(|sl| sl * sign > s.fast_rate);
        let kind = if s.notify_stale && now - latest.time > Duration::minutes(s.stale_minutes as i64) {
            Some(AlertKind::Stale)
        } else if s.notify_low && v < VERY_LOW {
            Some(AlertKind::UrgentLow)
        } else if s.notify_low && v <= s.low_threshold {
            Some(AlertKind::Low)
        } else if s.notify_predicted_low && predicted.is_some_and(|p| p < s.low_threshold) && slope.is_some_and(|sl| sl < 0.0) {
            Some(AlertKind::PredictedLow)
        } else if s.notify_falling && fast(-1.0, 1) {
            Some(AlertKind::FallingFast)
        } else if s.notify_high && v >= s.high_threshold {
            Some(AlertKind::High)
        } else if s.notify_rising && fast(1.0, 5) {
            Some(AlertKind::RisingFast)
        } else {
            None
        };
        let Some(kind) = kind else {
            // the episode is over
            self.active = None;
            self.shown.clear();
            self.muted = false;
            return None;
        };
        let urgent = kind == AlertKind::UrgentLow;
        if !urgent && self.snoozed_until(now).is_some() {
            return None;
        }
        if !urgent && (quiet || self.muted) {
            self.muted |= quiet;
            self.shown.insert(kind);
            self.active = Some(kind);
            return None;
        }
        let repeat_due = kind.repeats()
            && s.notify_repeat_minutes > 0
            && self.active == Some(kind)
            && self.last_shown.is_some_and(|t| now - t >= Duration::minutes(s.notify_repeat_minutes as i64));
        let first = !self.shown.contains(&kind);
        // a predicted low or a fast fall is old news once the value is low
        let superseded = matches!(kind, AlertKind::PredictedLow | AlertKind::FallingFast)
            && (self.shown.contains(&AlertKind::Low) || self.shown.contains(&AlertKind::UrgentLow));
        self.active = Some(kind);
        if (first && !superseded) || repeat_due {
            self.shown.insert(kind);
            self.last_shown = Some(now);
            return Some(Alert { kind, slope, predicted: (kind == AlertKind::PredictedLow).then_some(predicted).flatten() });
        }
        None
    }
}

pub fn show(title: &str, body: &str, urgent: bool) {
    if let Err(e) = notification(title, body, urgent).show() {
        tracing::warn!("notification failed: {e}");
    }
}

fn notification(title: &str, body: &str, urgent: bool) -> notify_rust::Notification {
    let mut n = notify_rust::Notification::new();
    n.summary(title).body(body).appname("Genius LibreLinkUp");
    #[cfg(target_os = "linux")]
    {
        n.icon("genius-librelinkup");
        if urgent {
            n.urgency(notify_rust::Urgency::Critical);
        }
    }
    #[cfg(windows)]
    {
        let _ = urgent;
        n.app_id(crate::platform::WINDOWS_APP_ID);
    }
    n
}

/// A notification with a "Snooze 30 min" button where the desktop supports actions (Linux);
/// elsewhere a plain notification.
pub fn show_with_snooze(title: &str, body: &str, urgent: bool, on_snooze: impl FnOnce() + Send + 'static) {
    #[cfg(target_os = "linux")]
    {
        let mut n = notification(title, body, urgent);
        n.action("snooze", "Snooze 30 min");
        match n.show() {
            // waits until the notification is closed, on its own thread
            Ok(handle) => {
                std::thread::spawn(move || {
                    let mut on_snooze = Some(on_snooze);
                    handle.wait_for_action(|action| {
                        if action == "snooze"
                            && let Some(f) = on_snooze.take()
                        {
                            f();
                        }
                    });
                });
            }
            Err(e) => tracing::warn!("notification failed: {e}"),
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = on_snooze;
        show(title, body, urgent);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(v: f32, now: DateTime<Utc>) -> Reading {
        Reading { time: now, mg_dl: v, trend: 3 }
    }

    /// 5-minute readings ending at `end`, from `start` mg/dL changing by `step` per reading.
    fn curve(end: DateTime<Utc>, n: i64, start: f32, step: f32) -> Vec<Reading> {
        (0..n).map(|i| r(start + step * i as f32, end - Duration::minutes(5 * (n - 1 - i)))).collect()
    }

    fn only_low() -> Settings {
        Settings {
            notify_low: true,
            low_threshold: 70.0,
            notify_repeat_minutes: 15,
            notify_predicted_low: false,
            notify_falling: false,
            notify_rising: false,
            ..Default::default()
        }
    }

    #[test]
    fn low_once_then_repeat() {
        let s = only_low();
        let mut st = AlertState::default();
        let t0 = Utc::now();
        let kind = |a: Option<Alert>| a.map(|a| a.kind);
        assert_eq!(kind(st.evaluate_with(&s, &[r(65.0, t0)], t0, false)), Some(AlertKind::Low));
        let t1 = t0 + Duration::minutes(5);
        assert_eq!(kind(st.evaluate_with(&s, &[r(64.0, t1)], t1, false)), None);
        let t2 = t0 + Duration::minutes(16);
        assert_eq!(kind(st.evaluate_with(&s, &[r(63.0, t2)], t2, false)), Some(AlertKind::Low));
        let t3 = t2 + Duration::minutes(1);
        assert_eq!(kind(st.evaluate_with(&s, &[r(90.0, t3)], t3, false)), None);
        assert_eq!(kind(st.evaluate_with(&s, &[r(60.0, t3)], t3, false)), Some(AlertKind::Low));
    }

    #[test]
    fn high_disabled_by_default() {
        let s = Settings::default();
        let mut st = AlertState::default();
        let t = Utc::now();
        assert_eq!(st.evaluate_with(&s, &[r(300.0, t)], t, false), None);
    }

    #[test]
    fn fits_a_line() {
        let t = Utc::now();
        let (slope, at) = fit(&curve(t, 5, 120.0, -10.0), 20).unwrap();
        assert!((slope + 2.0).abs() < 1e-3 && (at - 80.0).abs() < 1e-3, "{slope} {at}");
        // too few readings, or a gap over 10 minutes
        assert!(fit(&curve(t, 2, 120.0, -10.0), 20).is_none());
        let mut gap = curve(t, 5, 120.0, -10.0);
        gap.remove(2);
        gap.remove(2);
        assert!(fit(&gap, 20).is_none());
    }

    #[test]
    fn predicted_low_fires_once_per_fall() {
        let s = Settings { notify_predicted_low: true, ..only_low() };
        let mut st = AlertState::default();
        let t0 = Utc::now();
        // falling 1.5 mg/dL/min from 120: at 105 the projection (75) is still above 70
        let mut fired = Vec::new();
        for step in 0..8 {
            let now = t0 + Duration::minutes(5 * step);
            let readings = curve(now, 5 + step, 135.0, -7.5);
            if let Some(a) = st.evaluate_with(&s, &readings, now, false) {
                fired.push((step, a.kind, readings.last().unwrap().mg_dl));
            }
        }
        let kinds: Vec<AlertKind> = fired.iter().map(|f| f.1).collect();
        // one prediction while still above 70, then the low itself
        assert_eq!(kinds.iter().filter(|k| **k == AlertKind::PredictedLow).count(), 1, "{fired:?}");
        let predicted_at = fired.iter().find(|f| f.1 == AlertKind::PredictedLow).unwrap().2;
        assert!(predicted_at > 70.0, "{fired:?}");
        assert!(kinds.contains(&AlertKind::Low), "{fired:?}");
    }

    #[test]
    fn falling_fast_from_trend_or_slope() {
        let s = Settings { notify_falling: true, fast_rate: 2.0, notify_low: false, notify_predicted_low: false, ..Default::default() };
        let t = Utc::now();
        let mut st = AlertState::default();
        let steady = curve(t, 4, 150.0, 0.0);
        assert_eq!(st.evaluate_with(&s, &steady, t, false), None);
        let mut arrow = steady.clone();
        arrow.last_mut().unwrap().trend = 1;
        assert_eq!(st.evaluate_with(&s, &arrow, t, false).map(|a| a.kind), Some(AlertKind::FallingFast));
        let mut st = AlertState::default();
        let steep = curve(t, 4, 200.0, -12.5);
        let a = st.evaluate_with(&s, &steep, t, false).unwrap();
        assert_eq!(a.kind, AlertKind::FallingFast);
        assert!((a.slope.unwrap() + 2.5).abs() < 1e-3);
    }

    #[test]
    fn quiet_hours_only_let_urgent_lows_through() {
        let s = Settings { notify_high: true, high_threshold: 250.0, ..only_low() };
        let mut st = AlertState::default();
        let t0 = Utc::now();
        assert_eq!(st.evaluate_with(&s, &[r(65.0, t0)], t0, true), None);
        // quiet hours are over but the low continues: not shown afterwards, no repeat either
        let t1 = t0 + Duration::minutes(30);
        assert_eq!(st.evaluate_with(&s, &[r(64.0, t1)], t1, false), None);
        // an urgent low always notifies
        assert_eq!(st.evaluate_with(&s, &[r(50.0, t1)], t1, true).map(|a| a.kind), Some(AlertKind::UrgentLow));
        // a new episode after quiet hours notifies again
        assert_eq!(st.evaluate_with(&s, &[r(100.0, t1)], t1, false), None);
        assert_eq!(st.evaluate_with(&s, &[r(300.0, t1)], t1, false).map(|a| a.kind), Some(AlertKind::High));
        assert!(in_window(23 * 60, 22 * 60, 7 * 60) && in_window(60, 22 * 60, 7 * 60) && !in_window(12 * 60, 22 * 60, 7 * 60));
        assert!(in_window(13 * 60, 12 * 60, 14 * 60) && !in_window(14 * 60, 12 * 60, 14 * 60));
        assert_eq!(parse_hhmm("07:30"), Some(450));
        assert_eq!(parse_hhmm("7.30"), None);
    }

    #[test]
    fn snooze_mutes_until_it_expires() {
        let s = only_low();
        let mut st = AlertState::default();
        let t0 = Utc::now();
        st.snooze(Some(t0 + Duration::minutes(30)));
        assert_eq!(st.evaluate_with(&s, &[r(65.0, t0)], t0, false), None);
        // urgent lows are never snoozed
        let t1 = t0 + Duration::minutes(10);
        assert_eq!(st.evaluate_with(&s, &[r(52.0, t1)], t1, false).map(|a| a.kind), Some(AlertKind::UrgentLow));
        assert_eq!(st.snoozed_until(t1), Some(t0 + Duration::minutes(30)));
        // after the snooze the ongoing low is shown
        let t2 = t0 + Duration::minutes(31);
        assert_eq!(st.snoozed_until(t2), None);
        assert_eq!(st.evaluate_with(&s, &[r(66.0, t2)], t2, false).map(|a| a.kind), Some(AlertKind::Low));
    }
}
