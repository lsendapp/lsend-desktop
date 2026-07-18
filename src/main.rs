//! lsend desktop — tray-only LocalSend client (v1).
//!
//! No main window. The receiver is resident (started at launch, never tied to
//! a UI tab); sends are initiated from the tray menu's "Send file to…"
//! submenu via a native file picker. macOS menu bar / Windows system tray.
//!
//! Threading layout:
//! - **Main thread**: tao event loop + tray icon. m02/// │ muda menu events arrive
//!   here via a global channel; UI events from background tasks arrive here via
//!   the tao `EventLoopProxy`.
//! - **Background tokio runtime**: receiver supervisor + scanner + send tasks.
//!   They never touch the tray directly — they `send_event` back to main.

mod receiver;
mod scanner;
mod state;
mod tray;

use std::{cell::RefCell, path::Path, rc::Rc, sync::Arc};

use anyhow::Result;
use notify_rust::Notification;
use parking_lot::Mutex;
use tao::{
    event_loop::{ControlFlow, EventLoop, EventLoopBuilder},
    platform::macos::{ActivationPolicy, EventLoopExtMacOS},
};
use tokio::runtime::Runtime;
use tracing::info;
use tracing_subscriber::EnvFilter;
use tray_icon::TrayIconBuilder;
use tray_icon::menu::MenuEvent;

use lsend_core::discovery::DiscoveredDevice;
use lsend_core::events::ReceiveEvent;

use crate::receiver::ReceiverHandle;
use crate::state::{AppState, UiEvent};
use crate::tray::{ReceiverStatus, TrayAction};

/// User-facing product name in tray UI, notifications, and the macOS bundle.
pub const APP_DISPLAY_NAME: &str = "LSend";
#[cfg(target_os = "macos")]
const MACOS_BUNDLE_ID: &str = "app.lsend.desktop";

/// The LocalSend device display name (alias). Surfaced in the tray tooltip so
/// users running multiple `lsend-desktop` instances — or one alongside the
/// official LocalSend app — can tell whose tray icon this is at a glance.
///
/// Resolved once at startup from `AppConfig::alias` (which itself reads the
/// persisted `alias.txt` under `~/.config/lsend/`). If the alias changes at
/// runtime (`lsend alias set ...`) the change does not take effect until the
/// app is restarted; LocalSend peers likewise re-discover on restart.
/// Truncation: tray-icon tooltips are typically clipped at ~64 chars on
/// macOS/Windows; the alias is small enough that we don't worry.
fn tray_tooltip_text(alias: &str) -> String {
    format!("{alias} · {APP_DISPLAY_NAME}")
}

fn main() -> Result<()> {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("lsend=info")),
        )
        .try_init();

    configure_notifications();
    let app = AppState::load()?;

    // Build tao event loop with a custom user-event type so background tasks
    // can wake the loop via the EventLoopProxy.
    let mut event_loop_builder = EventLoopBuilder::<UiEvent>::with_user_event();
    let _ = &mut event_loop_builder; // keep borrow live until build
    let mut event_loop: EventLoop<UiEvent> = event_loop_builder.build();
    // Tray-only: hide from the macOS Dock, don't grab focus on launch.
    // (No-op on Windows / Linux.)
    event_loop.set_activation_policy(ActivationPolicy::Accessory);
    let proxy = event_loop.create_proxy();

    // Background tokio runtime. Lives for the whole process; receiver,
    // scanner, and ad-hoc send tasks all run on it.
    let rt = Runtime::new()?;
    let rt_handle = rt.handle().clone();
    // Spawn resident receiver + background scanner.
    let mut rx_handle: Option<ReceiverHandle> = Some(receiver::spawn(
        rt_handle.clone(),
        app.config.clone(),
        app.identity.clone(),
        proxy.clone(),
    ));

    let scanner_handle = scanner::spawn(
        rt_handle.clone(),
        app.config.clone(),
        app.identity.clone(),
        app.devices.clone(),
        proxy.clone(),
    );
    // Reusable pieces for the closure.
    let icon = tray::build_icon()?;
    let initial_menu = tray::build_menu(&[], ReceiverStatus::Starting, &app.config.alias, false)?;
    // Tooltip: "{alias} · LSend" — the alias is the LocalSend device name
    // peers will see and the user can recognise across multiple instances.
    let tooltip = tray_tooltip_text(&app.config.alias);
    let mut tray_builder = TrayIconBuilder::new()
        .with_menu(Box::new(initial_menu))
        .with_tooltip(tooltip)
        .with_icon(icon);
    #[cfg(target_os = "macos")]
    {
        tray_builder = tray_builder.with_icon_as_template(true);
    }
    let tray = tray_builder.build()?;
    let tray = Rc::new(RefCell::new(tray));
    // Reusable pieces for the closure.
    let devices_ref = app.devices.clone();
    let last_message_ref = app.last_message.clone();
    let config_ref = app.config.clone();
    let identity_ref = app.identity.clone();
    let alias_ref = app.config.alias.clone();
    let tray_ref = tray.clone();

    let mut receiver_status = ReceiverStatus::Starting;
    let mut receive_notify_state = ReceiveNotifyState::default();

    event_loop.run(move |event, target, control_flow| {
        *control_flow = ControlFlow::Wait;
        // 1) Tray menu events (muda global channel).
        if let Ok(menu_event) = MenuEvent::receiver().try_recv() {
            let id = menu_event.id.0.as_str().to_string();
            let action = tray::parse_action(&id);
            match action {
                TrayAction::SendFile(device_key) => {
                    handle_send_file(
                        &rt_handle,
                        config_ref.clone(),
                        identity_ref.clone(),
                        &devices_ref,
                        &device_key,
                        proxy.clone(),
                    );
                }
                TrayAction::SendText(device_key) => {
                    handle_send_text(
                        &rt_handle,
                        config_ref.clone(),
                        identity_ref.clone(),
                        &devices_ref,
                        &device_key,
                        proxy.clone(),
                    );
                }
                TrayAction::CopyLastMessage => {
                    copy_last_message(&last_message_ref);
                }
                TrayAction::Refresh => {
                    scanner_handle.refresh();
                    info!("immediate device refresh requested");
                }
                TrayAction::Pause => {
                    if let Some(h) = rx_handle.take() {
                        let stopped_proxy = proxy.clone();
                        rt_handle.spawn(async move {
                            h.stop().await;
                            let _ = stopped_proxy.send_event(UiEvent::ReceiverStopped);
                        });
                        receiver_status = ReceiverStatus::Stopping;
                        rebuild_menu(
                            &tray_ref,
                            &devices_ref,
                            &last_message_ref,
                            receiver_status,
                            &alias_ref,
                        );
                    }
                }
                TrayAction::Resume => {
                    if rx_handle.is_none()
                        && matches!(
                            receiver_status,
                            ReceiverStatus::Paused | ReceiverStatus::Error
                        )
                    {
                        rx_handle = Some(receiver::spawn(
                            rt_handle.clone(),
                            config_ref.clone(),
                            identity_ref.clone(),
                            proxy.clone(),
                        ));
                        receiver_status = ReceiverStatus::Starting;
                        rebuild_menu(
                            &tray_ref,
                            &devices_ref,
                            &last_message_ref,
                            receiver_status,
                            &alias_ref,
                        );
                    }
                }
                TrayAction::Quit => {
                    info!("quit requested");
                    if let Some(h) = rx_handle.take() {
                        rt_handle.spawn(async move { h.stop().await });
                    }
                    scanner_handle.abort();
                    // Break the event loop; tao's `run` then unwinds.
                    *control_flow = ControlFlow::Exit;
                }
                TrayAction::Ignored => {}
            }
        }

        // 2) User events from background tasks.
        if let tao::event::Event::UserEvent(ui) = event {
            match ui {
                UiEvent::Receive(ev) => {
                    let mut rebuild = false;
                    if matches!(&ev, ReceiveEvent::Ready { .. }) {
                        receiver_status = ReceiverStatus::Receiving;
                        rebuild = true;
                    }
                    if let ReceiveEvent::MessageReceived { text, .. } = &ev {
                        let mut guard = last_message_ref.lock();
                        *guard = Some(text.clone());
                        drop(guard);
                        rebuild = true;
                    }
                    if rebuild {
                        rebuild_menu(
                            &tray_ref,
                            &devices_ref,
                            &last_message_ref,
                            receiver_status,
                            &alias_ref,
                        );
                    }
                    handle_receive_event(
                        ev,
                        &mut receive_notify_state,
                        &config_ref.receive_dir,
                    );
                }
                UiEvent::DevicesRefreshed { .. } => {
                    rebuild_menu(
                        &tray_ref,
                        &devices_ref,
                        &last_message_ref,
                        receiver_status,
                        &alias_ref,
                    );
                }
                UiEvent::ReceiverError(msg) => {
                    rx_handle.take();
                    receiver_status = ReceiverStatus::Error;
                    rebuild_menu(
                        &tray_ref,
                        &devices_ref,
                        &last_message_ref,
                        receiver_status,
                        &alias_ref,
                    );
                    notify_error(&msg);
                }
                UiEvent::ReceiverStopped => {
                    receiver_status = ReceiverStatus::Paused;
                    rebuild_menu(
                        &tray_ref,
                        &devices_ref,
                        &last_message_ref,
                        receiver_status,
                        &alias_ref,
                    );
                    notify_status("Receiver paused");
                }
                UiEvent::SendResult(res) => match res {
                    Ok(summary) => notify_status(&format!("Sent {summary}")),
                    Err(msg) => notify_error(&format!("Send failed: {msg}")),
                },
            }
        }

        // Suppress unused-warning on `target` while still allowing future
        // EventLoopWindowTarget queries (e.g. hide_application on macOS).
        let _ = target;
    })
    // tao 0.32 `run` returns `!`; the closure's last expression is `ControlFlow`,
    // not a return. The `!` of `run` coerces to `Result<()>` at the boundary
    // of `main`, so there is no explicit `Ok(())` here — adding one would be
    // unreachable dead code.
}

#[cfg(target_os = "macos")]
fn configure_notifications() {
    if let Err(e) = notify_rust::set_application(MACOS_BUNDLE_ID) {
        tracing::warn!("could not register notification application: {e}");
    }
}

#[cfg(not(target_os = "macos"))]
fn configure_notifications() {}

/// Rebuild the tray menu from the current device cache and receiver status.
/// Runs on the main thread; the devices lock is held briefly.
fn rebuild_menu(
    tray: &Rc<RefCell<tray_icon::TrayIcon>>,
    devices: &Arc<Mutex<Vec<DiscoveredDevice>>>,
    last_message: &Arc<Mutex<Option<String>>>,
    receiver_status: ReceiverStatus,
    alias: &str,
) {
    let has_last_message = last_message.lock().is_some();
    let devices_guard = devices.lock();
    let menu = match tray::build_menu(&devices_guard, receiver_status, alias, has_last_message) {
        Ok(m) => m,
        Err(e) => {
            tracing::error!("tray menu build failed: {e}");
            return;
        }
    };
    drop(devices_guard);
    let t = tray.borrow();
    tray::set_tray_menu(&t, menu);
}

#[derive(Default)]
struct ReceiveNotifyState {
    sender_alias: Option<String>,
    saved_files: Vec<String>,
    message_notified: bool,
}

impl ReceiveNotifyState {
    fn reset(&mut self) {
        self.sender_alias = None;
        self.saved_files.clear();
        self.message_notified = false;
    }

    fn begin_file_transfer(&mut self, sender_alias: String) {
        self.reset();
        self.sender_alias = Some(sender_alias);
    }
}

fn message_preview(text: &str) -> String {
    if text.chars().count() > 160 {
        format!("{}…", text.chars().take(160).collect::<String>())
    } else {
        text.to_string()
    }
}

fn format_saved_files(files: &[String]) -> String {
    match files.len() {
        0 => String::new(),
        1 => files[0].clone(),
        2 => format!("{} and {}", files[0], files[1]),
        3 => format!("{}, {}, and {}", files[0], files[1], files[2]),
        n => format!("{}, {}, and {} more", files[0], files[1], n - 2),
    }
}

fn handle_receive_event(
    ev: ReceiveEvent,
    notify_state: &mut ReceiveNotifyState,
    receive_dir: &Path,
) {
    match ev {
        ReceiveEvent::Ready { .. } => {
            notify_status("Receiver ready on port 53317");
        }
        ReceiveEvent::TransferStarted { sender_alias, .. } => {
            notify_state.begin_file_transfer(sender_alias);
        }
        ReceiveEvent::MessageReceived {
            sender_alias, text, ..
        } => {
            let preview = message_preview(&text);
            notify_block(
                &format!("Message from {sender_alias}"),
                &format!("{preview}\nUse the tray menu to copy the full message."),
            );
            notify_state.message_notified = true;
            notify_state.sender_alias = Some(sender_alias);
        }
        ReceiveEvent::FileSaved { file_name, .. } => {
            notify_state.saved_files.push(file_name);
        }
        ReceiveEvent::TransferComplete => {
            if notify_state.message_notified {
                notify_state.reset();
                return;
            }
            if let Some(sender_alias) = notify_state.sender_alias.take() {
                let files = std::mem::take(&mut notify_state.saved_files);
                if !files.is_empty() {
                    notify_block(
                        &format!("File(s) from {sender_alias}"),
                        &format!(
                            "{}\nCheck {} for the saved file(s).",
                            format_saved_files(&files),
                            receive_dir.display(),
                        ),
                    );
                }
            }
            notify_state.reset();
        }
        ReceiveEvent::TransferFinishedWithErrors => {
            if notify_state.message_notified {
                notify_state.reset();
                return;
            }
            let sender_alias = notify_state
                .sender_alias
                .take()
                .unwrap_or_else(|| "Unknown".to_string());
            let files = std::mem::take(&mut notify_state.saved_files);
            let body = if files.is_empty() {
                "Some files could not be saved.".to_string()
            } else {
                format!(
                    "{}\nSome files could not be saved. Check {} for saved file(s).",
                    format_saved_files(&files),
                    receive_dir.display(),
                )
            };
            notify_block(&format!("File(s) from {sender_alias}"), &body);
            notify_state.reset();
        }
        ReceiveEvent::TransferCancelled => {
            notify_state.reset();
        }
        ReceiveEvent::Shutdown => {
            // Supervisor will surface any error separately via ReceiverError.
        }
    }
}

/// Synchronous native file picker (NSOpenPanel on macOS, GetOpenFileName on
/// Windows), then spawn the send. Blocking the tao loop on a modal picker is
/// fine on both platforms — there's no window to block.
fn handle_send_file(
    rt: &tokio::runtime::Handle,
    config: Arc<lsend_core::config::AppConfig>,
    identity: Arc<lsend_core::identity::Identity>,
    devices: &Arc<Mutex<Vec<DiscoveredDevice>>>,
    device_key: &str,
    proxy: tao::event_loop::EventLoopProxy<UiEvent>,
) {
    let Some(device) = find_device(devices, device_key) else {
        report_missing_device(&proxy);
        return;
    };

    let files = rfd::FileDialog::new()
        .set_title(format!("Send file(s) to {}", device.alias))
        .pick_files();

    let Some(paths) = files else {
        // User cancelled the picker — silently no-op.
        return;
    };

    let path_strings: Vec<String> = paths
        .into_iter()
        .map(|p| p.to_string_lossy().to_string())
        .collect();
    if path_strings.is_empty() {
        return;
    }

    let config2 = config;
    let identity2 = identity;
    let device_label = device.alias.clone();
    let device_ip = device.ip.clone();
    let count = path_strings.len();
    let proxy = proxy;

    rt.spawn(async move {
        let result = lsend_core::send::send_files(
            &config2,
            &identity2,
            &device_ip,
            &path_strings,
            /* text_stdin */ false,
            /* message */ None,
            /* clipboard */ false,
            /* pin */ None,
            /* no_scan */ true,
            lsend_core::output::OutputOptions::new(false, true),
        )
        .await;

        let payload = match result {
            Ok(_) => Ok(format!("{} file(s) to {}", count, device_label)),
            Err(e) => Err(format!("{e:#}")),
        };
        let _ = proxy.send_event(UiEvent::SendResult(payload));
    });
}

fn handle_send_text(
    rt: &tokio::runtime::Handle,
    config: Arc<lsend_core::config::AppConfig>,
    identity: Arc<lsend_core::identity::Identity>,
    devices: &Arc<Mutex<Vec<DiscoveredDevice>>>,
    device_key: &str,
    proxy: tao::event_loop::EventLoopProxy<UiEvent>,
) {
    let Some(device) = find_device(devices, device_key) else {
        report_missing_device(&proxy);
        return;
    };

    let Some(text) = tinyfiledialogs::input_box(
        &format!("Send text to {}", device.alias),
        "Enter the message to send:",
        "",
    ) else {
        return;
    };
    if text.is_empty() {
        let _ = proxy.send_event(UiEvent::SendResult(Err(
            "Message must not be empty.".to_string()
        )));
        return;
    }

    rt.spawn(async move {
        let result = lsend_core::send::send_files(
            &config,
            &identity,
            &device.ip,
            &[],
            /* text_stdin */ false,
            /* message */ Some(&text),
            /* clipboard */ false,
            /* pin */ None,
            /* no_scan */ true,
            lsend_core::output::OutputOptions::new(false, true),
        )
        .await;

        let payload = match result {
            Ok(_) => Ok(format!("message to {}", device.alias)),
            Err(e) => Err(format!("{e:#}")),
        };
        let _ = proxy.send_event(UiEvent::SendResult(payload));
    });
}

fn find_device(
    devices: &Arc<Mutex<Vec<DiscoveredDevice>>>,
    device_key: &str,
) -> Option<DiscoveredDevice> {
    let guard = devices.lock();
    guard
        .iter()
        .find(|device| tray::device_key(device) == device_key)
        .cloned()
}

fn report_missing_device(proxy: &tao::event_loop::EventLoopProxy<UiEvent>) {
    let _ = proxy.send_event(UiEvent::SendResult(Err(
        "Selected device is no longer available.".to_string(),
    )));
}

fn copy_last_message(last_message: &Arc<Mutex<Option<String>>>) {
    let text = last_message.lock().clone();
    let Some(text) = text else {
        notify_status("No received message is available to copy.");
        return;
    };

    match arboard::Clipboard::new().and_then(|mut clipboard| clipboard.set_text(text)) {
        Ok(()) => notify_status("Copied the last received message."),
        Err(e) => notify_error(&format!("Could not copy message: {e}")),
    }
}

fn notify_status(body: &str) {
    if let Err(e) = Notification::new()
        .summary(APP_DISPLAY_NAME)
        .body(body)
        .show()
    {
        tracing::warn!("notification failed: {e}");
    }
}

fn notify_block(summary: &str, body: &str) {
    if let Err(e) = Notification::new().summary(summary).body(body).show() {
        tracing::warn!("notification failed: {e}");
    }
}

fn notify_error(body: &str) {
    if let Err(e) = Notification::new()
        .summary("LSend — error")
        .body(body)
        .show()
    {
        tracing::warn!("notification failed: {e}");
    }
}
