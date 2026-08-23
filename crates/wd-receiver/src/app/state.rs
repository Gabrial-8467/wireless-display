use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::Mutex;

use crate::config::Config;
use crate::diag::{MetricValue, MetricsRegistry, SystemInfo};
use crate::net::{PairedDevice, PairingManager};
use crate::session::SessionManager;

#[derive(Debug, Clone)]
pub enum UiEvent {
    State(crate::session::State),
    Peer(Option<String>),
    KnownDevices(Vec<PairedDevice>),
    PairingPrompt { code: String },
    PairingOutcome { ok: bool, message: String },
    Metrics(BTreeMap<String, MetricValue>),
}

pub struct AppState {
    pub config: Mutex<Config>,
    pub metrics: MetricsRegistry,
    pub system: SystemInfo,
    pub session: Arc<SessionManager>,
    /// Shared with the network stack so the UI, listener and store agree.
    pub pairing: Arc<PairingManager>,
    ui_tx: async_channel::Sender<UiEvent>,
    ui_rx: async_channel::Receiver<UiEvent>,
}

impl AppState {
    pub fn new(config: Config, system: SystemInfo, pairing: Arc<PairingManager>) -> Self {
        let (ui_tx, ui_rx) = async_channel::unbounded();
        Self {
            config: Mutex::new(config),
            metrics: MetricsRegistry::new(),
            system,
            session: Arc::new(SessionManager::new()),
            pairing,
            ui_tx,
            ui_rx,
        }
    }

    pub fn ui_receiver(&self) -> async_channel::Receiver<UiEvent> {
        self.ui_rx.clone()
    }

    pub fn ui_sender(&self) -> async_channel::Sender<UiEvent> {
        self.ui_tx.clone()
    }

    /// Opens a fresh pairing window and publishes the code to the UI.
    pub fn start_pairing(&self) -> String {
        let code = self.pairing.open_window();
        if self
            .ui_tx
            .try_send(UiEvent::PairingPrompt { code: code.clone() })
            .is_err()
        {
            tracing::warn!("no UI attached; pairing window still open for 120s");
        }
        code
    }

    pub fn cancel_pairing(&self) {
        self.pairing.close_window();
    }
}

async fn forward_session(state: Arc<AppState>) {
    let handles = state.session.handles();
    let mut peer_rx = handles.peer_rx;
    let mut events_rx = handles.events_rx;
    let mut last_state = state.session.state();
    let _ = state.ui_tx.send(UiEvent::State(last_state)).await;
    loop {
        tokio::select! {
            changed = peer_rx.changed() => {
                if changed.is_err() { break; }
                let peer = peer_rx.borrow_and_update().clone();
                let _ = state.ui_tx.send(UiEvent::Peer(peer)).await;
            }
            ev = events_rx.recv() => {
                match ev {
                    Ok(event) => {
                        if let crate::session::Event::StateChanged { .. } = &event {
                            last_state = state.session.state();
                            let _ = state.ui_tx.send(UiEvent::State(last_state)).await;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(_) => break,
                }
            }
        }
    }
}

async fn forward_metrics(state: Arc<AppState>) {
    let mut rx = state.metrics.subscribe();
    loop {
        if rx.changed().await.is_err() {
            break;
        }
        let snapshot = rx.borrow_and_update().clone();
        let _ = state.ui_tx.send(UiEvent::Metrics(snapshot)).await;
    }
}

pub fn spawn_forwarders(runtime: &tokio::runtime::Runtime, state: Arc<AppState>) {
    runtime.spawn(forward_session(state.clone()));
    runtime.spawn(forward_metrics(state));
}
