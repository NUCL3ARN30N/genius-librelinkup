//! Decides when to show low/high/stale notifications and shows them as desktop notifications.

use chrono::{DateTime, Duration, Utc};

use crate::api::Reading;
use crate::config::Settings;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlertKind {
    Low,
    High,
    Stale,
}

#[derive(Default)]
pub struct AlertState {
    active: Option<AlertKind>,
    last_shown: Option<DateTime<Utc>>,
}

impl AlertState {
    /// Returns the alert to show now (if any). Alerts repeat every `notify_repeat_minutes`
    /// (0 = only once per episode) and reset once the value is back in range.
    pub fn evaluate(&mut self, s: &Settings, latest: Option<&Reading>, now: DateTime<Utc>) -> Option<AlertKind> {
        let kind = match latest {
            Some(r) if s.notify_stale && now - r.time > Duration::minutes(s.stale_minutes as i64) => {
                Some(AlertKind::Stale)
            }
            Some(r) if s.notify_low && r.mg_dl <= s.low_threshold => Some(AlertKind::Low),
            Some(r) if s.notify_high && r.mg_dl >= s.high_threshold => Some(AlertKind::High),
            Some(_) => None,
            None => None,
        };
        let Some(kind) = kind else {
            self.active = None;
            return None;
        };
        let repeat_due = s.notify_repeat_minutes > 0
            && self
                .last_shown
                .is_some_and(|t| now - t >= Duration::minutes(s.notify_repeat_minutes as i64));
        if self.active != Some(kind) || repeat_due {
            self.active = Some(kind);
            self.last_shown = Some(now);
            return Some(kind);
        }
        None
    }
}

pub fn show(title: &str, body: &str, urgent: bool) {
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
    if let Err(e) = n.show() {
        tracing::warn!("notification failed: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(v: f32, now: DateTime<Utc>) -> Reading {
        Reading { time: now, mg_dl: v, trend: 3 }
    }

    #[test]
    fn low_once_then_repeat() {
        let s = Settings { notify_low: true, low_threshold: 70.0, notify_repeat_minutes: 15, ..Default::default() };
        let mut st = AlertState::default();
        let t0 = Utc::now();
        assert_eq!(st.evaluate(&s, Some(&r(65.0, t0)), t0), Some(AlertKind::Low));
        let t1 = t0 + Duration::minutes(5);
        assert_eq!(st.evaluate(&s, Some(&r(64.0, t1)), t1), None);
        let t2 = t0 + Duration::minutes(16);
        assert_eq!(st.evaluate(&s, Some(&r(63.0, t2)), t2), Some(AlertKind::Low));
        let t3 = t2 + Duration::minutes(1);
        assert_eq!(st.evaluate(&s, Some(&r(90.0, t3)), t3), None);
        assert_eq!(st.evaluate(&s, Some(&r(60.0, t3)), t3), Some(AlertKind::Low));
    }

    #[test]
    fn high_disabled_by_default() {
        let s = Settings::default();
        let mut st = AlertState::default();
        let t = Utc::now();
        assert_eq!(st.evaluate(&s, Some(&r(300.0, t)), t), None);
    }
}
