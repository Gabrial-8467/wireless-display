use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::{mpsc, watch};

use wd_protocol::{AudioOffer, CodecError, Message, Version, VideoOffer, read_frame, write_frame};

use super::identity::Identity;
use super::pairing::PairingManager;
use crate::diag::MetricsRegistry;
use crate::media::{MediaCounters, MediaEvent, session::Sinks};
use crate::session::{SessionManager, State as SessionState};

const IDLE_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_VIDEO_BITRATE_KBPS: u32 = 25_000;

#[derive(Debug, Clone)]
pub enum ListenerEvent {
    Connected { name: String },
    Disconnected { clean: bool, reason: String },
    PairingSucceeded { device_id: String, name: String },
    MediaFirstFrame { decoder: String },
}

/// Everything needed to actually run the media plane when an offer arrives.
/// `None` in unit/integration tests that only exercise signalling.
#[derive(Clone)]
pub struct MediaHooks {
    pub sinks: Sinks,
    pub metrics: Arc<MetricsRegistry>,
}

pub struct NetContext {
    pub identity: Arc<Identity>,
    pub pairing: Arc<PairingManager>,
    pub session: Arc<SessionManager>,
    pub events_tx: mpsc::Sender<ListenerEvent>,
    pub idle_timeout: Duration,
    pub media: Option<MediaHooks>,
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
            media: None,
        }
    }

    #[must_use]
    pub fn with_media(mut self, hooks: MediaHooks) -> Self {
        self.media = Some(hooks);
        self
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ListenerStartError {
    #[error("failed to start QUIC listener on {addr}: {detail}")]
    Start { addr: SocketAddr, detail: String },
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

    let (certs, key) = ctx
        .identity
        .tls_material()
        .map_err(|e| fail(e.to_string()))?;
    super::rustls_crypto_provider();

    let mut server_crypto = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|e| fail(format!("invalid receiver identity: {e}")))?;
    server_crypto.alpn_protocols = vec![b"wdl/1".to_vec()];

    let quic_config = quinn::crypto::rustls::QuicServerConfig::try_from(server_crypto)
        .map_err(|e| fail(format!("quic config rejected identity: {e}")))?;
    let mut server_config = quinn::ServerConfig::with_crypto(Arc::new(quic_config));
    let mut transport = quinn::TransportConfig::default();
    super::media::enable_datagrams(&mut transport);
    server_config.transport_config(Arc::new(transport));
    let endpoint =
        quinn::Endpoint::server(server_config, bind_addr).map_err(|e| fail(e.to_string()))?;
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
            // Grace period so final frames (rejections, bye acks) are
            // delivered before the connection is torn down.
            tokio::time::sleep(Duration::from_millis(100)).await;
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

    let (proto_version, _hello_name, auth_token) = match read_frame(&mut recv).await {
        Ok(Message::Hello {
            proto_version,
            device_name,
            auth_token,
        }) => (proto_version, device_name, auth_token),
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

    // Protocol rule: the first message on the control stream is always
    // Hello. A valid token authenticates; anything else falls into the
    // pairing sub-flow on the same stream (or is refused cleanly).
    let known = auth_token
        .as_deref()
        .and_then(|t| ctx.pairing.find_by_token(t));
    let Some(device) = known else {
        return pairing_flow(conn, ctx, &mut send, &mut recv).await;
    };

    tracing::info!(device_id = %device.device_id, name = %device.name, "authenticated reconnect");
    let _ = ctx.session.transition(SessionState::Negotiating);
    ctx.session.set_peer(device.name.clone());
    let _ = ctx
        .events_tx
        .send(ListenerEvent::Connected {
            name: device.name.clone(),
        })
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

    let mut media: Option<MediaRuntime> = None;
    let (cmd_tx, mut cmd_rx) = mpsc::channel::<MediaCmd>(8);
    let outcome = serve_control_loop(
        &mut send,
        &mut recv,
        conn,
        ctx,
        &mut media,
        &cmd_tx,
        &mut cmd_rx,
    )
    .await;
    drop(media); // stops pipelines + watchdogs before FSM teardown
    outcome
}

async fn pairing_flow(
    conn: &quinn::Connection,
    ctx: &NetContext,
    send: &mut quinn::SendStream,
    recv: &mut quinn::RecvStream,
) -> SessionOutcome {
    let reject = |reason: String| Message::PairResult {
        accepted: false,
        reason: Some(reason.clone()),
        outcome: None,
    };

    if !ctx.pairing.window_is_open() {
        let reason: String = "not paired and no pairing window is open on the computer".into();
        let _ = write_frame(send, &reject(reason.clone())).await;
        return SessionOutcome::Clean(reason);
    }

    let (device_name, spake_message) = match read_frame(recv).await {
        Ok(Message::PairBegin {
            device_name,
            spake_message,
        }) => (device_name, spake_message),
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
    let outcome = match ctx
        .pairing
        .handle_pair_begin(&device_name, &spake_message, fingerprint)
    {
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

            // The phone can continue on this same connection immediately.
            let _ = ctx.session.transition(SessionState::Negotiating);
            ctx.session.set_peer(device.name.clone());
            let _ = ctx
                .events_tx
                .send(ListenerEvent::Connected {
                    name: device.name.clone(),
                })
                .await;
            if let Err(e) = write_frame(
                send,
                &Message::HelloAck {
                    proto_version: Version::CURRENT,
                    receiver_name: ctx.identity.host_name().to_string(),
                },
            )
            .await
            {
                return SessionOutcome::Lost(format!("failed to send HelloAck: {e}"));
            }
            let mut media: Option<MediaRuntime> = None;
            let (cmd_tx, mut cmd_rx) = mpsc::channel::<MediaCmd>(8);
            let outcome =
                serve_control_loop(send, recv, conn, ctx, &mut media, &cmd_tx, &mut cmd_rx).await;
            drop(media);
            outcome
        }
        Err(e) => {
            let reason = format!("pairing rejected: {e}");
            tracing::warn!(%reason, "pairing attempt failed at PairVerify");
            let _ = write_frame(send, &reject(reason.clone())).await;
            SessionOutcome::Clean(reason)
        }
    }
}

/// Commands injected into the control loop by the media watchdog/events tasks.
#[derive(Debug)]
enum MediaCmd {
    RequestKeyframe,
    Stall(String),
}

/// Owns everything a live media session needs; dropping it tears the media
/// plane down (pipelines → Null, ingest + watchdogs aborted).
struct MediaRuntime {
    _ingest: Option<super::media::MediaIngest>,
    _session: Option<crate::media::session::MediaSession>,
    helpers: Vec<tokio::task::JoinHandle<()>>,
}

impl MediaRuntime {
    fn is_live(&self) -> bool {
        self._session.is_some()
    }
}

impl Drop for MediaRuntime {
    fn drop(&mut self) {
        for h in self.helpers.splice(.., []) {
            h.abort();
        }
    }
}

async fn serve_control_loop(
    send: &mut quinn::SendStream,
    recv: &mut quinn::RecvStream,
    conn: &quinn::Connection,
    ctx: &NetContext,
    media: &mut Option<MediaRuntime>,
    cmd_tx: &mpsc::Sender<MediaCmd>,
    cmd_rx: &mut mpsc::Receiver<MediaCmd>,
) -> SessionOutcome {
    loop {
        tokio::select! {
            biased;
            cmd = cmd_rx.recv() => match cmd {
                Some(MediaCmd::RequestKeyframe) => {
                    if let Err(e) = write_frame(send, &Message::KeyframeRequest).await {
                        return SessionOutcome::Lost(format!("failed to send keyframe request: {e}"));
                    }
                }
                Some(MediaCmd::Stall(reason)) => return SessionOutcome::Lost(reason),
                None => {}
            },
            frame = tokio::time::timeout(ctx.idle_timeout, read_frame(recv)) => {
                match frame {
                    Err(_elapsed) => return SessionOutcome::Lost("idle timeout".into()),
                    // A FIN without an explicit Bye means the client vanished
                    // (crash, network drop, force-kill).
                    Ok(Err(CodecError::UnexpectedEof)) => {
                        return SessionOutcome::Lost("client closed connection without goodbye".into());
                    }
                    Ok(Err(e)) => return SessionOutcome::Lost(format!("control stream error: {e}")),
                    Ok(Ok(message)) => match message {
                        Message::Ping { sender_time_ms } => {
                            if let Err(e) = write_frame(
                                send,
                                &Message::Pong {
                                    echoed_time_ms: sender_time_ms,
                                },
                            )
                            .await
                            {
                                return SessionOutcome::Lost(format!("failed to pong: {e}"));
                            }
                        }
                        Message::ClockSync {
                            t1,
                            t2: _,
                            t3: _,
                            t4,
                        } => {
                            let now = unix_millis();
                            let reply = Message::ClockSync {
                                t1,
                                t2: now,
                                t3: now,
                                t4,
                            };
                            if let Err(e) = write_frame(send, &reply).await {
                                return SessionOutcome::Lost(format!("clock sync reply failed: {e}"));
                            }
                        }
                        Message::SessionOffer { video, audio } => {
                            let answer =
                                match start_media(conn, ctx, media, &video, &audio, cmd_tx) {
                                    Ok(a) => a,
                                    Err(reason) => Message::SessionAnswer {
                                        accepted: false,
                                        reason: Some(reason.clone()),
                                        max_video_bitrate_kbps: MAX_VIDEO_BITRATE_KBPS,
                                    },
                                };
                            let accepted = matches!(&answer, Message::SessionAnswer { accepted: true, .. });
                            tracing::info!(accepted, "session offer answered");
                            if let Err(e) = write_frame(send, &answer).await {
                                return SessionOutcome::Lost(format!("failed to answer offer: {e}"));
                            }
                        }
                        Message::KeyframeRequest => {
                            tracing::info!("phone requested a keyframe");
                        }
                        Message::BitrateHint { video_kbps } => {
                            ctx.metrics_text(&format!("sender bitrate hint {video_kbps} kbps"));
                            tracing::debug!(video_kbps, "bitrate hint from phone");
                        }
                        Message::Bye { reason } => {
                            let _ = write_frame(
                                send,
                                &Message::Bye {
                                    reason: format!("bye acknowledged: {reason}"),
                                },
                            )
                            .await;
                            return SessionOutcome::Clean(format!("client said bye: {reason}"));
                        }
                        other => {
                            let kind = kind_of(&other);
                            let _ = write_frame(
                                send,
                                &Message::Bye {
                                    reason: format!("unexpected message {kind}"),
                                },
                            )
                            .await;
                            return SessionOutcome::Clean(format!("unexpected message {kind} after auth"));
                        }
                    },
                }
            }
        }
    }
}

impl NetContext {
    fn metrics_text(&self, text: &str) {
        if let Some(hooks) = &self.media {
            hooks.metrics.set_text("net.last_hint", text);
        }
    }
}

fn start_media(
    conn: &quinn::Connection,
    ctx: &NetContext,
    slot: &mut Option<MediaRuntime>,
    video: &VideoOffer,
    audio: &AudioOffer,
    cmd_tx: &mpsc::Sender<MediaCmd>,
) -> Result<Message, String> {
    use crate::media::session as media_session;

    if slot.as_ref().is_some_and(MediaRuntime::is_live) {
        return Err("a media session is already active on this connection".into());
    }
    crate::media::validate_offer(video, audio)?;

    let Some(hooks) = ctx.media.clone() else {
        return Err("receiver media pipeline is not enabled".into());
    };
    if ctx.session.state() != SessionState::Negotiating {
        return Err("session is not in negotiating state".into());
    }

    let counters = Arc::new(MediaCounters::default());
    let (events_tx, events_rx) = std::sync::mpsc::channel::<MediaEvent>();
    let (video_tx, video_rx) = std::sync::mpsc::sync_channel::<Vec<u8>>(512);
    let (audio_tx, audio_rx) = std::sync::mpsc::sync_channel::<Vec<u8>>(1024);
    let ingest =
        super::media::MediaIngest::spawn(conn.clone(), video_tx, audio_tx, counters.clone());

    let vp = crate::media::VideoParams {
        width: video.width,
        height: video.height,
        fps: video.fps,
        bitrate_kbps: video.bitrate_kbps,
    };

    let session = media_session::MediaSession::start(
        Some(vp),
        Some((audio.sample_rate, audio.channels)),
        hooks.sinks.clone(),
        video_rx,
        audio_rx,
        counters.clone(),
        events_tx,
    )
    .map_err(|e| {
        tracing::error!(%e, "failed to build media pipelines");
        "receiver failed to build media pipelines".to_string()
    })?;

    let helpers = spawn_media_helpers(
        ctx.session.clone(),
        ctx.events_tx.clone(),
        counters,
        events_rx,
        cmd_tx.clone(),
        ingest.last_video.clone(),
        hooks.metrics.clone(),
    );

    *slot = Some(MediaRuntime {
        _ingest: Some(ingest),
        _session: Some(session),
        helpers,
    });

    tracing::info!(
        codec = %video.codec.name(),
        size = format!("{}x{}@{}", video.width, video.height, video.fps),
        audio = %audio.codec.name(),
        "media plane started"
    );

    Ok(Message::SessionAnswer {
        accepted: true,
        reason: None,
        max_video_bitrate_kbps: MAX_VIDEO_BITRATE_KBPS.min(video.bitrate_kbps.max(1)),
    })
}

/// Background helpers for a live media session:
/// 1. maps pipeline events → FSM transition / keyframe requests / UI
/// 2. media-idle watchdog (5 s silent → keyframe request, 7 s → stall)
/// 3. per-second fps/bitrate gauges
fn spawn_media_helpers(
    session: Arc<SessionManager>,
    ui_tx: mpsc::Sender<ListenerEvent>,
    counters: Arc<MediaCounters>,
    events_rx: std::sync::mpsc::Receiver<MediaEvent>,
    cmd_tx: mpsc::Sender<MediaCmd>,
    last_video: Arc<Mutex<Option<Instant>>>,
    metrics: Arc<MetricsRegistry>,
) -> Vec<tokio::task::JoinHandle<()>> {
    // 1. event pump
    let ev_session = session.clone();
    let ev_cmd = cmd_tx.clone();
    let events_task = tokio::spawn(async move {
        loop {
            match events_rx.recv_timeout(Duration::from_millis(200)) {
                Ok(MediaEvent::FirstVideoFrame { decoder }) => {
                    if ev_session.state() == SessionState::Negotiating {
                        match ev_session.transition(SessionState::Streaming) {
                            Ok(()) => {
                                tracing::info!(decoder = %decoder, "first frame decoded — streaming")
                            }
                            Err(e) => tracing::warn!(error = %e, "streaming transition refused"),
                        }
                    }
                    metrics.set_text("net.decoder", &decoder);
                    metrics.set_text("net.media", "streaming");
                    let _ = ui_tx.send(ListenerEvent::MediaFirstFrame { decoder }).await;
                }
                Ok(MediaEvent::VideoError { reason }) => {
                    tracing::warn!(%reason, "video pipeline error; requesting keyframe");
                    metrics.set_text("net.media", "recovering");
                    let _ = ev_cmd.send(MediaCmd::RequestKeyframe).await;
                }
                Ok(MediaEvent::AudioError { reason }) => {
                    tracing::warn!(%reason, "audio pipeline error");
                    metrics.set_text("net.audio", "error");
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return,
            }
        }
    });

    // 2. media idle watchdog
    let wd_cmd = cmd_tx;
    let watchdog_task = tokio::spawn(async move {
        let mut keyframe_requested = false;
        loop {
            tokio::time::sleep(Duration::from_millis(500)).await;
            if session.state() != SessionState::Streaming {
                keyframe_requested = false;
                continue;
            }
            let since_video = last_video
                .lock()
                .ok()
                .and_then(|guard| guard.map(|t| t.elapsed()));
            match since_video {
                Some(elapsed) if elapsed > Duration::from_secs(7) => {
                    tracing::warn!("no video for 7 s — failing session");
                    let _ = wd_cmd
                        .send(MediaCmd::Stall("no video data from phone".into()))
                        .await;
                    return;
                }
                Some(elapsed)
                    if elapsed > crate::media::MEDIA_IDLE_TIMEOUT && !keyframe_requested =>
                {
                    keyframe_requested = true;
                    tracing::info!("video stalled — requesting keyframe");
                    let _ = wd_cmd.send(MediaCmd::RequestKeyframe).await;
                }
                Some(_) => {}
                None => {}
            }
        }
    });

    // 3. throughput gauges
    let gauges_task = tokio::spawn(async move {
        let (mut f0, mut b0, mut a0, _) = counters.snapshot();
        loop {
            tokio::time::sleep(Duration::from_secs(1)).await;
            let (f1, b1, a1, dropped) = counters.snapshot();
            let fps = f1.saturating_sub(f0);
            let kbits = b1.saturating_sub(b0).saturating_mul(8) / 1000;
            let akbits = a1.saturating_sub(a0).saturating_mul(8) / 1000;
            metrics.set_gauge("net.video_fps", fps as f64);
            metrics.set_gauge("net.video_kbps", kbits as f64);
            metrics.set_gauge("net.audio_kbps", akbits as f64);
            metrics.set_gauge("net.dropped_datagrams", dropped as f64);
            f0 = f1;
            b0 = b1;
            a0 = a1;
        }
    });

    vec![events_task, watchdog_task, gauges_task]
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
