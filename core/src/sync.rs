//! Protocol between the desktop app and the caching server (`bllu-server`).
//!
//! Security model (defence in depth – every layer has to fail before data leaks):
//! 1. **TLS 1.3 only**, both directions authenticated (mutual TLS).
//! 2. **Server pinning:** a *pairing code* (`bllu1:…`, printed by `bllu-server pair`) carries the
//!    SHA-256 fingerprint of the server certificate. The client refuses any other certificate, so
//!    the connection cannot be intercepted even on a hostile network.
//! 3. **Client identity:** every desktop app creates its own ECDSA P-256 key pair (kept in the OS
//!    keychain) and presents the self-signed certificate on every connection. The server binds
//!    the client to that certificate's fingerprint at pairing time.
//! 4. **Bearer token** (256 bit) as second factor; only its SHA-256 is stored on the server, it is
//!    only accepted together with the bound client certificate and it is rotated regularly.
//! 5. **One-time pairing secret** (192 bit, short-lived, single use) authorises the binding.
//!
//! A stolen token is useless without the private key, a copied key is useless without the token,
//! and revoking a client (`bllu-server revoke`) invalidates both.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chrono::{DateTime, Utc};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, verify_tls12_signature, verify_tls13_signature};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::api::Reading;

pub const PAIRING_PREFIX: &str = "bllu1:";
pub const API_VERSION: u32 = 2;
/// Largest response body the client accepts (400 days of 1-minute readings fit comfortably).
const MAX_RESPONSE: usize = 32 * 1024 * 1024;
/// Rotate the bearer token after this many days.
pub const TOKEN_ROTATE_DAYS: i64 = 30;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PairingCode {
    /// e.g. `https://192.168.1.20:8443`
    pub url: String,
    /// lowercase hex SHA-256 of the server certificate (DER); empty = use public CAs
    #[serde(default)]
    pub fingerprint: String,
    pub secret: String,
}

impl PairingCode {
    pub fn encode(&self) -> String {
        format!("{PAIRING_PREFIX}{}", URL_SAFE_NO_PAD.encode(serde_json::to_vec(self).unwrap()))
    }

    pub fn decode(s: &str) -> Result<Self> {
        let body = s
            .trim()
            .strip_prefix(PAIRING_PREFIX)
            .ok_or_else(|| anyhow!("not a pairing code (it starts with \"{PAIRING_PREFIX}\")"))?;
        let bytes = URL_SAFE_NO_PAD.decode(body.trim()).context("pairing code is damaged")?;
        let code: PairingCode = serde_json::from_slice(&bytes).context("pairing code is damaged")?;
        if !code.url.starts_with("https://") {
            bail!("the server must be reached via https://");
        }
        if !code.fingerprint.is_empty() && (code.fingerprint.len() != 64 || hex::decode(&code.fingerprint).is_err()) {
            bail!("pairing code contains an invalid certificate fingerprint");
        }
        if code.secret.len() < 32 {
            bail!("pairing code is damaged");
        }
        Ok(code)
    }
}

/// Lowercase hex SHA-256 of a DER certificate.
pub fn fingerprint(der: &[u8]) -> String {
    hex::encode(Sha256::digest(der))
}

/// `AB:CD:EF:01:23:45:67:89` – the first 8 bytes, for display.
pub fn short_fingerprint(fp: &str) -> String {
    fp.as_bytes()
        .chunks(2)
        .take(8)
        .map(|c| String::from_utf8_lossy(c).to_uppercase())
        .collect::<Vec<_>>()
        .join(":")
}

pub fn hash_token(token: &str) -> String {
    hex::encode(Sha256::digest(token.as_bytes()))
}

/// Constant-time comparison (no early exit on the first differing byte).
pub fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Random URL-safe secret with `bytes` bytes of entropy.
pub fn random_secret(bytes: usize) -> String {
    let mut b = vec![0u8; bytes];
    rand::fill(&mut b[..]);
    URL_SAFE_NO_PAD.encode(b)
}

/// The desktop app's TLS client identity (self-signed ECDSA P-256 certificate + key).
#[derive(Clone, Serialize, Deserialize)]
pub struct ClientIdentity {
    pub cert_pem: String,
    pub key_pem: String,
}

impl std::fmt::Debug for ClientIdentity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClientIdentity").field("fingerprint", &self.fingerprint().ok()).finish_non_exhaustive()
    }
}

impl ClientIdentity {
    pub fn generate(name: &str) -> Result<Self> {
        let key = rcgen::KeyPair::generate()?; // ECDSA P-256
        let mut params = rcgen::CertificateParams::new(Vec::<String>::new())?;
        params.distinguished_name.push(rcgen::DnType::CommonName, name);
        params.not_before = rcgen::date_time_ymd(2025, 1, 1);
        params.not_after = rcgen::date_time_ymd(2125, 1, 1);
        let cert = params.self_signed(&key)?;
        Ok(Self { cert_pem: cert.pem(), key_pem: key.serialize_pem() })
    }

    fn cert_der(&self) -> Result<CertificateDer<'static>> {
        CertificateDer::from_pem_slice(self.cert_pem.as_bytes()).map_err(|e| anyhow!("client certificate: {e}"))
    }

    fn key_der(&self) -> Result<PrivateKeyDer<'static>> {
        PrivateKeyDer::from_pem_slice(self.key_pem.as_bytes()).map_err(|e| anyhow!("client key: {e}"))
    }

    pub fn fingerprint(&self) -> Result<String> {
        Ok(fingerprint(self.cert_der()?.as_ref()))
    }
}

/// Everything a paired client keeps in the OS keychain.
#[derive(Clone, Serialize, Deserialize)]
pub struct SyncCredentials {
    pub token: String,
    /// unix seconds when the token was issued (for rotation)
    pub token_issued: i64,
    pub identity: ClientIdentity,
}

impl SyncCredentials {
    pub fn rotation_due(&self) -> bool {
        Utc::now().timestamp() - self.token_issued > TOKEN_ROTATE_DAYS * 86_400
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Health {
    pub name: String,
    pub version: String,
    pub api: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PairRequest {
    pub secret: String,
    pub client_name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PairResponse {
    pub client_id: String,
    pub token: String,
    pub server_name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RotateResponse {
    pub token: String,
}

/// `[unix seconds, mg/dL, trend]`
pub type WireReading = (i64, f32, u8);

pub fn to_wire(r: &Reading) -> WireReading {
    (r.time.timestamp(), r.mg_dl, r.trend)
}

/// Converts and sanity-checks a reading received over the network.
pub fn from_wire(w: &WireReading) -> Option<Reading> {
    let now = Utc::now().timestamp();
    let plausible = (946_684_800..=now + 600).contains(&w.0) // 2000-01-01 … now
        && w.1.is_finite()
        && (10.0..=1000.0).contains(&w.1)
        && w.2 <= 5;
    plausible.then(|| Reading { time: DateTime::from_timestamp(w.0, 0).unwrap(), mg_dl: w.1, trend: w.2 })
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PatientInfo {
    pub id: String,
    pub first_name: String,
    pub last_name: String,
    pub uses_mg_dl: bool,
    pub target_low: f32,
    pub target_high: f32,
    pub sensor_serial: String,
    pub sensor_activated: Option<i64>,
    pub current: Option<WireReading>,
    /// unix seconds of the last successful LibreLinkUp poll
    pub last_poll: Option<i64>,
    pub last_error: Option<String>,
    /// unix seconds of the oldest stored reading
    pub first_reading: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReadingsResponse {
    pub patient: String,
    pub readings: Vec<WireReading>,
}

/// Accepts exactly one certificate: the one whose SHA-256 fingerprint was pinned at pairing.
#[derive(Debug)]
struct PinnedVerifier {
    fingerprint: String,
    provider: Arc<CryptoProvider>,
}

impl ServerCertVerifier for PinnedVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let actual = fingerprint(end_entity.as_ref());
        if ct_eq(actual.as_bytes(), self.fingerprint.as_bytes()) {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::General(format!(
                "server certificate does not match the paired one (got {})",
                short_fingerprint(&actual)
            )))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider.signature_verification_algorithms.supported_schemes()
    }
}

pub struct SyncClient {
    http: reqwest::Client,
    base: String,
    token: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum SyncError {
    #[error("this computer is no longer paired with the server (revoked)")]
    Unauthorized,
    #[error("the server is blocking this computer for a while after too many failed attempts")]
    Blocked,
    #[error("{0}")]
    Other(String),
}

impl From<anyhow::Error> for SyncError {
    fn from(e: anyhow::Error) -> Self {
        SyncError::Other(format!("{e:#}"))
    }
}

impl SyncClient {
    /// `pinned_fingerprint` empty = validate the server against public CAs (own CA certificate).
    pub fn new(url: &str, pinned_fingerprint: &str, token: Option<String>, identity: &ClientIdentity) -> Result<Self> {
        let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
        let builder = rustls::ClientConfig::builder_with_provider(provider.clone())
            .with_protocol_versions(&[&rustls::version::TLS13])?;
        let fp = pinned_fingerprint.trim().to_lowercase();
        let builder = if fp.is_empty() {
            let roots = rustls::RootCertStore { roots: webpki_roots::TLS_SERVER_ROOTS.to_vec() };
            builder.with_root_certificates(roots)
        } else {
            builder.dangerous().with_custom_certificate_verifier(Arc::new(PinnedVerifier { fingerprint: fp, provider }))
        };
        let mut config = builder.with_client_auth_cert(vec![identity.cert_der()?], identity.key_der()?)?;
        config.alpn_protocols = vec![b"http/1.1".to_vec()];
        let http = reqwest::Client::builder()
            .tls_backend_preconfigured(config)
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(60))
            .connect_timeout(Duration::from_secs(8))
            .user_agent(concat!("better-librelinkup-sync/", env!("CARGO_PKG_VERSION")))
            .build()?;
        Ok(Self { http, base: url.trim_end_matches('/').to_string(), token })
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base, path)
    }

    async fn body<T: for<'de> Deserialize<'de>>(resp: reqwest::Response) -> Result<T, SyncError> {
        let status = resp.status();
        if status == reqwest::StatusCode::UNAUTHORIZED {
            return Err(SyncError::Unauthorized);
        }
        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            return Err(SyncError::Blocked);
        }
        if resp.content_length().is_some_and(|l| l as usize > MAX_RESPONSE) {
            return Err(SyncError::Other("response too large".into()));
        }
        let mut resp = resp;
        let mut data = Vec::new();
        while let Some(chunk) = resp.chunk().await.map_err(|e| SyncError::Other(e.to_string()))? {
            if data.len() + chunk.len() > MAX_RESPONSE {
                return Err(SyncError::Other("response too large".into()));
            }
            data.extend_from_slice(&chunk);
        }
        if status.is_redirection() {
            return Err(SyncError::Other("the server tried to redirect – refused".into()));
        }
        if !status.is_success() {
            let text = String::from_utf8_lossy(&data[..data.len().min(300)]).to_string();
            return Err(SyncError::Other(format!("server answered {status}: {}", text.trim())));
        }
        serde_json::from_slice(&data).map_err(|e| SyncError::Other(format!("invalid server response: {e}")))
    }

    fn authed(&self, rb: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.token {
            Some(t) => rb.bearer_auth(t),
            None => rb,
        }
    }

    fn send_error(e: reqwest::Error) -> SyncError {
        // surface the TLS pinning message instead of a generic "error sending request"
        let mut msg = e.to_string();
        let mut src = std::error::Error::source(&e);
        while let Some(s) = src {
            msg = s.to_string();
            src = s.source();
        }
        SyncError::Other(format!("cannot reach the server: {msg}"))
    }

    pub async fn health(&self) -> Result<Health, SyncError> {
        let resp = self.http.get(self.url("/api/v1/health")).send().await.map_err(Self::send_error)?;
        Self::body(resp).await
    }

    pub async fn pair(&self, secret: &str, client_name: &str) -> Result<PairResponse, SyncError> {
        let req = PairRequest { secret: secret.to_string(), client_name: client_name.to_string() };
        let resp = self.http.post(self.url("/api/v1/pair")).json(&req).send().await.map_err(Self::send_error)?;
        match Self::body(resp).await {
            Err(SyncError::Unauthorized) => {
                Err(SyncError::Other("the pairing code is wrong, already used or expired".into()))
            }
            other => other,
        }
    }

    /// Replaces the bearer token; the old one stops working immediately.
    pub async fn rotate(&self) -> Result<String, SyncError> {
        let resp = self.authed(self.http.post(self.url("/api/v1/rotate"))).send().await.map_err(Self::send_error)?;
        Ok(Self::body::<RotateResponse>(resp).await?.token)
    }

    pub async fn patients(&self) -> Result<Vec<PatientInfo>, SyncError> {
        let resp = self.authed(self.http.get(self.url("/api/v1/patients"))).send().await.map_err(Self::send_error)?;
        Self::body(resp).await
    }

    pub async fn readings(&self, patient: &str, since: i64, until: i64) -> Result<Vec<Reading>, SyncError> {
        let resp = self
            .authed(self.http.get(self.url("/api/v1/readings")))
            .query(&[("patient", patient.to_string()), ("since", since.to_string()), ("until", until.to_string())])
            .send()
            .await
            .map_err(Self::send_error)?;
        let body: ReadingsResponse = Self::body(resp).await?;
        Ok(body.readings.iter().filter_map(from_wire).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn code() -> PairingCode {
        PairingCode { url: "https://10.0.0.5:8443".into(), fingerprint: "ab".repeat(32), secret: random_secret(24) }
    }

    #[test]
    fn pairing_code_roundtrip_and_validation() {
        let c = code();
        let s = c.encode();
        assert!(s.starts_with(PAIRING_PREFIX));
        assert_eq!(PairingCode::decode(&format!("  {s}\n")).unwrap(), c);
        assert!(PairingCode::decode("bllu1:!!!").is_err());
        assert!(PairingCode::decode(&PairingCode { url: "http://x".into(), ..code() }.encode()).is_err());
        assert!(PairingCode::decode(&PairingCode { fingerprint: "zz".into(), ..code() }.encode()).is_err());
        assert!(PairingCode::decode(&PairingCode { secret: "short".into(), ..code() }.encode()).is_err());
    }

    #[test]
    fn short_fp_and_ct_eq() {
        assert_eq!(short_fingerprint(&"0a".repeat(32)), "0A:0A:0A:0A:0A:0A:0A:0A");
        assert!(ct_eq(b"abc", b"abc"));
        assert!(!ct_eq(b"abc", b"abd"));
        assert!(!ct_eq(b"abc", b"ab"));
    }

    #[test]
    fn identity_roundtrip() {
        let id = ClientIdentity::generate("test").unwrap();
        assert_eq!(id.fingerprint().unwrap().len(), 64);
        assert!(id.key_der().is_ok());
        let other = ClientIdentity::generate("test").unwrap();
        assert_ne!(id.fingerprint().unwrap(), other.fingerprint().unwrap());
    }

    #[test]
    fn rejects_implausible_readings() {
        let now = Utc::now().timestamp();
        assert!(from_wire(&(now - 60, 110.0, 3)).is_some());
        assert!(from_wire(&(now + 86_400, 110.0, 3)).is_none());
        assert!(from_wire(&(now, f32::NAN, 3)).is_none());
        assert!(from_wire(&(now, 5000.0, 3)).is_none());
        assert!(from_wire(&(now, 110.0, 9)).is_none());
        assert!(from_wire(&(0, 110.0, 3)).is_none());
    }
}
