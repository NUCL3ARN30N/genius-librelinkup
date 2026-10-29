#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod about;
mod alerts;
mod config;
mod icon;
mod migrate;
mod platform;
mod secrets;
mod stats;
mod tray;
mod trayicon;
mod ui_model;
mod units;
mod updater;
mod worker;

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::time::Duration;

use anyhow::Result;
use slint::{ComponentHandle, Model};
use tokio::sync::{mpsc, oneshot};

use gllu_core::{api, demo, devlog, store};
use worker::Cmd;

slint::include_modules!();

/// Localhost port used to make sure only one instance polls LibreLinkUp; a second start
/// just brings the running window to the front.
const INSTANCE_PORT: u16 = 47613;

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let has = |f: &str| args.iter().any(|a| a == f);
    if has("--help") || has("-h") || has("--probe") {
        attach_console();
    }

    if has("--help") || has("-h") {
        println!(
            "genius-librelinkup [--minimized] [--dev] [--demo] [--probe] [--write-icons <dir>]\n\n\
             --minimized     start in the tray\n\
             --dev           enable developer mode (API log)\n\
             --demo          run with synthetic data (no account, nothing is saved)\n\
             --probe         test the LibreLinkUp API from the terminal using the LLU_EMAIL and\n\
             \u{20}               LLU_PASSWORD environment variables, print a redacted report and exit\n\
             --write-icons   write the generated icon assets to a folder"
        );
        return Ok(());
    }
    if let Some(i) = args.iter().position(|a| a == "--write-icons") {
        let dir = args.get(i + 1).map(String::as_str).unwrap_or("assets");
        icon::write_assets(std::path::Path::new(dir))?;
        return Ok(());
    }

    if let Some(i) = args.iter().position(|a| a == "--tray-preview") {
        let dir = std::path::PathBuf::from(args.get(i + 1).map(String::as_str).unwrap_or("tray-preview"));
        return write_tray_previews(&dir);
    }

    let _guard = init_logging(has("--dev") || has("--probe"));

    if has("--probe") {
        return probe();
    }

    if has("--updated") {
        updater::cleanup_after_update();
    }

    // single instance (the demo may run next to a real instance); right after an update the
    // old process may still be shutting down, so wait for it a little
    let port = if has("--demo") || config::home_override().is_some() { 0 } else { INSTANCE_PORT };
    let mut bound = TcpListener::bind(("127.0.0.1", port));
    if bound.is_err() && has("--updated") {
        for _ in 0..40 {
            std::thread::sleep(Duration::from_millis(250));
            bound = TcpListener::bind(("127.0.0.1", port));
            if bound.is_ok() {
                break;
            }
        }
    }
    let listener = match bound {
        Ok(l) => l,
        Err(_) => {
            if let Ok(mut s) = TcpStream::connect_timeout(&([127, 0, 0, 1], INSTANCE_PORT).into(), Duration::from_secs(1)) {
                let _ = s.write_all(b"show");
            }
            return Ok(());
        }
    };

    run_app(has("--minimized"), has("--dev"), has("--demo"), listener)
}

/// `--tray-preview <dir>`: one sheet per tray size with every shape × font, magnified 8× with
/// nearest-neighbour scaling so individual pixels can be judged.
fn write_tray_previews(dir: &std::path::Path) -> Result<()> {
    std::fs::create_dir_all(dir)?;
    let samples = [("6.2", 4u8, [0x2e, 0x7d, 0x32, 0xff]), ("112", 1, [0xf9, 0xa8, 0x25, 0xff]), ("3.4", 2, [0xd3, 0x2f, 0x2f, 0xff])];
    for size in [16u32, 20, 24, 32] {
        // rows 0–3: number with arrow per shape; rows 4–7: number alone and the separate arrow icons
        let (cols, rows) = (samples.len() as u32 * 3, 8u32);
        let cell = size + 4;
        let (w, h) = (cols * cell, rows * cell);
        let mut sheet = vec![0x30u8; (w * h * 4) as usize];
        let mut blit = |px: &[u8], col: u32, row: u32| {
            let (cx, cy) = (col * cell + 2, row * cell + 2);
            for y in 0..size {
                for x in 0..size {
                    let s = ((y * size + x) * 4) as usize;
                    let d = (((cy + y) * w + cx + x) * 4) as usize;
                    let a = px[s + 3] as u32;
                    for c in 0..3 {
                        sheet[d + c] = ((px[s + c] as u32 * a + 0x30 * (255 - a)) / 255) as u8;
                    }
                    sheet[d + 3] = 255;
                }
            }
        };
        for shape in 0..4u8 {
            for font in 0..3u8 {
                for (si, (text, trend, color)) in samples.iter().enumerate() {
                    let style = config::TrayStyle { shape, font, arrow: true, text_color: 0, arrow_separate: false };
                    blit(&trayicon::render(text, *trend, *color, &style, size), si as u32 * 3 + font as u32, shape as u32);
                }
            }
            let style = config::TrayStyle { shape, arrow_separate: true, ..Default::default() };
            let (text, _, color) = samples[0];
            blit(&trayicon::render(text, 0, color, &style, size), 0, 4 + shape as u32);
            for trend in 0..=5u8 {
                blit(&trayicon::render_arrow(trend, color, &style, size), 2 + trend as u32, 4 + shape as u32);
            }
        }
        let zoom = 8;
        let mut big = vec![0u8; (w * zoom * h * zoom * 4) as usize];
        for y in 0..h * zoom {
            for x in 0..w * zoom {
                let s = (((y / zoom) * w + x / zoom) * 4) as usize;
                let d = ((y * w * zoom + x) * 4) as usize;
                big[d..d + 4].copy_from_slice(&sheet[s..s + 4]);
            }
        }
        // icon::Image is square; pad the sheet into a square canvas
        let side = (w * zoom).max(h * zoom);
        let mut square = vec![0u8; (side * side * 4) as usize];
        for y in 0..h * zoom {
            let s = (y * w * zoom * 4) as usize;
            let d = (y * side * 4) as usize;
            square[d..d + (w * zoom * 4) as usize].copy_from_slice(&big[s..s + (w * zoom * 4) as usize]);
        }
        let img = icon::Image { size: side, rgba: square };
        std::fs::write(dir.join(format!("tray-{size}px.png")), icon::encode_png(&img))?;
    }
    println!("wrote previews to {}", dir.display());
    Ok(())
}

/// Release builds use the GUI subsystem on Windows; reattach to the terminal for CLI output.
#[cfg(windows)]
fn attach_console() {
    unsafe extern "system" {
        fn AttachConsole(process_id: u32) -> i32;
    }
    const ATTACH_PARENT_PROCESS: u32 = u32::MAX;
    unsafe {
        AttachConsole(ATTACH_PARENT_PROCESS);
    }
}

#[cfg(not(windows))]
fn attach_console() {}

fn init_logging(dev: bool) -> tracing_appender::non_blocking::WorkerGuard {
    use tracing_subscriber::prelude::*;
    let dir = config::app_log_dir();
    let _ = std::fs::create_dir_all(&dir);
    let appender = tracing_appender::rolling::Builder::new()
        .rotation(tracing_appender::rolling::Rotation::DAILY)
        .filename_prefix("genius-librelinkup")
        .filename_suffix("log")
        .max_log_files(7)
        .build(&dir)
        .expect("log appender");
    let (file, guard) = tracing_appender::non_blocking(appender);
    let default = if dev { "info,genius_librelinkup=debug" } else { "info" };
    let filter = tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| default.into());
    tracing_subscriber::registry()
        .with(filter)
        .with(tracing_subscriber::fmt::layer().with_writer(file).with_ansi(false))
        .with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr))
        .init();
    guard
}

fn run_app(minimized: bool, dev: bool, demo: bool, listener: TcpListener) -> Result<()> {
    if !demo && config::home_override().is_none() {
        migrate::from_better_librelinkup();
    }
    let mut settings = config::Settings::load();
    if dev {
        settings.dev_mode = true;
    }
    platform::register_notifications();

    let devlog = devlog::DevLog::default();
    devlog.set_enabled(settings.dev_mode);

    let ui = AppWindow::new()?;
    let (tx, rx) = mpsc::unbounded_channel::<Cmd>();
    wire_callbacks(&ui, &tx, &devlog);
    let about = ui.global::<About>();
    about.set_version(updater::CURRENT.into());
    about.set_changes(about::changes(updater::CURRENT).into());

    // tray
    let has_tray = {
        let weak = ui.as_weak();
        let tx = tx.clone();
        tray::init(move |action| {
            let weak = weak.clone();
            match action {
                tray::TrayAction::Show => {
                    let _ = slint::invoke_from_event_loop(move || show_window(&weak));
                }
                tray::TrayAction::Refresh => {
                    let _ = tx.send(Cmd::Refresh);
                }
                tray::TrayAction::Quit => {
                    let _ = slint::quit_event_loop();
                }
            }
        })
    };
    ui.window().on_close_requested(move || {
        if !has_tray {
            let _ = slint::quit_event_loop();
        }
        slint::CloseRequestResponse::HideWindow
    });

    // second instance → show
    {
        let weak = ui.as_weak();
        std::thread::spawn(move || {
            for mut stream in listener.incoming().flatten() {
                let mut buf = [0u8; 4];
                if stream.read_exact(&mut buf).is_ok() && &buf == b"show" {
                    let weak = weak.clone();
                    let _ = slint::invoke_from_event_loop(move || show_window(&weak));
                }
            }
        });
    }

    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build()?;
    rt.spawn(worker::run(settings.clone(), devlog, ui.as_weak(), rx, demo));

    if !(minimized || (settings.start_minimized && !demo)) || !has_tray {
        ui.show()?;
    }
    slint::run_event_loop_until_quit()?;

    // flush the log before exiting
    let (done_tx, done_rx) = oneshot::channel();
    if tx.send(Cmd::Quit(done_tx)).is_ok() {
        let _ = rt.block_on(async { tokio::time::timeout(Duration::from_secs(5), done_rx).await });
    }
    tray::shutdown();
    rt.shutdown_timeout(Duration::from_secs(2));
    Ok(())
}

fn show_window(weak: &slint::Weak<AppWindow>) {
    if let Some(w) = weak.upgrade() {
        let _ = w.show();
        w.window().set_minimized(false);
    }
}

fn wire_callbacks(ui: &AppWindow, tx: &mpsc::UnboundedSender<Cmd>, devlog: &devlog::DevLog) {
    macro_rules! send {
        ($tx:expr, $cmd:expr) => {{
            let _ = $tx.send($cmd);
        }};
    }
    let t = tx.clone();
    ui.on_login(move |email, password, remember, server| {
        send!(t, Cmd::Login {
            email: email.to_string(),
            password: password.to_string(),
            remember,
            server_index: server.max(0) as usize,
        })
    });
    let t = tx.clone();
    ui.on_accept_step(move || send!(t, Cmd::AcceptStep));
    let t = tx.clone();
    ui.on_decline_step(move || send!(t, Cmd::DeclineStep));
    let t = tx.clone();
    ui.on_unlock(move |pass, remember| send!(t, Cmd::Unlock { passphrase: pass.to_string(), remember }));
    let t = tx.clone();
    ui.on_skip_unlock(move || send!(t, Cmd::SkipUnlock));
    let t = tx.clone();
    ui.on_refresh(move || send!(t, Cmd::Refresh));

    let t = tx.clone();
    ui.global::<Dashboard>().on_select_patient(move |i| send!(t, Cmd::SelectPatient(i.max(0) as usize)));
    let t = tx.clone();
    ui.global::<Reports>().on_period_changed(move |i| send!(t, Cmd::ReportPeriod(i.max(0) as usize)));
    let t = tx.clone();
    ui.global::<Reports>().on_open_day(move |key| send!(t, Cmd::OpenDay(key)));

    let c = ui.global::<ChartBridge>();
    c.on_nearest_point(|points, x, max_dx| {
        let x_at = |i: usize| points.row_data(i).map(|p| p.x).unwrap_or(f32::NAN);
        ui_model::nearest_index(points.row_count(), x_at, x, max_dx)
    });
    c.on_nearest_note(|notes, x, max_dx| {
        let x_at = |i: usize| notes.row_data(i).map(|n| n.x).unwrap_or(f32::NAN);
        ui_model::nearest_index(notes.row_count(), x_at, x, max_dx)
    });

    let n = ui.global::<Notes>();
    n.on_now(|| ui_model::note_time_text(chrono::Utc::now()).into());
    let t = tx.clone();
    n.on_save(move |id, when, text| send!(t, Cmd::SaveNote(id.to_string(), when.to_string(), text.to_string())));
    let t = tx.clone();
    n.on_delete(move |id| send!(t, Cmd::DeleteNote(id.to_string())));

    ui.global::<About>().on_open_link(|i| {
        if let Some(url) = about::link(i) {
            platform::open_url(&url);
        }
    });

    let b = ui.global::<SettingsBridge>();
    let t = tx.clone();
    b.on_save(move |s| send!(t, Cmd::SaveSettings(s)));
    let t = tx.clone();
    b.on_appearance_changed(move |a| send!(t, Cmd::AppearanceChanged(a)));
    let t = tx.clone();
    b.on_reset_colors(move || send!(t, Cmd::ResetColors));
    b.on_browse_log_dir(|| {
        rfd::FileDialog::new()
            .set_title("Choose the glucose log folder")
            .pick_folder()
            .map(|p| p.display().to_string().into())
            .unwrap_or_default()
    });
    let t = tx.clone();
    b.on_set_passphrase(move |current, p| send!(t, Cmd::SetPassphrase(current.to_string(), p.to_string())));
    let t = tx.clone();
    b.on_remove_passphrase(move |current| send!(t, Cmd::RemovePassphrase(current.to_string())));
    let t = tx.clone();
    b.on_export_settings(move || {
        if let Some(p) = rfd::FileDialog::new()
            .set_file_name("genius-librelinkup-settings.json")
            .add_filter("JSON", &["json"])
            .save_file()
        {
            send!(t, Cmd::ExportSettings(p))
        }
    });
    let t = tx.clone();
    b.on_import_settings(move || {
        if let Some(p) = rfd::FileDialog::new().add_filter("JSON", &["json"]).pick_file() {
            send!(t, Cmd::ImportSettings(p))
        }
    });
    let t = tx.clone();
    b.on_export_csv(move || {
        if let Some(p) = rfd::FileDialog::new()
            .set_file_name("glucose-readings.csv")
            .add_filter("CSV", &["csv"])
            .save_file()
        {
            send!(t, Cmd::ExportCsv(p))
        }
    });
    let t = tx.clone();
    b.on_import_csv(move || {
        if let Some(p) = rfd::FileDialog::new().add_filter("CSV", &["csv", "txt"]).pick_file() {
            send!(t, Cmd::ImportCsv(p))
        }
    });
    let t = tx.clone();
    b.on_open_data_folder(move || send!(t, Cmd::OpenDataFolder));
    let t = tx.clone();
    b.on_logout(move || send!(t, Cmd::Logout));
    let t = tx.clone();
    b.on_wipe_data(move |include_log| send!(t, Cmd::WipeLocalData(include_log)));

    let d = ui.global::<DevBridge>();
    let t = tx.clone();
    d.on_clear(move || send!(t, Cmd::ClearDevLog));
    d.on_open_log_folder(|| platform::open_path(&config::app_log_dir()));
    let _ = devlog;

    let u = ui.global::<UpdateBridge>();
    let t = tx.clone();
    u.on_check(move || send!(t, Cmd::CheckUpdates));
    let t = tx.clone();
    u.on_install(move || send!(t, Cmd::InstallUpdate));
    let t = tx.clone();
    u.on_skip(move || send!(t, Cmd::SkipUpdate));
    let t = tx.clone();
    u.on_open_page(move || send!(t, Cmd::OpenReleasePage));
    let t = tx.clone();
    u.on_options_changed(move |auto| send!(t, Cmd::UpdateOptions(auto)));

    let sy = ui.global::<SyncBridge>();
    let t = tx.clone();
    sy.on_pair(move |code, source| send!(t, Cmd::PairServer(code.trim().to_string(), source)));
    let t = tx.clone();
    sy.on_unpair(move || send!(t, Cmd::Unpair));
    let t = tx.clone();
    sy.on_sync_now(move || send!(t, Cmd::SyncNow));
    let t = tx.clone();
    sy.on_source_changed(move |on| send!(t, Cmd::SyncSource(on)));
}

/// `--probe`: exercises discovery, login, connections and graph and prints a redacted summary.
fn probe() -> Result<()> {
    let email = std::env::var("LLU_EMAIL").map_err(|_| anyhow::anyhow!("set LLU_EMAIL"))?;
    let password = std::env::var("LLU_PASSWORD").map_err(|_| anyhow::anyhow!("set LLU_PASSWORD"))?;
    let server = std::env::var("LLU_SERVER").ok();
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
    rt.block_on(async move {
        let devlog = devlog::DevLog::default();
        devlog.set_enabled(true);
        let mut client = api::Client::new(server, api::Identity::default(), devlog.clone());
        let servers = client.servers().await;
        println!("Known servers ({}):", servers.len());
        for s in &servers {
            println!("  {:<6} {}", s.region, s.url);
        }
        print!("\nLogin via {} … ", client.server());
        match client.login(&email, &password).await {
            Ok(()) => println!("ok → resolved server {}, identity {:?}", client.server(), client.identity()),
            Err(e) => {
                println!("failed: {e}");
                dump(&devlog);
                return Ok(());
            }
        }
        let conns = client.connections().await?;
        println!("Connections: {}", conns.len());
        for c in &conns {
            let graph = client.graph(&c.patient_id).await?;
            let cur = graph.connection.current;
            println!(
                "  patient #{} – unit {} – current {} – {} history points ({} → {})",
                conns.iter().position(|x| x.patient_id == c.patient_id).unwrap_or(0) + 1,
                if graph.connection.uses_mg_dl { "mg/dL" } else { "mmol/L" },
                cur.map(|r| format!("{} mg/dL trend {} at {}", r.mg_dl, r.trend, r.time)).unwrap_or("none".into()),
                graph.history.len(),
                graph.history.first().map(|r| r.time.to_rfc3339()).unwrap_or_default(),
                graph.history.last().map(|r| r.time.to_rfc3339()).unwrap_or_default(),
            );
        }
        if std::env::var("LLU_DUMP").is_ok() {
            dump(&devlog);
        }
        anyhow::Ok(())
    })
}

fn dump(devlog: &devlog::DevLog) {
    for e in devlog.entries().iter().rev() {
        println!("\n--- {} {} → {:?} ({} ms)\n{}", e.method, e.url, e.status, e.millis, e.response);
    }
}
