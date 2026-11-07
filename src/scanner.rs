//! Background device scanner. Polls [`lsend_core::discovery::scan`] on a fixed
//! interval and updates a shared device cache, then pokes the tao event loop
//! to rebuild the tray menu.
//!
//! The interval is intentionally coarse (5 s) — device presence is a CLI
//! convenience for the tray submenu, not a real-time requirement. The scan is
//! uses passive discovery because the resident receiver owns port 53317.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use tokio::runtime::Handle;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use lsend_core::config::AppConfig;
use lsend_core::discovery;
use lsend_core::identity::Identity;

use crate::state::UiEvent;

const SCAN_INTERVAL: Duration = Duration::from_secs(5);
const SCAN_TIMEOUT_MS: u64 = 1500;
const DEVICE_TTL: Duration = Duration::from_secs(60);

pub struct ScannerHandle {
    join: JoinHandle<()>,
    refresh: mpsc::UnboundedSender<()>,
    registrations: mpsc::UnboundedSender<discovery::DiscoveredDevice>,
}

impl ScannerHandle {
    pub fn refresh(&self) {
        let _ = self.refresh.send(());
    }

    pub fn register(&self, device: discovery::DiscoveredDevice) {
        let _ = self.registrations.send(device);
    }

    pub fn abort(&self) {
        self.join.abort();
    }
}

/// Spawn the scanner. Returns a handle that can request an immediate scan or
/// abort the task when the process quits.
pub fn spawn(
    rt: Handle,
    config: Arc<AppConfig>,
    identity: Arc<Identity>,
    devices: Arc<Mutex<Vec<discovery::DiscoveredDevice>>>,
    proxy: tao::event_loop::EventLoopProxy<UiEvent>,
) -> ScannerHandle {
    let (refresh_tx, mut refresh_rx) = mpsc::unbounded_channel();
    let (registrations, mut registration_rx) = mpsc::unbounded_channel();
    let join = rt.spawn(async move {
        let mut cache = DeviceCache::default();
        loop {
            let scan = discovery::scan_passive(&config, &identity, SCAN_TIMEOUT_MS);
            tokio::pin!(scan);
            // HTTP registrations arrive while the UDP scan is in progress.
            let result = loop {
                tokio::select! {
                    result = &mut scan => break result,
                    Some(device) = registration_rx.recv() => {
                        cache.observe(device, Instant::now());
                        *devices.lock() = cache.current(Instant::now());
                        let _ = proxy.send_event(UiEvent::DevicesRefreshed { err: None });
                    }
                }
            };
            let err = match result {
                Ok(found) => {
                    for device in found {
                        cache.observe(device, Instant::now());
                    }
                    None
                }
                Err(e) => Some(e.to_string()),
            };
            *devices.lock() = cache.current(Instant::now());
            let _ = proxy.send_event(UiEvent::DevicesRefreshed { err });

            let delay = tokio::time::sleep(SCAN_INTERVAL);
            tokio::pin!(delay);
            loop {
                tokio::select! {
                    _ = &mut delay => break,
                    refresh = refresh_rx.recv() => {
                        if refresh.is_none() { return; }
                        break;
                    }
                    Some(device) = registration_rx.recv() => {
                        cache.observe(device, Instant::now());
                        *devices.lock() = cache.current(Instant::now());
                        let _ = proxy.send_event(UiEvent::DevicesRefreshed { err: None });
                    }
                }
            }
        }
    });

    ScannerHandle {
        join,
        refresh: refresh_tx,
        registrations,
    }
}

/// Merge UDP observations and HTTP registrations; expire peers only after a
/// quiet period so a scan cannot erase a registration it did not observe.
#[derive(Default)]
struct DeviceCache {
    peers: HashMap<String, (discovery::DiscoveredDevice, Instant)>,
}

impl DeviceCache {
    fn observe(&mut self, device: discovery::DiscoveredDevice, now: Instant) {
        self.peers
            .insert(crate::tray::device_key(&device), (device, now));
    }

    fn current(&mut self, now: Instant) -> Vec<discovery::DiscoveredDevice> {
        self.peers
            .retain(|_, (_, seen)| now.duration_since(*seen) < DEVICE_TTL);
        let mut peers: Vec<_> = self
            .peers
            .values()
            .map(|(device, _)| device.clone())
            .collect();
        peers.sort_by(|a, b| a.alias.cmp(&b.alias).then_with(|| a.ip.cmp(&b.ip)));
        peers
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registrations_survive_empty_scans_and_expire_after_silence() {
        let now = Instant::now();
        let mut cache = DeviceCache::default();
        cache.observe(
            discovery::DiscoveredDevice {
                alias: "Peer".into(),
                ip: "192.168.1.10".into(),
                port: 6000,
                https: false,
                fingerprint: "fp".into(),
                version: "2.1".into(),
                device_type: None,
                device_model: None,
            },
            now,
        );
        assert_eq!(cache.current(now + SCAN_INTERVAL).len(), 1);
        let mut peer = cache.current(now)[0].clone();
        peer.alias = "Renamed".into();
        cache.observe(peer, now + SCAN_INTERVAL);
        let peers = cache.current(now + DEVICE_TTL);
        assert_eq!(peers.len(), 1);
        assert_eq!(peers[0].alias, "Renamed");
        assert!(cache.current(now + DEVICE_TTL + SCAN_INTERVAL).is_empty());
    }
}
