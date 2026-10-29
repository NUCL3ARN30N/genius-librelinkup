//! Background worker: owns the API client, the glucose log and all state; talks to the UI
//! thread through commands (in) and `upgrade_in_event_loop` closures (out).

use std::time::Duration as StdDuration;

use chrono::{DateTime, Duration, Local, Utc};
use slint::{ModelRc, SharedString, VecModel, Weak};
use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;

use crate::alerts::{self, AlertKind, AlertState};
use crate::api::{ApiError, Client, Connection, GLOBAL_SERVER, PendingStep, Reading, Server};
use crate::config::{
    Appearance, RangeColors, Settings, SyncSettings, TrayStyle, UnitPref, app_log_dir, config_path, default_log_dir, parse_hex,
};
use crate::devlog::DevLog;
use crate::store::{Store, VaultState, vault_state};
use crate::units::{Units, level, trend_arrow};
use crate::{
    AppWindow, Dashboard, DevBridge, LogItem, Reports, SettingsBridge, SyncBridge, Theme, Toast, UiAppearance, UiSettings,
    UpdateBridge, platform, secrets, stats, tray, trayicon, ui_model, updater,
};
use bllu_core::sync::{self, ClientIdentity, PairingCode, PatientInfo, SyncClient, SyncCredentials, SyncError};
use slint::ComponentHandle;

pub enum Cmd {
    Login { email: String, password: String, remember: bool, server_index: usize },
    Logout,
    AcceptStep,
    DeclineStep,
    Unlock { passphrase: String, remember: bool },
    SkipUnlock,
    Refresh,
    SelectPatient(usize),
    ReportPeriod(usize),
    SaveSettings(UiSettings),
    /// (current passphrase, new passphrase)
    SetPassphrase(String, String),
    /// current passphrase
    RemovePassphrase(String),
    ExportSettings(std::path::PathBuf),
    ImportSettings(std::path::PathBuf),
    ExportCsv(std::path::PathBuf),
    ImportCsv(std::path::PathBuf),
    OpenDataFolder,
    ClearDevLog,
    AppearanceChanged(UiAppearance),
    ResetColors,
    CheckUpdates,
    InstallUpdate,
    SkipUpdate,
    OpenReleasePage,
    UpdateOptions(bool),
    OpenDay(i32),
    /// Deletes everything stored on this computer (true: including the glucose log), then quits.
    WipeLocalData(bool),
    PairServer(String, bool),
    Unpair,
    SyncNow,
    SyncSource(bool),
    Quit(oneshot::Sender<()>),
}

const REPORT_DAYS: [i64; 4] = [7, 14, 30, 90];
const LIVE_KEEP_HOURS: i64 = 26;

struct Worker {
    s: Settings,
    client: Client,
    devlog: DevLog,
    ui: Weak<AppWindow>,
    password: Option<String>,
    logged_in: bool,
    pending: Option<(PendingStep, String, String, bool)>,
    store: Option<Store>,
    store_locked: bool,
    live: Vec<Reading>,
    connections: Vec<Connection>,
    connections_at: Option<Instant>,
    patient: Option<Connection>,
    account_mg: bool,
    alerts: AlertState,
    report_index: usize,
    next_poll: Instant,
    servers: Vec<Server>,
    store_reloaded: Instant,
    last_error: Option<String>,
    demo: bool,
    started: Instant,
    update: Option<updater::Release>,
    update_checked: Option<Instant>,
    notified_update: String,
    last_backfill: Option<Instant>,
    /// Set after "delete all local data": nothing may be written any more.
    wiped: bool,
}

pub async fn run(
    settings: Settings,
    devlog: DevLog,
    ui: Weak<AppWindow>,
    mut rx: mpsc::UnboundedReceiver<Cmd>,
    demo: bool,
) {
    let client = Client::new(
        Some(if settings.server_override.is_empty() { settings.resolved_server.clone() } else { settings.server_override.clone() }),
        settings.identity.clone(),
        devlog.clone(),
    );
    let mut w = Worker {
        s: settings,
        client,
        devlog,
        ui,
        password: None,
        logged_in: false,
        pending: None,
        store: None,
        store_locked: false,
        live: Vec::new(),
        connections: Vec::new(),
        connections_at: None,
        patient: None,
        account_mg: false,
        alerts: AlertState::default(),
        report_index: 1,
        next_poll: Instant::now() + StdDuration::from_secs(3600),
        servers: Vec::new(),
        store_reloaded: Instant::now(),
        last_error: None,
        demo,
        started: Instant::now(),
        update: None,
        update_checked: None,
        notified_update: String::new(),
        last_backfill: None,
        wiped: false,
    };
    w.push_settings();
    w.push_appearance(true);
    w.push_update_state("", false, -1.0);
    w.push_sync_state("", false, false);
    w.servers = w.client.servers().await;
    w.push_servers();
    if demo {
        w.start_demo();
    } else {
        // re-apply on every start: the entry may have been removed (uninstaller, cleanup tools)
        // or point to an old exe path after the app was moved (not for dev builds from target/)
        if w.s.autostart
            && !cfg!(debug_assertions)
            && let Err(e) = platform::set_autostart(true)
        {
            tracing::warn!("could not register autostart: {e:#}");
        }
        w.init_store().await;
        w.auto_login().await;
    }
    w.push_report();

    let mut tick = tokio::time::interval(StdDuration::from_secs(30));
    loop {
        tokio::select! {
            cmd = rx.recv() => match cmd {
                None => break,
                Some(Cmd::Quit(done)) => {
                    w.flush_store();
                    let _ = done.send(());
                    break;
                }
                Some(cmd) => w.handle(cmd).await,
            },
            _ = tokio::time::sleep_until(w.next_poll) => w.poll().await,
            _ = tick.tick() => {
                w.push_dashboard();
                w.maybe_check_updates().await;
            }
        }
    }
}

impl Worker {
    fn units(&self) -> Units {
        match self.s.units {
            UnitPref::Auto => Units { mg_dl: self.account_mg },
            UnitPref::MmolL => Units { mg_dl: false },
            UnitPref::MgDl => Units { mg_dl: true },
        }
    }

    fn save_settings(&self) {
        if self.demo || self.wiped {
            return;
        }
        if let Err(e) = self.s.save() {
            tracing::error!("saving settings failed: {e:#}");
        }
    }

    fn schedule_poll(&mut self, secs: u64) {
        self.next_poll = Instant::now() + StdDuration::from_secs(secs);
    }

    // ---------------------------------------------------------------- UI helpers

    fn ui<F: FnOnce(AppWindow) + Send + 'static>(&self, f: F) {
        let _ = self.ui.upgrade_in_event_loop(f);
    }

    fn set_status(&mut self, text: impl Into<String>, error: bool) {
        let text: SharedString = text.into().into();
        self.ui(move |w| {
            w.set_status(text);
            w.set_status_error(error);
        });
    }

    fn set_busy(&self, busy: bool) {
        self.ui(move |w| w.set_busy(busy));
    }

    fn toast(&self, msg: impl Into<String>) {
        self.settings_message(msg);
    }

    fn settings_message(&self, msg: impl Into<String>) {
        let msg: SharedString = msg.into().into();
        self.ui(move |w| w.global::<Toast>().set_message(msg));
    }

    fn push_login_state(&self, error: Option<String>, busy: bool) {
        let logged_in = self.logged_in;
        let email: SharedString = self.s.email.clone().into();
        let error: SharedString = error.unwrap_or_default().into();
        self.ui(move |w| {
            w.set_logged_in(logged_in);
            w.set_login_email(email);
            w.set_login_error(error);
            w.set_login_busy(busy);
        });
    }

    fn push_servers(&self) {
        let mut names: Vec<SharedString> = vec!["Automatic (recommended)".into()];
        names.extend(self.servers.iter().map(|s| SharedString::from(format!("{} – {}", s.region.to_uppercase(), s.url))));
        self.ui(move |w| {
            let model: ModelRc<SharedString> = ModelRc::new(VecModel::from(names));
            w.set_login_servers(model.clone());
            w.global::<SettingsBridge>().set_servers(model);
        });
        self.push_settings();
    }

    fn server_index(&self) -> i32 {
        if self.s.server_override.is_empty() {
            return 0;
        }
        self.servers
            .iter()
            .position(|s| s.url == self.s.server_override)
            .map(|i| i as i32 + 1)
            .unwrap_or(0)
    }

    fn push_settings(&self) {
        let s = &self.s;
        let u = self.units();
        let ui = UiSettings {
            email: s.email.clone().into(),
            server_index: self.server_index(),
            units: match s.units {
                UnitPref::Auto => 0,
                UnitPref::MmolL => 1,
                UnitPref::MgDl => 2,
            },
            poll_seconds: s.poll_seconds as i32,
            target_low: u.fmt(s.target_low).into(),
            target_high: u.fmt(s.target_high).into(),
            notify_low: s.notify_low,
            low_threshold: u.fmt(s.low_threshold).into(),
            notify_high: s.notify_high,
            high_threshold: u.fmt(s.high_threshold).into(),
            notify_stale: s.notify_stale,
            stale_minutes: s.stale_minutes as i32,
            repeat_minutes: s.notify_repeat_minutes as i32,
            log_enabled: s.log_enabled,
            log_dir: s.log_dir.clone().into(),
            remember_passphrase: s.remember_passphrase,
            dev_mode: s.dev_mode,
            start_minimized: s.start_minimized,
            autostart: s.autostart,
            theme: match s.dark_mode {
                None => 0,
                Some(false) => 1,
                Some(true) => 2,
            },
            product: s.identity.product.clone().into(),
            version: s.identity.version.clone().into(),
        };
        let theme = ui.theme;
        let dev = s.dev_mode;
        let encrypted = self.store.as_ref().is_some_and(|st| st.is_encrypted());
        let info: SharedString = match (&self.store, s.log_enabled, self.store_locked) {
            (_, false, _) => "Logging is off.".into(),
            (None, true, true) => "The log is locked.".into(),
            (None, true, false) => "The log is not loaded in this session.".into(),
            (Some(st), _, _) => format!(
                "{} readings since {} in {} · {}",
                st.len(),
                st.first_time().map(|t| t.with_timezone(&Local).format("%d %b %Y").to_string()).unwrap_or("–".into()),
                st.dir().display(),
                if st.is_encrypted() { "encrypted (XChaCha20-Poly1305, Argon2id)" } else { "not encrypted" }
            )
            .into(),
        };
        let log_path: SharedString = app_log_dir().display().to_string().into();
        let log_folder: SharedString = s.log_dir().display().to_string().into();
        self.ui(move |w| {
            let b = w.global::<SettingsBridge>();
            b.set_settings(ui);
            b.set_log_folder(log_folder);
            b.set_store_info(info);
            b.set_store_encrypted(encrypted);
            w.set_theme_mode(theme);
            w.set_dev_mode(dev);
            w.global::<DevBridge>().set_log_path(log_path);
        });
    }

    fn push_store_lock(&self, error: Option<String>, busy: bool) {
        let locked = self.store_locked;
        let new = self.s.log_enabled && vault_state(&self.s.log_dir()) == VaultState::New;
        let error: SharedString = error.unwrap_or_default().into();
        self.ui(move |w| {
            w.set_store_locked(locked);
            w.set_store_new(new);
            w.set_unlock_error(error);
            w.set_unlock_busy(busy);
        });
    }

    fn recent(&self, hours: i64) -> Vec<Reading> {
        let from = Utc::now() - Duration::hours(hours);
        let mut out: Vec<Reading> = self.live.iter().filter(|r| r.time >= from).copied().collect();
        if let Some(st) = &self.store {
            out.extend(st.range(from, Utc::now() + Duration::minutes(5)));
        }
        out.sort_by_key(|r| r.time);
        out.dedup_by_key(|r| r.time.timestamp());
        out
    }

    fn push_dashboard(&self) {
        let u = self.units();
        let (lo, hi) = (self.s.target_low, self.s.target_high);
        let look = self.s.appearance.clone();
        let hours = look.graph_hours.max(3) as i64;
        let readings = self.recent(hours.max(24));
        let latest = self.patient.as_ref().and_then(|p| p.current).or_else(|| readings.last().copied());
        let graph = ui_model::graph(&readings, u, lo, hi, hours, look.graph_style);
        let graph_title = format!("Last {hours} hours");

        let report = stats::build_report(&readings, Utc::now() - Duration::hours(24), Utc::now(), lo, hi, 154.0);
        let stats_items = if report.readings > 0 {
            vec![
                ui_model::stat("In range", format!("{:.0} %", report.tir.target), ""),
                ui_model::stat("Below range", format!("{:.0} %", report.tir.low + report.tir.very_low), ""),
                ui_model::stat("Average", u.fmt_unit(report.mean), ""),
                ui_model::stat("Variability", format!("{:.0} % CV", report.cv_pct), ""),
            ]
        } else {
            vec![]
        };

        let (value, trend, trend_text, lvl, age, stale) = match latest {
            Some(r) => {
                let (arrow, text) = trend_arrow(r.trend);
                let stale = Utc::now() - r.time > Duration::minutes(15);
                (
                    u.fmt_reading(r.mg_dl),
                    arrow.to_string(),
                    if stale { "Signal lost – no recent reading".to_string() } else { text.to_string() },
                    level(r.mg_dl, lo, hi),
                    ui_model::age_text(r.time),
                    stale,
                )
            }
            None => ("–".into(), String::new(), String::new(), 0, String::new(), false),
        };
        let sensor = self
            .patient
            .as_ref()
            .map(|p| {
                let mut parts = Vec::new();
                if !p.sensor_serial.is_empty() {
                    parts.push(format!("Sensor {}", p.sensor_serial));
                }
                if let Some(a) = p.sensor_activated {
                    let day = (Utc::now() - a).num_days() + 1;
                    parts.push(format!("started {} (day {day})", a.with_timezone(&Local).format("%d %b")));
                }
                parts.join(" · ")
            })
            .unwrap_or_default();

        // tray
        let (tray_content, tooltip) = match latest {
            Some(r) => {
                let color = if stale { [0x75, 0x75, 0x75, 0xff] } else { range_rgba(&look.colors, lvl) };
                let (arrow, text) = trend_arrow(r.trend);
                (
                    tray::TrayContent {
                        text: u.fmt_reading(r.mg_dl),
                        trend: if stale { 0 } else { r.trend },
                        color,
                        style: look.tray.clone(),
                    },
                    format!("{} {} {arrow} {text}\n{}", u.fmt_reading(r.mg_dl), u.label(), ui_model::age_text(r.time)),
                )
            }
            None => (
                tray::TrayContent { text: "-".into(), trend: 0, color: [0x75, 0x75, 0x75, 0xff], style: look.tray.clone() },
                "Better LibreLinkUp – no data".into(),
            ),
        };

        let trend_index = latest.filter(|_| !stale).map(|r| r.trend as i32).unwrap_or(0);
        let unit: SharedString = u.label().into();
        self.ui(move |w| {
            let d = w.global::<Dashboard>();
            d.set_value(value.into());
            d.set_unit(unit);
            d.set_trend(trend.into());
            d.set_trend_index(trend_index);
            d.set_trend_text(trend_text.into());
            d.set_level(lvl);
            d.set_age(age.into());
            d.set_stale(stale);
            d.set_sensor(sensor.into());
            d.set_line_path(graph.path.into());
            d.set_dots(ModelRc::new(VecModel::from(graph.dots)));
            d.set_y_labels(ModelRc::new(VecModel::from(graph.y_labels)));
            d.set_x_labels(ModelRc::new(VecModel::from(graph.x_labels)));
            d.set_band_top(graph.band_top);
            d.set_band_bottom(graph.band_bottom);
            d.set_stats(ModelRc::new(VecModel::from(stats_items)));
            d.set_graph_title(graph_title.into());
            d.set_graph_style(look.graph_style as i32);
            d.set_colored_card(look.colored_card);
            d.set_show_stats(look.show_stats);
            tray::update(tray_content, &tooltip);
        });
    }

    fn push_patients(&self) {
        let names: Vec<SharedString> = self
            .connections
            .iter()
            .map(|c| format!("{} {}", c.first_name, c.last_name).trim().to_string().into())
            .collect();
        let idx = self
            .connections
            .iter()
            .position(|c| c.patient_id == self.s.patient_id)
            .unwrap_or(0) as i32;
        self.ui(move |w| {
            let d = w.global::<Dashboard>();
            d.set_patients(ModelRc::new(VecModel::from(names)));
            d.set_patient_index(idx);
        });
    }

    /// Start of the report period: local midnight, so the first day is complete.
    fn report_from(&self) -> DateTime<Utc> {
        let days = REPORT_DAYS[self.report_index.min(3)];
        let now = Utc::now();
        let from = (now - Duration::days(days)).with_timezone(&Local).date_naive().and_hms_opt(0, 0, 0).unwrap();
        let from = from.and_local_timezone(Local).earliest().map(|t| t.with_timezone(&Utc)).unwrap_or(now - Duration::days(days));
        from + Duration::days(1)
    }

    /// Logged plus live readings in `[from, to)`, sorted and without duplicates.
    fn readings_between(&self, from: DateTime<Utc>, to: DateTime<Utc>) -> Vec<Reading> {
        let Some(st) = &self.store else { return Vec::new() };
        let mut r = st.range(from, to);
        r.extend(self.live.iter().filter(|x| x.time >= from && x.time < to).copied());
        r.sort_by_key(|x| x.time);
        r.dedup_by_key(|x| x.time.timestamp());
        r
    }

    fn open_day(&self, key: i32) {
        let Some(date) = ui_model::day_from_key(key) else { return };
        let midnight = |d: chrono::NaiveDate| {
            d.and_hms_opt(0, 0, 0)
                .and_then(|t| t.and_local_timezone(Local).earliest())
                .map(|t| t.with_timezone(&Utc))
        };
        let (Some(start), Some(end)) = (midnight(date), midnight(date + Duration::days(1))) else { return };
        let readings = self.readings_between(start, end);
        // neighbouring days with data inside the report period
        let days: std::collections::BTreeSet<chrono::NaiveDate> = self
            .readings_between(self.report_from(), Utc::now() + Duration::minutes(1))
            .iter()
            .map(|r| r.time.with_timezone(&Local).date_naive())
            .collect();
        let prev = days.range(..date).next_back().map(|d| ui_model::day_key(*d)).unwrap_or(-1);
        let next = days.range(date + Duration::days(1)..).next().map(|d| ui_model::day_key(*d)).unwrap_or(-1);
        let m = ui_model::day_detail(
            date,
            &readings,
            (start, end),
            self.units(),
            self.s.target_low,
            self.s.target_high,
            self.s.appearance.graph_style,
        );
        self.ui(move |w| {
            let r = w.global::<Reports>();
            r.set_detail_title(m.title.into());
            r.set_detail_line(m.graph.path.into());
            r.set_detail_dots(ModelRc::new(VecModel::from(m.graph.dots)));
            r.set_detail_y_labels(ModelRc::new(VecModel::from(m.graph.y_labels)));
            r.set_detail_x_labels(ModelRc::new(VecModel::from(m.graph.x_labels)));
            r.set_detail_band_top(m.graph.band_top);
            r.set_detail_band_bottom(m.graph.band_bottom);
            r.set_detail_stats(ModelRc::new(VecModel::from(m.stats)));
            r.set_detail_tir(ModelRc::new(VecModel::from(m.tir)));
            r.set_detail_prev(prev);
            r.set_detail_next(next);
            r.set_detail_open(true);
        });
    }

    fn push_report(&self) {
        let unavailable = if !self.s.log_enabled {
            Some("Turn on the glucose log in Settings to build reports – LibreLinkUp itself only keeps the last 12 hours.")
        } else if self.store.is_none() {
            Some("The glucose log is locked or not loaded. Unlock it to see reports.")
        } else {
            None
        };
        let now = Utc::now();
        let from = self.report_from();
        let readings = self.readings_between(from, now + Duration::minutes(1));
        let unavailable = unavailable.map(str::to_string).or_else(|| {
            (readings.len() < 12).then(|| {
                "Not enough data yet. Keep the app running or import a LibreView CSV export (Settings → Import readings).".to_string()
            })
        });
        let index = self.report_index as i32;
        if let Some(text) = unavailable {
            self.ui(move |w| {
                let r = w.global::<Reports>();
                r.set_available(false);
                r.set_unavailable_text(text.into());
                r.set_subtitle(SharedString::default());
                r.set_period_index(index);
            });
            return;
        }
        let report = stats::build_report(&readings, from, now, self.s.target_low, self.s.target_high, 154.0);
        let m = ui_model::report_model(&report, self.units(), self.s.target_low, self.s.target_high);
        self.ui(move |w| {
            let r = w.global::<Reports>();
            r.set_available(true);
            r.set_period_index(index);
            r.set_title(m.title.into());
            r.set_subtitle(m.subtitle.into());
            r.set_stats(ModelRc::new(VecModel::from(m.stats)));
            r.set_tir(ModelRc::new(VecModel::from(m.tir)));
            r.set_outer_path(m.outer.into());
            r.set_inner_path(m.inner.into());
            r.set_median_path(m.median.into());
            r.set_y_labels(ModelRc::new(VecModel::from(m.y_labels)));
            r.set_x_labels(ModelRc::new(VecModel::from(m.x_labels)));
            r.set_band_top(m.band_top);
            r.set_band_bottom(m.band_bottom);
            r.set_insights(ModelRc::new(VecModel::from(m.insights)));
            r.set_days(ModelRc::new(VecModel::from(m.days)));
            r.set_day_band_top(m.day_band_top);
            r.set_day_band_bottom(m.day_band_bottom);
        });
    }

    fn push_devlog(&self) {
        if !self.devlog.enabled() {
            return;
        }
        let items: Vec<LogItem> = self
            .devlog
            .entries()
            .into_iter()
            .map(|e| {
                let status = e.status.map(|s| s.to_string()).unwrap_or_else(|| "failed".into());
                LogItem {
                    time: e.time.format("%H:%M:%S").to_string().into(),
                    title: format!("{} {}", e.method, e.url).into(),
                    status: format!("HTTP {status} · {} ms", e.millis).into(),
                    ok: e.status.is_some_and(|s| s < 400),
                    detail: format!(
                        "{} {}\nHTTP {status} · {} ms · {}\n\nRequest body:\n{}\n\nResponse:\n{}",
                        e.method,
                        e.url,
                        e.millis,
                        e.time.format("%Y-%m-%d %H:%M:%S"),
                        if e.request.is_empty() { "(none)" } else { &e.request },
                        e.response
                    )
                    .into(),
                }
            })
            .collect();
        self.ui(move |w| w.global::<DevBridge>().set_entries(ModelRc::new(VecModel::from(items))));
    }

    // ---------------------------------------------------------------- store

    async fn init_store(&mut self) {
        self.flush_store();
        self.store = None;
        self.store_locked = false;
        if self.s.log_enabled {
            let dir = self.s.log_dir();
            match vault_state(&dir) {
                VaultState::New => self.store_locked = true,
                VaultState::Plain => self.open_store(None).await.unwrap_or_else(|e| self.store_error(e)),
                VaultState::Encrypted => match secrets::passphrase() {
                    Some(p) => {
                        if let Err(e) = self.open_store(Some(p)).await {
                            tracing::warn!("stored passphrase did not unlock the log: {e:#}");
                            self.store_locked = true;
                        }
                    }
                    None => self.store_locked = true,
                },
            }
        }
        self.push_store_lock(None, false);
        self.push_settings();
    }

    fn store_error(&mut self, e: anyhow::Error) {
        tracing::error!("opening log failed: {e:#}");
        self.set_status(format!("Log error: {e:#}"), true);
    }

    async fn open_store(&mut self, passphrase: Option<String>) -> anyhow::Result<()> {
        let dir = self.s.log_dir();
        let mut store = tokio::task::spawn_blocking(move || Store::open(&dir, passphrase.as_deref())).await??;
        store.insert(&self.live);
        store.flush()?;
        self.store = Some(store);
        self.store_locked = false;
        self.store_reloaded = Instant::now();
        Ok(())
    }

    /// "Delete all local data": settings, app/API logs, keychain entries, autostart and
    /// optionally the glucose log files. Quits when everything is gone.
    fn wipe_local_data(&mut self, include_log: bool) {
        if self.demo {
            self.toast("Demo mode stores nothing permanent – there is nothing to delete");
            return;
        }
        self.wiped = true;
        // dropped without flushing: unsaved readings would only recreate the files
        self.store = None;
        let mut failed: Vec<String> = Vec::new();
        let remove_file = |p: &std::path::Path, failed: &mut Vec<String>| {
            if let Err(e) = std::fs::remove_file(p)
                && e.kind() != std::io::ErrorKind::NotFound
            {
                failed.push(format!("{}: {e}", p.display()));
            }
        };

        if include_log {
            let mut dirs = vec![self.s.log_dir(), default_log_dir()];
            dirs.dedup();
            for dir in dirs {
                // only our own files: the folder may be a synced folder with other content
                let Ok(entries) = std::fs::read_dir(&dir) else { continue };
                for entry in entries.flatten() {
                    let name = entry.file_name().to_string_lossy().to_string();
                    let ours = name == "vault.json"
                        || name == "vault.tmp"
                        || (name.starts_with("glucose-") && (name.ends_with(".bllu") || name.ends_with(".tmp")));
                    if ours {
                        remove_file(&entry.path(), &mut failed);
                    }
                }
                let _ = std::fs::remove_dir(&dir);
            }
        }

        secrets::forget_password(&self.s.email);
        secrets::forget_passphrase();
        secrets::forget_sync_credentials();
        // both are per OS user, not per BLLU_HOME: a test or portable copy must not remove the
        // entries of the normal installation
        if self.s.autostart
            && let Err(e) = platform::set_autostart(false)
        {
            failed.push(format!("autostart: {e}"));
        }
        if crate::config::home_override().is_none() {
            platform::unregister_notifications();
        }

        let cfg = config_path();
        remove_file(&cfg, &mut failed);
        remove_file(&cfg.with_extension("json.tmp"), &mut failed);
        // the current log file is still open; Windows deletes it once the app has exited
        let _ = std::fs::remove_dir_all(app_log_dir());
        for dir in [cfg.parent(), default_log_dir().parent(), app_log_dir().parent()].into_iter().flatten() {
            // remove the (now empty) app folders, e.g. %APPDATA%\better-librelinkup\…\config
            let mut d = Some(dir);
            while let Some(p) = d {
                if crate::config::home_override().is_some_and(|h| h == p) || std::fs::remove_dir(p).is_err() {
                    break;
                }
                d = p.parent().filter(|p| p.file_name().is_some_and(|n| n.to_string_lossy().contains("better-librelinkup")));
            }
        }

        if failed.is_empty() {
            self.ui(|_| {
                let _ = slint::quit_event_loop();
            });
        } else {
            self.toast(format!("Some data could not be deleted: {}", failed.join("; ")));
        }
    }

    fn flush_store(&mut self) {
        if let Some(st) = &mut self.store {
            if let Err(e) = st.flush() {
                tracing::error!("writing log failed: {e:#}");
            }
        }
    }

    // ---------------------------------------------------------------- login / polling

    async fn auto_login(&mut self) {
        self.password = (!self.s.email.is_empty()).then(|| secrets::password(&self.s.email)).flatten();
        if self.server_is_source() {
            // values come from the paired caching server, no LibreLinkUp login needed
            self.logged_in = true;
            self.push_login_state(None, false);
            self.poll().await;
            return;
        }
        if self.s.email.is_empty() {
            self.push_login_state(None, false);
            return;
        }
        self.password = secrets::password(&self.s.email);
        if self.password.is_none() {
            self.push_login_state(None, false);
            return;
        }
        // Show the dashboard immediately; errors surface as status.
        self.logged_in = true;
        self.push_login_state(None, false);
        self.poll().await;
    }

    async fn relogin(&mut self) -> Result<(), ApiError> {
        let Some(password) = self.password.clone() else {
            self.logged_in = false;
            self.push_login_state(Some("Please sign in again.".into()), false);
            return Err(ApiError::NotLoggedIn);
        };
        let email = self.s.email.clone();
        match self.client.login(&email, &password).await {
            Ok(()) => {
                self.remember_resolution();
                Ok(())
            }
            Err(ApiError::BadCredentials) => {
                self.logged_in = false;
                self.password = None;
                self.push_login_state(Some("LibreLinkUp rejected the saved password. Please sign in again.".into()), false);
                Err(ApiError::BadCredentials)
            }
            Err(ApiError::StepRequired(step)) => {
                self.show_step(step, email, password, true);
                Err(ApiError::NotLoggedIn)
            }
            Err(e) => Err(e),
        }
    }

    fn remember_resolution(&mut self) {
        let server = self.client.server().to_string();
        let identity = self.client.identity().clone();
        if self.s.resolved_server != server || self.s.identity != identity {
            self.s.resolved_server = server;
            self.s.identity = identity;
            self.save_settings();
            self.push_settings();
        }
    }

    fn show_step(&mut self, step: PendingStep, email: String, password: String, remember: bool) {
        let text = format!(
            "LibreLinkUp asks you to accept the {} ({}). You can review the document in the official \
             LibreLinkUp app. Do you accept it so this app can continue?",
            step.describe(),
            if step.component.is_empty() { step.kind.clone() } else { format!("{} / {}", step.kind, step.component) }
        );
        self.pending = Some((step, email, password, remember));
        self.ui(move |w| w.set_step_text(text.into()));
    }

    async fn do_login(&mut self, email: String, password: String, remember: bool) {
        self.push_login_state(None, true);
        match self.client.login(&email, &password).await {
            Ok(()) => {
                if self.s.email != email {
                    self.s.patient_id.clear();
                    self.connections.clear();
                }
                self.s.email = email.clone();
                self.s.remember_password = remember;
                self.remember_resolution();
                self.save_settings();
                if remember {
                    if !secrets::store_password(&email, &password) {
                        self.set_status("Could not save the password in the system keychain", true);
                    }
                } else {
                    secrets::forget_password(&email);
                }
                self.password = Some(password);
                self.logged_in = true;
                self.push_login_state(None, false);
                self.push_settings();
                self.connections_at = None;
                self.poll().await;
            }
            Err(ApiError::StepRequired(step)) => {
                self.push_login_state(None, false);
                self.show_step(step, email, password, remember);
            }
            Err(e) => {
                self.push_devlog();
                self.push_login_state(Some(e.to_string()), false);
            }
        }
    }

    async fn poll(&mut self) {
        self.schedule_poll(self.s.poll_seconds as u64);
        if self.demo {
            self.demo_tick();
            return;
        }
        if !self.logged_in {
            return;
        }
        if self.server_is_source() {
            self.set_busy(true);
            self.run_sync(true).await;
            self.set_busy(false);
            return;
        }
        self.set_busy(true);
        let mut result = self.fetch().await;
        if matches!(result, Err(ApiError::Unauthorized)) {
            result = match self.relogin().await {
                Ok(()) => self.fetch().await,
                Err(e) => Err(e),
            };
        }
        match result {
            Ok(()) => {
                self.last_error = None;
                self.set_status(format!("Updated {}", Local::now().format("%H:%M")), false);
                if self.backfill_due() {
                    // fill gaps from while this computer was off
                    self.run_sync(false).await;
                }
            }
            Err(ApiError::RateLimited(secs)) => {
                self.schedule_poll(secs.max(self.s.poll_seconds as u64));
                self.set_status(format!("LibreLinkUp rate limit – next try in {secs} s"), true);
            }
            Err(ApiError::NotLoggedIn) => {}
            Err(e) => {
                tracing::warn!("update failed: {e}");
                self.schedule_poll((self.s.poll_seconds as u64).min(60));
                self.set_status(e.to_string(), true);
                self.last_error = Some(e.to_string());
            }
        }
        self.set_busy(false);
        self.push_devlog();
    }

    async fn fetch(&mut self) -> Result<(), ApiError> {
        if !self.client.is_authenticated() {
            self.relogin().await?;
        }
        let stale_list = self.connections_at.is_none_or(|t| t.elapsed() > StdDuration::from_secs(600));
        if self.connections.is_empty() || stale_list {
            self.connections = self.client.connections().await?;
            self.connections_at = Some(Instant::now());
            if !self.connections.iter().any(|c| c.patient_id == self.s.patient_id) {
                if let Some(first) = self.connections.first() {
                    // First time we see this patient: adopt the target range configured in LibreLinkUp.
                    self.s.patient_id = first.patient_id.clone();
                    if (40.0..first.target_high).contains(&first.target_low) && first.target_high <= 400.0 {
                        self.s.target_low = first.target_low;
                        self.s.target_high = first.target_high;
                        self.s.sanitize();
                    }
                    self.save_settings();
                    self.push_settings();
                }
            }
            self.push_patients();
        }
        if self.s.patient_id.is_empty() {
            return Err(ApiError::Protocol(
                "No LibreLinkUp connection found. Accept a sharing invitation in the LibreLinkUp app first.".into(),
            ));
        }
        let graph = self.client.graph(&self.s.patient_id.clone()).await?;
        self.account_mg = graph.connection.uses_mg_dl;

        // merge
        self.live.extend(graph.history.iter().copied());
        self.live.sort_by_key(|r| r.time);
        self.live.dedup_by_key(|r| r.time.timestamp());
        let keep_from = Utc::now() - Duration::hours(LIVE_KEEP_HOURS);
        self.live.retain(|r| r.time >= keep_from);
        let mut new_data = 0;
        if let Some(st) = &mut self.store {
            new_data = st.insert(&graph.history);
            if self.store_reloaded.elapsed() > StdDuration::from_secs(900) {
                // pick up readings synced from other machines
                if let Err(e) = st.reload() {
                    tracing::warn!("reloading log failed: {e:#}");
                }
                self.store_reloaded = Instant::now();
            } else if new_data > 0 {
                if let Err(e) = st.flush() {
                    tracing::error!("writing log failed: {e:#}");
                }
            }
        }
        let latest = graph.connection.current;
        self.patient = Some(graph.connection);
        self.check_alerts(latest);
        self.push_dashboard();
        if new_data > 0 {
            self.push_report();
            self.push_settings();
        }
        Ok(())
    }

    fn check_alerts(&mut self, latest: Option<Reading>) {
        let Some(kind) = self.alerts.evaluate(&self.s, latest.as_ref(), Utc::now()) else { return };
        let u = self.units();
        let Some(r) = latest else { return };
        let (arrow, _) = trend_arrow(r.trend);
        match kind {
            AlertKind::Low => alerts::show(
                &format!("Low glucose: {} {arrow}", u.fmt_unit(r.mg_dl)),
                &format!("Measured {}.", ui_model::age_text(r.time)),
                true,
            ),
            AlertKind::High => alerts::show(
                &format!("High glucose: {} {arrow}", u.fmt_unit(r.mg_dl)),
                &format!("Measured {}.", ui_model::age_text(r.time)),
                false,
            ),
            AlertKind::Stale => alerts::show(
                "No new glucose data",
                &format!(
                    "Last reading {} at {}.",
                    u.fmt_unit(r.mg_dl),
                    r.time.with_timezone(&Local).format("%H:%M")
                ),
                false,
            ),
        }
    }

    // ---------------------------------------------------------------- commands

    async fn handle(&mut self, cmd: Cmd) {
        match cmd {
            Cmd::Login { email, password, remember, server_index } => {
                self.s.server_override = server_index
                    .checked_sub(1)
                    .and_then(|i| self.servers.get(i))
                    .map(|s| s.url.clone())
                    .unwrap_or_default();
                let server = if self.s.server_override.is_empty() {
                    if self.s.resolved_server.is_empty() { GLOBAL_SERVER.to_string() } else { self.s.resolved_server.clone() }
                } else {
                    self.s.server_override.clone()
                };
                self.client.set_server(&server);
                self.do_login(email.trim().to_string(), password, remember).await;
            }
            Cmd::Logout => {
                secrets::forget_password(&self.s.email);
                self.client.logout();
                self.password = None;
                self.logged_in = false;
                self.connections.clear();
                self.patient = None;
                self.push_login_state(None, false);
                self.push_dashboard();
            }
            Cmd::AcceptStep => {
                if let Some((step, email, password, remember)) = self.pending.take() {
                    self.ui(|w| w.set_step_text(SharedString::default()));
                    match self.client.accept_step(&step).await {
                        Ok(()) => self.do_login(email, password, remember).await,
                        Err(e) => {
                            self.push_login_state(Some(format!("Accepting failed: {e}")), false);
                        }
                    }
                    self.push_devlog();
                }
            }
            Cmd::DeclineStep => {
                self.pending = None;
                self.logged_in = false;
                self.ui(|w| w.set_step_text(SharedString::default()));
                self.push_login_state(
                    Some("Accept the updated documents in the official LibreLinkUp app, then sign in again.".into()),
                    false,
                );
            }
            Cmd::Unlock { passphrase, remember } => {
                self.push_store_lock(None, true);
                let new = vault_state(&self.s.log_dir()) == VaultState::New;
                let pass = (!passphrase.is_empty()).then_some(passphrase.clone());
                match self.open_store(pass).await {
                    Ok(()) => {
                        self.s.remember_passphrase = remember;
                        self.save_settings();
                        if remember && !passphrase.is_empty() {
                            secrets::store_passphrase(&passphrase);
                        } else {
                            secrets::forget_passphrase();
                        }
                        self.push_store_lock(None, false);
                        self.push_settings();
                        self.push_dashboard();
                        self.push_report();
                        if self.s.sync.paired() {
                            // fill the new/unlocked log from the caching server right away
                            let live = self.server_is_source();
                            self.run_sync(live).await;
                        }
                        if new {
                            self.settings_message("Glucose log created");
                        }
                    }
                    Err(e) => self.push_store_lock(Some(format!("{e:#}")), false),
                }
            }
            Cmd::SkipUnlock => {
                self.store_locked = false;
                self.push_store_lock(None, false);
                self.push_settings();
                self.push_report();
            }
            Cmd::Refresh => {
                if self.logged_in {
                    self.poll().await;
                }
            }
            Cmd::SelectPatient(i) => {
                if let Some(c) = self.connections.get(i) {
                    self.s.patient_id = c.patient_id.clone();
                    self.save_settings();
                    self.live.clear();
                    self.patient = None;
                    self.poll().await;
                }
            }
            Cmd::ReportPeriod(i) => {
                self.report_index = i.min(3);
                self.push_report();
            }
            Cmd::SaveSettings(ui) => {
                let new = self.settings_from_ui(&ui);
                self.apply_settings(new).await;
                self.settings_message("Saved");
            }
            Cmd::SetPassphrase(current, p) => {
                let remember = self.s.remember_passphrase;
                let result = match &mut self.store {
                    Some(st) => tokio::task::block_in_place(|| {
                        st.verify_passphrase(&current).map_err(|_| anyhow::anyhow!("the current passphrase is wrong"))?;
                        st.rekey(Some(&p))
                    }),
                    None => Err(anyhow::anyhow!("unlock or enable the log first")),
                };
                match result {
                    Ok(()) => {
                        if remember {
                            secrets::store_passphrase(&p);
                        }
                        self.settings_message("Log encrypted with the new passphrase");
                    }
                    Err(e) => self.settings_message(format!("Failed: {e:#}")),
                }
                self.push_settings();
            }
            Cmd::RemovePassphrase(current) => {
                let result = match &mut self.store {
                    Some(st) => tokio::task::block_in_place(|| {
                        st.verify_passphrase(&current).map_err(|_| anyhow::anyhow!("the current passphrase is wrong"))?;
                        st.rekey(None)
                    }),
                    None => Err(anyhow::anyhow!("log not loaded")),
                };
                match result {
                    Ok(()) => {
                        secrets::forget_passphrase();
                        self.settings_message("Encryption removed");
                    }
                    Err(e) => self.settings_message(format!("Failed: {e:#}")),
                }
                self.push_settings();
            }
            Cmd::ExportSettings(path) => match self.s.exportable().save_to(&path) {
                Ok(()) => self.settings_message(format!("Settings exported to {}", path.display())),
                Err(e) => self.settings_message(format!("Export failed: {e:#}")),
            },
            Cmd::ImportSettings(path) => match Settings::load_from(&path) {
                Ok(mut imported) => {
                    if imported.email.is_empty() {
                        imported.email = self.s.email.clone();
                    }
                    imported.resolved_server = self.s.resolved_server.clone();
                    imported.patient_id = self.s.patient_id.clone();
                    self.apply_settings(imported).await;
                    self.settings_message("Settings imported");
                }
                Err(e) => self.settings_message(format!("Import failed: {e:#}")),
            },
            Cmd::ExportCsv(path) => {
                let res = match &self.store {
                    Some(st) => st.export_csv(&path),
                    None => Err(anyhow::anyhow!("the log is not loaded")),
                };
                match res {
                    Ok(n) => self.settings_message(format!("Exported {n} readings")),
                    Err(e) => self.settings_message(format!("Export failed: {e:#}")),
                }
            }
            Cmd::ImportCsv(path) => {
                let res = match &mut self.store {
                    Some(st) => st.import_csv(&path),
                    None => Err(anyhow::anyhow!("enable and unlock the log first")),
                };
                match res {
                    Ok(n) => {
                        self.settings_message(format!("Imported {n} new readings"));
                        self.push_report();
                        self.push_dashboard();
                        self.push_settings();
                    }
                    Err(e) => self.settings_message(format!("Import failed: {e:#}")),
                }
            }
            Cmd::OpenDataFolder => platform::open_path(&self.s.log_dir()),
            Cmd::ClearDevLog => {
                self.devlog.clear();
                self.ui(|w| w.global::<DevBridge>().set_entries(ModelRc::default()));
            }
            Cmd::AppearanceChanged(ui) => {
                self.s.appearance = appearance_from_ui(&ui, &self.s.appearance);
                self.s.sanitize();
                self.save_settings();
                self.push_appearance(false);
                self.push_dashboard();
            }
            Cmd::ResetColors => {
                self.s.appearance.colors = RangeColors::default();
                self.save_settings();
                self.push_appearance(true);
                self.push_dashboard();
            }
            Cmd::CheckUpdates => self.check_updates(true).await,
            Cmd::InstallUpdate => self.install_update().await,
            Cmd::SkipUpdate => {
                if let Some(r) = self.update.take() {
                    self.s.updates.skipped = r.version().to_string();
                    self.save_settings();
                    self.toast(format!("Version {} will not be offered again", r.version()));
                }
                self.push_update_state("", false, -1.0);
            }
            Cmd::OpenReleasePage => {
                let url = self
                    .update
                    .as_ref()
                    .map(|r| r.html_url.clone())
                    .unwrap_or_else(|| format!("https://github.com/{}/releases", updater::REPO));
                platform::open_url(&url);
            }
            Cmd::OpenDay(key) => self.open_day(key),
            Cmd::WipeLocalData(include_log) => self.wipe_local_data(include_log),
            Cmd::UpdateOptions(auto) => {
                self.s.updates.auto_check = auto;
                self.save_settings();
            }
            Cmd::PairServer(code, source) => self.pair_server(code, source).await,
            Cmd::Unpair => {
                self.unpair(None);
                self.toast("Unpaired from the caching server");
            }
            Cmd::SyncNow => {
                let live = self.server_is_source();
                self.run_sync(live).await;
            }
            Cmd::SyncSource(on) => {
                self.s.sync.use_as_source = on;
                self.save_settings();
                if self.s.sync.paired() {
                    if on && !self.logged_in {
                        self.logged_in = true;
                        self.push_login_state(None, false);
                    }
                    if !on && self.password.is_none() {
                        self.logged_in = false;
                        self.push_login_state(Some("Sign in to LibreLinkUp to poll from this computer.".into()), false);
                    }
                    self.schedule_poll(1);
                }
            }
            Cmd::Quit(_) => unreachable!(),
        }
    }

    fn settings_from_ui(&self, ui: &UiSettings) -> Settings {
        let u = self.units();
        let mut s = self.s.clone();
        s.server_override = (ui.server_index as usize)
            .checked_sub(1)
            .and_then(|i| self.servers.get(i))
            .map(|s| s.url.clone())
            .unwrap_or_default();
        s.units = match ui.units {
            1 => UnitPref::MmolL,
            2 => UnitPref::MgDl,
            _ => UnitPref::Auto,
        };
        s.poll_seconds = ui.poll_seconds.max(30) as u32;
        let parse = |text: &SharedString, old: f32| u.parse(text).unwrap_or(old);
        s.target_low = parse(&ui.target_low, s.target_low);
        s.target_high = parse(&ui.target_high, s.target_high);
        s.notify_low = ui.notify_low;
        s.low_threshold = parse(&ui.low_threshold, s.low_threshold);
        s.notify_high = ui.notify_high;
        s.high_threshold = parse(&ui.high_threshold, s.high_threshold);
        s.notify_stale = ui.notify_stale;
        s.stale_minutes = ui.stale_minutes.max(0) as u32;
        s.notify_repeat_minutes = ui.repeat_minutes.max(0) as u32;
        s.log_enabled = ui.log_enabled;
        s.log_dir = ui.log_dir.trim().to_string();
        s.remember_passphrase = ui.remember_passphrase;
        s.dev_mode = ui.dev_mode;
        s.start_minimized = ui.start_minimized;
        s.autostart = ui.autostart;
        s.dark_mode = match ui.theme {
            1 => Some(false),
            2 => Some(true),
            _ => None,
        };
        s.identity.product = ui.product.trim().to_string();
        s.identity.version = ui.version.trim().to_string();
        s.sanitize();
        s
    }

    async fn apply_settings(&mut self, mut new: Settings) {
        new.sanitize();
        let old = std::mem::replace(&mut self.s, new);
        let s = &self.s;

        if s.autostart != old.autostart && !self.demo {
            if let Err(e) = platform::set_autostart(s.autostart) {
                self.settings_message(format!("Autostart: {e:#}"));
            }
        }
        if !s.remember_passphrase && old.remember_passphrase {
            secrets::forget_passphrase();
        }
        self.devlog.set_enabled(s.dev_mode);
        self.client.set_identity(s.identity.clone());
        let server = if s.server_override.is_empty() {
            if s.resolved_server.is_empty() { GLOBAL_SERVER.to_string() } else { s.resolved_server.clone() }
        } else {
            s.server_override.clone()
        };
        self.client.set_server(&server);
        let log_changed = s.log_enabled != old.log_enabled || s.log_dir() != old.log_dir();
        let poll_changed = s.poll_seconds != old.poll_seconds;
        self.save_settings();
        if log_changed {
            self.init_store().await;
        }
        self.push_settings();
        self.push_appearance(true);
        self.push_dashboard();
        self.push_report();
        self.push_devlog();
        if poll_changed || !self.client.is_authenticated() {
            self.schedule_poll(1);
        }
    }
}

// ---------------------------------------------------------------- demo mode

impl Worker {
    /// `--demo`: synthetic data in a throw-away log, no network and no settings are written.
    fn start_demo(&mut self) {
        secrets::disable();
        let dir = std::env::temp_dir().join(format!("better-librelinkup-demo-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        self.s.log_enabled = true;
        self.s.log_dir = dir.display().to_string();
        let now = Utc::now();
        let history = crate::demo::readings(now - Duration::days(14), now, 300);
        match Store::open(&dir, None) {
            Ok(mut st) => {
                st.insert(&history);
                let _ = st.flush();
                self.store = Some(st);
            }
            Err(e) => self.store_error(e),
        }
        self.live = crate::demo::readings(now - Duration::hours(12), now, 60);
        self.logged_in = true;
        self.s.email = "demo@example.com".into();
        self.connections = vec![crate::demo::connection("Demo", "Patient", self.live.last().copied())];
        self.patient = self.connections.first().cloned();
        self.push_login_state(None, false);
        self.push_patients();
        self.push_store_lock(None, false);
        self.push_settings();
        self.push_dashboard();
        self.push_report();
        self.set_status("Demo mode – synthetic data", false);
    }

    fn demo_tick(&mut self) {
        let now = Utc::now();
        let r = crate::demo::readings(now - Duration::seconds(30), now, 60);
        self.live.extend(r.iter().copied());
        if let Some(st) = &mut self.store {
            st.insert(&r);
        }
        if let (Some(p), Some(last)) = (&mut self.patient, r.last()) {
            p.current = Some(*last);
        }
        self.check_alerts(r.last().copied());
        self.push_dashboard();
    }
}

// ---------------------------------------------------------------- appearance

const GRAPH_HOURS: [u32; 4] = [3, 6, 12, 24];
const GREY: [u8; 4] = [0x75, 0x75, 0x75, 0xff];

fn range_rgba(c: &RangeColors, level: i32) -> [u8; 4] {
    let hex = match level {
        1 => &c.very_low,
        2 => &c.low,
        3 => &c.target,
        4 => &c.high,
        5 => &c.very_high,
        _ => return GREY,
    };
    parse_hex(hex).unwrap_or(GREY)
}

fn to_color(c: [u8; 4]) -> slint::Color {
    slint::Color::from_argb_u8(c[3], c[0], c[1], c[2])
}

fn is_light(c: [u8; 4]) -> bool {
    0.299 * c[0] as f32 + 0.587 * c[1] as f32 + 0.114 * c[2] as f32 > 160.0
}

/// Keeps the previous value for any colour the user is still typing (not yet valid hex).
fn appearance_from_ui(ui: &UiAppearance, old: &Appearance) -> Appearance {
    let hex = |s: &SharedString, old: &String| {
        if parse_hex(s).is_some() {
            let s = s.trim();
            if s.starts_with('#') { s.to_string() } else { format!("#{s}") }
        } else {
            old.clone()
        }
    };
    Appearance {
        accent: hex(&ui.accent, &old.accent),
        colors: RangeColors {
            very_low: hex(&ui.very_low, &old.colors.very_low),
            low: hex(&ui.low, &old.colors.low),
            target: hex(&ui.target, &old.colors.target),
            high: hex(&ui.high, &old.colors.high),
            very_high: hex(&ui.very_high, &old.colors.very_high),
        },
        corners: ui.corners.clamp(0, 2) as u8,
        graph_hours: GRAPH_HOURS[ui.graph_hours.clamp(0, 3) as usize],
        graph_style: ui.graph_style.clamp(0, 2) as u8,
        colored_card: ui.colored_card,
        show_stats: ui.show_stats,
        tray: TrayStyle {
            shape: ui.tray_shape.clamp(0, 3) as u8,
            arrow: ui.tray_arrow,
            text_color: ui.tray_text.clamp(0, 2) as u8,
            font: ui.tray_font.clamp(0, 2) as u8,
        },
    }
}

impl Worker {
    /// Applies colours/corners to the theme and refreshes the tray previews. With `full` the
    /// appearance page's fields are reloaded too (skipped while the user is editing them).
    fn push_appearance(&self, full: bool) {
        let a = self.s.appearance.clone();
        let accent = parse_hex(&a.accent).unwrap_or([0x1d, 0x5f, 0xa8, 0xff]);
        let ui_a = UiAppearance {
            accent: a.accent.clone().into(),
            very_low: a.colors.very_low.clone().into(),
            low: a.colors.low.clone().into(),
            target: a.colors.target.clone().into(),
            high: a.colors.high.clone().into(),
            very_high: a.colors.very_high.clone().into(),
            corners: a.corners as i32,
            graph_hours: GRAPH_HOURS.iter().position(|h| *h == a.graph_hours).unwrap_or(2) as i32,
            graph_style: a.graph_style as i32,
            colored_card: a.colored_card,
            show_stats: a.show_stats,
            tray_shape: a.tray.shape as i32,
            tray_arrow: a.tray.arrow,
            tray_text: a.tray.text_color as i32,
            tray_font: a.tray.font as i32,
        };
        // preview with the live value if there is one
        let u = self.units();
        let (text, trend, color) = match self.patient.as_ref().and_then(|p| p.current) {
            Some(r) => (
                u.fmt_reading(r.mg_dl),
                r.trend,
                range_rgba(&a.colors, level(r.mg_dl, self.s.target_low, self.s.target_high)),
            ),
            None => (u.fmt(112.0), 4, range_rgba(&a.colors, 3)),
        };
        self.ui(move |w| {
            let t = w.global::<Theme>();
            t.set_accent(to_color(accent));
            t.set_accent_is_light(is_light(accent));
            t.set_corners(a.corners as i32);
            t.set_very_low(to_color(range_rgba(&a.colors, 1)));
            t.set_low(to_color(range_rgba(&a.colors, 2)));
            t.set_target(to_color(range_rgba(&a.colors, 3)));
            t.set_high(to_color(range_rgba(&a.colors, 4)));
            t.set_very_high(to_color(range_rgba(&a.colors, 5)));

            // rendered on the UI thread, which owns the font database used by the tray
            let b = w.global::<SettingsBridge>();
            let image = |size: u32| {
                let rgba = trayicon::render(&text, trend, color, &a.tray, size);
                slint::Image::from_rgba8(slint::SharedPixelBuffer::clone_from_slice(&rgba, size, size))
            };
            b.set_tray_preview(image(64));
            let actual = trayicon::tray_size();
            b.set_tray_preview_actual(image(actual));
            b.set_tray_actual_size(actual as i32);
            if full {
                b.set_appearance(ui_a);
            }
        });
    }
}

// ---------------------------------------------------------------- updates

const UPDATE_INTERVAL: StdDuration = StdDuration::from_secs(12 * 3600);
const BACKFILL_INTERVAL: StdDuration = StdDuration::from_secs(15 * 60);

impl Worker {
    fn push_update_state(&self, status: &str, busy: bool, progress: f32) {
        let kind = updater::install_kind();
        let available = self.update.as_ref().filter(|r| updater::is_newer(r.version(), updater::CURRENT)).cloned();
        let notes: String = available
            .as_ref()
            .and_then(|r| r.body.clone())
            .unwrap_or_default()
            .chars()
            .take(800)
            .collect();
        let can_install = available.as_ref().is_some_and(|r| updater::pick_asset(r, kind).is_some());
        let (status, auto) = (status.to_string(), self.s.updates.auto_check);
        self.ui(move |w| {
            let u = w.global::<UpdateBridge>();
            u.set_current_version(updater::CURRENT.into());
            u.set_install_kind(kind.describe().into());
            u.set_status(status.into());
            u.set_available(available.is_some());
            u.set_latest_version(available.as_ref().map(|r| r.version().to_string()).unwrap_or_default().into());
            u.set_notes(notes.trim().to_string().into());
            u.set_busy(busy);
            u.set_progress(progress);
            u.set_can_install(can_install);
            u.set_auto_check(auto);
        });
    }

    async fn maybe_check_updates(&mut self) {
        if self.demo || !self.s.updates.auto_check {
            return;
        }
        let due = match self.update_checked {
            None => self.started.elapsed() > StdDuration::from_secs(15),
            Some(t) => t.elapsed() > UPDATE_INTERVAL,
        };
        if due {
            self.check_updates(false).await;
        }
    }

    async fn check_updates(&mut self, manual: bool) {
        self.update_checked = Some(Instant::now());
        self.push_update_state("Checking for updates…", true, -1.0);
        match updater::latest().await {
            Ok(Some(release)) => {
                let version = release.version().to_string();
                let newer = updater::is_newer(&version, updater::CURRENT);
                let skipped = !manual && self.s.updates.skipped == version;
                self.update = (newer && !skipped).then_some(release);
                let checked = format!("checked {}", Local::now().format("%H:%M"));
                if self.update.is_some() {
                    self.push_update_state(&checked, false, -1.0);
                    if self.notified_update != version {
                        self.notified_update = version.clone();
                        alerts::show(
                            "Update available",
                            &format!("Better LibreLinkUp {version} is available – open Settings → Updates."),
                            false,
                        );
                    }
                    if manual {
                        self.toast(format!("Version {version} is available"));
                    }
                } else {
                    self.push_update_state(&format!("You have the latest version ({checked})"), false, -1.0);
                    if manual {
                        self.toast("Better LibreLinkUp is up to date");
                    }
                }
            }
            Ok(None) => {
                self.update = None;
                self.push_update_state("No release has been published yet", false, -1.0);
            }
            Err(e) => {
                tracing::warn!("update check failed: {e:#}");
                self.push_update_state(&format!("Update check failed: {e:#}"), false, -1.0);
            }
        }
    }

    async fn install_update(&mut self) {
        let Some(release) = self.update.clone() else { return };
        let kind = updater::install_kind();
        let Some(asset) = updater::pick_asset(&release, kind).cloned() else {
            self.push_update_state("This release has no package for this kind of installation", false, -1.0);
            return;
        };
        self.push_update_state(&format!("Downloading {}…", asset.name), true, 0.0);
        let ui = self.ui.clone();
        let last = std::sync::atomic::AtomicU32::new(0);
        let progress = move |p: f32| {
            // at most ~100 UI updates
            let step = (p * 100.0) as u32;
            if last.swap(step, std::sync::atomic::Ordering::Relaxed) != step {
                let _ = ui.upgrade_in_event_loop(move |w| w.global::<UpdateBridge>().set_progress(p));
            }
        };
        let file = match updater::download(&release, &asset, progress).await {
            Ok(f) => f,
            Err(e) => {
                self.push_update_state(&format!("Download failed: {e:#}"), false, -1.0);
                return;
            }
        };
        self.push_update_state("Installing…", true, -1.0);
        self.flush_store();
        match tokio::task::spawn_blocking(move || updater::install(kind, &file)).await {
            Ok(Ok(true)) => {
                self.toast(format!("Restarting into version {}…", release.version()));
                let _ = slint::invoke_from_event_loop(|| {
                    let _ = slint::quit_event_loop();
                });
            }
            Ok(Ok(false)) => self.push_update_state("Installed – restart the app to use the new version", false, -1.0),
            Ok(Err(e)) => self.push_update_state(&format!("Installation failed: {e:#}"), false, -1.0),
            Err(e) => self.push_update_state(&format!("Installation failed: {e}"), false, -1.0),
        }
    }
}

// ---------------------------------------------------------------- caching server

fn connection_from(p: &PatientInfo) -> Connection {
    Connection {
        patient_id: p.id.clone(),
        first_name: p.first_name.clone(),
        last_name: p.last_name.clone(),
        target_low: p.target_low,
        target_high: p.target_high,
        uses_mg_dl: p.uses_mg_dl,
        current: p.current.as_ref().and_then(sync::from_wire),
        sensor_serial: p.sensor_serial.clone(),
        sensor_activated: p.sensor_activated.and_then(|t| chrono::DateTime::from_timestamp(t, 0)),
    }
}

impl Worker {
    fn server_is_source(&self) -> bool {
        !self.demo && self.s.sync.paired() && self.s.sync.use_as_source
    }

    fn push_sync_state(&self, status: &str, error: bool, busy: bool) {
        let s = &self.s.sync;
        let paired = s.paired();
        let server = if paired {
            format!(
                "{} · {} · {}",
                if s.server_name.is_empty() { "Caching server" } else { &s.server_name },
                s.url,
                if s.fingerprint.is_empty() {
                    "public certificate".to_string()
                } else {
                    format!("certificate {}", sync::short_fingerprint(&s.fingerprint))
                }
            )
        } else {
            String::new()
        };
        let (status, source) = (status.to_string(), s.use_as_source || !paired);
        self.ui(move |w| {
            let b = w.global::<SyncBridge>();
            b.set_paired(paired);
            b.set_server(server.into());
            b.set_status(status.into());
            b.set_status_error(error);
            b.set_busy(busy);
            b.set_use_as_source(source);
        });
    }

    /// Client for the paired server; rotates the bearer token when it is older than 30 days.
    async fn sync_client(&mut self) -> Result<SyncClient, String> {
        let s = self.s.sync.clone();
        let Some(mut creds) = secrets::sync_credentials() else {
            self.unpair(Some("The pairing keys are missing from the system keychain – pair again.".into()));
            return Err("not paired".into());
        };
        let client = SyncClient::new(&s.url, &s.fingerprint, Some(creds.token.clone()), &creds.identity)
            .map_err(|e| format!("{e:#}"))?;
        if !creds.rotation_due() {
            return Ok(client);
        }
        match client.rotate().await {
            Ok(token) => {
                creds.token = token;
                creds.token_issued = Utc::now().timestamp();
                if !secrets::store_sync_credentials(&creds) {
                    return Err("could not store the rotated token in the system keychain".into());
                }
                tracing::info!("rotated the caching server token");
                SyncClient::new(&s.url, &s.fingerprint, Some(creds.token), &creds.identity).map_err(|e| format!("{e:#}"))
            }
            Err(SyncError::Unauthorized) => {
                self.unpair(Some("The server revoked this computer – pair it again.".into()));
                Err("pairing revoked".into())
            }
            // keep using the current token, try again next time
            Err(e) => {
                tracing::warn!("token rotation failed: {e}");
                Ok(client)
            }
        }
    }

    async fn pair_server(&mut self, code: String, use_as_source: bool) {
        self.push_sync_state("Connecting…", false, true);
        let result: Result<(), String> = async {
            let code = PairingCode::decode(&code).map_err(|e| format!("{e:#}"))?;
            let host = std::env::var("COMPUTERNAME").or_else(|_| std::env::var("HOSTNAME")).unwrap_or_else(|_| "desktop".into());
            let name = format!("Better LibreLinkUp on {host}");
            // a fresh key pair per pairing; the private key never leaves this computer
            let identity = ClientIdentity::generate(&name).map_err(|e| format!("{e:#}"))?;
            let client = SyncClient::new(&code.url, &code.fingerprint, None, &identity).map_err(|e| format!("{e:#}"))?;
            client.health().await.map_err(|e| e.to_string())?;
            let resp = client.pair(&code.secret, &name).await.map_err(|e| e.to_string())?;
            let creds = SyncCredentials { token: resp.token.clone(), token_issued: Utc::now().timestamp(), identity };
            if !secrets::store_sync_credentials(&creds) {
                return Err("could not store the pairing keys in the system keychain".into());
            }
            self.s.sync = SyncSettings {
                url: code.url,
                fingerprint: code.fingerprint,
                client_id: resp.client_id,
                server_name: resp.server_name,
                use_as_source,
                synced_until: Default::default(),
            };
            self.save_settings();
            Ok(())
        }
        .await;
        match result {
            Ok(()) => {
                self.toast(format!("Paired with {}", self.s.sync.server_name));
                if self.server_is_source() && !self.logged_in {
                    self.logged_in = true;
                    self.push_login_state(None, false);
                }
                self.push_sync_state("Paired – fetching history…", false, true);
                self.run_sync(true).await;
            }
            Err(e) => self.push_sync_state(&format!("Pairing failed: {e}"), true, false),
        }
    }

    fn unpair(&mut self, reason: Option<String>) {
        secrets::forget_sync_credentials();
        let was_source = self.server_is_source();
        self.s.sync = SyncSettings::default();
        self.save_settings();
        self.push_sync_state(reason.as_deref().unwrap_or(""), reason.is_some(), false);
        if was_source {
            self.patient = None;
            if self.password.is_none() {
                self.logged_in = false;
                self.push_login_state(reason, false);
            } else {
                self.schedule_poll(1);
            }
        }
    }

    /// Fetches everything since the newest local reading. With `live` the server also provides
    /// the current value, sensor and patient list (this computer does not poll LibreLinkUp).
    async fn sync_from_server(&mut self, live: bool) -> Result<usize, String> {
        let client = self.sync_client().await?;
        let patients = match client.patients().await {
            Ok(p) => p,
            Err(SyncError::Unauthorized) => {
                self.unpair(Some("The server revoked this computer – pair it again.".into()));
                return Err("pairing revoked".into());
            }
            Err(e) => return Err(e.to_string()),
        };
        if patients.is_empty() {
            return Err("the server has no data yet – check LLU_EMAIL / LLU_PASSWORD on the server".into());
        }
        if !patients.iter().any(|p| p.id == self.s.patient_id) {
            self.s.patient_id = patients[0].id.clone();
            self.save_settings();
        }
        let p = patients.iter().find(|p| p.id == self.s.patient_id).cloned().unwrap();
        let now = Utc::now();
        // Fetch the full server history once (new log, other folder, first pairing), afterwards
        // only what is new since the last sync – that covers any time this computer was off.
        let server_first = p.first_reading.unwrap_or(now.timestamp() - 90 * 86_400);
        let since = match &self.store {
            Some(st) => {
                let has_start = st.first_time().is_some_and(|t| t.timestamp() <= server_first + 86_400);
                match (has_start, self.s.sync.synced_until.get(&p.id)) {
                    (true, Some(t)) => chrono::DateTime::from_timestamp(t - 3600, 0).unwrap_or(now),
                    _ => chrono::DateTime::from_timestamp(server_first - 60, 0).unwrap_or(now),
                }
            }
            None => now - Duration::hours(LIVE_KEEP_HOURS),
        };
        let readings = client
            .readings(&p.id, since.timestamp(), now.timestamp() + 300)
            .await
            .map_err(|e| e.to_string())?;

        let keep_from = now - Duration::hours(LIVE_KEEP_HOURS);
        self.live.extend(readings.iter().filter(|r| r.time >= keep_from).copied());
        self.live.sort_by_key(|r| r.time);
        self.live.dedup_by_key(|r| r.time.timestamp());
        self.live.retain(|r| r.time >= keep_from);
        let mut added = 0;
        if let Some(st) = &mut self.store {
            added = st.insert(&readings);
            if added > 0 {
                st.flush().map_err(|e| format!("writing the log: {e:#}"))?;
            }
        }
        if self.store.is_some() {
            let previous = self.s.sync.synced_until.insert(p.id.clone(), now.timestamp());
            // persist at most every 15 minutes (live mode syncs every minute)
            if previous.is_none_or(|t| now.timestamp() - t > 900) || added > 0 {
                self.save_settings();
            }
        }
        if live {
            let first = self.connections.is_empty();
            self.connections = patients.iter().map(connection_from).collect();
            if first {
                self.push_patients();
            }
            self.account_mg = p.uses_mg_dl;
            let conn = connection_from(&p);
            let latest = conn.current;
            self.patient = Some(conn);
            self.check_alerts(latest);
        }
        self.last_backfill = Some(Instant::now());
        self.push_dashboard();
        if added > 0 {
            self.push_report();
            self.push_settings();
        }
        Ok(added)
    }

    async fn run_sync(&mut self, live: bool) {
        self.push_sync_state("Syncing…", false, true);
        match self.sync_from_server(live).await {
            Ok(n) => {
                let msg = format!(
                    "Last sync {}{}",
                    Local::now().format("%H:%M"),
                    if n > 0 { format!(" · {n} readings added to the log") } else { String::new() }
                );
                self.push_sync_state(&msg, false, false);
                if live {
                    self.set_status(format!("Updated {} via server", Local::now().format("%H:%M")), false);
                }
            }
            Err(e) => {
                if self.s.sync.paired() {
                    self.push_sync_state(&format!("Sync failed: {e}"), true, false);
                }
                if live {
                    self.set_status(format!("Server: {e}"), true);
                }
            }
        }
    }

    fn backfill_due(&self) -> bool {
        self.s.sync.paired() && self.last_backfill.is_none_or(|t| t.elapsed() > BACKFILL_INTERVAL)
    }
}
