#![allow(dead_code)]

use std::fmt;

#[derive(Debug, thiserror::Error)]
#[error("illegal session transition {from} -> {to}")]
pub struct InvalidTransition {
    pub from: State,
    pub to: State,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum State {
    Idle,
    Discovering,
    Pairing,
    Connecting,
    Negotiating,
    Streaming,
    Recovering,
    Failed,
    Closed,
}

impl fmt::Display for State {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Self::Idle => "Idle",
            Self::Discovering => "Discovering devices",
            Self::Pairing => "Pairing",
            Self::Connecting => "Connecting",
            Self::Negotiating => "Negotiating session",
            Self::Streaming => "Streaming",
            Self::Recovering => "Recovering connection",
            Self::Failed => "Connection failed",
            Self::Closed => "Disconnected",
        };
        f.write_str(s)
    }
}

impl State {
    pub const fn allowed(from: State, to: State) -> bool {
        use State::*;
        matches!(
            (from, to),
            (Idle, Discovering)
                | (Idle, Connecting)
                | (Idle, Closed)
                | (Discovering, Pairing)
                | (Discovering, Connecting)
                | (Discovering, Idle)
                | (Discovering, Failed)
                | (Pairing, Connecting)
                | (Pairing, Discovering)
                | (Pairing, Failed)
                | (Connecting, Negotiating)
                | (Connecting, Recovering)
                | (Connecting, Failed)
                | (Connecting, Idle)
                | (Negotiating, Streaming)
                | (Negotiating, Failed)
                | (Streaming, Recovering)
                | (Streaming, Negotiating)
                | (Streaming, Closed)
                | (Streaming, Failed)
                | (Recovering, Streaming)
                | (Recovering, Connecting)
                | (Recovering, Failed)
                | (Recovering, Closed)
                | (Failed, Idle)
                | (Failed, Closed)
                | (Closed, Idle)
                | (Closed, Connecting)
        )
    }

    pub fn is_streaming(self) -> bool {
        matches!(self, Self::Streaming | Self::Recovering | Self::Negotiating)
    }
}

#[derive(Debug, Clone)]
pub enum Event {
    StateChanged { from: State, to: State },
    PeerChanged { name: String },
}

pub struct SessionManager {
    state_tx: tokio::sync::watch::Sender<State>,
    events_tx: tokio::sync::broadcast::Sender<Event>,
    peer_name_tx: tokio::sync::watch::Sender<Option<String>>,
}

pub struct SessionHandles {
    pub state_rx: tokio::sync::watch::Receiver<State>,
    pub events_rx: tokio::sync::broadcast::Receiver<Event>,
    pub peer_rx: tokio::sync::watch::Receiver<Option<String>>,
}

impl Default for SessionManager {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionManager {
    pub fn new() -> Self {
        let (state_tx, _) = tokio::sync::watch::channel(State::Idle);
        let (events_tx, _) = tokio::sync::broadcast::channel(64);
        let (peer_name_tx, _) = tokio::sync::watch::channel(None);
        Self {
            state_tx,
            events_tx,
            peer_name_tx,
        }
    }

    pub fn handles(&self) -> SessionHandles {
        SessionHandles {
            state_rx: self.state_tx.subscribe(),
            events_rx: self.events_tx.subscribe(),
            peer_rx: self.peer_name_tx.subscribe(),
        }
    }

    pub fn state(&self) -> State {
        *self.state_tx.borrow()
    }

    pub fn set_peer(&self, name: impl Into<String>) {
        self.peer_name_tx.send_replace(Some(name.into()));
        if let Some(name) = self.peer_name_tx.borrow().clone() {
            let _ = self.events_tx.send(Event::PeerChanged { name });
        }
    }

    pub fn transition(&self, to: State) -> Result<(), InvalidTransition> {
        let from = self.state();
        if from == to || !State::allowed(from, to) {
            return Err(InvalidTransition { from, to });
        }
        tracing::info!(from = %from, to = %to, "session state changed");
        self.state_tx.send_replace(to);
        let _ = self.events_tx.send(Event::StateChanged { from, to });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn happy_path_transitions_are_legal() {
        let mgr = SessionManager::new();
        for step in [
            State::Discovering,
            State::Connecting,
            State::Negotiating,
            State::Streaming,
            State::Closed,
        ] {
            mgr.transition(step).expect("legal step");
        }
        assert_eq!(mgr.state(), State::Closed);
    }

    #[test]
    fn recovery_path_is_legal() {
        let mgr = SessionManager::new();
        for step in [
            State::Connecting,
            State::Negotiating,
            State::Streaming,
            State::Recovering,
        ] {
            mgr.transition(step).unwrap();
        }
        mgr.transition(State::Streaming).unwrap();
        mgr.transition(State::Failed).unwrap();
        mgr.transition(State::Closed).unwrap();
    }

    #[test]
    fn failure_and_retry_is_legal() {
        let mgr = SessionManager::new();
        mgr.transition(State::Connecting).unwrap();
        mgr.transition(State::Failed).unwrap();
        mgr.transition(State::Idle).unwrap();
    }

    #[test]
    fn illegal_jumps_are_rejected_and_state_preserved() {
        let mgr = SessionManager::new();
        assert!(mgr.transition(State::Streaming).is_err());
        assert!(mgr.transition(State::Pairing).is_err());
        mgr.transition(State::Discovering).unwrap();
        assert!(mgr.transition(State::Streaming).is_err());
        assert_eq!(mgr.state(), State::Discovering);
    }

    #[test]
    fn same_state_transition_is_rejected() {
        let mgr = SessionManager::new();
        assert!(mgr.transition(State::Idle).is_err());
    }

    #[test]
    fn events_are_broadcast_to_subscribers() {
        let mgr = SessionManager::new();
        let mut handles = mgr.handles();
        mgr.set_peer("Test Phone");
        mgr.transition(State::Discovering).unwrap();
        match handles.events_rx.try_recv().expect("peer event") {
            Event::PeerChanged { name } => assert_eq!(name, "Test Phone"),
            other => panic!("unexpected event {other:?}"),
        }
        match handles.events_rx.try_recv().expect("state event") {
            Event::StateChanged { from, to } => {
                assert_eq!((from, to), (State::Idle, State::Discovering))
            }
            other => panic!("unexpected event {other:?}"),
        }
    }
}
