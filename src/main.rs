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

use std::sync::Arc;

use anyhow::Result;
use notify_rust::Notification;
use parking_lot::Mutex;
use tao::{
    event_loop::{ControlFlow, EventLoopBuilder, EventLoop},
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
use crate::tray::TrayAction;

const APP_NAME: &str = "lsend";

/// Resolve the current macOS/Linux user. We prefer `id -un` (POSIX) over
/// `whoami` because `whoami` on macOS reads from `/etc/utmp` and can return a
/// stale name if the process is launched via `open` from a different session.
/// Falls back to the env vars `USER` / `USERNAME`, finally to `"unknown"`.
fn current_user() -> String {
    use std::process::Command;

    if let Ok(out) = Command::new("id").arg("-un").output() {
        if out.status.success() {
            if let Ok(s) = String::from_utf8(out.stdout) {
                let name = s.trim();
                if !name.is_empty() {
                    return name.to_string();
                }
            }
        }
    }
    for var in ["USER", "USERNAME"] {
        if let Ok(v) = std::env::var(var) {
            let v = v.trim();
            if !v.is_empty() {
                return v.to_string();
            }
        }
    }
    "unknown".to_string()
}

fn main() -> Result<()> {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("lsend=info")),
        )
        .try_init();

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

    // Tray icon + initial menu. Wrapped in an Arc<Mutex<_>> so the event loop
    // closure (a `move`) can also rebuild the menu — `TrayIcon::set_menu`
    // swaps without dropping the icon.
    let icon = tray::build_icon()?;
    let initial_menu = tray::build_menu(&[], /* receiving = */ false)?;
    let tray = TrayIconBuilder::new()
        .with_menu(Box::new(initial_menu))
        .with_tooltip(APP_NAME)
        .with_icon(icon)
        .build()?;
    let tray = Arc::new(Mutex::new(tray));

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
    let devices_ref = app.devices.clone();
    let config_ref = app.config.clone();
    let identity_ref = app.identity.clone();
    let tray_ref = tray.clone();

    // Track whether the tray currently reflects "Receiving". Drives menu
    // rebuilds on pause/resume.
    let mut tray_receiving = false;

    event_loop.run(move |event, target, control_flow| {
        *control_flow = ControlFlow::Wait;

        // 1) Tray menu events (muda global channel).
        if let Ok(menu_event) = MenuEvent::receiver().try_recv() {
            let id = menu_event.id.0.as_str().to_string();
            let action = tray::parse_action(&id);
            match action {
                TrayAction::Send(idx) => {
                    handle_send_file(
                        &rt_handle,
                        config_ref.clone(),
                        identity_ref.clone(),
                        &devices_ref,
                        idx,
                        proxy.clone(),
                    );
                }
                TrayAction::Refresh => {
                    // Scanner cycles on its own; this is a no-op hint for the
                    // user (a future improvement: kick the scanner now).
                    info!("refresh requested; scanner will update shortly");
                }
                TrayAction::Pause => {
                    if let Some(h) = rx_handle.take() {
                        rt_handle.spawn(async move {
                            h.stop().await;
                        });
                        tray_receiving = false;
                        rebuild_menu(&tray_ref, &devices_ref, tray_receiving);
                        notify_status("Receiver paused");
                    }
                }
                TrayAction::Resume => {
                    if rx_handle.is_none() {
                        rx_handle = Some(receiver::spawn(
                            rt_handle.clone(),
                            config_ref.clone(),
                            identity_ref.clone(),
                            proxy.clone(),
                        ));
                        tray_receiving = true;
                        rebuild_menu(&tray_ref, &devices_ref, tray_receiving);
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
                UiEvent::Receive(ev) => handle_receive_event(ev, tray_receiving),
                UiEvent::DevicesRefreshed { .. } => {
                    rebuild_menu(&tray_ref, &devices_ref, tray_receiving);
                }
                UiEvent::ReceiverError(msg) => {
                    tray_receiving = false;
                    rebuild_menu(&tray_ref, &devices_ref, tray_receiving);
                    notify_error(&msg);
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

/// Rebuild the tray menu from the current device cache + receiving flag.
/// Runs on the main thread; the devices lock is held briefly.
fn rebuild_menu(
    tray: &Arc<Mutex<tray_icon::TrayIcon>>,
    devices: &Arc<Mutex<Vec<DiscoveredDevice>>>,
    receiving: bool,
) {
    let devices_guard = devices.lock();
    let menu = match tray::build_menu(&devices_guard, receiving) {
        Ok(m) => m,
        Err(e) => {
            tracing::error!("tray menu build failed: {e}");
            return;
        }
    };
    drop(devices_guard);
    let t = tray.lock();
    tray::set_tray_menu(&t, menu);
}

fn handle_receive_event(ev: ReceiveEvent, _receiving: bool) {
    match ev {
        ReceiveEvent::Ready { .. } => {
            notify_status("Receiver ready on port 53317");
        }
        ReceiveEvent::TransferStarted { sender_alias, file_count } => {
            notify_status(&format!("Receiving {file_count} file(s) from {sender_alias}"));
        }
        ReceiveEvent::MessageReceived { sender_alias, text, .. } => {
            let body = if text.chars().count() > 160 {
                format!("{}…", text.chars().take(160).collect::<String>())
            } else {
                text
            };
            notify_block(&format!("Message from {sender_alias}"), &body);
        }
        ReceiveEvent::FileSaved { file_name, .. } => {
            notify_status(&format!("Saved {file_name}"));
        }
        ReceiveEvent::TransferComplete => {
            notify_status("Transfer complete");
        }
        ReceiveEvent::TransferFinishedWithErrors => {
            notify_error("Transfer finished with errors");
        }
        ReceiveEvent::TransferCancelled => {
            notify_status("Transfer cancelled");
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
    idx: usize,
    proxy: tao::event_loop::EventLoopProxy<UiEvent>,
) {
    let guard = devices.lock();
    let Some(device) = guard.get(idx).cloned() else {
        let _ = proxy.send_event(UiEvent::SendResult(Err(
            "Selected device is no longer available (rescan in progress)."
                .to_string(),
        )));
        return;
    };
    drop(guard);

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

fn notify_status(body: &str) {
    if let Err(e) = Notification::new().summary("lsend").body(body).show() {
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
        .summary("lsend — error")
        .body(body)
        .show()
    {
        tracing::warn!("notification failed: {e}");
    }
}