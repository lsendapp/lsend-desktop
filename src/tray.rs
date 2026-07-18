//! Tray icon + menu construction. The menu is rebuilt whenever the discovered
//! device list or receiver state changes; each rebuild is a fresh
//! `tray_icon::menu::Menu` swapped onto the existing tray icon.

use anyhow::{Context, Result};
use image::GenericImageView;
use tray_icon::Icon;
use tray_icon::TrayIcon;
use tray_icon::menu::{Menu, MenuId, MenuItem, PredefinedMenuItem, Submenu};

use crate::APP_DISPLAY_NAME;
use lsend_core::config::AppConfig;
use lsend_core::discovery::DiscoveredDevice;

/// Stable menu IDs. Keep these short — macOS pads visible labels by the
/// longest ID string on some systems.
const ID_RESUME: &str = "resume";
const ID_PAUSE: &str = "pause";
const ID_REFRESH: &str = "refresh";
const ID_QUIT: &str = "quit";
const ID_COPY_LAST_MESSAGE: &str = "copy-last-message";
const ID_SEND_FILE_PREFIX: &str = "send-file:";
const ID_SEND_TEXT_PREFIX: &str = "send-text:";

const TRAY_ICON_PNG: &[u8] = include_bytes!("../assets/tray-icon.png");

/// Build a tray icon from the embedded PNG asset.
pub fn build_icon() -> Result<Icon> {
    build_icon_template()
}

/// macOS menu bar icons must be monochrome template images (alpha mask only).
fn build_icon_template() -> Result<Icon> {
    let (width, height, rgba) = decode_tray_icon_png(TRAY_ICON_PNG)?;
    Ok(Icon::from_rgba(rgba, width, height)?)
}

fn decode_tray_icon_png(png: &[u8]) -> Result<(u32, u32, Vec<u8>)> {
    let image = image::load_from_memory(png).context("decode tray icon PNG")?;
    let (width, height) = image.dimensions();
    let mut rgba = image.to_rgba8().into_raw();

    for chunk in rgba.chunks_exact_mut(4) {
        if chunk[3] > 0 {
            chunk[0] = 0;
            chunk[1] = 0;
            chunk[2] = 0;
            chunk[3] = 0xFF;
        }
    }

    Ok((width, height, rgba))
}

/// Current receiver lifecycle state as presented by the tray.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReceiverStatus {
    Starting,
    Receiving,
    Stopping,
    Paused,
    Error,
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
    receiver_status: ReceiverStatus,
    alias: &str,
    has_last_message: bool,
) -> tray_icon::menu::Result<Menu> {
    let menu = Menu::new();

    // Status line: disabled item reflecting current state. The first line
    // identifies the device (the LocalSend alias peers see); the second
    // is the receiver port / paused indicator.
    let status_text = match receiver_status {
        ReceiverStatus::Starting => format!("Starting — {alias}"),
        ReceiverStatus::Receiving => format!("Receiving — {alias} · port 53317"),
        ReceiverStatus::Stopping => format!("Pausing — {alias}"),
        ReceiverStatus::Paused => format!("Paused — {alias} (not receiving)"),
        ReceiverStatus::Error => format!("Receiver error — {alias}"),
    };
    let status = MenuItem::new(status_text, false, None);
    menu.append(&status)?;
    menu.append(&PredefinedMenuItem::separator())?;

    // Send submenus: one entry per discovered device.
    let send_file_submenu = Submenu::new("Send file to…", true);
    let send_text_submenu = Submenu::new("Send text to…", true);
    if devices.is_empty() {
        let empty_label = "(no devices nearby)";
        send_file_submenu.append(&MenuItem::new(empty_label, false, None))?;
        send_text_submenu.append(&MenuItem::new(empty_label, false, None))?;
    } else {
        for d in devices {
            let label = format!("{}  ({}{})", d.alias, d.ip, port_suffix(d));
            let key = device_key(d);
            send_file_submenu.append(&MenuItem::with_id(
                MenuId::new(format!("{ID_SEND_FILE_PREFIX}{key}")),
                label.clone(),
                true,
                None,
            ))?;
            send_text_submenu.append(&MenuItem::with_id(
                MenuId::new(format!("{ID_SEND_TEXT_PREFIX}{key}")),
                label,
                true,
                None,
            ))?;
        }
    }
    menu.append(&send_file_submenu)?;
    menu.append(&send_text_submenu)?;
    menu.append(&MenuItem::with_id(
        MenuId::new(ID_REFRESH),
        "Refresh devices",
        true,
        None,
    ))?;

    menu.append(&PredefinedMenuItem::separator())?;
    menu.append(&MenuItem::with_id(
        MenuId::new(ID_COPY_LAST_MESSAGE),
        "Copy last received message",
        has_last_message,
        None,
    ))?;

    match receiver_status {
        ReceiverStatus::Receiving => menu.append(&MenuItem::with_id(
            MenuId::new(ID_PAUSE),
            "Pause receiving",
            true,
            None,
        ))?,
        ReceiverStatus::Paused | ReceiverStatus::Error => menu.append(&MenuItem::with_id(
            MenuId::new(ID_RESUME),
            "Resume receiving",
            true,
            None,
        ))?,
        ReceiverStatus::Starting => {
            menu.append(&MenuItem::new("Starting receiver…", false, None))?
        }
        ReceiverStatus::Stopping => {
            menu.append(&MenuItem::new("Pausing receiver…", false, None))?
        }
    }
    menu.append(&PredefinedMenuItem::separator())?;
    let quit_label = format!("Quit {APP_DISPLAY_NAME}");
    menu.append(&MenuItem::with_id(
        MenuId::new(ID_QUIT),
        &quit_label,
        true,
        None,
    ))?;

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

/// Stable identity used by menu actions. Unlike an array index, this still
/// identifies the same peer if a scan reorders or replaces the device cache.
pub fn device_key(d: &DiscoveredDevice) -> String {
    format!("{}:{}:{}", d.ip, d.port, d.fingerprint)
}

/// A parsed tray menu action received from muda's event channel.
#[derive(Debug, Clone)]
pub enum TrayAction {
    /// Stable identity of the selected peer.
    SendFile(String),
    SendText(String),
    CopyLastMessage,
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
        ID_COPY_LAST_MESSAGE => TrayAction::CopyLastMessage,
        other if other.starts_with(ID_SEND_FILE_PREFIX) => {
            TrayAction::SendFile(other[ID_SEND_FILE_PREFIX.len()..].to_owned())
        }
        other if other.starts_with(ID_SEND_TEXT_PREFIX) => {
            TrayAction::SendText(other[ID_SEND_TEXT_PREFIX.len()..].to_owned())
        }
        _ => TrayAction::Ignored,
    }
}

/// Apply the menu to an existing tray icon. Swaps the menu atomically. Menu
/// is moved into a `Box<dyn ContextMenu>` as required by `set_menu`.
pub fn set_tray_menu(tray: &TrayIcon, menu: Menu) {
    tray.set_menu(Some(Box::new(menu)));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device(ip: &str, port: u16, fingerprint: &str) -> DiscoveredDevice {
        DiscoveredDevice {
            alias: "Peer".to_owned(),
            ip: ip.to_owned(),
            port,
            fingerprint: fingerprint.to_owned(),
            https: true,
            version: "2.1".to_owned(),
            device_type: None,
            device_model: None,
        }
    }

    #[test]
    fn send_action_round_trips_stable_device_key() {
        let peer = device("192.168.1.20", 53317, "abc123");
        let key = device_key(&peer);

        match parse_action(&format!("{ID_SEND_FILE_PREFIX}{key}")) {
            TrayAction::SendFile(parsed_key) => assert_eq!(parsed_key, key),
            action => panic!("unexpected action: {action:?}"),
        }

        match parse_action(&format!("{ID_SEND_TEXT_PREFIX}{key}")) {
            TrayAction::SendText(parsed_key) => assert_eq!(parsed_key, key),
            action => panic!("unexpected action: {action:?}"),
        }
    }

    #[test]
    fn device_key_distinguishes_endpoint_and_identity() {
        let first = device("192.168.1.20", 53317, "abc123");
        let different_port = device("192.168.1.20", 53318, "abc123");
        let different_identity = device("192.168.1.20", 53317, "def456");

        assert_ne!(device_key(&first), device_key(&different_port));
        assert_ne!(device_key(&first), device_key(&different_identity));
    }

    #[test]
    fn copy_last_message_action_is_parsed() {
        assert!(matches!(
            parse_action(ID_COPY_LAST_MESSAGE),
            TrayAction::CopyLastMessage
        ));
    }

    #[test]
    fn tray_icon_loads_embedded_png() {
        let (width, height, rgba) =
            decode_tray_icon_png(TRAY_ICON_PNG).expect("decode embedded tray icon PNG");
        assert_eq!((width, height), (128, 128));
        assert!(
            rgba.chunks_exact(4).any(|pixel| pixel[3] == 0xFF),
            "expected opaque pixels in tray icon"
        );
        assert!(
            rgba.chunks_exact(4).any(|pixel| pixel[3] == 0),
            "expected transparent pixels in tray icon"
        );
    }
}
