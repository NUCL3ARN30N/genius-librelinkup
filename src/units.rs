//! Glucose unit handling. Everything is stored in mg/dL internally.

use crate::stats::{VERY_HIGH, VERY_LOW};
use crate::store::MMOL_FACTOR;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Units {
    pub mg_dl: bool,
}

impl Units {
    pub fn label(&self) -> &'static str {
        if self.mg_dl { "mg/dL" } else { "mmol/L" }
    }

    /// Number only, e.g. "6.2" or "112".
    pub fn fmt(&self, mg: f32) -> String {
        if self.mg_dl { format!("{:.0}", mg) } else { format!("{:.1}", mg / MMOL_FACTOR) }
    }

    pub fn fmt_unit(&self, mg: f32) -> String {
        format!("{} {}", self.fmt(mg), self.label())
    }

    /// Value as shown on the sensor: LO/HI outside the 40–500 mg/dL measuring range.
    pub fn fmt_reading(&self, mg: f32) -> String {
        if mg < 40.0 {
            "LO".into()
        } else if mg > 500.0 {
            "HI".into()
        } else {
            self.fmt(mg)
        }
    }

    pub fn parse(&self, s: &str) -> Option<f32> {
        let v: f32 = s.trim().replace(',', ".").parse().ok()?;
        if !v.is_finite() || v <= 0.0 {
            return None;
        }
        // Be forgiving: values that are obviously in the other unit are accepted as such.
        let mg = if self.mg_dl {
            if v < 35.0 { v * MMOL_FACTOR } else { v }
        } else if v > 35.0 {
            v
        } else {
            v * MMOL_FACTOR
        };
        Some((mg * 10.0).round() / 10.0)
    }
}

/// Range level as used by the UI: 0 unknown, 1 very low, 2 low, 3 target, 4 high, 5 very high.
pub fn level(mg: f32, target_low: f32, target_high: f32) -> i32 {
    if mg < VERY_LOW {
        1
    } else if mg < target_low {
        2
    } else if mg <= target_high {
        3
    } else if mg <= VERY_HIGH {
        4
    } else {
        5
    }
}

pub fn trend_arrow(trend: u8) -> (&'static str, &'static str) {
    match trend {
        1 => ("↓", "Falling quickly"),
        2 => ("↘", "Falling"),
        3 => ("→", "Stable"),
        4 => ("↗", "Rising"),
        5 => ("↑", "Rising quickly"),
        _ => ("", ""),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_and_parses() {
        let mmol = Units { mg_dl: false };
        assert_eq!(mmol.fmt(112.0), "6.2");
        assert_eq!(mmol.parse("3,9").unwrap().round(), 70.0);
        let mg = Units { mg_dl: true };
        assert_eq!(mg.fmt(112.4), "112");
        assert_eq!(mg.parse("180").unwrap(), 180.0);
        assert_eq!(mg.fmt_reading(30.0), "LO");
    }
}
