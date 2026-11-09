//! Tray icon + menu construction. The menu is rebuilt whenever the discovered
//! device list or receiver state changes; each rebuild is a fresh
//! `tray_icon::menu::Menu` swapped onto the existing tray icon.

use std::time::{Duration, Instant};

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
const SEND_FRAME_2_PNG: &[u8] = include_bytes!("../assets/send-frame-2.png");
const SEND_FRAME_3_PNG: &[u8] = include_bytes!("../assets/send-frame-3.png");
const RECEIVE_FRAME_2_PNG: &[u8] = include_bytes!("../assets/receive-frame-2.png");
const RECEIVE_FRAME_3_PNG: &[u8] = include_bytes!("../assets/receive-frame-3.png");
const FRAME_INTERVAL: Duration = Duration::from_millis(200);

/// Send cycle: send-frame-2 → send-frame-3 → idle.
const SEND_FRAME_PNGS: &[&[u8]] = &[SEND_FRAME_2_PNG, SEND_FRAME_3_PNG, TRAY_ICON_PNG];

/// Receive cycle: receive-frame-2 → receive-frame-3 → idle.
const RECEIVE_FRAME_PNGS: &[&[u8]] = &[RECEIVE_FRAME_2_PNG, RECEIVE_FRAME_3_PNG, TRAY_ICON_PNG];

/// Which transfer animation to show in the menu bar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferDirection {
    Send,
    Receive,
}

/// Idle icon plus send/receive animation frame sequences.
pub struct TrayIconSet {
    idle: Icon,
    sending: Vec<Icon>,
    receiving: Vec<Icon>,
}

impl TrayIconSet {
    pub fn idle(&self) -> &Icon {
        &self.idle
    }
}

/// Drives the menu-bar animation while sends or receives are in flight.
pub struct TrayAnimator {
    icons: TrayIconSet,
    active_sends: u32,
    active_receives: u32,
    /// Full frame cycles still to play after the last transfer ends.
    cycles_remaining: u32,
    display_direction: TransferDirection,
    frame_index: usize,
    next_frame_at: Option<Instant>,
    using_transfer_icon: bool,
}

impl TrayAnimator {
    pub fn new(icons: TrayIconSet) -> Self {
        Self {
            icons,
            active_sends: 0,
            active_receives: 0,
            cycles_remaining: 0,
            display_direction: TransferDirection::Receive,
            frame_index: 0,
            next_frame_at: None,
            using_transfer_icon: false,
        }
    }

    pub fn is_animating(&self) -> bool {
        self.active_sends > 0 || self.active_receives > 0 || self.cycles_remaining > 0
    }

    pub fn begin_send(&mut self) {
        self.active_sends += 1;
        self.begin_activity(TransferDirection::Send);
    }

    pub fn begin_receive(&mut self) {
        self.active_receives += 1;
        self.begin_activity(TransferDirection::Receive);
    }

    pub fn end_send(&mut self) {
        self.active_sends = self.active_sends.saturating_sub(1);
        self.sync_direction_after_activity_change();
    }

    pub fn end_receive(&mut self) {
        self.active_receives = self.active_receives.saturating_sub(1);
        self.sync_direction_after_activity_change();
    }

    pub fn cancel_receiving(&mut self) {
        self.active_receives = 0;
        self.sync_direction_after_activity_change();
    }

    fn begin_activity(&mut self, direction: TransferDirection) {
        self.cycles_remaining = self.cycles_remaining.max(1);
        let first_activity = self.active_sends + self.active_receives == 1;
        if first_activity && !self.using_transfer_icon {
            self.display_direction = direction;
            self.frame_index = 0;
            self.next_frame_at = None;
        }
    }

    fn preferred_direction(&self) -> TransferDirection {
        if self.active_receives > 0 {
            TransferDirection::Receive
        } else if self.active_sends > 0 {
            TransferDirection::Send
        } else {
            self.display_direction
        }
    }

    fn sync_direction_after_activity_change(&mut self) {
        let preferred = self.preferred_direction();
        if preferred != self.display_direction && self.is_animating() {
            self.display_direction = preferred;
            self.frame_index = 0;
            self.next_frame_at = None;
        }
    }

    /// Advance the tray icon when due. Returns the next wake time while animating.
    pub fn tick(&mut self, tray: &TrayIcon) -> Option<Instant> {
        if !self.is_animating() {
            if self.using_transfer_icon {
                if let Err(e) = set_tray_icon(tray, self.icons.idle()) {
                    tracing::warn!("tray icon restore failed: {e}");
                }
                self.using_transfer_icon = false;
            }
            self.frame_index = 0;
            self.next_frame_at = None;
            return None;
        }

        let preferred = self.preferred_direction();
        if preferred != self.display_direction {
            self.display_direction = preferred;
            self.frame_index = 0;
            self.next_frame_at = None;
        }

        let now = Instant::now();
        if let Some(deadline) = self.next_frame_at {
            if now < deadline {
                return Some(deadline);
            }
        }

        let (frame_count, icon) = match self.display_direction {
            TransferDirection::Send => {
                let count = self.icons.sending.len().max(1);
                let icon = self
                    .icons
                    .sending
                    .get(self.frame_index)
                    .cloned()
                    .unwrap_or_else(|| self.icons.idle().clone());
                (count, icon)
            }
            TransferDirection::Receive => {
                let count = self.icons.receiving.len().max(1);
                let icon = self
                    .icons
                    .receiving
                    .get(self.frame_index)
                    .cloned()
                    .unwrap_or_else(|| self.icons.idle().clone());
                (count, icon)
            }
        };
        if let Err(e) = set_tray_icon(tray, &icon) {
            tracing::warn!("tray icon update failed: {e}");
        }
        self.using_transfer_icon = true;
        self.frame_index = (self.frame_index + 1) % frame_count;
        if self.frame_index == 0 {
            self.cycles_remaining = self.cycles_remaining.saturating_sub(1);
        }
        let deadline = now + FRAME_INTERVAL;
        self.next_frame_at = Some(deadline);
        Some(deadline)
    }
}

/// Load the idle PNG and embedded send/receive animation frames.
pub fn load_icon_set() -> Result<TrayIconSet> {
    let idle = decode_png_to_icon(TRAY_ICON_PNG, "tray icon")?;
    let expected_size = png_dimensions(TRAY_ICON_PNG)?;
    let sending = load_frame_sequence(SEND_FRAME_PNGS, "send", expected_size)?;
    let receiving = load_frame_sequence(RECEIVE_FRAME_PNGS, "receive", expected_size)?;
    Ok(TrayIconSet {
        idle,
        sending,
        receiving,
    })
}

fn load_frame_sequence(
    pngs: &[&[u8]],
    label: &str,
    expected_size: (u32, u32),
) -> Result<Vec<Icon>> {
    let mut frames = Vec::with_capacity(pngs.len());
    for (index, png) in pngs.iter().enumerate() {
        let size = png_dimensions(png)?;
        anyhow::ensure!(
            size == expected_size,
            "{label} frame {} is {}x{}, expected {}x{}",
            index + 1,
            size.0,
            size.1,
            expected_size.0,
            expected_size.1
        );
        frames.push(decode_png_to_icon(
            png,
            &format!("{label} frame {}", index + 1),
        )?);
    }
    Ok(frames)
}

/// Build a tray icon from the embedded PNG asset.
pub fn build_icon() -> Result<Icon> {
    decode_png_to_icon(TRAY_ICON_PNG, "tray icon")
}

fn png_dimensions(png: &[u8]) -> Result<(u32, u32)> {
    let image = image::load_from_memory(png).context("read png dimensions")?;
    Ok(image.dimensions())
}

fn decode_png_to_icon(png: &[u8], label: &str) -> Result<Icon> {
    let (width, height, rgba) =
        decode_tray_icon_png(png).with_context(|| format!("decode {label}"))?;
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

fn set_tray_icon(tray: &TrayIcon, icon: &Icon) -> Result<()> {
    #[cfg(target_os = "macos")]
    tray.set_icon_with_as_template(Some(icon.clone()), true)?;
    #[cfg(not(target_os = "macos"))]
    tray.set_icon(Some(icon.clone()))?;
    Ok(())
}

/// Current receiver lifecycle state as presented by the tray.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReceiverStatus {
    Starting,
    Receiving,
    Transferring,
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
        ReceiverStatus::Transferring => format!("Transferring — {alias}"),
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
        ReceiverStatus::Receiving | ReceiverStatus::Transferring => {
            menu.append(&MenuItem::with_id(
                MenuId::new(ID_PAUSE),
                "Pause receiving",
                receiver_status == ReceiverStatus::Receiving,
                None,
            ))?
        }
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

    #[test]
    fn transfer_animation_loads_send_and_receive_png_frames() {
        let icons = load_icon_set().expect("load icon set");
        assert_eq!(icons.sending.len(), SEND_FRAME_PNGS.len());
        assert_eq!(icons.receiving.len(), RECEIVE_FRAME_PNGS.len());
        let expected = png_dimensions(TRAY_ICON_PNG).expect("tray icon dimensions");
        assert_eq!(
            png_dimensions(SEND_FRAME_2_PNG).expect("send frame 2"),
            expected
        );
        assert_eq!(
            png_dimensions(RECEIVE_FRAME_2_PNG).expect("receive frame 2"),
            expected
        );
    }

    #[test]
    fn tray_animator_plays_at_least_one_cycle_after_receive_ends() {
        let icons = load_icon_set().expect("load icon set");
        let mut animator = TrayAnimator::new(icons);
        animator.begin_receive();
        assert!(animator.is_animating());
        animator.end_receive();
        assert!(
            animator.is_animating(),
            "animation should continue until one full cycle finishes"
        );
    }
}
