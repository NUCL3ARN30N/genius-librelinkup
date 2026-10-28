//! System tray icon (Windows notification area / StatusNotifierItem on Linux).
//! Must be created and updated on the UI thread.

use std::cell::RefCell;

use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

use crate::config::TrayStyle;
use crate::trayicon::{self, Rgba};

pub enum TrayAction {
    Show,
    Refresh,
    Quit,
}

/// Everything that determines the icon's pixels; re-rendered only when it changes.
#[derive(Clone, PartialEq)]
pub struct TrayContent {
    pub text: String,
    pub trend: u8,
    pub color: Rgba,
    pub style: TrayStyle,
}

struct Tray {
    icon: TrayIcon,
    last: Option<(TrayContent, u32)>,
}

thread_local! {
    static TRAY: RefCell<Option<Tray>> = const { RefCell::new(None) };
}

fn make_icon(c: &TrayContent, size: u32) -> Option<tray_icon::Icon> {
    let rgba = trayicon::render(&c.text, c.trend, c.color, &c.style, size);
    tray_icon::Icon::from_rgba(rgba, size, size).ok()
}

/// Creates the tray icon. Returns false when the platform has no tray (e.g. GNOME without the
/// AppIndicator extension), in which case the window should not hide on close.
pub fn init(on_action: impl Fn(TrayAction) + Send + Sync + 'static) -> bool {
    let menu = Menu::new();
    let show = MenuItem::with_id("show", "Show Better LibreLinkUp", true, None);
    let refresh = MenuItem::with_id("refresh", "Refresh now", true, None);
    let quit = MenuItem::with_id("quit", "Quit", true, None);
    let _ = menu.append_items(&[&show, &refresh, &PredefinedMenuItem::separator(), &quit]);

    let placeholder = TrayContent {
        text: "-".into(),
        trend: 0,
        color: [0x75, 0x75, 0x75, 0xff],
        style: TrayStyle::default(),
    };
    let mut builder = TrayIconBuilder::new()
        .with_id("better-librelinkup")
        .with_tooltip("Better LibreLinkUp")
        .with_menu(Box::new(menu))
        .with_menu_on_left_click(false);
    if let Some(icon) = make_icon(&placeholder, trayicon::tray_size()) {
        builder = builder.with_icon(icon);
    }
    let tray = match builder.build() {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!("system tray unavailable: {e}");
            return false;
        }
    };

    let on_action = std::sync::Arc::new(on_action);
    let a = on_action.clone();
    MenuEvent::set_event_handler(Some(move |e: MenuEvent| match e.id().as_ref() {
        "show" => a(TrayAction::Show),
        "refresh" => a(TrayAction::Refresh),
        "quit" => a(TrayAction::Quit),
        _ => {}
    }));
    TrayIconEvent::set_event_handler(Some(move |e: TrayIconEvent| match e {
        TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. }
        | TrayIconEvent::DoubleClick { button: MouseButton::Left, .. } => on_action(TrayAction::Show),
        _ => {}
    }));

    TRAY.with(|t| *t.borrow_mut() = Some(Tray { icon: tray, last: None }));
    true
}

pub fn update(content: TrayContent, tooltip: &str) {
    TRAY.with(|t| {
        let mut t = t.borrow_mut();
        let Some(tray) = t.as_mut() else { return };
        // the size changes when the user switches display scaling
        let size = trayicon::tray_size();
        let key = (content, size);
        if tray.last.as_ref() != Some(&key) {
            if let Err(e) = tray.icon.set_icon(make_icon(&key.0, size)) {
                tracing::warn!("tray icon update failed: {e}");
            }
            tray.last = Some(key);
        }
        let _ = tray.icon.set_tooltip(Some(tooltip));
    });
}

pub fn shutdown() {
    TRAY.with(|t| t.borrow_mut().take());
}
