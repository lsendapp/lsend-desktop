//! Shared desktop runtime state: config, identity, and the live set of
//! discovered peer devices. Loaded once at startup; mutated only from the
//! main (tao) thread unless otherwise noted.

use std::sync::Arc;

use anyhow::Result;
use parking_lot::Mutex;

use lsend_core::config::AppConfig;
use lsend_core::discovery::DiscoveredDevice;
use lsend_core::events::ReceiveEvent;
use lsend_core::identity::Identity;

/// State owned by the tao event loop. Cloning is cheap: the heavy fields are
/// behind an `Arc` so background tasks can hold a reference.
#[derive(Clone)]
pub struct AppState {
    pub config: Arc<AppConfig>,
    pub identity: Arc<Identity>,
    /// Latest scan result. Locks are very brief (menu rebuilds + lookups by
    /// tray index), so a plain `Mutex` is fine.
    pub devices: Arc<Mutex<Vec<DiscoveredDevice>>>,
    /// Full text of the most recently received message. The tray reads a clone
    /// before touching the system clipboard, so the lock is never held across
    /// an OS call.
    pub last_message: Arc<Mutex<Option<String>>>,
}

impl AppState {
    /// Load config + identity from the same on-disk locations the CLI uses
    /// (`~/.config/lsend/` on macOS/Linux, `%APPDATA%\lsend` on Windows) so
    /// the desktop app and `lsend` CLI share one identity.
    pub fn load() -> Result<Self> {
        let config = AppConfig::new(None, AppConfig::DEFAULT_PORT, true, None)?;
        let identity = Identity::load_or_create(&config.config_dir, config.https)?;
        Ok(Self {
            config: Arc::new(config),
            identity: Arc::new(identity),
            devices: Arc::new(Mutex::new(Vec::new())),
            last_message: Arc::new(Mutex::new(None)),
        })
    }
}

/// Cross-thread events delivered into the tao event loop via `EventLoopProxy`.
/// Background tokio tasks `send_event` these; the main loop matches on them.
#[derive(Clone, Debug)]
pub enum UiEvent {
    Receive(ReceiveEvent),
    /// The background scan loop refreshed `AppState::devices` (or scan
    /// errored — check `err`).
    DevicesRefreshed {
        err: Option<String>,
    },
    /// The resident listener failed to start (port in use, etc.) or stopped
    /// unexpectedly. Carries a human-readable reason for the tray status.
    ReceiverError(String),
    /// A requested receiver shutdown has completed and the listening port is
    /// available for a subsequent resume.
    ReceiverStopped,
    SendResult(std::result::Result<String, String>),
    /// Outbound transfer began (file picker finished / send task spawned).
    TransferStarted,
    /// Outbound transfer finished (success or failure).
    TransferEnded,
}
