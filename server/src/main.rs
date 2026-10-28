//! `bllu-server` – always-on caching server for Better LibreLinkUp.
//!
//! Polls LibreLinkUp around the clock, keeps every reading in the same (optionally encrypted)
//! monthly store the desktop app uses and serves the history to paired desktop apps. See
//! `bllu_core::sync` for the security model; this binary adds the server-side defences:
//! TLS 1.3 + mandatory client certificates, tokens bound to those certificates, per-IP rate
//! limiting with temporary bans, an optional IP allowlist, request size/time/concurrency limits,
//! private file permissions and an audit log.

use std::collections::HashMap;
use std::future::Future;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result, anyhow, bail};
use axum::extract::{ConnectInfo, DefaultBodyLimit, Query, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Extension, Json, Router};
use axum_server::accept::Accept;
use axum_server::tls_rustls::{RustlsAcceptor, RustlsConfig};
use bllu_core::api::{self, ApiError, Client, Identity};
use bllu_core::devlog::DevLog;
use bllu_core::store::Store;
use bllu_core::sync::{
    self, API_VERSION, Health, PairRequest, PairResponse, PairingCode, PatientInfo, ReadingsResponse, RotateResponse,
};
use chrono::Utc;
use ipnet::IpNet;
use rustls::DistinguishedName;
use rustls::crypto::{CryptoProvider, verify_tls12_signature, verify_tls13_signature};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, UnixTime};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncWrite};
use tower_http::add_extension::AddExtension;

const VERSION: &str = env!("CARGO_PKG_VERSION");
const MAX_RANGE_DAYS: i64 = 400;
/// Failed authentication / pairing attempts per IP before a temporary ban.
const MAX_FAILURES: u32 = 10;
const FAILURE_WINDOW: Duration = Duration::from_secs(600);
const BAN_DURATION: Duration = Duration::from_secs(900);
const MAX_PENDING_CODES: usize = 5;
const DEFAULT_PAIR_TTL_MIN: i64 = 10;

// ---------------------------------------------------------------- configuration

#[derive(Clone)]
struct Config {
    data_dir: PathBuf,
    listen: SocketAddr,
    public_url: Option<String>,
    name: String,
    tls_sans: Vec<String>,
    tls_cert: Option<PathBuf>,
    tls_key: Option<PathBuf>,
    llu_email: Option<String>,
    llu_password: Option<String>,
    llu_server: Option<String>,
    identity: Identity,
    poll_seconds: u64,
    passphrase: Option<String>,
    /// Only these networks may connect at all (empty = everyone who can reach the port).
    allow: Vec<IpNet>,
    /// `BLLU_DEMO=1`: serve synthetic data instead of polling LibreLinkUp (for trying it out).
    demo: bool,
}

/// Reads `NAME`, or the file named by `NAME_FILE` (Docker secrets).
fn env_secret(name: &str) -> Option<String> {
    if let Ok(path) = std::env::var(format!("{name}_FILE")) {
        return std::fs::read_to_string(path).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    }
    std::env::var(name).ok().filter(|s| !s.is_empty())
}

fn env_or(name: &str, default: &str) -> String {
    std::env::var(name).ok().filter(|s| !s.is_empty()).unwrap_or_else(|| default.to_string())
}

fn parse_allow(s: &str) -> Result<Vec<IpNet>> {
    s.split(',')
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .map(|p| {
            p.parse::<IpNet>()
                .or_else(|_| p.parse::<IpAddr>().map(IpNet::from))
                .with_context(|| format!("BLLU_ALLOW_CIDRS: invalid network \"{p}\""))
        })
        .collect()
}

impl Config {
    fn from_env() -> Result<Self> {
        let listen: SocketAddr = env_or("BLLU_LISTEN", "0.0.0.0:8443").parse().context("BLLU_LISTEN")?;
        let host = std::env::var("HOSTNAME")
            .or_else(|_| std::env::var("COMPUTERNAME"))
            .unwrap_or_else(|_| "localhost".into());
        let mut sans: Vec<String> = env_or("BLLU_TLS_SANS", "")
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        if sans.is_empty() {
            sans = vec![host.clone(), "localhost".into(), "127.0.0.1".into()];
        }
        let mut identity = Identity::default();
        if let Ok(v) = std::env::var("LLU_VERSION") {
            identity.version = v;
        }
        if let Ok(p) = std::env::var("LLU_PRODUCT") {
            identity.product = p;
        }
        Ok(Self {
            data_dir: PathBuf::from(env_or("BLLU_DATA_DIR", "./data")),
            listen,
            public_url: std::env::var("BLLU_PUBLIC_URL").ok().filter(|s| !s.is_empty()),
            name: env_or("BLLU_NAME", &format!("bllu-server on {host}")),
            tls_sans: sans,
            tls_cert: std::env::var("BLLU_TLS_CERT").ok().map(PathBuf::from),
            tls_key: std::env::var("BLLU_TLS_KEY").ok().map(PathBuf::from),
            llu_email: env_secret("LLU_EMAIL"),
            llu_password: env_secret("LLU_PASSWORD"),
            llu_server: std::env::var("LLU_SERVER").ok().filter(|s| !s.is_empty()),
            identity,
            poll_seconds: env_or("BLLU_POLL_SECONDS", "60").parse::<u64>().unwrap_or(60).clamp(30, 900),
            passphrase: env_secret("BLLU_DATA_PASSPHRASE"),
            allow: parse_allow(&env_or("BLLU_ALLOW_CIDRS", ""))?,
            demo: std::env::var("BLLU_DEMO").is_ok_and(|v| v == "1" || v == "true"),
        })
    }

    /// URL put into pairing codes.
    fn public_url(&self) -> String {
        self.public_url.clone().unwrap_or_else(|| {
            let host = self.tls_sans.first().cloned().unwrap_or_else(|| "localhost".into());
            format!("https://{host}:{}", self.listen.port())
        })
    }

    fn allowed(&self, ip: IpAddr) -> bool {
        let ip = match ip {
            IpAddr::V6(v6) => v6.to_ipv4_mapped().map(IpAddr::V4).unwrap_or(ip),
            v4 => v4,
        };
        self.allow.is_empty() || self.allow.iter().any(|n| n.contains(&ip))
    }
}

// ---------------------------------------------------------------- persistent files

/// Restricts a file or directory to its owner (0600 / 0700) on Unix.
fn make_private(path: &Path, dir: bool) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(if dir { 0o700 } else { 0o600 }));
    }
    #[cfg(not(unix))]
    let _ = (path, dir);
}

/// `bllu-server pair` is often run as root next to a service running as `bllu`: hand files
/// over to the owner of their directory so the service can still rewrite them.
fn match_dir_owner(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if let Some(m) = path.parent().and_then(|d| std::fs::metadata(d).ok()) {
            let _ = std::os::unix::fs::chown(path, Some(m.uid()), Some(m.gid()));
        }
    }
    #[cfg(not(unix))]
    let _ = path;
}

fn write_private(path: &Path, data: &[u8]) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, data)?;
    make_private(&tmp, false);
    std::fs::rename(&tmp, path)?;
    match_dir_owner(path);
    Ok(())
}

fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    write_private(path, &serde_json::to_vec_pretty(value)?)
}

fn read_json<T: for<'de> Deserialize<'de> + Default>(path: &Path) -> Result<T> {
    match std::fs::read(path) {
        Ok(b) => serde_json::from_slice(&b).with_context(|| format!("parsing {}", path.display())),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(T::default()),
        Err(e) => Err(e.into()),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ClientEntry {
    id: String,
    name: String,
    token_sha256: String,
    /// fingerprint of the client certificate the token is bound to
    cert_sha256: String,
    created: i64,
    #[serde(default)]
    token_issued: i64,
    #[serde(default)]
    last_seen: Option<i64>,
    #[serde(default)]
    last_ip: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct ClientsFile {
    clients: Vec<ClientEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PendingPairing {
    secret_sha256: String,
    expires: i64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct PairingFile {
    pending: Vec<PendingPairing>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct PatientsFile {
    patients: Vec<PatientInfo>,
}

fn clients_path(dir: &Path) -> PathBuf {
    dir.join("clients.json")
}
fn pairing_path(dir: &Path) -> PathBuf {
    dir.join("pairing.json")
}
fn patients_path(dir: &Path) -> PathBuf {
    dir.join("patients.json")
}

/// Append-only security log (`audit.log`, JSON lines): pairing, rotation, revocation and every
/// rejected attempt. Never contains secrets.
fn audit(dir: &Path, event: &str, ip: Option<IpAddr>, detail: &str) {
    use std::io::Write;
    let line = serde_json::json!({
        "time": Utc::now().to_rfc3339(),
        "event": event,
        "ip": ip.map(|i| i.to_string()),
        "detail": detail,
    });
    let path = dir.join("audit.log");
    let new = !path.exists();
    match std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        Ok(mut f) => {
            let _ = writeln!(f, "{line}");
            if new {
                make_private(&path, false);
                match_dir_owner(&path);
            }
        }
        Err(e) => tracing::error!("audit log: {e}"),
    }
    tracing::info!(target: "audit", "{event} {} {detail}", ip.map(|i| i.to_string()).unwrap_or_default());
}

// ---------------------------------------------------------------- TLS

struct Tls {
    certs: Vec<CertificateDer<'static>>,
    key: PrivateKeyDer<'static>,
    fingerprint: String,
}

/// Loads the configured certificate, or the self-signed one in `<data>/tls`, creating it on
/// first start (ECDSA P-256).
fn load_or_create_tls(cfg: &Config) -> Result<Tls> {
    let (cert_path, key_path) = match (&cfg.tls_cert, &cfg.tls_key) {
        (Some(c), Some(k)) => (c.clone(), k.clone()),
        _ => {
            let dir = cfg.data_dir.join("tls");
            let (c, k) = (dir.join("cert.pem"), dir.join("key.pem"));
            if !c.exists() || !k.exists() {
                std::fs::create_dir_all(&dir)?;
                make_private(&dir, true);
                match_dir_owner(&dir);
                let mut params = rcgen::CertificateParams::new(cfg.tls_sans.clone())?;
                params.distinguished_name.push(rcgen::DnType::CommonName, cfg.name.clone());
                params.not_before = rcgen::date_time_ymd(2025, 1, 1);
                params.not_after = rcgen::date_time_ymd(2125, 1, 1);
                let key = rcgen::KeyPair::generate()?;
                let cert = params.self_signed(&key)?;
                write_private(&k, key.serialize_pem().as_bytes())?;
                write_private(&c, cert.pem().as_bytes())?;
                tracing::info!("created self-signed certificate for {:?}", cfg.tls_sans);
            }
            (c, k)
        }
    };
    let certs: Vec<CertificateDer<'static>> = CertificateDer::pem_file_iter(&cert_path)
        .with_context(|| format!("reading {}", cert_path.display()))?
        .collect::<Result<_, _>>()?;
    let key = PrivateKeyDer::from_pem_file(&key_path).with_context(|| format!("reading {}", key_path.display()))?;
    let first = certs.first().ok_or_else(|| anyhow!("no certificate in {}", cert_path.display()))?;
    let fingerprint = sync::fingerprint(first.as_ref());
    Ok(Tls { certs, key, fingerprint })
}

/// Requires a client certificate on every connection and checks that the client owns its key
/// (TLS handshake signature). *Which* certificate is acceptable is decided per request: the one
/// bound to the presented token.
#[derive(Debug)]
struct RequireClientCert {
    provider: Arc<CryptoProvider>,
}

impl ClientCertVerifier for RequireClientCert {
    fn offer_client_auth(&self) -> bool {
        true
    }

    fn client_auth_mandatory(&self) -> bool {
        true
    }

    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }

    fn verify_client_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> Result<ClientCertVerified, rustls::Error> {
        Ok(ClientCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.provider.signature_verification_algorithms.supported_schemes()
    }
}

fn server_tls_config(tls: Tls) -> Result<rustls::ServerConfig> {
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let mut config = rustls::ServerConfig::builder_with_provider(provider.clone())
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .with_client_cert_verifier(Arc::new(RequireClientCert { provider }))
        .with_single_cert(tls.certs, tls.key)?;
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    Ok(config)
}

/// SHA-256 of the client certificate of the TLS connection a request came in on.
#[derive(Debug, Clone)]
struct PeerCert(Option<String>);

/// Wraps the rustls acceptor and exposes the client certificate to the request handlers.
#[derive(Clone)]
struct PeerCertAcceptor(RustlsAcceptor);

impl<I, S> Accept<I, S> for PeerCertAcceptor
where
    I: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    S: Send + 'static,
{
    type Stream = tokio_rustls::server::TlsStream<I>;
    type Service = AddExtension<S, PeerCert>;
    type Future = Pin<Box<dyn Future<Output = io::Result<(Self::Stream, Self::Service)>> + Send>>;

    fn accept(&self, stream: I, service: S) -> Self::Future {
        let inner = self.0.clone();
        Box::pin(async move {
            let (stream, service) = inner.accept(stream, service).await?;
            let fp = stream
                .get_ref()
                .1
                .peer_certificates()
                .and_then(|c| c.first())
                .map(|c| sync::fingerprint(c.as_ref()));
            Ok((stream, AddExtension::new(service, PeerCert(fp))))
        })
    }
}

// ---------------------------------------------------------------- rate limiting

#[derive(Default)]
struct Attempts {
    failures: u32,
    window_start: Option<Instant>,
    banned_until: Option<Instant>,
}

#[derive(Default)]
struct Limiter(Mutex<HashMap<IpAddr, Attempts>>);

impl Limiter {
    fn banned(&self, ip: IpAddr) -> bool {
        let mut map = self.0.lock().unwrap();
        match map.get_mut(&ip) {
            Some(a) => match a.banned_until {
                Some(t) if t > Instant::now() => true,
                Some(_) => {
                    *a = Attempts::default();
                    false
                }
                None => false,
            },
            None => false,
        }
    }

    /// Records a failure; returns true when the IP just got banned.
    fn fail(&self, ip: IpAddr) -> bool {
        let mut map = self.0.lock().unwrap();
        if map.len() > 10_000 {
            map.retain(|_, a| a.banned_until.is_some());
        }
        let a = map.entry(ip).or_default();
        let now = Instant::now();
        if a.window_start.is_none_or(|s| now - s > FAILURE_WINDOW) {
            a.window_start = Some(now);
            a.failures = 0;
        }
        a.failures += 1;
        if a.failures >= MAX_FAILURES && a.banned_until.is_none() {
            a.banned_until = Some(now + BAN_DURATION);
            return true;
        }
        false
    }

    fn success(&self, ip: IpAddr) {
        let mut map = self.0.lock().unwrap();
        if map.get(&ip).is_some_and(|a| a.banned_until.is_none()) {
            map.remove(&ip);
        }
    }
}

// ---------------------------------------------------------------- shared state

struct AppState {
    cfg: Config,
    patients: RwLock<HashMap<String, PatientInfo>>,
    stores: Mutex<HashMap<String, Store>>,
    clients: Mutex<(ClientsFile, Option<SystemTime>)>,
    limiter: Limiter,
}

impl AppState {
    fn store_dir(&self, patient_id: &str) -> PathBuf {
        let h = sync::hash_token(patient_id);
        self.cfg.data_dir.join("patients").join(&h[..16])
    }

    /// Opens (once) the store of a patient. Argon2 makes this slow when encrypted.
    fn with_store<R>(&self, patient_id: &str, f: impl FnOnce(&mut Store) -> R) -> Result<R> {
        let mut stores = self.stores.lock().unwrap();
        if !stores.contains_key(patient_id) {
            let dir = self.store_dir(patient_id);
            let store = Store::open(&dir, self.cfg.passphrase.as_deref())
                .with_context(|| format!("opening the store of patient {}", &sync::hash_token(patient_id)[..8]))?;
            make_private(&dir, true);
            stores.insert(patient_id.to_string(), store);
        }
        Ok(f(stores.get_mut(patient_id).unwrap()))
    }

    /// Clients are re-read when `clients.json` changes, so `bllu-server revoke` takes effect
    /// immediately in the running server.
    fn with_clients<R>(&self, f: impl FnOnce(&mut ClientsFile) -> (R, bool)) -> Result<R> {
        let path = clients_path(&self.cfg.data_dir);
        let mut guard = self.clients.lock().unwrap();
        let mtime = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
        if mtime != guard.1 || mtime.is_none() {
            guard.0 = read_json(&path)?;
            guard.1 = mtime;
        }
        let (result, changed) = f(&mut guard.0);
        if changed {
            write_json(&path, &guard.0)?;
            guard.1 = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
        }
        Ok(result)
    }

    /// The client whose token *and* bound certificate match (both compared in constant time).
    fn authenticate(&self, token: &str, cert: &str, ip: IpAddr) -> Option<ClientEntry> {
        let token_hash = sync::hash_token(token);
        let now = Utc::now().timestamp();
        self.with_clients(|file| {
            let mut found: Option<usize> = None;
            for (i, c) in file.clients.iter().enumerate() {
                let ok = sync::ct_eq(c.token_sha256.as_bytes(), token_hash.as_bytes())
                    & sync::ct_eq(c.cert_sha256.as_bytes(), cert.as_bytes());
                if ok {
                    found = Some(i);
                }
            }
            let Some(i) = found else { return (None, false) };
            let c = &mut file.clients[i];
            let ip_s = ip.to_string();
            let save = c.last_seen.is_none_or(|t| now - t > 3600) || c.last_ip.as_deref() != Some(&ip_s);
            c.last_seen = Some(now);
            c.last_ip = Some(ip_s);
            (Some(c.clone()), save)
        })
        .unwrap_or_else(|e| {
            tracing::error!("{e:#}");
            None
        })
    }

    fn save_patients(&self) {
        let list: Vec<PatientInfo> = self.patients.read().unwrap().values().cloned().collect();
        if let Err(e) = write_json(&patients_path(&self.cfg.data_dir), &PatientsFile { patients: list }) {
            tracing::error!("saving patients.json: {e:#}");
        }
    }
}

// ---------------------------------------------------------------- HTTP API

struct ApiErr(StatusCode, &'static str);

impl IntoResponse for ApiErr {
    fn into_response(self) -> Response {
        (self.0, Json(serde_json::json!({ "error": self.1 }))).into_response()
    }
}

/// Runs before every handler: IP allowlist, temporary bans and hardening headers.
async fn guard(
    State(state): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    req: Request,
    next: Next,
) -> Response {
    let ip = addr.ip();
    if !state.cfg.allowed(ip) {
        audit(&state.cfg.data_dir, "denied_network", Some(ip), "not in BLLU_ALLOW_CIDRS");
        return ApiErr(StatusCode::FORBIDDEN, "forbidden").into_response();
    }
    if state.limiter.banned(ip) {
        return ApiErr(StatusCode::TOO_MANY_REQUESTS, "too many failed attempts, try again later").into_response();
    }
    let mut resp = next.run(req).await;
    let h = resp.headers_mut();
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    h.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    h.insert("x-frame-options", HeaderValue::from_static("DENY"));
    h.insert("referrer-policy", HeaderValue::from_static("no-referrer"));
    h.insert(header::STRICT_TRANSPORT_SECURITY, HeaderValue::from_static("max-age=31536000"));
    resp
}

fn failure(state: &AppState, ip: IpAddr, event: &str, detail: &str) {
    audit(&state.cfg.data_dir, event, Some(ip), detail);
    if state.limiter.fail(ip) {
        audit(&state.cfg.data_dir, "banned", Some(ip), &format!("{MAX_FAILURES} failures, blocked for 15 min"));
    }
}

fn authorize(state: &AppState, headers: &HeaderMap, peer: &PeerCert, ip: IpAddr) -> Result<ClientEntry, ApiErr> {
    let token = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::trim);
    let (Some(token), Some(cert)) = (token, peer.0.as_deref()) else {
        failure(state, ip, "auth_failed", "missing token or client certificate");
        return Err(ApiErr(StatusCode::UNAUTHORIZED, "unauthorized"));
    };
    match state.authenticate(token, cert, ip) {
        Some(c) => {
            state.limiter.success(ip);
            Ok(c)
        }
        None => {
            failure(state, ip, "auth_failed", "unknown token or token/certificate mismatch");
            Err(ApiErr(StatusCode::UNAUTHORIZED, "unauthorized"))
        }
    }
}

fn internal(e: anyhow::Error) -> ApiErr {
    tracing::error!("{e:#}");
    ApiErr(StatusCode::INTERNAL_SERVER_ERROR, "internal error")
}

async fn health(State(state): State<Arc<AppState>>) -> Json<Health> {
    Json(Health { name: state.cfg.name.clone(), version: VERSION.into(), api: API_VERSION })
}

async fn pair(
    State(state): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Extension(peer): Extension<PeerCert>,
    Json(req): Json<PairRequest>,
) -> Result<Json<PairResponse>, ApiErr> {
    let ip = addr.ip();
    let dir = state.cfg.data_dir.clone();
    let Some(cert) = peer.0 else {
        return Err(ApiErr(StatusCode::UNAUTHORIZED, "client certificate required"));
    };
    let now = Utc::now().timestamp();
    let mut pending: PairingFile = read_json(&pairing_path(&dir)).map_err(internal)?;
    let before = pending.pending.len();
    pending.pending.retain(|p| p.expires > now);
    if pending.pending.len() != before {
        write_json(&pairing_path(&dir), &pending).map_err(internal)?;
    }
    if pending.pending.is_empty() {
        // pairing is closed unless an administrator created a code on the server
        failure(&state, ip, "pair_closed", "no pairing code is pending");
        return Err(ApiErr(StatusCode::UNAUTHORIZED, "invalid or expired pairing code"));
    }
    let hash = sync::hash_token(&req.secret);
    let matched = pending.pending.iter().position(|p| sync::ct_eq(p.secret_sha256.as_bytes(), hash.as_bytes()));
    let Some(i) = matched else {
        // slow down guessing (the secret has 192 bits anyway)
        tokio::time::sleep(Duration::from_millis(500)).await;
        failure(&state, ip, "pair_failed", "wrong pairing secret");
        return Err(ApiErr(StatusCode::UNAUTHORIZED, "invalid or expired pairing code"));
    };
    pending.pending.remove(i); // single use
    write_json(&pairing_path(&dir), &pending).map_err(internal)?;

    let token = sync::random_secret(32);
    let entry = ClientEntry {
        id: hex::encode(rand::random::<[u8; 4]>()),
        name: req.client_name.chars().filter(|c| !c.is_control()).take(64).collect(),
        token_sha256: sync::hash_token(&token),
        cert_sha256: cert.clone(),
        created: now,
        token_issued: now,
        last_seen: Some(now),
        last_ip: Some(ip.to_string()),
    };
    state
        .with_clients(|f| {
            // one binding per client certificate
            f.clients.retain(|c| c.cert_sha256 != cert);
            f.clients.push(entry.clone());
            ((), true)
        })
        .map_err(internal)?;
    state.limiter.success(ip);
    audit(
        &dir,
        "paired",
        Some(ip),
        &format!("client {} \"{}\" certificate {}", entry.id, entry.name, sync::short_fingerprint(&cert)),
    );
    Ok(Json(PairResponse { client_id: entry.id, token, server_name: state.cfg.name.clone() }))
}

async fn rotate(
    State(state): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Extension(peer): Extension<PeerCert>,
    headers: HeaderMap,
) -> Result<Json<RotateResponse>, ApiErr> {
    let client = authorize(&state, &headers, &peer, addr.ip())?;
    let token = sync::random_secret(32);
    let hash = sync::hash_token(&token);
    let now = Utc::now().timestamp();
    state
        .with_clients(|f| {
            if let Some(c) = f.clients.iter_mut().find(|c| c.id == client.id) {
                c.token_sha256 = hash;
                c.token_issued = now;
            }
            ((), true)
        })
        .map_err(internal)?;
    audit(&state.cfg.data_dir, "token_rotated", Some(addr.ip()), &format!("client {}", client.id));
    Ok(Json(RotateResponse { token }))
}

async fn patients(
    State(state): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Extension(peer): Extension<PeerCert>,
    headers: HeaderMap,
) -> Result<Json<Vec<PatientInfo>>, ApiErr> {
    authorize(&state, &headers, &peer, addr.ip())?;
    let mut list: Vec<PatientInfo> = state.patients.read().unwrap().values().cloned().collect();
    list.sort_by(|a, b| a.first_name.cmp(&b.first_name));
    Ok(Json(list))
}

#[derive(Deserialize)]
struct ReadingsQuery {
    patient: String,
    since: i64,
    until: Option<i64>,
}

async fn readings(
    State(state): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Extension(peer): Extension<PeerCert>,
    headers: HeaderMap,
    Query(q): Query<ReadingsQuery>,
) -> Result<Json<ReadingsResponse>, ApiErr> {
    authorize(&state, &headers, &peer, addr.ip())?;
    if q.patient.len() > 128 || !state.patients.read().unwrap().contains_key(&q.patient) {
        return Err(ApiErr(StatusCode::NOT_FOUND, "unknown patient"));
    }
    let now = Utc::now().timestamp();
    let until = q.until.unwrap_or(now + 300).clamp(0, now + 300);
    let since = q.since.clamp(until - MAX_RANGE_DAYS * 86_400, until);
    let (from, to) = (
        chrono::DateTime::from_timestamp(since, 0).unwrap_or_default(),
        chrono::DateTime::from_timestamp(until, 0).unwrap_or_default(),
    );
    let list = state.with_store(&q.patient, |s| s.range(from, to)).map_err(internal)?;
    Ok(Json(ReadingsResponse { patient: q.patient, readings: list.iter().map(sync::to_wire).collect() }))
}

fn router(state: Arc<AppState>) -> Router {
    use tower::ServiceBuilder;
    Router::new()
        .route("/api/v1/health", get(health))
        .route("/api/v1/pair", post(pair))
        .route("/api/v1/rotate", post(rotate))
        .route("/api/v1/patients", get(patients))
        .route("/api/v1/readings", get(readings))
        .layer(middleware::from_fn_with_state(state.clone(), guard))
        .layer(DefaultBodyLimit::max(8 * 1024))
        .layer(
            ServiceBuilder::new()
                .layer(tower_http::timeout::TimeoutLayer::with_status_code(
                    StatusCode::REQUEST_TIMEOUT,
                    Duration::from_secs(30),
                ))
                .layer(tower::limit::ConcurrencyLimitLayer::new(64)),
        )
        .with_state(state)
}

// ---------------------------------------------------------------- LibreLinkUp poller

async fn poller(state: Arc<AppState>) {
    let cfg = &state.cfg;
    let (Some(email), Some(password)) = (cfg.llu_email.clone(), cfg.llu_password.clone()) else {
        tracing::error!("LLU_EMAIL and LLU_PASSWORD (or LLU_PASSWORD_FILE) are not set – not polling LibreLinkUp");
        return;
    };
    let devlog = DevLog::default();
    devlog.set_enabled(std::env::var("BLLU_DEBUG_API").is_ok());
    let resolved_path = cfg.data_dir.join("llu-server.txt");
    let cached = std::fs::read_to_string(&resolved_path).ok().map(|s| s.trim().to_string());
    let mut client = Client::new(cfg.llu_server.clone().or(cached), cfg.identity.clone(), devlog);
    let mut connections: Vec<api::Connection> = Vec::new();
    let mut connections_at: Option<tokio::time::Instant> = None;

    loop {
        let mut wait = cfg.poll_seconds;
        let result: Result<(), ApiError> = async {
            if !client.is_authenticated() {
                client.login(&email, &password).await?;
                let _ = write_private(&resolved_path, client.server().as_bytes());
                tracing::info!("logged in to LibreLinkUp via {}", client.server());
            }
            if connections_at.is_none_or(|t| t.elapsed() > Duration::from_secs(600)) {
                connections = client.connections().await?;
                connections_at = Some(tokio::time::Instant::now());
                if connections.is_empty() {
                    tracing::warn!("this LibreLinkUp account follows nobody yet");
                }
            }
            for c in connections.clone() {
                let graph = client.graph(&c.patient_id).await?;
                let pid = c.patient_id.clone();
                let new = state
                    .with_store(&pid, |s| {
                        let n = s.insert(&graph.history);
                        if n > 0 {
                            if let Err(e) = s.flush() {
                                tracing::error!("writing store: {e:#}");
                            }
                        }
                        (n, s.first_time())
                    })
                    .map_err(|e| ApiError::Protocol(format!("{e:#}")))?;
                let g = &graph.connection;
                let info = PatientInfo {
                    id: pid.clone(),
                    first_name: g.first_name.clone(),
                    last_name: g.last_name.clone(),
                    uses_mg_dl: g.uses_mg_dl,
                    target_low: g.target_low,
                    target_high: g.target_high,
                    sensor_serial: g.sensor_serial.clone(),
                    sensor_activated: g.sensor_activated.map(|t| t.timestamp()),
                    current: g.current.as_ref().map(sync::to_wire),
                    last_poll: Some(Utc::now().timestamp()),
                    last_error: None,
                    first_reading: new.1.map(|t| t.timestamp()),
                };
                state.patients.write().unwrap().insert(pid, info);
                tracing::debug!("{} new readings", new.0);
            }
            state.save_patients();
            Ok(())
        }
        .await;

        if let Err(e) = result {
            match &e {
                ApiError::RateLimited(s) => wait = (*s).max(wait),
                ApiError::Unauthorized => client.logout(),
                ApiError::BadCredentials => wait = 900,
                ApiError::StepRequired(step) => {
                    tracing::error!(
                        "LibreLinkUp requires accepting the {} – open the LibreLinkUp app (or the desktop app) and accept it",
                        step.describe()
                    );
                    wait = 1800;
                }
                _ => wait = wait.min(60),
            }
            tracing::warn!("poll failed: {e} (retrying in {wait} s)");
            for p in state.patients.write().unwrap().values_mut() {
                p.last_error = Some(e.to_string());
            }
        }
        tokio::time::sleep(Duration::from_secs(wait)).await;
    }
}

async fn demo_poller(state: Arc<AppState>) {
    use bllu_core::demo;
    let pid = "demo-patient".to_string();
    let now = Utc::now();
    let seeded = state.with_store(&pid, |s| {
        s.insert(&demo::readings(now - chrono::Duration::days(14), now - chrono::Duration::hours(1), 300));
        s.flush()
    });
    if let Err(e) = seeded {
        tracing::error!("demo: {e:#}");
        return;
    }
    loop {
        let now = Utc::now();
        let recent = demo::readings(now - chrono::Duration::hours(1), now, 60);
        let first = state.with_store(&pid, |s| {
            s.insert(&recent);
            let _ = s.flush();
            s.first_time()
        });
        let c = demo::connection("Demo", "Patient", recent.last().copied());
        let info = PatientInfo {
            id: pid.clone(),
            first_name: c.first_name,
            last_name: c.last_name,
            uses_mg_dl: c.uses_mg_dl,
            target_low: c.target_low,
            target_high: c.target_high,
            sensor_serial: c.sensor_serial,
            sensor_activated: c.sensor_activated.map(|t| t.timestamp()),
            current: c.current.as_ref().map(sync::to_wire),
            last_poll: Some(now.timestamp()),
            last_error: None,
            first_reading: first.ok().flatten().map(|t| t.timestamp()),
        };
        state.patients.write().unwrap().insert(pid.clone(), info);
        state.save_patients();
        tokio::time::sleep(Duration::from_secs(60)).await;
    }
}

// ---------------------------------------------------------------- commands

fn create_pairing_code(cfg: &Config, tls: &Tls, ttl_minutes: i64, url: Option<String>) -> Result<String> {
    let secret = sync::random_secret(24);
    let path = pairing_path(&cfg.data_dir);
    let mut file: PairingFile = read_json(&path)?;
    let now = Utc::now().timestamp();
    file.pending.retain(|p| p.expires > now);
    if file.pending.len() >= MAX_PENDING_CODES {
        // the oldest unused code is dropped
        file.pending.remove(0);
    }
    file.pending.push(PendingPairing { secret_sha256: sync::hash_token(&secret), expires: now + ttl_minutes * 60 });
    write_json(&path, &file)?;
    audit(&cfg.data_dir, "pairing_code_created", None, &format!("valid {ttl_minutes} min"));
    // a certificate from a public CA does not need pinning
    let pinned = if cfg.tls_cert.is_some() && std::env::var("BLLU_PIN_CUSTOM_CERT").is_err() {
        String::new()
    } else {
        tls.fingerprint.clone()
    };
    Ok(PairingCode { url: url.unwrap_or_else(|| cfg.public_url()), fingerprint: pinned, secret }.encode())
}

fn print_pairing(cfg: &Config, tls: &Tls, code: &str, ttl: i64) {
    println!();
    println!("  Pairing code for {} (valid {ttl} minutes, single use):", cfg.public_url());
    println!();
    println!("  {code}");
    println!();
    println!("  Paste it in Better LibreLinkUp → Settings → Caching server.");
    println!("  Treat it like a password: whoever uses it first is paired.");
    println!("  Certificate fingerprint: {}", sync::short_fingerprint(&tls.fingerprint));
    println!();
}

fn arg_value(args: &[String], name: &str) -> Option<String> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned()
}

/// Creates the data folder and keeps it private to the service user: it holds pairing
/// secrets, token hashes, the TLS key and the readings.
fn prepare_data_dir(cfg: &Config) -> Result<()> {
    std::fs::create_dir_all(&cfg.data_dir).with_context(|| format!("creating {}", cfg.data_dir.display()))?;
    make_private(&cfg.data_dir, true);
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info,bllu_server=info".into()),
        )
        .init();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = args.first().map(String::as_str).unwrap_or("serve");
    let cfg = Config::from_env()?;
    prepare_data_dir(&cfg)?;

    match cmd {
        "serve" => serve(cfg).await,
        "pair" => {
            let tls = load_or_create_tls(&cfg)?;
            let ttl = arg_value(&args, "--ttl").and_then(|v| v.parse().ok()).unwrap_or(DEFAULT_PAIR_TTL_MIN).clamp(1, 60);
            let code = create_pairing_code(&cfg, &tls, ttl, arg_value(&args, "--url"))?;
            print_pairing(&cfg, &tls, &code, ttl);
            Ok(())
        }
        "clients" => {
            let file: ClientsFile = read_json(&clients_path(&cfg.data_dir))?;
            if file.clients.is_empty() {
                println!("no paired clients");
            }
            let fmt = |t: Option<i64>| {
                t.and_then(|t| chrono::DateTime::from_timestamp(t, 0))
                    .map(|t| t.format("%Y-%m-%d %H:%M").to_string())
                    .unwrap_or("never".into())
            };
            for c in file.clients {
                println!(
                    "{}  {:<30} cert {}  paired {}  token {}  last seen {} from {}",
                    c.id,
                    c.name,
                    sync::short_fingerprint(&c.cert_sha256),
                    fmt(Some(c.created)),
                    fmt(Some(c.token_issued)),
                    fmt(c.last_seen),
                    c.last_ip.unwrap_or_else(|| "-".into())
                );
            }
            Ok(())
        }
        "revoke" => {
            let id = args.get(1).ok_or_else(|| anyhow!("usage: bllu-server revoke <client-id> | --all"))?;
            let path = clients_path(&cfg.data_dir);
            let mut file: ClientsFile = read_json(&path)?;
            let before = file.clients.len();
            if id == "--all" {
                file.clients.clear();
            } else {
                file.clients.retain(|c| &c.id != id);
            }
            if file.clients.len() == before {
                bail!("no client with id {id}");
            }
            write_json(&path, &file)?;
            audit(&cfg.data_dir, "revoked", None, id);
            println!("revoked {}", if id == "--all" { "all clients" } else { id });
            Ok(())
        }
        "close-pairing" => {
            write_json(&pairing_path(&cfg.data_dir), &PairingFile::default())?;
            audit(&cfg.data_dir, "pairing_closed", None, "all pending codes removed");
            println!("all pending pairing codes removed");
            Ok(())
        }
        "fingerprint" => {
            let tls = load_or_create_tls(&cfg)?;
            println!("{}", tls.fingerprint);
            Ok(())
        }
        "--version" | "version" => {
            println!("bllu-server {VERSION}");
            Ok(())
        }
        _ => {
            println!(
                "bllu-server {VERSION}\n\nUSAGE: bllu-server [serve | pair [--ttl MIN] [--url URL] | clients | revoke ID|--all |\n\
                 \u{20}                  close-pairing | fingerprint]\n\n\
                 Environment: LLU_EMAIL, LLU_PASSWORD[_FILE], LLU_SERVER, BLLU_DATA_DIR, BLLU_LISTEN,\n\
                 BLLU_PUBLIC_URL, BLLU_TLS_SANS, BLLU_TLS_CERT/BLLU_TLS_KEY, BLLU_DATA_PASSPHRASE[_FILE],\n\
                 BLLU_ALLOW_CIDRS, BLLU_POLL_SECONDS, BLLU_NAME"
            );
            Ok(())
        }
    }
}

async fn serve(cfg: Config) -> Result<()> {
    let handle = axum_server::Handle::new();
    {
        let handle = handle.clone();
        tokio::spawn(async move {
            shutdown_signal().await;
            tracing::info!("shutting down");
            handle.graceful_shutdown(Some(Duration::from_secs(5)));
        });
    }
    serve_with(cfg, handle).await
}

async fn serve_with(cfg: Config, handle: axum_server::Handle<SocketAddr>) -> Result<()> {
    let tls = load_or_create_tls(&cfg)?;
    let known: PatientsFile = read_json(&patients_path(&cfg.data_dir))?;
    let clients: ClientsFile = read_json(&clients_path(&cfg.data_dir))?;
    if clients.clients.is_empty() {
        let code = create_pairing_code(&cfg, &tls, DEFAULT_PAIR_TTL_MIN, None)?;
        println!("No client is paired yet.");
        print_pairing(&cfg, &tls, &code, DEFAULT_PAIR_TTL_MIN);
    }
    if cfg.passphrase.is_none() {
        tracing::warn!("BLLU_DATA_PASSPHRASE is not set – readings are stored unencrypted in {}", cfg.data_dir.display());
    }
    if cfg.allow.is_empty() && !cfg.listen.ip().is_loopback() {
        tracing::info!("BLLU_ALLOW_CIDRS is not set – any host that can reach port {} may try to connect", cfg.listen.port());
    }
    tracing::info!(
        "bllu-server {VERSION} listening on {} (public URL {}, certificate {}, TLS 1.3 + client certificates)",
        cfg.listen,
        cfg.public_url(),
        sync::short_fingerprint(&tls.fingerprint)
    );

    let acceptor = PeerCertAcceptor(
        RustlsAcceptor::new(RustlsConfig::from_config(Arc::new(server_tls_config(tls)?)))
            .handshake_timeout(Duration::from_secs(10)),
    );
    let listen = cfg.listen;
    let state = Arc::new(AppState {
        patients: RwLock::new(known.patients.into_iter().map(|p| (p.id.clone(), p)).collect()),
        stores: Mutex::new(HashMap::new()),
        clients: Mutex::new((ClientsFile::default(), None)),
        limiter: Limiter::default(),
        cfg,
    });
    if state.cfg.demo {
        tracing::warn!("BLLU_DEMO is set – serving synthetic data, LibreLinkUp is not polled");
        tokio::spawn(demo_poller(state.clone()));
    } else {
        tokio::spawn(poller(state.clone()));
    }

    let app = router(state.clone());
    axum_server::bind(listen)
        .acceptor(acceptor)
        .handle(handle)
        .serve(app.into_make_service_with_connect_info::<SocketAddr>())
        .await?;

    for store in state.stores.lock().unwrap().values_mut() {
        let _ = store.flush();
    }
    Ok(())
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).expect("signal");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    let _ = tokio::signal::ctrl_c().await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use bllu_core::api::Reading;
    use bllu_core::sync::{ClientIdentity, SyncClient, SyncError};

    fn test_config(dir: &Path) -> Config {
        Config {
            data_dir: dir.to_path_buf(),
            listen: "127.0.0.1:0".parse().unwrap(),
            public_url: None,
            name: "test server".into(),
            tls_sans: vec!["localhost".into()],
            tls_cert: None,
            tls_key: None,
            llu_email: None,
            llu_password: None,
            llu_server: None,
            identity: Identity::default(),
            poll_seconds: 60,
            passphrase: None,
            allow: Vec::new(),
            demo: false,
        }
    }

    async fn start(cfg: Config) -> (String, axum_server::Handle<SocketAddr>) {
        prepare_data_dir(&cfg).unwrap();
        let handle = axum_server::Handle::new();
        tokio::spawn(serve_with(cfg, handle.clone()));
        let addr = handle.listening().await.expect("server listening");
        (format!("https://localhost:{}", addr.port()), handle)
    }

    fn seed(dir: &Path, pid: &str) -> i64 {
        let now = Utc::now().timestamp();
        let info = PatientInfo {
            id: pid.into(),
            first_name: "Test".into(),
            last_name: "Patient".into(),
            uses_mg_dl: false,
            target_low: 70.0,
            target_high: 180.0,
            sensor_serial: "S1".into(),
            sensor_activated: None,
            current: Some((now - 60, 110.0, 3)),
            last_poll: Some(now),
            last_error: None,
            first_reading: Some(now - 600),
        };
        write_json(&patients_path(dir), &PatientsFile { patients: vec![info] }).unwrap();
        let store_dir = dir.join("patients").join(&sync::hash_token(pid)[..16]);
        let mut st = Store::open(&store_dir, None).unwrap();
        let r = |ts: i64, v: f32| Reading { time: chrono::DateTime::from_timestamp(ts, 0).unwrap(), mg_dl: v, trend: 3 };
        st.insert(&[r(now - 600, 100.0), r(now - 300, 105.0)]);
        st.flush().unwrap();
        now
    }

    fn tmp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("bllu-server-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn pairing_mtls_binding_rotation_and_revocation() {
        let dir = tmp("e2e");
        let cfg = test_config(&dir);
        let pid = "patient-1";
        let now = seed(&dir, pid);
        let tls = load_or_create_tls(&cfg).unwrap();
        let (url, handle) = start(cfg.clone()).await;
        let code = PairingCode::decode(&create_pairing_code(&cfg, &tls, 10, Some(url.clone())).unwrap()).unwrap();
        assert_eq!(code.fingerprint, tls.fingerprint);
        let me = ClientIdentity::generate("me").unwrap();
        let thief = ClientIdentity::generate("thief").unwrap();

        // 1. a different server certificate is refused before anything is sent
        let wrong = SyncClient::new(&url, &"00".repeat(32), None, &me).unwrap();
        let err = wrong.health().await.unwrap_err().to_string();
        assert!(err.contains("does not match"), "{err}");

        // 2. TLS without a client certificate does not even complete
        let no_cert = reqwest::Client::builder().danger_accept_invalid_certs(true).build().unwrap();
        assert!(no_cert.get(format!("{url}/api/v1/health")).send().await.is_err());

        // 3. pairing works exactly once
        let client = SyncClient::new(&url, &code.fingerprint, None, &me).unwrap();
        assert_eq!(client.health().await.unwrap().name, "test server");
        let paired = client.pair(&code.secret, "test client").await.unwrap();
        assert!(client.pair(&code.secret, "again").await.is_err());

        let authed = SyncClient::new(&url, &code.fingerprint, Some(paired.token.clone()), &me).unwrap();
        let patients = authed.patients().await.unwrap();
        assert_eq!(patients.len(), 1);
        assert_eq!(patients[0].current.unwrap().1, 110.0);
        assert_eq!(authed.readings(pid, now - 3600, now + 60).await.unwrap().len(), 2);
        assert!(authed.readings("someone-else", 0, now).await.is_err());

        // 4. a stolen token is useless with another key
        let stolen = SyncClient::new(&url, &code.fingerprint, Some(paired.token.clone()), &thief).unwrap();
        assert!(matches!(stolen.patients().await, Err(SyncError::Unauthorized)));

        // 5. rotation: new token works, the old one is dead
        let new_token = authed.rotate().await.unwrap();
        assert!(matches!(authed.patients().await, Err(SyncError::Unauthorized)));
        let rotated = SyncClient::new(&url, &code.fingerprint, Some(new_token), &me).unwrap();
        assert_eq!(rotated.patients().await.unwrap().len(), 1);

        // 6. revoking takes effect in the running server
        let path = clients_path(&dir);
        let mut clients: ClientsFile = read_json(&path).unwrap();
        assert_eq!(clients.clients.len(), 1);
        assert_eq!(clients.clients[0].cert_sha256, me.fingerprint().unwrap());
        clients.clients.clear();
        std::thread::sleep(Duration::from_millis(20));
        write_json(&path, &clients).unwrap();
        assert!(matches!(rotated.patients().await, Err(SyncError::Unauthorized)));

        // 7. the audit log recorded it, without secrets
        let audit_log = std::fs::read_to_string(dir.join("audit.log")).unwrap();
        assert!(audit_log.contains("\"paired\"") && audit_log.contains("\"token_rotated\""));
        assert!(!audit_log.contains(&paired.token) && !audit_log.contains(&code.secret));

        handle.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn brute_force_is_banned_and_pairing_closed() {
        let dir = tmp("ban");
        let cfg = test_config(&dir);
        let tls = load_or_create_tls(&cfg).unwrap();
        let (url, handle) = start(cfg.clone()).await;
        let id = ClientIdentity::generate("attacker").unwrap();
        // the startup code is the only pending one; remove it → pairing closed
        write_json(&pairing_path(&dir), &PairingFile::default()).unwrap();
        let c = SyncClient::new(&url, &tls.fingerprint, Some("guess".into()), &id).unwrap();
        assert!(c.pair(&sync::random_secret(24), "x").await.is_err());
        for _ in 0..MAX_FAILURES {
            let _ = c.patients().await;
        }
        assert!(matches!(c.patients().await, Err(SyncError::Blocked)));
        assert!(matches!(c.health().await, Err(SyncError::Blocked)));
        handle.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn allowlist_blocks_other_networks() {
        let dir = tmp("allow");
        let mut cfg = test_config(&dir);
        cfg.allow = parse_allow("10.0.0.0/8, 192.168.1.5").unwrap();
        let tls = load_or_create_tls(&cfg).unwrap();
        let (url, handle) = start(cfg).await;
        let c = SyncClient::new(&url, &tls.fingerprint, None, &ClientIdentity::generate("x").unwrap()).unwrap();
        let err = c.health().await.unwrap_err().to_string();
        assert!(err.contains("403"), "{err}");
        handle.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parses_allowlist() {
        let nets = parse_allow("10.0.0.0/8, 192.168.1.5 ,fd00::/8").unwrap();
        assert_eq!(nets.len(), 3);
        assert!(parse_allow("nonsense").is_err());
        let mut cfg = test_config(Path::new("."));
        cfg.allow = nets;
        assert!(cfg.allowed("10.1.2.3".parse().unwrap()));
        assert!(cfg.allowed("::ffff:192.168.1.5".parse().unwrap()));
        assert!(!cfg.allowed("8.8.8.8".parse().unwrap()));
    }
}
