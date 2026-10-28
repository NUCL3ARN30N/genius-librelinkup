//! Developer mode: records every API call (redacted) in memory and in the rolling log file.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{DateTime, Local};
use serde_json::Value;

const MAX_ENTRIES: usize = 300;
const MAX_BODY: usize = 8 * 1024;
const SECRET_KEYS: &[&str] = &[
    "password", "token", "email", "firstName", "lastName", "id", "patientId", "userId",
    "accountId", "sn", "deviceId", "did", "dateOfBirth", "phone", "country",
];

#[derive(Debug, Clone)]
pub struct ApiLogEntry {
    pub time: DateTime<Local>,
    pub method: String,
    pub url: String,
    pub status: Option<u16>,
    pub millis: u128,
    pub request: String,
    pub response: String,
}

#[derive(Clone, Default)]
pub struct DevLog {
    enabled: Arc<AtomicBool>,
    entries: Arc<Mutex<VecDeque<ApiLogEntry>>>,
}

impl DevLog {
    pub fn set_enabled(&self, on: bool) {
        self.enabled.store(on, Ordering::Relaxed);
    }

    pub fn enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }

    pub fn api(
        &self,
        method: &str,
        url: &str,
        status: Option<u16>,
        elapsed: Duration,
        request: Option<&Value>,
        response: &str,
    ) {
        tracing::debug!(target: "api", "{method} {} -> {status:?} ({} ms)", redact_url(url), elapsed.as_millis());
        if !self.enabled() {
            return;
        }
        let request = request.map(|r| redact_json(r.clone()).to_string()).unwrap_or_default();
        let response = match serde_json::from_str::<Value>(response) {
            Ok(v) => serde_json::to_string_pretty(&redact_json(v)).unwrap_or_default(),
            Err(_) => response.to_string(),
        };
        let response = truncate(response);
        tracing::info!(target: "api", "{method} {} -> {status:?}\nrequest: {request}\nresponse: {response}", redact_url(url));
        let mut entries = self.entries.lock().unwrap();
        if entries.len() >= MAX_ENTRIES {
            entries.pop_front();
        }
        entries.push_back(ApiLogEntry {
            time: Local::now(),
            method: method.to_string(),
            url: redact_url(url),
            status,
            millis: elapsed.as_millis(),
            request,
            response,
        });
    }

    pub fn entries(&self) -> Vec<ApiLogEntry> {
        self.entries.lock().unwrap().iter().rev().cloned().collect()
    }

    pub fn clear(&self) {
        self.entries.lock().unwrap().clear();
    }
}

fn truncate(mut s: String) -> String {
    if s.len() > MAX_BODY {
        let mut cut = MAX_BODY;
        while !s.is_char_boundary(cut) {
            cut -= 1;
        }
        s.truncate(cut);
        s.push_str("\n… (truncated)");
    }
    s
}

/// Patient ids appear in URL paths (`/llu/connections/{id}/graph`).
fn redact_url(url: &str) -> String {
    let mut parts: Vec<String> = url.split('/').map(str::to_string).collect();
    for i in 1..parts.len() {
        if parts[i - 1] == "connections" && !parts[i].is_empty() {
            parts[i] = "<patient-id>".into();
        }
    }
    parts.join("/")
}

pub fn redact_json(mut v: Value) -> Value {
    fn walk(v: &mut Value) {
        match v {
            Value::Object(map) => {
                for (k, val) in map.iter_mut() {
                    if SECRET_KEYS.iter().any(|s| s.eq_ignore_ascii_case(k)) && !val.is_object() && !val.is_array() {
                        *val = Value::String("<redacted>".into());
                    } else {
                        walk(val);
                    }
                }
            }
            Value::Array(a) => a.iter_mut().for_each(walk),
            _ => {}
        }
    }
    walk(&mut v);
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_secrets() {
        let v = serde_json::json!({"email": "a@b.c", "data": {"authTicket": {"token": "x"}, "user": {"id": "u"}}, "status": 0});
        let r = redact_json(v);
        assert_eq!(r["email"], "<redacted>");
        assert_eq!(r["data"]["authTicket"]["token"], "<redacted>");
        assert_eq!(r["data"]["user"]["id"], "<redacted>");
        assert_eq!(r["status"], 0);
    }

    #[test]
    fn redacts_patient_in_url() {
        assert_eq!(
            redact_url("https://x/llu/connections/abc-123/graph"),
            "https://x/llu/connections/<patient-id>/graph"
        );
    }
}
