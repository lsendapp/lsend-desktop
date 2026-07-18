//! Background device scanner. Polls [`lsend_core::discovery::scan`] on a fixed
//! interval and updates a shared device cache, then pokes the tao event loop
//! to rebuild the tray menu.
//!
//! The interval is intentionally coarse (5 s) — device presence is a CLI
//! convenience for the tray submenu, not a real-time requirement. The scan is
//! also short-circuited: if the resident receiver has 53317 bound, the scan's
//! own discovery server cannot bind the same port and we fall back to UDP
//! multicast only (already non-fatal inside `discovery::scan`).

use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use tokio::runtime::Handle;
use tokio::task::JoinHandle;

use lsend_core::config::AppConfig;
use lsend_core::discovery;
use lsend_core::identity::Identity;
use lsend_core::output::OutputOptions;

use crate::state::UiEvent;

const SCAN_INTERVAL: Duration = Duration::from_secs(5);
const SCAN_TIMEOUT_MS: u64 = 1500;

/// Spawn the scanner. Returns a handle whose task runs for the process
/// lifetime; abort it on quit.
pub fn spawn(
    rt: Handle,
    config: Arc<AppConfig>,
    identity: Arc<Identity>,
    devices: Arc<Mutex<Vec<discovery::DiscoveredDevice>>>,
    proxy: tao::event_loop::EventLoopProxy<UiEvent>,
) -> JoinHandle<()> {
    rt.spawn(async move {
        // OutputOptions deliberately unused: scan returns devices, we don't
        // print human text. Pass a quiet human mode.
        let _output = OutputOptions::new(false, true);

        loop {
            // Re-clone AppConfig/Identity per scan: scan mutates (binding ports)
            // and we don't want lifetime fights with the Arc's contents.
            let result = discovery::scan(
                &config,
                &identity,
                SCAN_TIMEOUT_MS,
            )
            .await;

            match result {
                Ok(mut devices_list) => {
                    // Sort by alias then ip for stable tray ordering.
                    devices_list.sort_by(|a, b| {
                        a.alias.cmp(&b.alias).then_with(|| a.ip.cmp(&b.ip))
                    });
                    // Dedup by (ip, port, fingerprint); keep first after sort.
                    let mut seen = std::collections::HashSet::new();
                    devices_list.retain(|d| {
                        seen.insert((d.ip.clone(), d.port, d.fingerprint.clone()))
                    });
                    let mut guard = devices.lock();
                    *guard = devices_list;
                    let _ = proxy.send_event(UiEvent::DevicesRefreshed { err: None });
                }
                Err(e) => {
                    let _ = proxy.send_event(UiEvent::DevicesRefreshed {
                        err: Some(e.to_string()),
                    });
                }
            }

            tokio::time::sleep(SCAN_INTERVAL).await;
        }
    })
}