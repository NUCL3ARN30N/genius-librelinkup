//! Scheduled email reports: daily (yesterday), weekly (last Monday–Sunday) and monthly (the
//! previous calendar month), each with the PDF report attached – optionally inside an AES-256 ZIP,
//! then the email body carries no glucose values. Runs as a task of `serve` when `GLLU_SMTP_URL`
//! is set; `gllu-server report …` sends or previews one report by hand.
//!
//! The last period sent per schedule and patient is kept in `reports.json`, so a restart neither
//! skips nor repeats a report; after downtime only the latest missed period is sent.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration as StdDuration;

use anyhow::{Context, Result, anyhow, bail};
use chrono::{DateTime, Datelike, Duration, Local, NaiveDate, NaiveTime, TimeZone, Utc};
use gllu_core::i18n::{self, Lang};
use gllu_core::report_pdf::{self, PdfOptions};
use gllu_core::tr;
use gllu_core::stats;
use gllu_core::sync::{self, PatientInfo};
use lettre::message::header::ContentType;
use lettre::message::{Attachment, Mailbox, MultiPart};
use lettre::transport::smtp::authentication::Credentials;
use lettre::{AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor};
use serde::{Deserialize, Serialize};

use crate::{AppState, audit, env_or, env_secret, env_var, read_json, write_json};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Schedule {
    Daily,
    Weekly,
    Monthly,
}

impl Schedule {
    pub const ALL: [Schedule; 3] = [Schedule::Daily, Schedule::Weekly, Schedule::Monthly];

    pub fn name(self) -> &'static str {
        match self {
            Schedule::Daily => "daily",
            Schedule::Weekly => "weekly",
            Schedule::Monthly => "monthly",
        }
    }

    pub fn parse(s: &str) -> Option<Schedule> {
        Schedule::ALL.into_iter().find(|x| x.name().eq_ignore_ascii_case(s.trim()))
    }
}

/// A report period of whole local days, `[from, to)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Period {
    pub schedule: Schedule,
    pub from: NaiveDate,
    pub to: NaiveDate,
}

impl Period {
    /// The latest complete period that ended on or before `today` (i.e. before its midnight).
    pub fn before(schedule: Schedule, today: NaiveDate) -> Period {
        let (from, to) = match schedule {
            Schedule::Daily => (today - Duration::days(1), today),
            Schedule::Weekly => {
                let monday = today - Duration::days(today.weekday().num_days_from_monday() as i64);
                (monday - Duration::days(7), monday)
            }
            Schedule::Monthly => {
                let first = today.with_day(1).unwrap();
                let prev = (first - Duration::days(1)).with_day(1).unwrap();
                (prev, first)
            }
        };
        Period { schedule, from, to }
    }

    /// Identifies the period in `reports.json`: "2026-11-01", "2026-W44", "2026-10".
    pub fn key(&self) -> String {
        match self.schedule {
            Schedule::Daily => self.from.format("%Y-%m-%d").to_string(),
            Schedule::Weekly => {
                let w = self.from.iso_week();
                format!("{}-W{:02}", w.year(), w.week())
            }
            Schedule::Monthly => self.from.format("%Y-%m").to_string(),
        }
    }

    /// "Mon 02 Nov 2026", "26 Oct – 01 Nov 2026", "October 2026"
    pub fn title(&self) -> String {
        let last = self.to - Duration::days(1);
        match self.schedule {
            Schedule::Daily => i18n::date(self.from.format("%a %d %b %Y")),
            Schedule::Weekly => i18n::date(format!("{} – {}", self.from.format("%d %b"), last.format("%d %b %Y"))),
            Schedule::Monthly => i18n::date(self.from.format("%B %Y")),
        }
    }

    /// When the report of this period is due: the day after it, at `time`.
    pub fn due<Tz: TimeZone>(&self, time: NaiveTime, tz: &Tz) -> DateTime<Utc> {
        local_instant(self.to, time, tz)
    }

    /// The period as UTC instants (local midnights).
    pub fn range<Tz: TimeZone>(&self, tz: &Tz) -> (DateTime<Utc>, DateTime<Utc>) {
        (local_instant(self.from, NaiveTime::MIN, tz), local_instant(self.to, NaiveTime::MIN, tz))
    }
}

/// `date` at `time` in `tz`; inside a DST gap the first valid time after it.
pub fn local_instant<Tz: TimeZone>(date: NaiveDate, time: NaiveTime, tz: &Tz) -> DateTime<Utc> {
    let naive = date.and_time(time);
    (0..=180)
        .step_by(15)
        .find_map(|m| tz.from_local_datetime(&(naive + Duration::minutes(m))).earliest())
        .map(|t| t.with_timezone(&Utc))
        .unwrap_or_else(|| Utc.from_utc_datetime(&naive))
}

/// The newest period of `schedule` whose report is due at `now`.
pub fn latest_due<Tz: TimeZone>(schedule: Schedule, now: DateTime<Utc>, time: NaiveTime, tz: &Tz) -> Period {
    let today = now.with_timezone(tz).date_naive();
    let p = Period::before(schedule, today);
    if p.due(time, tz) <= now { p } else { Period::before(schedule, today - Duration::days(1)) }
}

// ---------------------------------------------------------------- configuration

/// `smtps://user@host:465` (TLS), `smtp://user@host:587` (STARTTLS required) or
/// `smtp://host:25?tls=none` (plain, only for a trusted local relay).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SmtpUrl {
    pub implicit_tls: bool,
    pub starttls: bool,
    pub user: Option<String>,
    pub host: String,
    pub port: u16,
}

impl SmtpUrl {
    pub fn parse(url: &str) -> Result<SmtpUrl> {
        let (scheme, rest) = url.trim().split_once("://").ok_or_else(|| anyhow!("GLLU_SMTP_URL needs smtps:// or smtp://"))?;
        let implicit_tls = match scheme.to_ascii_lowercase().as_str() {
            "smtps" => true,
            "smtp" => false,
            other => bail!("GLLU_SMTP_URL: unsupported scheme {other}:// (use smtps:// or smtp://)"),
        };
        let (rest, query) = rest.split_once('?').unwrap_or((rest, ""));
        let plain = query.split('&').any(|q| q.eq_ignore_ascii_case("tls=none"));
        let rest = rest.trim_end_matches('/');
        // the user name may itself be an email address: split at the last '@'
        let (user, hostport) = match rest.rsplit_once('@') {
            Some((u, h)) => (Some(u.replace("%40", "@")).filter(|u| !u.is_empty()), h),
            None => (None, rest),
        };
        if user.as_deref().is_some_and(|u| u.contains(':')) {
            bail!("GLLU_SMTP_URL must not contain the password – set GLLU_SMTP_PASSWORD or GLLU_SMTP_PASSWORD_FILE");
        }
        let (host, port) = match hostport.rsplit_once(':') {
            Some((h, p)) => (h, Some(p.parse::<u16>().context("GLLU_SMTP_URL: invalid port")?)),
            None => (hostport, None),
        };
        if host.is_empty() {
            bail!("GLLU_SMTP_URL has no host");
        }
        let default_port = if implicit_tls { 465 } else if plain { 25 } else { 587 };
        Ok(SmtpUrl { implicit_tls, starttls: !implicit_tls && !plain, user, host: host.to_string(), port: port.unwrap_or(default_port) })
    }
}

/// Everything about the email reports. Deliberately not `Debug`: it holds the SMTP and ZIP
/// passwords.
pub struct ReportConfig {
    pub smtp: SmtpUrl,
    smtp_password: Option<String>,
    pub from: Mailbox,
    pub to: Vec<Mailbox>,
    pub schedules: Vec<Schedule>,
    pub time: NaiveTime,
    zip_password: Option<String>,
    /// mg/dL; `None`: the target range of the LibreLinkUp connection
    pub target: Option<(f32, f32)>,
    /// `Some(true)` mmol/L, `Some(false)` mg/dL, `None`: as in LibreLinkUp
    pub mmol: Option<bool>,
    pub lang: String,
}

impl ReportConfig {
    /// `None` when `GLLU_SMTP_URL` is not set (no email reports).
    pub fn from_env() -> Result<Option<ReportConfig>> {
        let Some(url) = env_var("GLLU_SMTP_URL") else { return Ok(None) };
        let smtp = SmtpUrl::parse(&url)?;
        let to: Vec<Mailbox> = env_or("GLLU_REPORT_TO", "")
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| s.parse::<Mailbox>().with_context(|| format!("GLLU_REPORT_TO: invalid address {s}")))
            .collect::<Result<_>>()?;
        if to.is_empty() {
            bail!("GLLU_SMTP_URL is set but GLLU_REPORT_TO is empty");
        }
        let from = match env_var("GLLU_REPORT_FROM") {
            Some(f) => f.parse::<Mailbox>().with_context(|| format!("GLLU_REPORT_FROM: invalid address {f}"))?,
            None => smtp
                .user
                .as_deref()
                .filter(|u| u.contains('@'))
                .ok_or_else(|| anyhow!("set GLLU_REPORT_FROM (the sender address)"))?
                .parse::<Mailbox>()?,
        };
        let schedules = env_or("GLLU_REPORT_SCHEDULE", "monthly")
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| Schedule::parse(s).ok_or_else(|| anyhow!("GLLU_REPORT_SCHEDULE: unknown schedule {s} (daily, weekly, monthly)")))
            .collect::<Result<Vec<_>>>()?;
        let time = NaiveTime::parse_from_str(&env_or("GLLU_REPORT_TIME", "07:00"), "%H:%M").context("GLLU_REPORT_TIME (HH:MM)")?;
        let num = |name: &str| env_var(name).map(|v| v.replace(',', ".").parse::<f32>().with_context(|| format!("{name}: not a number"))).transpose();
        let target = match (num("GLLU_TARGET_LOW")?, num("GLLU_TARGET_HIGH")?) {
            (Some(lo), Some(hi)) if (40.0..hi).contains(&lo) && hi <= 400.0 => Some((lo, hi)),
            (None, None) => None,
            _ => bail!("GLLU_TARGET_LOW / GLLU_TARGET_HIGH: set both, in mg/dL, low < high"),
        };
        let mmol = match env_var("GLLU_UNITS").map(|u| u.to_ascii_lowercase()) {
            None => None,
            Some(u) if u.starts_with("mmol") => Some(true),
            Some(u) if u.starts_with("mg") => Some(false),
            Some(u) => bail!("GLLU_UNITS: {u} (use mmol or mg)"),
        };
        let lang = env_or("GLLU_LANG", "en").to_ascii_lowercase();
        if !matches!(lang.as_str(), "en" | "de") {
            bail!("GLLU_LANG: {lang} (use en or de)");
        }
        Ok(Some(ReportConfig {
            smtp,
            smtp_password: env_secret("GLLU_SMTP_PASSWORD"),
            from,
            to,
            schedules,
            time,
            zip_password: env_secret("GLLU_REPORT_ZIP_PASSWORD"),
            target,
            mmol,
            lang,
        }))
    }

    fn transport(&self) -> Result<AsyncSmtpTransport<Tokio1Executor>> {
        let s = &self.smtp;
        let builder = if s.implicit_tls {
            AsyncSmtpTransport::<Tokio1Executor>::relay(&s.host)?
        } else if s.starttls {
            AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(&s.host)?
        } else {
            AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(&s.host)
        };
        let mut builder = builder.port(s.port).timeout(Some(StdDuration::from_secs(60)));
        if let Some(user) = &s.user {
            builder = builder.credentials(Credentials::new(user.clone(), self.smtp_password.clone().unwrap_or_default()));
        }
        Ok(builder.build())
    }
}

// ---------------------------------------------------------------- content

/// The numbers quoted in the email body.
#[derive(Debug, Clone, PartialEq)]
pub struct Summary {
    pub patient: String,
    pub period: String,
    pub readings: usize,
    pub coverage_pct: f32,
    pub in_range_pct: f32,
    pub below_pct: f32,
    pub above_pct: f32,
    pub mean: String,
    pub gmi_pct: f32,
    pub lows: usize,
    pub highs: usize,
    /// e.g. "3.9–10.0 mmol/L"
    pub target: String,
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// Plain-text and HTML body. With `values` false (encrypted ZIP) neither contains a glucose value.
pub fn bodies(s: &Summary, values: bool) -> (String, String) {
    let footer = tr!("Sent by gllu-server (Genius LibreLinkUp) – not a medical device. Do not make treatment decisions based on this report.");
    let heading = if s.patient.is_empty() { tr!("Glucose report").to_string() } else { tr!("Glucose report for {}", s.patient) };
    let open = "<html><body style=\"font-family:Arial,sans-serif;color:#191c20\">";
    let title = format!(
        "<h2 style=\"margin:0 0 4px\">{}</h2><p style=\"color:#5b5f67;margin:0 0 16px\">{}</p>",
        html_escape(&heading),
        html_escape(&s.period)
    );
    let small = |t: &str| format!("<p style=\"color:#5b5f67;font-size:12px\">{}</p></body></html>", html_escape(t));
    if !values {
        let zip = tr!("The report is attached as a ZIP file encrypted with AES-256. Open it with 7-Zip, WinRAR or Keka and the password you agreed on.");
        let text = format!("{heading} – {}\n\n{zip}\n\n{footer}\n", s.period);
        let html = format!("{open}{title}<p>{}</p>{}", html_escape(zip), small(footer));
        return (text, html);
    }
    let rows = [
        (tr!("Time in range ({})", s.target), format!("{:.0} %", s.in_range_pct)),
        (tr!("Below range").to_string(), format!("{:.0} %", s.below_pct)),
        (tr!("Above range").to_string(), format!("{:.0} %", s.above_pct)),
        (tr!("Average glucose").to_string(), s.mean.clone()),
        (tr!("GMI (approximate HbA1c)").to_string(), i18n::numbers(&format!("{:.1} %", s.gmi_pct))),
        (tr!("Low episodes (15 min or longer)").to_string(), s.lows.to_string()),
        (tr!("High episodes (15 min or longer)").to_string(), s.highs.to_string()),
        (tr!("Sensor active").to_string(), format!("{:.0} % · {}", s.coverage_pct, gllu_core::count(s.readings, "reading"))),
    ];
    let attached = tr!("The full report is attached as a PDF.");
    let mut text = format!("{heading} – {}\n\n", s.period);
    for (k, v) in &rows {
        text.push_str(&format!("{k}: {v}\n"));
    }
    text.push_str(&format!("\n{attached}\n\n{footer}\n"));
    let mut html = format!("{open}{title}<table style=\"border-collapse:collapse\">");
    for (k, v) in &rows {
        html.push_str(&format!(
            "<tr><td style=\"padding:4px 24px 4px 0\">{}</td><td style=\"padding:4px 0;font-weight:bold;text-align:right\">{}</td></tr>",
            html_escape(k),
            html_escape(v)
        ));
    }
    html.push_str(&format!("</table><p>{}</p>{}", html_escape(attached), small(footer)));
    (text, html)
}

/// The email with the PDF (or the encrypted ZIP holding it) attached.
pub fn build_message(
    from: &Mailbox,
    to: &[Mailbox],
    summary: &Summary,
    key: &str,
    pdf: &[u8],
    zip_password: Option<&str>,
) -> Result<Message> {
    let mut b = Message::builder().from(from.clone()).subject(if summary.patient.is_empty() {
        tr!("Glucose report {}", summary.period)
    } else {
        tr!("Glucose report {} – {}", summary.period, summary.patient)
    });
    for t in to {
        b = b.to(t.clone());
    }
    let (text, html) = bodies(summary, zip_password.is_none());
    let pdf_name = format!("glucose-report-{key}.pdf");
    let attachment = match zip_password {
        Some(pw) => {
            let zip = gllu_core::export::encrypted_zip(&[(&pdf_name, pdf)], pw)?;
            Attachment::new(format!("glucose-report-{key}.zip")).body(zip, ContentType::parse("application/zip")?)
        }
        None => Attachment::new(pdf_name).body(pdf.to_vec(), ContentType::parse("application/pdf")?),
    };
    Ok(b.multipart(MultiPart::mixed().multipart(MultiPart::alternative_plain_html(text, html)).singlepart(attachment))?)
}

/// The report of one patient and period: PDF and body numbers. `None` without readings.
fn make_report(
    state: &AppState,
    rc_target: Option<(f32, f32)>,
    rc_mmol: Option<bool>,
    lang: &str,
    patient: &PatientInfo,
    (from, to): (DateTime<Utc>, DateTime<Utc>),
    title: &str,
) -> Result<Option<(Vec<u8>, Summary)>> {
    let end = to - Duration::seconds(1);
    let (readings, notes) = state.with_store(&patient.id, |s| (s.range(from, end), s.notes(from, end)))?;
    if readings.is_empty() {
        return Ok(None);
    }
    let (lo, hi) = rc_target.unwrap_or((patient.target_low, patient.target_high));
    let mmol = rc_mmol.unwrap_or(!patient.uses_mg_dl);
    let report = stats::build_report(&readings, from, end, lo, hi, 154.0);
    let totals = stats::daily_totals(&notes, from, end);
    let name = format!("{} {}", patient.first_name, patient.last_name).trim().to_string();
    let opts = PdfOptions {
        title: tr!("Glucose report").into(),
        patient: name.clone(),
        period: title.to_string(),
        generated: Local::now().format("%Y-%m-%d %H:%M").to_string(),
        mmol,
        target_low: lo,
        target_high: hi,
        lang: lang.to_string(),
    };
    let pdf = report_pdf::render(&report, &notes, &totals, &opts)?;
    let fmt = |mg: f32| if mmol { format!("{:.1}", mg / gllu_core::store::MMOL_FACTOR) } else { format!("{mg:.0}") };
    let unit = if mmol { "mmol/L" } else { "mg/dL" };
    let summary = Summary {
        patient: name,
        period: title.to_string(),
        readings: report.readings,
        coverage_pct: report.sensor_active_pct,
        in_range_pct: report.tir.target,
        below_pct: report.tir.low + report.tir.very_low,
        above_pct: report.tir.high + report.tir.very_high,
        mean: i18n::numbers(&format!("{} {unit}", fmt(report.mean))),
        gmi_pct: report.gmi_pct,
        lows: stats::episodes(&readings, |v| v < lo, 15),
        highs: stats::episodes(&readings, |v| v > hi, 15),
        target: i18n::numbers(&format!("{}–{} {unit}", fmt(lo), fmt(hi))),
    };
    Ok(Some((pdf, summary)))
}

/// Builds and sends the report of `period` for `patient`. `Ok(false)`: no readings, nothing sent.
async fn send_one(state: &AppState, rc: &ReportConfig, patient: &PatientInfo, period: &Period) -> Result<bool> {
    let Some((pdf, summary)) = make_report(state, rc.target, rc.mmol, &rc.lang, patient, period.range(&Local), &period.title())? else {
        return Ok(false);
    };
    let msg = build_message(&rc.from, &rc.to, &summary, &period.key(), &pdf, rc.zip_password.as_deref())?;
    rc.transport()?.send(msg).await.context("sending the email")?;
    Ok(true)
}

// ---------------------------------------------------------------- schedule state

/// `reports.json`: schedule → patient (hash prefix) → key of the last period handled.
#[derive(Debug, Default, Serialize, Deserialize)]
struct SentFile {
    #[serde(default)]
    sent: HashMap<String, HashMap<String, String>>,
}

fn sent_path(dir: &Path) -> std::path::PathBuf {
    dir.join("reports.json")
}

fn patient_key(id: &str) -> String {
    sync::hash_token(id)[..16].to_string()
}

fn recipients(rc: &ReportConfig) -> String {
    gllu_core::count(rc.to.len(), "recipient")
}

/// The scheduler task of `serve`: checks once a minute which reports are due.
pub async fn scheduler(state: Arc<AppState>, rc: ReportConfig) {
    i18n::set_language(Lang::from_code(&rc.lang));
    let dir = state.cfg.data_dir.clone();
    tracing::info!(
        "email reports: {} at {} ({}, now UTC{}) to {} via {}:{}",
        rc.schedules.iter().map(|s| s.name()).collect::<Vec<_>>().join(", "),
        rc.time.format("%H:%M"),
        std::env::var("TZ").ok().filter(|t| !t.is_empty()).unwrap_or_else(|| "system time zone".into()),
        Local::now().format("%:z"),
        recipients(&rc),
        rc.smtp.host,
        rc.smtp.port
    );
    let mut retry_after: HashMap<(Schedule, String), tokio::time::Instant> = HashMap::new();
    loop {
        let patients: Vec<PatientInfo> = state.patients.read().unwrap().values().cloned().collect();
        let mut file: SentFile = read_json(&sent_path(&dir)).unwrap_or_default();
        let now = Utc::now();
        for schedule in &rc.schedules {
            let period = latest_due(*schedule, now, rc.time, &Local);
            for p in &patients {
                let pk = patient_key(&p.id);
                let done = file.sent.entry(schedule.name().into()).or_default();
                match done.get(&pk) {
                    // first run for this patient: start with the next period instead of sending an old one
                    None => {
                        done.insert(pk.clone(), period.key());
                        let next = Period::before(*schedule, period.to + match schedule {
                            Schedule::Daily => Duration::days(1),
                            Schedule::Weekly => Duration::days(7),
                            Schedule::Monthly => Duration::days(32),
                        });
                        tracing::info!(
                            "first {} report on {}",
                            schedule.name(),
                            next.due(rc.time, &Local).with_timezone(&Local).format("%Y-%m-%d %H:%M")
                        );
                        if let Err(e) = write_json(&sent_path(&dir), &file) {
                            tracing::error!("writing reports.json: {e:#}");
                        }
                        continue;
                    }
                    Some(k) if *k == period.key() => continue,
                    Some(_) => {}
                }
                if retry_after.get(&(*schedule, pk.clone())).is_some_and(|t| tokio::time::Instant::now() < *t) {
                    continue;
                }
                let what = format!("{} {} ({})", schedule.name(), period.key(), &pk[..8]);
                match send_one(&state, &rc, p, &period).await {
                    Ok(sent) => {
                        if sent {
                            audit(&dir, "report_sent", None, &format!("{what} to {}", recipients(&rc)));
                        } else {
                            tracing::warn!("no readings for the {what} report – nothing sent");
                        }
                        retry_after.remove(&(*schedule, pk.clone()));
                        file.sent.entry(schedule.name().into()).or_default().insert(pk, period.key());
                        if let Err(e) = write_json(&sent_path(&dir), &file) {
                            tracing::error!("writing reports.json: {e:#}");
                        }
                    }
                    Err(e) => {
                        tracing::error!("{what} report failed (retrying in 15 min): {e:#}");
                        audit(&dir, "report_failed", None, &format!("{what}: {e:#}"));
                        retry_after.insert((*schedule, pk), tokio::time::Instant::now() + StdDuration::from_secs(900));
                    }
                }
            }
        }
        tokio::time::sleep(StdDuration::from_secs(60)).await;
    }
}

// ---------------------------------------------------------------- CLI

/// `gllu-server report --send-now SCHEDULE` and `gllu-server report --preview FILE [SCHEDULE]`.
pub async fn command(state: Arc<AppState>, args: &[String]) -> Result<()> {
    let value = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned();
    let patients: Vec<PatientInfo> = state.patients.read().unwrap().values().cloned().collect();
    if patients.is_empty() {
        bail!("no patient is known yet – start the server and let it poll LibreLinkUp first");
    }
    let schedule_arg = |v: Option<String>| -> Result<Option<Schedule>> {
        v.map(|s| Schedule::parse(&s).ok_or_else(|| anyhow!("unknown schedule {s} (daily, weekly, monthly)"))).transpose()
    };
    if let Some(name) = value("--send-now") {
        let rc = ReportConfig::from_env()?.ok_or_else(|| anyhow!("GLLU_SMTP_URL is not set"))?;
        i18n::set_language(Lang::from_code(&rc.lang));
        let schedule = schedule_arg(Some(name))?.unwrap();
        let period = latest_due(schedule, Utc::now(), rc.time, &Local);
        for p in &patients {
            let what = format!("{} {} ({})", schedule.name(), period.key(), &patient_key(&p.id)[..8]);
            match send_one(&state, &rc, p, &period).await {
                Ok(true) => {
                    audit(&state.cfg.data_dir, "report_sent", None, &format!("{what} to {} (manual)", recipients(&rc)));
                    println!("sent the {what} report to {}", recipients(&rc));
                }
                Ok(false) => println!("no readings for the {what} report – nothing sent"),
                Err(e) => {
                    audit(&state.cfg.data_dir, "report_failed", None, &format!("{what}: {e:#}"));
                    return Err(e.context(format!("the {what} report")));
                }
            }
        }
        return Ok(());
    }
    if let Some(file) = value("--preview") {
        let rc = ReportConfig::from_env()?;
        let schedule = schedule_arg(args.iter().skip_while(|a| *a != "--preview").nth(2).cloned())?;
        let (range, title) = match schedule {
            Some(s) => {
                let p = latest_due(s, Utc::now(), rc.as_ref().map(|r| r.time).unwrap_or(NaiveTime::MIN), &Local);
                (p.range(&Local), p.title())
            }
            None => {
                let to = Utc::now();
                let from = to - Duration::days(14);
                (
                    (from, to + Duration::seconds(1)),
                    i18n::date(format!("{} – {}", from.with_timezone(&Local).format("%d %b %Y"), to.with_timezone(&Local).format("%d %b %Y"))),
                )
            }
        };
        let (target, mmol, lang) = rc.as_ref().map(|r| (r.target, r.mmol, r.lang.clone())).unwrap_or((None, None, "en".into()));
        i18n::set_language(Lang::from_code(&lang));
        for (i, p) in patients.iter().enumerate() {
            let path = if i == 0 { std::path::PathBuf::from(&file) } else { Path::new(&file).with_extension(format!("{}.pdf", i + 1)) };
            match make_report(&state, target, mmol, &lang, p, range, &title)? {
                Some((pdf, _)) => {
                    std::fs::write(&path, &pdf).with_context(|| format!("writing {}", path.display()))?;
                    println!("wrote {} ({} pages)", path.display(), report_pdf::page_count(&pdf));
                }
                None => println!("no readings in {title} for patient {}", &patient_key(&p.id)[..8]),
            }
        }
        return Ok(());
    }
    bail!("usage: gllu-server report --send-now daily|weekly|monthly | --preview FILE.pdf [daily|weekly|monthly]")
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono_tz::{America::Santiago, Europe::Vienna};

    fn d(y: i32, m: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, day).unwrap()
    }

    #[test]
    fn period_boundaries() {
        let p = Period::before(Schedule::Daily, d(2026, 3, 1));
        assert_eq!((p.from, p.to, p.key()), (d(2026, 2, 28), d(2026, 3, 1), "2026-02-28".into()));
        // Monday: the week before; Sunday: still the week before the current one
        let w = Period::before(Schedule::Weekly, d(2026, 11, 2));
        assert_eq!((w.from, w.to, w.key()), (d(2026, 10, 26), d(2026, 11, 2), "2026-W44".into()));
        assert_eq!(Period::before(Schedule::Weekly, d(2026, 11, 8)), w);
        assert_eq!(w.title(), "26 Oct – 01 Nov 2026");
        let m = Period::before(Schedule::Monthly, d(2026, 3, 15));
        assert_eq!((m.from, m.to, m.key(), m.title()), (d(2026, 2, 1), d(2026, 3, 1), "2026-02".into(), "February 2026".into()));
        assert_eq!(Period::before(Schedule::Monthly, d(2028, 3, 31)).to - Period::before(Schedule::Monthly, d(2028, 3, 31)).from, Duration::days(29));
        assert_eq!(Period::before(Schedule::Monthly, d(2027, 1, 1)).key(), "2026-12");
        // ISO week 1 of 2027 starts on Mon 4 Jan
        assert_eq!(Period::before(Schedule::Weekly, d(2027, 1, 11)).key(), "2027-W01");
    }

    #[test]
    fn latest_due_waits_for_the_report_time() {
        let seven = NaiveTime::from_hms_opt(7, 0, 0).unwrap();
        let at = |y, m, day, h| local_instant(d(y, m, day), NaiveTime::from_hms_opt(h, 0, 0).unwrap(), &Vienna);
        // 1 Nov 06:00: October is not due yet, September is the latest
        assert_eq!(latest_due(Schedule::Monthly, at(2026, 11, 1, 6), seven, &Vienna).key(), "2026-09");
        assert_eq!(latest_due(Schedule::Monthly, at(2026, 11, 1, 7), seven, &Vienna).key(), "2026-10");
        assert_eq!(latest_due(Schedule::Daily, at(2026, 11, 2, 6), seven, &Vienna).key(), "2026-10-31");
        assert_eq!(latest_due(Schedule::Daily, at(2026, 11, 2, 8), seven, &Vienna).key(), "2026-11-01");
        assert_eq!(latest_due(Schedule::Weekly, at(2026, 11, 2, 6), seven, &Vienna).key(), "2026-W43");
    }

    #[test]
    fn dst_days_and_gaps() {
        // Vienna springs forward on 29 Mar 2026 (23 h) and falls back on 25 Oct 2026 (25 h)
        let (from, to) = Period::before(Schedule::Daily, d(2026, 3, 30)).range(&Vienna);
        assert_eq!(to - from, Duration::hours(23));
        let (from, to) = Period::before(Schedule::Daily, d(2026, 10, 26)).range(&Vienna);
        assert_eq!(to - from, Duration::hours(25));
        // a report time inside the gap moves to the first valid time
        let gap = local_instant(d(2026, 3, 29), NaiveTime::from_hms_opt(2, 30, 0).unwrap(), &Vienna);
        assert_eq!(gap.with_timezone(&Vienna).format("%H:%M").to_string(), "03:00");
        // Santiago skips midnight on 6 Sep 2026: the day starts at 01:00
        let start = local_instant(d(2026, 9, 6), NaiveTime::MIN, &Santiago);
        assert_eq!(start.with_timezone(&Santiago).format("%d %H:%M").to_string(), "06 01:00");
    }

    #[test]
    fn parses_smtp_urls() {
        let u = SmtpUrl::parse("smtps://reports%40example.com@mail.example.com").unwrap();
        assert_eq!((u.implicit_tls, u.starttls, u.port, u.user.as_deref()), (true, false, 465, Some("reports@example.com")));
        let u = SmtpUrl::parse("smtp://me@example.com@smtp.example.com:2587").unwrap();
        assert_eq!((u.starttls, u.port, u.host.as_str(), u.user.as_deref()), (true, 2587, "smtp.example.com", Some("me@example.com")));
        let u = SmtpUrl::parse("smtp://127.0.0.1:2525?tls=none").unwrap();
        assert_eq!((u.starttls, u.implicit_tls, u.user), (false, false, None));
        assert!(SmtpUrl::parse("smtps://user:secret@host").is_err());
        assert!(SmtpUrl::parse("http://host").is_err());
        assert!(SmtpUrl::parse("smtp://").is_err());
    }

    fn summary() -> Summary {
        Summary {
            patient: "Demo <Patient>".into(),
            period: "October 2026".into(),
            readings: 2880,
            coverage_pct: 99.0,
            in_range_pct: 87.0,
            below_pct: 3.0,
            above_pct: 10.0,
            mean: "6.9 mmol/L".into(),
            gmi_pct: 6.3,
            lows: 4,
            highs: 9,
            target: "3.9–10.0 mmol/L".into(),
        }
    }

    #[test]
    fn mime_with_pdf_and_values() {
        let from: Mailbox = "gllu <reports@example.com>".parse().unwrap();
        let to: Vec<Mailbox> = vec!["doc@example.com".parse().unwrap(), "me@example.com".parse().unwrap()];
        let msg = build_message(&from, &to, &summary(), "2026-10", b"%PDF-1.7 test", None).unwrap();
        let raw = String::from_utf8(msg.formatted()).unwrap();
        assert!(raw.contains("Subject: Glucose report October 2026"));
        assert!(raw.contains("filename=\"glucose-report-2026-10.pdf\""));
        assert!(raw.contains("Content-Type: application/pdf"));
        assert!(raw.contains("text/html") && raw.contains("text/plain"));
        assert!(raw.contains("Demo &lt;Patient&gt;"));
        assert!(raw.contains("87 %") && raw.contains("6.9 mmol/L"));
    }

    #[test]
    fn german_bodies() {
        let (text, html) = i18n::with_language(Lang::De, || bodies(&summary(), true));
        assert!(text.contains("Glukosebericht für Demo <Patient>") && text.contains("Zeit im Zielbereich"), "{text}");
        assert!(text.contains("GMI (ungefährer HbA1c): 6,3 %") && html.contains("Unterzuckerphasen"), "{text}");
    }

    #[test]
    fn mime_with_zip_has_no_values() {
        let from: Mailbox = "reports@example.com".parse().unwrap();
        let to: Vec<Mailbox> = vec!["doc@example.com".parse().unwrap()];
        let msg = build_message(&from, &to, &summary(), "2026-W44", b"%PDF-1.7 test", Some("zip password")).unwrap();
        let raw = String::from_utf8(msg.formatted()).unwrap();
        assert!(raw.contains("filename=\"glucose-report-2026-W44.zip\"") && raw.contains("application/zip"));
        assert!(!raw.contains("application/pdf"));
        let (text, html) = bodies(&summary(), false);
        for body in [&text, &html] {
            assert!(!body.contains("mmol") && !body.contains("87") && !body.contains("6.3"), "{body}");
        }
    }
}
