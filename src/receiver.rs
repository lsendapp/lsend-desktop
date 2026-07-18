//! Resident receiver supervisor.
//!
//! Wraps [`lsend_core::receive::run_with_events`] in a tokio task that owns the
//! receiver's server + discovery tasks. The desktop main loop can `stop` the
//! supervisor (drop the `JoinHandle` after asking it to stop) to "pause"
//! receiving, and spawn a new one to "resume".
//!
//! The supervisor forwards every [`ReceiveEvent`] to the tao event loop via
//! the supplied proxy so the tray can react (notifications, status updates).

use std::sync::Arc;

use tokio::runtime::Handle;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

use lsend_core::config::AppConfig;
use lsend_core::identity::Identity;
use lsend_core::output::OutputMode;
use lsend_core::receive;

use crate::state::UiEvent;

/// Stop signal: sent to the supervisor to ask it to shut the receiver down
/// gracefully (the supervisor itself forwards `ReceiveEvent::Shutdown`).
pub struct ReceiverHandle {
    stop: Option<oneshot::Sender<()>>,
    join: JoinHandle<()>,
}

impl ReceiverHandle {
    /// Signal the supervisor to stop and wait for it to finish tearing down.
    pub async fn stop(mut self) {
        if let Some(tx) = self.stop.take() {
            let _ = tx.send(());
        }
        let _ = (&mut self.join).await;
    }
}

/// Spawn the resident receiver. Errors from `run_with_events` (most commonly
/// `port_in_use` on 53317) are surfaced as `UiEvent::ReceiverError`.
pub fn spawn(
    rt: Handle,
    config: Arc<AppConfig>,
    identity: Arc<Identity>,
    proxy: tao::event_loop::EventLoopProxy<UiEvent>,
) -> ReceiverHandle {
    let (stop_tx, stop_rx) = oneshot::channel::<()>();

    // run_with_events returns (stop_tx_tokio, event_rx). We bridge our oneshot
    // stop into that tokio stop_tx by forwarding in a small adapter task.

    let join = rt.spawn(async move {
        let result = receive::run_with_events(
            (*config).clone(),
            (*identity).clone(),
            None,
            OutputMode::Human,
        )
        .await;

        let (tokio_stop_tx, mut event_rx) = match result {
            Ok(pair) => pair,
            Err(e) => {
                let _ = proxy.send_event(UiEvent::ReceiverError(format!(
                    "Could not start receiver: {e}"
                )));
                return;
            }
        };

        // Bridge oneshot stop -> tokio stop_tx (drop also stops).
        let bridge = tokio::spawn(async move {
            let _ = stop_rx.await;
            let _ = tokio_stop_tx.send(());
        });

        while let Some(ev) = event_rx.recv().await {
            let _ = proxy.send_event(UiEvent::Receive(ev));
        }
        // event_rx closed → supervisor done. Let the bridge exit too.
        bridge.abort();
    });

    ReceiverHandle {
        stop: Some(stop_tx),
        join,
    }
}
