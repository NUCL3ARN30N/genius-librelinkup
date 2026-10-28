//! Synthetic glucose data for `--demo` (UI development, screenshots).

use chrono::{DateTime, Duration, Local, Timelike, Utc};

use crate::api::{Connection, Reading};

/// Smooth, deterministic curve: overnight dip, meal peaks and a slow day-to-day drift.
fn value_at(t: DateTime<Utc>) -> f32 {
    let local = t.with_timezone(&Local);
    let h = local.hour() as f32 + local.minute() as f32 / 60.0;
    let day = (t.timestamp() / 86_400) as f32;
    let bump = |center: f32, height: f32| {
        let d = h - center;
        if d < 0.0 { height * (-(d * d) / 0.15).exp() } else { height * (-(d * d) / 1.6).exp() }
    };
    let meals = bump(8.0, 75.0 + 25.0 * (day * 1.7).sin())
        + bump(13.0, 60.0 + 30.0 * (day * 0.9).cos())
        + bump(19.5, 95.0 + 70.0 * (day * 1.3).sin().max(0.0));
    // some nights dip low, some evenings run high – exercises the pattern insights
    let night = -(18.0 + 48.0 * (day * 2.1).sin().max(0.0)) * (-((h - 3.5) * (h - 3.5)) / 3.0).exp();
    let wobble = 9.0 * (t.timestamp() as f32 / 1900.0).sin() + 5.0 * (t.timestamp() as f32 / 700.0).cos();
    (105.0 + 10.0 * (day * 0.5).sin() + meals + night + wobble).clamp(45.0, 380.0)
}

pub fn readings(from: DateTime<Utc>, to: DateTime<Utc>, step_secs: i64) -> Vec<Reading> {
    let mut out = Vec::new();
    let mut t = DateTime::from_timestamp(from.timestamp() / step_secs * step_secs, 0).unwrap();
    while t <= to {
        let v = value_at(t);
        let slope = value_at(t + Duration::minutes(10)) - value_at(t - Duration::minutes(10));
        let trend = match slope {
            s if s < -25.0 => 1,
            s if s < -10.0 => 2,
            s if s <= 10.0 => 3,
            s if s <= 25.0 => 4,
            _ => 5,
        };
        out.push(Reading { time: t, mg_dl: v.round(), trend });
        t += Duration::seconds(step_secs);
    }
    out
}

pub fn connection(first: &str, last: &str, current: Option<Reading>) -> Connection {
    Connection {
        patient_id: "demo".into(),
        first_name: first.into(),
        last_name: last.into(),
        target_low: 70.0,
        target_high: 180.0,
        uses_mg_dl: false,
        current,
        sensor_serial: "DEMO1234".into(),
        sensor_activated: Some(Utc::now() - Duration::days(5)),
    }
}
