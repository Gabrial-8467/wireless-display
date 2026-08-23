use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tokio::sync::{mpsc, watch};

use wd_protocol::{CodecError, Message, Version, read_frame, write_frame};

use super::identity::Identity;
use super::pairing::PairingManager;
use crate::session::{SessionManager, State as SessionState};

const IDLE_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone)]
pub enum ListenerEvent {
    Connected { name: String },
    Disconnected { clean: bool, reason: String },
    PairingSucceeded { device_id: String, name: String },
}

pub struct NetContext {
    pub identity: Arc<Identity>,
    pub pairing: Arc<PairingManager>,
    pub session: Arc<SessionManager>,
    pub events_tx: mpsc::Sender<ListenerEvent>,
    pub idle_timeout: Duration,
}

impl NetContext {
    pub fn new(
        identity: Arc<Identity>,
        pairing: Arc<PairingManager>,
        session: Arc<SessionManager>,
        events_tx: mpsc::Sender<ListenerEvent>,
    ) -> Self {
        Self {
            identity,
            pairing,
            session,
            events_tx,
            idle_timeout: IDLE_TIMEOUT,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ListenerStartError {
    #[error("failed to start QUIC listener on {addr}: {detail}")]
    Start {
        addr: SocketAddr,
        detail: String,
    },
}

pub struct ListenerHandle {
    endpoint: quinn::Endpoint,
    local_addr: SocketAddr,
    shutdown_tx: watch::Sender<bool>,
    accept_task: Option<tokio::task::JoinHandle<()>>,
}

impl ListenerHandle {
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    pub fn shutdown(mut self) {
        let _ = self.shutdown_tx.send(true);
        self.endpoint.close(0u32.into(), b"shutdown");
        if let Some(task) = self.accept_task.take() {
            task.abort();
        }
    }
}

impl Drop for ListenerHandle {
    fn drop(&mut self) {
        let _ = self.shutdown_tx.send(true);
        self.endpoint.close(0u32.into(), b"shutdown");
        if let Some(task) = self.accept_task.take() {
            task.abort();
        }
    }
}

/// Builds the QUIC endpoint and spawns the accept loop.
pub fn start_listener(
    ctx: NetContext,
    bind_addr: SocketAddr,
) -> Result<ListenerHandle, ListenerStartError> {
    let fail = |detail: String| ListenerStartError::Start {
        addr: bind_addr,
        detail,
    };

    let (certs, key) = ctx.identity.tls_material().map_err(|e| fail(e.to_string()))?;
    super::rustls_crypto_provider();

    let mut server_crypto = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|e| fail(format!("invalid receiver identity: {e}")))?;
    server_crypto.alpn_protocols = vec![b"wdl/1".to_vec()];

    let quic_config =
        quinn::crypto::rustls::QuicServerConfig::try_from(server_crypto)
            .map_err(|e| fail(format!("quic config rejected identity: {e}")))?;
    let endpoint = quinn::Endpoint::server(quinn::ServerConfig::with_crypto(Arc::new(quic_config)), bind_addr)
        .map_err(|e| fail(e.to_string()))?;
    let local_addr = super::endpoint_local_addr(&endpoint);
    tracing::info!(%local_addr, "QUIC listener ready");

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let accept_task = tokio::spawn(accept_loop(endpoint.clone(), ctx, shutdown_rx));

    Ok(ListenerHandle {
        endpoint,
        local_addr,
        shutdown_tx,
        accept_task: Some(accept_task),
    })
}

async fn accept_loop(
    endpoint: quinn::Endpoint,
    ctx: NetContext,
    mut shutdown: watch::Receiver<bool>,
) {
    // One mirrored screen at a time; extra connections are refused.
    let busy = Arc::new(AtomicBool::new(false));

    loop {
        tokio::select! {
            incoming = endpoint.accept() => {
                let Some(incoming) = incoming else { break };
                if busy.swap(true, Ordering::SeqCst) {
                    tracing::info!(peer = %incoming.remote_address(), "refusing connection: already busy");
                    incoming.refuse();
                    continue;
                }
                match incoming.accept() {
                    Ok(connecting) => handle_connection(connecting.await, &ctx, &busy).await,
                    Err(e) => {
                        tracing::debug!(error = %e, "connection failed before QUIC handshake");
                        busy.store(false, Ordering::SeqCst);
                    }
                }
            }
            _ = shutdown.changed() => break,
        }
    }
    tracing::info!("accept loop stopped");
}

async fn handle_connection(
    conn_result: Result<quinn::Connection, quinn::ConnectionError>,
    ctx: &NetContext,
    busy: &AtomicBool,
) {
    match conn_result {
        Ok(conn) => {
            tracing::info!(peer = %conn.remote_address(), "QUIC connection established");
            run_session(&conn, ctx).await;
        }
        Err(e) => tracing::warn!(error = %e, "QUIC handshake failed"),
    }
    busy.store(false, Ordering::SeqCst);

    // Return the FSM to Idle so the next connection starts fresh.
    if ctx.session.state() != SessionState::Idle {
        let _ = ctx.session.transition(SessionState::Failed);
        let _ = ctx.session.transition(SessionState::Idle);
    }
}

enum SessionOutcome {
    Clean(String),
    Lost(String),
}

async fn run_session(conn: &quinn::Connection, ctx: &NetContext) {
    let _ = ctx.session.transition(SessionState::Connecting);
    let outcome = establish_and_serve(conn, ctx).await;

    let (clean, reason) = match outcome {
        SessionOutcome::Clean(reason) => (true, reason),
        SessionOutcome::Lost(reason) => (false, reason),
    };

    if !matches!(
        ctx.session.state(),
        SessionState::Closed | SessionState::Idle | SessionState::Failed
    ) {
        let _ = ctx.session.transition(SessionState::Failed);
    }

    tracing::info!(clean, %reason, "connection ended");
    let _ = ctx
        .events_tx
        .send(ListenerEvent::Disconnected { clean, reason })
        .await;
}

async fn establish_and_serve(conn: &quinn::Connection, ctx: &NetContext) -> SessionOutcome {
    let (mut send, mut recv) = match conn.accept_bi().await {
        Ok(streams) => streams,
        Err(e) => return SessionOutcome::Lost(format!("control stream failed: {e}")),
    };

    let (proto_version, _device_name, auth_token) = match read_frame(&mut recv).await {
        Ok(Message::Hello { proto_version, device_name, auth_token }) => {
            (proto_version, device_name, auth_token)
        }
        Ok(other) => {
            let _ = write_frame(
                &mut send,
                &Message::Bye {
                    reason: format!("expected Hello, got {}", kind_of(&other)),
                },
            )
            .await;
            return SessionOutcome::Clean("unexpected first message".into());
        }
        Err(e) => return SessionOutcome::Lost(format!("failed to read Hello: {e}")),
    };

    if !Version::CURRENT.compatible_with(proto_version) {
        let _ = write_frame(
            &mut send,
            &Message::Bye {
                reason: format!(
                    "protocol version {proto_version} incompatible with receiver {}",
                    Version::CURRENT
                ),
            },
        )
        .await;
        return SessionOutcome::Clean(format!("incompatible protocol version {proto_version}"));
    }

    let Some(device) = auth_token.as_deref().and_then(|t| ctx.pairing.find_by_token(t)) else {
        return pairing_flow(ctx, &mut send, &mut recv).await;
    };

    tracing::info!(device_id = %device.device_id, name = %device.name, "authenticated reconnect");
    let _ = ctx.session.transition(SessionState::Negotiating);
    ctx.session.set_peer(device.name.clone());
    let _ = ctx
        .events_tx
        .send(ListenerEvent::Connected { name: device.name.clone() })
        .await;

    if let Err(e) = write_frame(
        &mut send,
        &Message::HelloAck {
            proto_version: Version::CURRENT,
            receiver_name: ctx.identity.host_name().to_string(),
        },
    )
    .await
    {
        return SessionOutcome::Lost(format!("failed to send HelloAck: {e}"));
    }

    serve_control_loop(&mut send, &mut recv, ctx).await
}

async fn pairing_flow(
    ctx: &NetContext,
    send: &mut quinn::SendStream,
    recv: &mut quinn::RecvStream,
) -> SessionOutcome {
    let reject = |reason: String| Message::PairResult {
        accepted: false,
        reason: Some(reason.clone()),
        outcome: None,
    };

    let (phone_name, spake_message) = match read_frame(recv).await {
        Ok(Message::PairBegin { device_name, spake_message }) => (device_name, spake_message),
        Ok(other) => {
            let reason = format!("expected PairBegin, got {}", kind_of(&other));
            let _ = write_frame(send, &reject(reason.clone())).await;
            return SessionOutcome::Clean(reason);
        }
        Err(e) => return SessionOutcome::Lost(format!("failed to read PairBegin: {e}")),
    };

    if !ctx.pairing.window_is_open() {
        let reason: String = "no pairing window is open on the computer".into();
        let _ = write_frame(send, &reject(reason.clone())).await;
        return SessionOutcome::Clean(reason);
    }

    let fingerprint = ctx.identity.fingerprint_hex().to_string();
    let outcome = match ctx.pairing.handle_pair_begin(&phone_name, &spake_message, fingerprint) {
        Ok(o) => o,
        Err(e) => {
            let reason = format!("pairing rejected: {e}");
            tracing::warn!(%reason, "pairing attempt failed at PairBegin");
            let _ = write_frame(send, &reject(reason.clone())).await;
            return SessionOutcome::Clean(reason);
        }
    };

    if let Err(e) = write_frame(
        send,
        &Message::PairChallenge {
            spake_reply: outcome.spake_reply,
            receiver_fingerprint: outcome.receiver_fingerprint,
            receiver_confirmation: outcome.receiver_confirmation,
        },
    )
    .await
    {
        return SessionOutcome::Lost(format!("failed to send PairChallenge: {e}"));
    }

    let phone_confirmation = match read_frame(recv).await {
        Ok(Message::PairVerify { phone_confirmation }) => phone_confirmation,
        Ok(other) => {
            let reason = format!("expected PairVerify, got {}", kind_of(&other));
            let _ = write_frame(send, &reject(reason.clone())).await;
            return SessionOutcome::Clean(reason);
        }
        Err(e) => return SessionOutcome::Lost(format!("failed to read PairVerify: {e}")),
    };

    match ctx.pairing.handle_pair_verify(&phone_confirmation) {
        Ok(device) => {
            let result = Message::PairResult {
                accepted: true,
                reason: None,
                outcome: Some(wd_protocol::PairingOutcomeInfo {
                    device_id: device.device_id.clone(),
                    device_token: device.token.clone(),
                }),
            };
            if let Err(e) = write_frame(send, &result).await {
                return SessionOutcome::Lost(format!("pairing succeeded but reply failed: {e}"));
            }
            let _ = ctx
                .events_tx
                .send(ListenerEvent::PairingSucceeded {
                    device_id: device.device_id.clone(),
                    name: device.name.clone(),
                })
                .await;
            SessionOutcome::Clean(format!("paired with {}", device.name))
        }
        Err(e) => {
            let reason = format!("pairing rejected: {e}");
            tracing::warn!(%reason, "pairing attempt failed at PairVerify");
            let _ = write_frame(send, &reject(reason.clone())).await;
            SessionOutcome::Clean(reason)
        }
    }
}

async fn serve_control_loop(
    send: &mut quinn::SendStream,
    recv: &mut quinn::RecvStream,
    ctx: &NetContext,
) -> SessionOutcome {
    loop {
        let frame = tokio::time::timeout(ctx.idle_timeout, read_frame(recv)).await;
        match frame {
            Err(_elapsed) => return SessionOutcome::Lost("idle timeout".into()),
            Ok(Err(CodecError::UnexpectedEof)) => {
                return SessionOutcome::Clean("client closed control stream".into());
            }
            Ok(Err(e)) => return SessionOutcome::Lost(format!("control stream error: {e}")),
            Ok(Ok(message)) => match message {
                Message::Ping { sender_time_ms } => {
                    if let Err(e) =
                        write_frame(send, &Message::Pong { echoed_time_ms: sender_time_ms }).await
                    {
                        return SessionOutcome::Lost(format!("failed to pong: {e}"));
                    }
                }
                Message::ClockSync { t1, t2: _, t3: _, t4 } => {
                    let now = unix_millis();
                    let reply = Message::ClockSync { t1, t2: now, t3: now, t4 };
                    if let Err(e) = write_frame(send, &reply).await {
                        return SessionOutcome::Lost(format!("clock sync reply failed: {e}"));
                    }
                }
                Message::SessionOffer { video, audio } => {
                    // Media pipeline arrives in Phase 3; accept negotiation but
                    // hold the session in Negotiating until frames flow.
                    tracing::info!(
                        video = %video.codec.name(),
                        audio = %audio.codec.name(),
                        "session offer received (media not yet supported)"
                    );
                    let answer = Message::SessionAnswer {
                        accepted: false,
                        reason: Some("receiver media pipeline is not enabled yet".into()),
                        max_video_bitrate_kbps: 25_000,
                    };
                    if let Err(e) = write_frame(send, &answer).await {
                        return SessionOutcome::Lost(format!("failed to answer offer: {e}"));
                    }
                }
                Message::KeyframeRequest | Message::BitrateHint { .. } => {
                    tracing::debug!("media hint ignored until Phase 3");
                }
                Message::Bye { reason } => {
                    let _ = write_frame(
                        send,
                        &Message::Bye { reason: format!("bye acknowledged: {reason}") },
                    )
                    .await;
                    return SessionOutcome::Clean(format!("client said bye: {reason}"));
                }
                other => {
                    let kind = kind_of(&other);
                    let _ = write_frame(send, &Message::Bye { reason: format!("unexpected message {kind}") }).await;
                    return SessionOutcome::Clean(format!("unexpected message {kind} after auth"));
                }
            },
        }
    }
}

fn kind_of(message: &Message) -> &'static str {
    match message {
        Message::Hello { .. } => "Hello",
        Message::HelloAck { .. } => "HelloAck",
        Message::PairBegin { .. } => "PairBegin",
        Message::PairChallenge { .. } => "PairChallenge",
        Message::PairVerify { .. } => "PairVerify",
        Message::PairResult { .. } => "PairResult",
        Message::SessionOffer { .. } => "SessionOffer",
        Message::SessionAnswer { .. } => "SessionAnswer",
        Message::KeyframeRequest => "KeyframeRequest",
        Message::BitrateHint { .. } => "BitrateHint",
        Message::ClockSync { .. } => "ClockSync",
        Message::Ping { .. } => "Ping",
        Message::Pong { .. } => "Pong",
        Message::Bye { .. } => "Bye",
    }
}

fn unix_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
