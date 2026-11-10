//! lsend desktop — tray-only LocalSend client (v1).
//!
//! No main window. The receiver is resident (started at launch, never tied to
//! a UI tab); sends are initiated from the tray menu's "Send file to…"
//! submenu via a native file picker. Platform integrations are cfg-gated:
//! macOS runs as a menu-bar app (built and shipped); the tray/notification
//! code for other desktops compiles behind cfg flags but is not built or
//! tested here yet.
//!
//! Threading layout:
//! - **Main thread**: tao event loop + tray icon. Menu actions and background
//!   updates wake this loop through the tao `EventLoopProxy`.
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
use tao::event_loop::{ControlFlow, EventLoop, EventLoopBuilder};
#[cfg(target_os = "macos")]
use tao::platform::macos::{ActivationPolicy, EventLoopExtMacOS};
use tokio::runtime::Runtime;
use tracing::info;
use tracing_subscriber::EnvFilter;
use tray_icon::TrayIconBuilder;
use tray_icon::menu::MenuEvent;

use lsend_core::discovery::DiscoveredDevice;
use lsend_core::events::ReceiveEvent;

use crate::receiver::ReceiverHandle;
use crate::state::{AppState, UiEvent};
use crate::tray::{ReceiverStatus, TrayAction, TrayAnimator};

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
    #[cfg_attr(not(target_os = "macos"), allow(unused_mut))]
    let mut event_loop: EventLoop<UiEvent> = event_loop_builder.build();
    // Tray-only: hide from the macOS Dock, don't grab focus on launch.
    // (macOS-only API; other platforms have no equivalent to set.)
    #[cfg(target_os = "macos")]
    event_loop.set_activation_policy(ActivationPolicy::Accessory);
    let proxy = event_loop.create_proxy();
    let menu_proxy = proxy.clone();
    MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
        let action = tray::parse_action(&event.id.0);
        let _ = menu_proxy.send_event(UiEvent::MenuAction(action));
    }));

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
    let icon_set = tray::load_icon_set()?;
    let icon = icon_set.idle().clone();
    let mut tray_anim = TrayAnimator::new(icon_set);
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
    let mut quit_requested = false;

    event_loop.run(move |event, target, control_flow| {
        *control_flow = ControlFlow::Wait;
        // Menu actions are delivered as user events instead of polling a
        // separate channel that cannot wake ControlFlow::Wait.
        if let tao::event::Event::UserEvent(UiEvent::MenuAction(action)) = &event {
            let action = if quit_requested {
                TrayAction::Ignored
            } else {
                action.clone()
            };
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
                            &tray_anim,
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
                            &tray_anim,
                        );
                    }
                }
                TrayAction::Quit => {
                    info!("quit requested");
                    quit_requested = true;
                    scanner_handle.abort();
                    if let Some(h) = rx_handle.take() {
                        let stopped_proxy = proxy.clone();
                        rt_handle.spawn(async move {
                            h.stop().await;
                            let _ = stopped_proxy.send_event(UiEvent::ReceiverStopped);
                        });
                        receiver_status = ReceiverStatus::Stopping;
                    } else if receiver_status != ReceiverStatus::Stopping {
                        std::process::exit(0);
                    }
                    rebuild_menu(
                        &tray_ref,
                        &devices_ref,
                        &last_message_ref,
                        receiver_status,
                        &alias_ref,
                        &tray_anim,
                    );
                }
                TrayAction::Ignored => {}
            }
        }

        // 2) User events from background tasks.
        if let tao::event::Event::UserEvent(ui) = event {
            match ui {
                UiEvent::MenuAction(_) => {} // Handled above.
                UiEvent::Receive(ev) => {
                    let mut rebuild = false;
                    match &ev {
                        ReceiveEvent::PeerDiscovered(device) => {
                            scanner_handle.register(device.clone());
                        }
                        ReceiveEvent::Ready { .. } => {
                            receiver_status = ReceiverStatus::Receiving;
                            rebuild = true;
                        }
                        ReceiveEvent::TransferStarted { .. } => {
                            tray_anim.begin_receive();
                            rebuild = true;
                        }
                        ReceiveEvent::TransferComplete
                        | ReceiveEvent::TransferFinishedWithErrors
                        | ReceiveEvent::TransferCancelled => {
                            tray_anim.end_receive();
                            rebuild = true;
                        }
                        ReceiveEvent::MessageReceived { text, .. } => {
                            tray_anim.begin_receive();
                            let mut guard = last_message_ref.lock();
                            *guard = Some(text.clone());
                            rebuild = true;
                        }
                        ReceiveEvent::TransferRejected { sender_alias, .. } => {
                            notify_status(&format!(
                                "Rejected transfer from {sender_alias} (not paired)"
                            ));
                        }
                        _ => {}
                    }
                    if rebuild {
                        rebuild_menu(
                            &tray_ref,
                            &devices_ref,
                            &last_message_ref,
                            receiver_status,
                            &alias_ref,
                            &tray_anim,
                        );
                    }
                    handle_receive_event(ev, &mut receive_notify_state, &config_ref.receive_dir);
                }
                UiEvent::DevicesRefreshed { err } => {
                    if let Some(msg) = err {
                        tracing::warn!("device scan failed: {msg}");
                    }
                    rebuild_menu(
                        &tray_ref,
                        &devices_ref,
                        &last_message_ref,
                        receiver_status,
                        &alias_ref,
                        &tray_anim,
                    );
                }
                UiEvent::ReceiverError(msg) => {
                    rx_handle.take();
                    receiver_status = ReceiverStatus::Error;
                    tray_anim.cancel_receiving();
                    rebuild_menu(
                        &tray_ref,
                        &devices_ref,
                        &last_message_ref,
                        receiver_status,
                        &alias_ref,
                        &tray_anim,
                    );
                    notify_error(&msg);
                }
                UiEvent::ReceiverStopped => {
                    if quit_requested {
                        // Receiver connections have drained and partial files are removed.
                        // Avoid muda shutdown teardown after that cleanup completes.
                        std::process::exit(0);
                    }
                    receiver_status = ReceiverStatus::Paused;
                    tray_anim.cancel_receiving();
                    rebuild_menu(
                        &tray_ref,
                        &devices_ref,
                        &last_message_ref,
                        receiver_status,
                        &alias_ref,
                        &tray_anim,
                    );
                    notify_status("Receiver paused");
                }
                UiEvent::TransferStarted => {
                    tray_anim.begin_send();
                    rebuild_menu(
                        &tray_ref,
                        &devices_ref,
                        &last_message_ref,
                        receiver_status,
                        &alias_ref,
                        &tray_anim,
                    );
                }
                UiEvent::TransferEnded => {
                    tray_anim.end_send();
                    rebuild_menu(
                        &tray_ref,
                        &devices_ref,
                        &last_message_ref,
                        receiver_status,
                        &alias_ref,
                        &tray_anim,
                    );
                }
                UiEvent::SendResult(res) => match res {
                    Ok(summary) => notify_status(&format!("Sent {summary}")),
                    Err(msg) => notify_error(&format!("Send failed: {msg}")),
                },
            }
        }

        let was_animating = tray_anim.is_animating();
        let anim_deadline = tray_anim.tick(&tray_ref.borrow());
        if was_animating && !tray_anim.is_animating() {
            rebuild_menu(
                &tray_ref,
                &devices_ref,
                &last_message_ref,
                receiver_status,
                &alias_ref,
                &tray_anim,
            );
        }
        if !matches!(*control_flow, ControlFlow::ExitWithCode(_)) {
            *control_flow = anim_deadline
                .map(ControlFlow::WaitUntil)
                .unwrap_or(ControlFlow::Wait);
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
    animator: &TrayAnimator,
) {
    let has_last_message = last_message.lock().is_some();
    let menu_status = menu_receiver_status(receiver_status, animator);
    let devices_guard = devices.lock();
    let menu = match tray::build_menu(&devices_guard, menu_status, alias, has_last_message) {
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

fn menu_receiver_status(
    receiver_status: ReceiverStatus,
    animator: &TrayAnimator,
) -> ReceiverStatus {
    if animator.is_animating() {
        match receiver_status {
            ReceiverStatus::Receiving => ReceiverStatus::Transferring,
            other => other,
        }
    } else {
        receiver_status
    }
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
        ReceiveEvent::Ready { port, .. } => {
            notify_status(&format!("Receiver ready on port {port}"));
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
        ReceiveEvent::TransferRejected { .. } => {
            // Notified directly from the event-loop arm; no state to update
            // because no session was created.
        }
        ReceiveEvent::PeerDiscovered(_) => {}
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

    let _ = proxy.send_event(UiEvent::TransferStarted);

    let device_label = device.alias.clone();
    let device_ip = device.ip.clone();
    let count = path_strings.len();

    rt.spawn(async move {
        let request = lsend_core::send::SendRequest {
            target: &device_ip,
            paths: &path_strings,
            text_stdin: false,
            message: None,
            clipboard: false,
            pin: None,
            no_scan: true,
        };
        let result = lsend_core::send::send_files_to_device(
            &config,
            &identity,
            &request,
            lsend_core::output::OutputOptions::new(false, true),
            &device,
            "scan",
        )
        .await;

        let payload = match result {
            Ok(outcome) => describe_file_outcome(outcome, count, &device_label),
            Err(e) => Err(format!("{e:#}")),
        };
        let _ = proxy.send_event(UiEvent::TransferEnded);
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

    let _ = proxy.send_event(UiEvent::TransferStarted);

    rt.spawn(async move {
        let request = lsend_core::send::SendRequest {
            target: &device.ip,
            paths: &[],
            text_stdin: false,
            message: Some(&text),
            clipboard: false,
            pin: None,
            no_scan: true,
        };
        let result = lsend_core::send::send_files_to_device(
            &config,
            &identity,
            &request,
            lsend_core::output::OutputOptions::new(false, true),
            &device,
            "scan",
        )
        .await;

        let payload = match result {
            Ok(outcome) => describe_message_outcome(outcome, &device.alias),
            Err(e) => Err(format!("{e:#}")),
        };
        let _ = proxy.send_event(UiEvent::TransferEnded);
        let _ = proxy.send_event(UiEvent::SendResult(payload));
    });
}

/// Map a finished file send to the tray notification payload: a delivered
/// outcome is a success, and any failed upload is an error even when the
/// other files finished.
fn describe_file_outcome(
    outcome: lsend_core::send::SendOutcome,
    count: usize,
    device_label: &str,
) -> Result<String, String> {
    match outcome {
        lsend_core::send::SendOutcome::Delivered => {
            Ok(format!("{count} file(s) to {device_label}"))
        }
        lsend_core::send::SendOutcome::FailedFiles(failed) => Err(format!(
            "{failed} of {count} file(s) failed to upload to {device_label}"
        )),
    }
}

/// Same mapping for message sends, where any failed upload means the whole
/// message was not delivered.
fn describe_message_outcome(
    outcome: lsend_core::send::SendOutcome,
    device_alias: &str,
) -> Result<String, String> {
    match outcome {
        lsend_core::send::SendOutcome::Delivered => Ok(format!("message to {device_alias}")),
        lsend_core::send::SendOutcome::FailedFiles(_) => Err("message delivery failed".to_string()),
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use lsend_core::send::SendOutcome;

    #[test]
    fn sending_does_not_hide_paused_receivers_resume_action() {
        let mut animator = TrayAnimator::new(tray::load_icon_set().unwrap());
        animator.begin_send();
        assert_eq!(
            menu_receiver_status(ReceiverStatus::Paused, &animator),
            ReceiverStatus::Paused
        );
        assert_eq!(
            menu_receiver_status(ReceiverStatus::Receiving, &animator),
            ReceiverStatus::Transferring
        );
        animator.end_send();
        assert_eq!(
            menu_receiver_status(ReceiverStatus::Paused, &animator),
            ReceiverStatus::Paused
        );
    }

    #[test]
    fn file_outcome_reports_partial_upload_failure() {
        assert_eq!(
            describe_file_outcome(SendOutcome::Delivered, 3, "Laptop"),
            Ok("3 file(s) to Laptop".to_string())
        );
        assert_eq!(
            describe_file_outcome(SendOutcome::FailedFiles(2), 3, "Laptop"),
            Err("2 of 3 file(s) failed to upload to Laptop".to_string())
        );
    }

    #[test]
    fn message_outcome_reports_any_failure_as_error() {
        assert_eq!(
            describe_message_outcome(SendOutcome::Delivered, "Laptop"),
            Ok("message to Laptop".to_string())
        );
        assert_eq!(
            describe_message_outcome(SendOutcome::FailedFiles(1), "Laptop"),
            Err("message delivery failed".to_string())
        );
    }
}
