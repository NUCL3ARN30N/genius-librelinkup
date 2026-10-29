//! System tray icons (Windows notification area / StatusNotifierItem on Linux): the value, and
//! optionally the trend arrow as a second icon. Must be created and updated on the UI thread.

use std::cell::RefCell;
use std::sync::Arc;

use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

use crate::config::TrayStyle;
use crate::trayicon::{self, Rgba};

pub enum TrayAction {
    Show,
    Refresh,
    Quit,
}

/// Everything that determines the icons' pixels; re-rendered only when it changes.
#[derive(Clone, PartialEq)]
pub struct TrayContent {
    pub text: String,
    pub trend: u8,
    pub color: Rgba,
    pub style: TrayStyle,
}

impl TrayContent {
    fn separate_arrow(&self) -> bool {
        self.style.arrow && self.style.arrow_separate
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Value,
    Arrow,
}

impl Kind {
    fn id(self) -> &'static str {
        match self {
            Kind::Value => "genius-librelinkup",
            Kind::Arrow => "genius-librelinkup-trend",
        }
    }

    fn rgba(self, c: &TrayContent, size: u32) -> Vec<u8> {
        match self {
            Kind::Value => trayicon::render(&c.text, c.trend, c.color, &c.style, size),
            Kind::Arrow => trayicon::render_arrow(c.trend, c.color, &c.style, size),
        }
    }
}

struct Slot {
    icon: TrayIcon,
    /// what the icon currently shows (content, pixel size)
    last: Option<(TrayContent, u32)>,
}

struct Tray {
    value: Option<Slot>,
    arrow: Option<Slot>,
}

thread_local! {
    static TRAY: RefCell<Option<Tray>> = const { RefCell::new(None) };
}

fn make_icon(kind: Kind, c: &TrayContent, size: u32) -> Option<tray_icon::Icon> {
    tray_icon::Icon::from_rgba(kind.rgba(c, size), size, size).ok()
}

/// Each icon gets its own menu with the same entries; the handler is global.
fn build(kind: Kind, content: &TrayContent, tooltip: &str) -> tray_icon::Result<Slot> {
    let menu = Menu::new();
    let show = MenuItem::with_id("show", "Show Genius LibreLinkUp", true, None);
    let refresh = MenuItem::with_id("refresh", "Refresh now", true, None);
    let quit = MenuItem::with_id("quit", "Quit", true, None);
    let _ = menu.append_items(&[&show, &refresh, &PredefinedMenuItem::separator(), &quit]);

    let size = trayicon::tray_size();
    let mut builder = TrayIconBuilder::new()
        .with_id(kind.id())
        .with_tooltip(tooltip)
        .with_menu(Box::new(menu))
        .with_menu_on_left_click(false);
    if let Some(icon) = make_icon(kind, content, size) {
        builder = builder.with_icon(icon);
    }
    let icon = builder.build()?;
    Ok(Slot { icon, last: Some((content.clone(), size)) })
}

/// Creates the tray icon. Returns false when the platform has no tray (e.g. GNOME without the
/// AppIndicator extension), in which case the window should not hide on close.
pub fn init(on_action: impl Fn(TrayAction) + Send + Sync + 'static) -> bool {
    let content = TrayContent {
        text: "-".into(),
        trend: 0,
        color: [0x75, 0x75, 0x75, 0xff],
        style: TrayStyle { arrow: false, ..TrayStyle::default() },
    };
    let value = match build(Kind::Value, &content, "Genius LibreLinkUp") {
        Ok(slot) => slot,
        Err(e) => {
            tracing::warn!("system tray unavailable: {e}");
            return false;
        }
    };

    let on_action = Arc::new(on_action);
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

    TRAY.with(|t| *t.borrow_mut() = Some(Tray { value: Some(value), arrow: None }));
    true
}

/// Brings one icon up to date. On Windows a failed update means the icon is not in the
/// notification area at all: registering it fails while Explorer is still starting (autostart
/// at sign-in) and tray-icon only retries when the taskbar is re-created. The icon is then
/// built again, on every update until it succeeds.
fn sync_slot(slot: &mut Option<Slot>, kind: Kind, content: &TrayContent, tooltip: &str, wanted: bool) {
    if !wanted {
        *slot = None;
        return;
    }
    let size = trayicon::tray_size();
    let mut failed = false;
    if let Some(s) = slot.as_mut() {
        // the size changes when the user switches display scaling
        let key = (content.clone(), size);
        if s.last.as_ref() != Some(&key) {
            match s.icon.set_icon(make_icon(kind, content, size)) {
                Ok(()) => s.last = Some(key),
                Err(e) => {
                    tracing::warn!("tray icon update failed: {e}");
                    failed = true;
                }
            }
        }
        if let Err(e) = s.icon.set_tooltip(Some(tooltip)) {
            tracing::warn!("tray tooltip update failed: {e}");
            failed = true;
        }
    }
    if slot.is_none() || (failed && cfg!(windows)) {
        // drop first: the old icon must be unregistered before the new one is added
        *slot = None;
        match build(kind, content, tooltip) {
            Ok(s) => *slot = Some(s),
            Err(e) => tracing::warn!("could not create the tray icon: {e}"),
        }
    }
}

pub fn update(content: TrayContent, tooltip: &str) {
    TRAY.with(|t| {
        let mut t = t.borrow_mut();
        let Some(tray) = t.as_mut() else { return };
        let separate = content.separate_arrow();
        let mut value_content = content.clone();
        if separate {
            // the arrow has its own icon, the number gets the full width
            value_content.style.arrow = false;
        }
        sync_slot(&mut tray.value, Kind::Value, &value_content, tooltip, true);
        sync_slot(&mut tray.arrow, Kind::Arrow, &content, tooltip, separate);
    });
}

pub fn shutdown() {
    TRAY.with(|t| t.borrow_mut().take());
}
