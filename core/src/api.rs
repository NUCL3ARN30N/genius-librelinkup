//! Minimal client for the (unofficial) LibreLinkUp follower API.
//!
//! Endpoints and behaviour are reverse engineered by the community, see
//! <https://github.com/PaulCoughlin/llu> and <https://github.com/smpurkis/libre-linkup-py>.
//! Notable changes this client handles:
//! * Nov 2024 / Oct 2025: `Account-Id` header (sha256 of the user id) and a minimum client version.
//! * Oct 2026: LibreLinkUp 5.x identity, ~1h token lifetime and "login steps" (status 4) that
//!   require the user to re-accept terms of use / privacy policy.
//! * Server discovery: `GET /llu/config/country` returns a `regionalMap` of all regional
//!   API hosts; login responses may redirect to another region.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use chrono::{DateTime, NaiveDateTime, Utc};
use reqwest::{Method, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::devlog::DevLog;

pub const GLOBAL_SERVER: &str = "https://api.libreview.io";
pub const DEFAULT_PRODUCT: &str = "llu.android";
pub const DEFAULT_VERSION: &str = "5.0.1";

/// Servers known to exist even when the regional map cannot be fetched.
/// `ru` and `cn` are not part of Abbott's `regionalMap` but are used by some accounts.
const BUILTIN_SERVERS: &[(&str, &str)] = &[
    ("global", "https://api.libreview.io"),
    ("us", "https://api-us.libreview.io"),
    ("eu", "https://api-eu.libreview.io"),
    ("eu2", "https://api-eu2.libreview.io"),
    ("de", "https://api-de.libreview.io"),
    ("fr", "https://api-fr.libreview.io"),
    ("jp", "https://api-jp.libreview.io"),
    ("ap", "https://api-ap.libreview.io"),
    ("au", "https://api-au.libreview.io"),
    ("ae", "https://api-ae.libreview.io"),
    ("ca", "https://api-ca.libreview.io"),
    ("la", "https://api-la.libreview.io"),
    ("ru", "https://api.libreview.ru"),
    ("cn", "https://api-cn.myfreestyle.cn"),
];

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Identity {
    pub product: String,
    pub version: String,
}

impl Default for Identity {
    fn default() -> Self {
        Self { product: DEFAULT_PRODUCT.into(), version: DEFAULT_VERSION.into() }
    }
}

impl Identity {
    fn user_agent(&self) -> String {
        format!("LibreLinkUp/{}", self.version)
    }
}

#[derive(Debug, Clone)]
pub struct PendingStep {
    /// e.g. `tou` (terms of use) or `pp` (privacy policy)
    pub kind: String,
    pub component: String,
    ticket: String,
    server: String,
}

impl PendingStep {
    pub fn describe(&self) -> &'static str {
        match self.kind.as_str() {
            "tou" => "updated Terms of Use",
            "pp" => "updated Privacy Policy",
            _ => "an updated legal document",
        }
    }
}

#[derive(thiserror::Error, Debug, Clone)]
pub enum ApiError {
    #[error("Wrong email or password")]
    BadCredentials,
    #[error("LibreLinkUp asks you to accept {}", .0.describe())]
    StepRequired(PendingStep),
    #[error("Too many requests – retrying in {0} s")]
    RateLimited(u64),
    #[error("Client version rejected (server requires ≥ {0})")]
    VersionTooOld(String),
    #[error("Session expired")]
    Unauthorized,
    #[error("Not logged in")]
    NotLoggedIn,
    #[error("Network error: {0}")]
    Network(String),
    #[error("Unexpected response: {0}")]
    Protocol(String),
}

pub type ApiResult<T> = Result<T, ApiError>;

#[derive(Debug, Clone)]
struct Auth {
    token: String,
    expires: i64,
    account_id: String,
}

/// A single glucose reading as reported by LibreLinkUp.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Reading {
    pub time: DateTime<Utc>,
    pub mg_dl: f32,
    /// 0 = unknown, 1 = falling fast … 5 = rising fast
    pub trend: u8,
}

#[derive(Debug, Clone)]
pub struct Connection {
    pub patient_id: String,
    pub first_name: String,
    pub last_name: String,
    pub target_low: f32,
    pub target_high: f32,
    /// true when the account is configured for mg/dL
    pub uses_mg_dl: bool,
    pub current: Option<Reading>,
    pub sensor_serial: String,
    pub sensor_activated: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone)]
pub struct Graph {
    pub connection: Connection,
    pub history: Vec<Reading>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Server {
    pub region: String,
    pub url: String,
}

pub struct Client {
    http: reqwest::Client,
    server: String,
    identity: Identity,
    auth: Option<Auth>,
    regional_map: Option<BTreeMap<String, String>>,
    devlog: DevLog,
    /// Last time a 429 told us to back off.
    blocked_until: Option<Instant>,
}

impl Client {
    pub fn new(server: Option<String>, identity: Identity, devlog: DevLog) -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .gzip(true)
            .build()
            .expect("failed to build HTTP client");
        Self {
            http,
            server: server.filter(|s| !s.is_empty()).unwrap_or_else(|| GLOBAL_SERVER.into()),
            identity,
            auth: None,
            regional_map: None,
            devlog,
            blocked_until: None,
        }
    }

    pub fn server(&self) -> &str {
        &self.server
    }

    pub fn set_server(&mut self, server: &str) {
        if self.server != server {
            self.server = server.trim_end_matches('/').to_string();
            self.auth = None;
        }
    }

    pub fn set_identity(&mut self, identity: Identity) {
        if self.identity != identity {
            self.identity = identity;
            self.auth = None;
        }
    }

    pub fn identity(&self) -> &Identity {
        &self.identity
    }

    pub fn is_authenticated(&self) -> bool {
        self.auth.as_ref().is_some_and(|a| a.expires > Utc::now().timestamp() + 60)
    }

    pub fn logout(&mut self) {
        self.auth = None;
    }

    fn base_headers(&self, rb: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        rb.header("product", &self.identity.product)
            .header("version", &self.identity.version)
            .header("User-Agent", self.identity.user_agent())
            .header("Accept", "application/json")
            .header("Accept-Language", "en-US,en;q=0.9")
            .header("Cache-Control", "no-cache")
            .header("Pragma", "no-cache")
    }

    /// Performs a request and returns the parsed JSON body. Also feeds the developer log.
    async fn request(
        &mut self,
        method: Method,
        url: &str,
        body: Option<&Value>,
        bearer: Option<(&str, Option<&str>)>,
    ) -> ApiResult<(StatusCode, Value)> {
        if let Some(until) = self.blocked_until {
            let now = Instant::now();
            if until > now {
                return Err(ApiError::RateLimited((until - now).as_secs().max(1)));
            }
            self.blocked_until = None;
        }

        let mut rb = self.base_headers(self.http.request(method.clone(), url));
        if let Some((token, account_id)) = bearer {
            rb = rb.bearer_auth(token);
            if let Some(id) = account_id {
                rb = rb.header("Account-Id", id);
            }
        }
        if let Some(b) = body {
            rb = rb.json(b);
        } else if method == Method::POST {
            rb = rb.header("Content-Type", "application/json").body("{}");
        }

        let started = Instant::now();
        let result = rb.send().await;
        let elapsed = started.elapsed();
        let resp = match result {
            Ok(r) => r,
            Err(e) => {
                self.devlog.api(method.as_str(), url, None, elapsed, body, &format!("error: {e}"));
                return Err(ApiError::Network(e.to_string()));
            }
        };
        let status = resp.status();
        let retry_after = resp
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok());
        let text = resp.text().await.map_err(|e| ApiError::Network(e.to_string()))?;
        self.devlog.api(method.as_str(), url, Some(status.as_u16()), elapsed, body, &text);

        let json: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        let body_status = json.get("status").and_then(Value::as_i64);

        if status == StatusCode::TOO_MANY_REQUESTS || body_status == Some(429) {
            let secs = retry_after
                .or_else(|| json.pointer("/data/lockout").and_then(Value::as_u64))
                .unwrap_or(60)
                .clamp(5, 900);
            self.blocked_until = Some(Instant::now() + Duration::from_secs(secs));
            return Err(ApiError::RateLimited(secs));
        }
        if body_status == Some(920) {
            let min = json
                .pointer("/data/minimumVersion")
                .and_then(Value::as_str)
                .unwrap_or("?")
                .to_string();
            return Err(ApiError::VersionTooOld(min));
        }
        if status == StatusCode::UNAUTHORIZED {
            return Err(ApiError::Unauthorized);
        }
        if json.is_null() {
            return Err(ApiError::Protocol(format!("HTTP {status}, body is not JSON")));
        }
        Ok((status, json))
    }

    /// Returns the map region → API base URL, fetched once from Abbott's country config and
    /// merged with the built-in list (so newly added regions are picked up automatically).
    pub async fn servers(&mut self) -> Vec<Server> {
        if self.regional_map.is_none() {
            let url = format!("{GLOBAL_SERVER}/llu/config/country?country=DE");
            let mut map = BTreeMap::new();
            if let Ok((_, json)) = self.request(Method::GET, &url, None, None).await {
                if let Some(obj) = json.pointer("/data/regionalMap").and_then(Value::as_object) {
                    for (region, entry) in obj {
                        if let Some(api) = entry.get("lslApi").and_then(Value::as_str) {
                            map.insert(region.to_lowercase(), api.trim_end_matches('/').to_string());
                        }
                    }
                }
            }
            if !map.is_empty() {
                self.regional_map = Some(map);
            }
        }
        let mut out: Vec<Server> = BUILTIN_SERVERS
            .iter()
            .map(|(r, u)| Server { region: r.to_string(), url: u.to_string() })
            .collect();
        if let Some(map) = &self.regional_map {
            for (region, url) in map {
                match out.iter_mut().find(|s| &s.region == region) {
                    Some(s) => s.url = url.clone(),
                    None => out.push(Server { region: region.clone(), url: url.clone() }),
                }
            }
        }
        out
    }

    async fn server_for_region(&mut self, region: &str) -> String {
        let region = region.to_lowercase();
        self.servers()
            .await
            .into_iter()
            .find(|s| s.region == region)
            .map(|s| s.url)
            .unwrap_or_else(|| format!("https://api-{region}.libreview.io"))
    }

    /// Logs in, following regional redirects. On success the resolved server is kept and can
    /// be persisted via [`Client::server`].
    pub async fn login(&mut self, email: &str, password: &str) -> ApiResult<()> {
        let body = json!({ "email": email, "password": password });
        let mut version_bumped = false;
        let mut hops = 0;
        loop {
            let url = format!("{}/llu/auth/login", self.server);
            let (_, json) = match self.request(Method::POST, &url, Some(&body), None).await {
                Err(ApiError::VersionTooOld(min)) if !version_bumped && min != "?" => {
                    // The server tells us the minimum version; adopt it once and retry.
                    if version_cmp(&min, &self.identity.version).is_gt() {
                        tracing::warn!("server requires client version {min}, upgrading identity");
                        self.identity.version = min;
                    }
                    version_bumped = true;
                    continue;
                }
                other => other?,
            };
            let status = json.get("status").and_then(Value::as_i64).unwrap_or(-1);
            let data = json.get("data").cloned().unwrap_or(Value::Null);

            if data.get("redirect").and_then(Value::as_bool) == Some(true) {
                let region = data.get("region").and_then(Value::as_str).unwrap_or_default();
                hops += 1;
                if region.is_empty() || hops > 4 {
                    return Err(ApiError::Protocol("too many region redirects".into()));
                }
                let next = self.server_for_region(region).await;
                tracing::info!("login redirected to region '{region}' ({next})");
                self.server = next;
                continue;
            }

            match status {
                0 => {
                    let token = data
                        .pointer("/authTicket/token")
                        .and_then(Value::as_str)
                        .ok_or_else(|| ApiError::Protocol("login response without token".into()))?;
                    let expires = data
                        .pointer("/authTicket/expires")
                        .and_then(Value::as_i64)
                        .unwrap_or_else(|| Utc::now().timestamp() + 3600);
                    let user_id = data
                        .pointer("/user/id")
                        .and_then(Value::as_str)
                        .ok_or_else(|| ApiError::Protocol("login response without user id".into()))?;
                    self.auth = Some(Auth {
                        token: token.to_string(),
                        expires,
                        account_id: hex::encode(Sha256::digest(user_id.as_bytes())),
                    });
                    return Ok(());
                }
                4 => {
                    let step = data.get("step").cloned().unwrap_or(Value::Null);
                    let ticket = data
                        .pointer("/authTicket/token")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    return Err(ApiError::StepRequired(PendingStep {
                        kind: step.get("type").and_then(Value::as_str).unwrap_or("tou").to_string(),
                        component: step
                            .get("componentName")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                        ticket,
                        server: self.server.clone(),
                    }));
                }
                2 => return Err(ApiError::BadCredentials),
                _ => {
                    let msg = json
                        .pointer("/error/message")
                        .and_then(Value::as_str)
                        .unwrap_or("login failed");
                    return Err(ApiError::Protocol(format!("status {status}: {msg}")));
                }
            }
        }
    }

    /// Accepts a pending login step (only called after the user explicitly agreed in the UI).
    pub async fn accept_step(&mut self, step: &PendingStep) -> ApiResult<()> {
        let url = format!("{}/auth/continue/{}", step.server, step.kind);
        let (_, json) = self
            .request(Method::POST, &url, None, Some((&step.ticket, None)))
            .await?;
        match json.get("status").and_then(Value::as_i64) {
            Some(0) => Ok(()),
            other => Err(ApiError::Protocol(format!("accepting step failed (status {other:?})"))),
        }
    }

    fn bearer(&self) -> ApiResult<(String, String)> {
        let auth = self.auth.as_ref().ok_or(ApiError::NotLoggedIn)?;
        Ok((auth.token.clone(), auth.account_id.clone()))
    }

    async fn get_authed(&mut self, path: &str) -> ApiResult<Value> {
        let (token, account_id) = self.bearer()?;
        let url = format!("{}{}", self.server, path);
        let (status, json) = self
            .request(Method::GET, &url, None, Some((&token, Some(&account_id))))
            .await?;
        let body_status = json.get("status").and_then(Value::as_i64);
        if status == StatusCode::FORBIDDEN || body_status == Some(4) {
            // Expired / revoked session or a newly pending legal step: force a fresh login.
            self.auth = None;
            return Err(ApiError::Unauthorized);
        }
        if body_status != Some(0) {
            return Err(ApiError::Protocol(format!("HTTP {status}, status {body_status:?}")));
        }
        Ok(json)
    }

    pub async fn connections(&mut self) -> ApiResult<Vec<Connection>> {
        let json = self.get_authed("/llu/connections").await?;
        let list = json.get("data").and_then(Value::as_array).cloned().unwrap_or_default();
        Ok(list.iter().map(parse_connection).collect())
    }

    pub async fn graph(&mut self, patient_id: &str) -> ApiResult<Graph> {
        let json = self.get_authed(&format!("/llu/connections/{patient_id}/graph")).await?;
        let data = json.get("data").cloned().unwrap_or(Value::Null);
        let mut connection = parse_connection(data.get("connection").unwrap_or(&Value::Null));
        if connection.sensor_activated.is_none() {
            connection.sensor_activated = data
                .pointer("/activeSensors/0/sensor/a")
                .and_then(Value::as_i64)
                .and_then(|a| DateTime::from_timestamp(a, 0));
        }
        let mut history: Vec<Reading> = data
            .get("graphData")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(parse_reading).collect())
            .unwrap_or_default();
        if let Some(cur) = connection.current {
            history.push(cur);
        }
        history.sort_by_key(|r| r.time);
        history.dedup_by_key(|r| r.time);
        Ok(Graph { connection, history })
    }
}

fn parse_connection(v: &Value) -> Connection {
    let s = |k: &str| v.get(k).and_then(Value::as_str).unwrap_or_default().to_string();
    let f = |k: &str, d: f32| v.get(k).and_then(Value::as_f64).map(|x| x as f32).unwrap_or(d);
    let gm = v.get("glucoseMeasurement").or_else(|| v.get("glucoseItem"));
    let current = gm.and_then(parse_reading);
    let units = gm.and_then(|g| g.get("GlucoseUnits")).and_then(Value::as_i64);
    Connection {
        patient_id: s("patientId"),
        first_name: s("firstName"),
        last_name: s("lastName"),
        target_low: f("targetLow", 70.0),
        target_high: f("targetHigh", 180.0),
        uses_mg_dl: units.map(|u| u == 1).unwrap_or_else(|| v.get("uom").and_then(Value::as_i64) == Some(1)),
        current,
        sensor_serial: v.pointer("/sensor/sn").and_then(Value::as_str).unwrap_or_default().to_string(),
        sensor_activated: v
            .pointer("/sensor/a")
            .and_then(Value::as_i64)
            .and_then(|a| DateTime::from_timestamp(a, 0)),
    }
}

fn parse_reading(v: &Value) -> Option<Reading> {
    let mg_dl = v.get("ValueInMgPerDl").and_then(Value::as_f64)? as f32;
    let ts = v
        .get("FactoryTimestamp")
        .or_else(|| v.get("Timestamp"))
        .and_then(Value::as_str)?;
    Some(Reading {
        time: parse_llu_time(ts)?,
        mg_dl,
        trend: v.get("TrendArrow").and_then(Value::as_u64).unwrap_or(0).min(5) as u8,
    })
}

/// LibreLinkUp uses US formatted timestamps, `FactoryTimestamp` is UTC: `10/28/2026 9:21:09 PM`.
pub fn parse_llu_time(s: &str) -> Option<DateTime<Utc>> {
    NaiveDateTime::parse_from_str(s.trim(), "%m/%d/%Y %I:%M:%S %p")
        .ok()
        .map(|n| n.and_utc())
}

fn version_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    let parse = |s: &str| s.split('.').map(|p| p.parse::<u32>().unwrap_or(0)).collect::<Vec<_>>();
    parse(a).cmp(&parse(b))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_factory_timestamp() {
        let t = parse_llu_time("10/28/2026 9:21:09 PM").unwrap();
        assert_eq!(t.to_rfc3339(), "2026-10-28T21:21:09+00:00");
    }

    #[test]
    fn parses_connection() {
        let v = json!({
            "patientId": "p1", "firstName": "A", "lastName": "B",
            "targetLow": 70, "targetHigh": 180,
            "glucoseMeasurement": {
                "FactoryTimestamp": "1/2/2026 3:04:05 AM", "ValueInMgPerDl": 112,
                "TrendArrow": 4, "GlucoseUnits": 0, "isHigh": false, "isLow": false
            },
            "sensor": { "sn": "ABC", "a": 1767225600 }
        });
        let c = parse_connection(&v);
        assert_eq!(c.patient_id, "p1");
        assert!(!c.uses_mg_dl);
        let r = c.current.unwrap();
        assert_eq!(r.mg_dl, 112.0);
        assert_eq!(r.trend, 4);
        assert_eq!(c.sensor_serial, "ABC");
    }

    #[test]
    fn compares_versions() {
        assert!(version_cmp("5.0.1", "4.16.0").is_gt());
        assert!(version_cmp("4.16.0", "4.17.0").is_lt());
    }
}
