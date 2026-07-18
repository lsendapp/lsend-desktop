//! Tray icon + menu construction. The menu is rebuilt whenever the discovered
//! device list or receiver state changes; each rebuild is a fresh
//! `tray_icon::menu::Menu` swapped onto the existing tray icon.

use anyhow::Result;
use tray_icon::Icon;
use tray_icon::TrayIcon;
use tray_icon::menu::{Menu, MenuId, MenuItem, PredefinedMenuItem, Submenu};

use lsend_core::discovery::DiscoveredDevice;
use lsend_core::config::AppConfig;

/// Stable menu IDs. Keep these short — macOS pads visible labels by the
/// longest ID string on some systems.
const ID_RESUME: &str = "resume";
const ID_PAUSE: &str = "pause";
const ID_REFRESH: &str = "refresh";
const ID_QUIT: &str = "quit";
const ID_SEND_PREFIX: &str = "send:";

/// Build a 32×32 RGBA tray icon without an on-disk asset. The shape is a
/// filled rounded square in lsend's brand green so the icon is recognizable
/// even before we ship packaged `.icns`/`.ico` files.
pub fn build_icon() -> Result<Icon> {
    const S: u32 = 32;
    let mut rgba = Vec::with_capacity((S as usize) * (S as usize) * 4);

    // Brand green (#16A34A) outer, lighter inner ring for contrast against both
    // light and dark menu bars.
    let outer = [0x16u8, 0xA3, 0x4A, 0xFF];
    let inner = [0x4Au8, 0xD0, 0x78, 0xFF];
    let transparent = [0u8, 0, 0, 0];

    for y in 0..S {
        for x in 0..S {
            // Rounded corner mask: drop corner pixels where the nearest edge
            // sum is < 2 from both axes.
            let in_corner = (x < 2 || x >= S - 2) && (y < 2 || y >= S - 2);
            let corner_diag = (x.min(S - 1 - x) as i32 + y.min(S - 1 - y) as i32) < 2;
            let color = if in_corner && corner_diag {
                transparent
            } else if (4..S - 4).contains(&x) && (4..S - 4).contains(&y) {
                inner
            } else {
                outer
            };
            rgba.extend_from_slice(&color);
        }
    }

    Ok(Icon::from_rgba(rgba, S, S)?)
}

/// Build the full tray menu for the current state.
///
/// Returns `tray_icon::menu::Result` so the inner `?` operators bind to muda's
/// own error type directly; anyhoo's `?` at the main-loop call site converts
/// smoothly because `tray_icon::menu::Error` implements `std::error::Error`.
///
/// `alias` is the LocalSend device display name (peer-visible). It appears
/// in the top status line so the menu itself tells the user *whose* tray
/// this is — keeping it in sync with the tooltip set in `main.rs`.
pub fn build_menu(
    devices: &[DiscoveredDevice],
    receiving: bool,
    alias: &str,
) -> tray_icon::menu::Result<Menu> {
    let menu = Menu::new();

    // Status line: disabled item reflecting current state. The first line
    // identifies the device (the LocalSend alias peers see); the second
    // is the receiver port / paused indicator.
    let status_text = if receiving {
        format!("Receiving — {alias} · port 53317")
    } else {
        format!("Paused — {alias} (not receiving)")
    };
    let status = MenuItem::new(status_text, false, None);
    menu.append(&status)?;
    menu.append(&PredefinedMenuItem::separator())?;

    // "Send file to…" submenu: one entry per discovered device.
    let send_submenu = Submenu::new("Send file to…", true);
    if devices.is_empty() {
        send_submenu.append(&MenuItem::new(
            "(no devices nearby — refresh)",
            false,
            None,
        ))?;
    } else {
        for (i, d) in devices.iter().enumerate() {
            let label = format!("{}  ({}{})", d.alias, d.ip, port_suffix(d));
            send_submenu.append(&MenuItem::with_id(
                MenuId::new(format!("{ID_SEND_PREFIX}{i}")),
                label,
                true,
                None,
            ))?;
        }
    }
    send_submenu.append(&PredefinedMenuItem::separator())?;
    send_submenu.append(&MenuItem::with_id(
        MenuId::new(ID_REFRESH),
        "Refresh devices",
        true,
        None,
    ))?;
    menu.append(&send_submenu)?;

    menu.append(&PredefinedMenuItem::separator())?;

    if receiving {
        menu.append(&MenuItem::with_id(
            MenuId::new(ID_PAUSE),
            "Pause receiving",
            true,
            None,
        ))?;
    } else {
        menu.append(&MenuItem::with_id(
            MenuId::new(ID_RESUME),
            "Resume receiving",
            true,
            None,
        ))?;
    }
    menu.append(&MenuItem::with_id(
        MenuId::new(ID_REFRESH),
        "Refresh devices",
        true,
        None,
    ))?;
    menu.append(&PredefinedMenuItem::separator())?;
    menu.append(&MenuItem::with_id(MenuId::new(ID_QUIT), "Quit lsend", true, None))?;

    Ok(menu)
}

fn port_suffix(d: &DiscoveredDevice) -> String {
    // Default port 53317 is boring to show; only print non-default.
    if d.port == AppConfig::DEFAULT_PORT {
        String::new()
    } else {
        format!(":{}", d.port)
    }
}

/// A parsed tray menu action received from muda's event channel.
#[derive(Debug, Clone)]
pub enum TrayAction {
    /// Send-to-device index in the current device list.
    Send(usize),
    Refresh,
    Pause,
    Resume,
    Quit,
    /// Some other/unknown id (muda fires events for items we did not build;
    /// we just ignore them).
    Ignored,
}

pub fn parse_action(id: &str) -> TrayAction {
    match id {
        ID_PAUSE => TrayAction::Pause,
        ID_RESUME => TrayAction::Resume,
        ID_REFRESH => TrayAction::Refresh,
        ID_QUIT => TrayAction::Quit,
        other if other.starts_with(ID_SEND_PREFIX) => {
            other[ID_SEND_PREFIX.len()..]
                .parse::<usize>()
                .map(TrayAction::Send)
                .unwrap_or(TrayAction::Ignored)
        }
        _ => TrayAction::Ignored,
    }
}

/// Apply the menu to an existing tray icon. Swaps the menu atomically. Menu
/// is moved into a `Box<dyn ContextMenu>` as required by `set_menu`.
pub fn set_tray_menu(tray: &TrayIcon, menu: Menu) {
    tray.set_menu(Some(Box::new(menu)));
}