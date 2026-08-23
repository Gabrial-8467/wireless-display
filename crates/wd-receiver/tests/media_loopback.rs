//! End-to-end media loopback: an H.264 test pattern encoded with GStreamer
//! travels as RTP-over-QUIC-datagrams into the receiver's real ingest+decode
//! pipelines and drives the session FSM into Streaming
//! (docs/06-phase3-media-plan.md).

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use sha2::Digest;
use spake2::{Ed25519Group, Identity as SpakeIdentity, Password, Spake2};
use tokio::sync::mpsc;
use wd_protocol::{
    AudioCodec, AudioOffer, Message, Version, VideoCodec, VideoOffer, read_frame, write_frame,
};
use wd_receiver::diag::{MetricValue, MetricsRegistry};
use wd_receiver::media::session::Sinks;
use wd_receiver::net::{
    Identity, ListenerEvent, ListenerHandle, MediaHooks, NetContext, PairingManager,
};
use wd_receiver::session::State;

const CONFIRM_RX: &[u8] = b"wdl-confirm-receiver";
const CONFIRM_PHONE: &[u8] = b"wdl-confirm-phone";
const PHONE_IDENTITY: &[u8] = b"wdl-phone";
const RECEIVER_IDENTITY: &[u8] = b"wdl-receiver";

/// Upper bound for waiting on the first decoded frame after the offer.
const STREAMING_BUDGET: Duration = Duration::from_secs(20);

struct TestReceiver {
    handle: ListenerHandle,
    pairing: Arc<PairingManager>,
    session: Arc<wd_receiver::session::SessionManager>,
    events: mpsc::Receiver<ListenerEvent>,
    metrics: Option<Arc<MetricsRegistry>>,
    _dir: tempfile::TempDir,
}

fn init_test_logging() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| wd_receiver::diag::init_tracing("debug"));
}

fn init_gstreamer() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| gst::init().expect("gstreamer::init failed"));
}

async fn spawn_test_receiver(
    idle_timeout: Duration,
    with_media: bool,
) -> anyhow::Result<TestReceiver> {
    init_test_logging();
    let dir = tempfile::tempdir()?;
    let identity = Arc::new(Identity::load_or_create(dir.path())?);
    let pairing = Arc::new(PairingManager::new(&dir.path().join("paired.json")));
    let session = Arc::new(wd_receiver::session::SessionManager::new());
    let metrics = with_media.then(|| Arc::new(MetricsRegistry::new()));
    let (events_tx, events) = mpsc::channel(32);
    let mut ctx = NetContext::new(identity, pairing.clone(), session.clone(), events_tx);
    ctx.idle_timeout = idle_timeout;
    if let Some(metrics) = metrics.clone() {
        // Pipelines built on offer need an initialized GStreamer.
        init_gstreamer();
        ctx = ctx.with_media(MediaHooks {
            sinks: Sinks::default(),
            metrics,
        });
    }
    // Port 0 lets the OS pick a free port.
    let handle = wd_receiver::net::start_listener(ctx, "127.0.0.1:0".parse()?)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok(TestReceiver {
        handle,
        pairing,
        session,
        events,
        metrics,
        _dir: dir,
    })
}

// ---- Minimal client-side machinery (mirrors tools/mock-phone) ----

#[derive(Debug)]
struct SkipVerification;

impl rustls::client::danger::ServerCertVerifier for SkipVerification {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &rustls::crypto::ring::default_provider().signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &rustls::crypto::ring::default_provider().signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

fn client_endpoint() -> anyhow::Result<quinn::Endpoint> {
    let provider = rustls::crypto::ring::default_provider();
    let _ = provider.install_default();
    let mut crypto = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(SkipVerification))
        .with_no_client_auth();
    crypto.alpn_protocols = vec![b"wdl/1".to_vec()];
    let mut endpoint = quinn::Endpoint::client("127.0.0.1:0".parse()?)?;
    endpoint.set_default_client_config(quinn::ClientConfig::new(Arc::new(
        quinn::crypto::rustls::QuicClientConfig::try_from(crypto)?,
    )));
    Ok(endpoint)
}

async fn dial(addr: SocketAddr) -> anyhow::Result<(quinn::Endpoint, quinn::Connection)> {
    let endpoint = client_endpoint()?;
    let conn = endpoint.connect(addr, "wdl")?.await?;
    Ok((endpoint, conn))
}

fn confirmation(key: &[u8], suffix: &[u8]) -> [u8; 32] {
    let mut hasher = sha2::Sha256::new();
    hasher.update(key);
    hasher.update(suffix);
    hasher.finalize().into()
}

/// Runs one pairing attempt as a phone would. Returns the PairResult.
async fn attempt_pair(
    addr: SocketAddr,
    code_used: &str,
    phone_name: &str,
) -> anyhow::Result<Message> {
    let (_ep, conn) = dial(addr).await?;
    let (mut send, mut recv) = conn.open_bi().await?;

    // Protocol rule: Hello always comes first (no token yet).
    write_frame(
        &mut send,
        &Message::Hello {
            proto_version: Version::CURRENT,
            device_name: phone_name.into(),
            auth_token: None,
        },
    )
    .await?;

    let (spake, msg_a) = Spake2::<Ed25519Group>::start_a(
        &Password::new(code_used),
        &SpakeIdentity::new(PHONE_IDENTITY),
        &SpakeIdentity::new(RECEIVER_IDENTITY),
    );
    write_frame(
        &mut send,
        &Message::PairBegin {
            device_name: phone_name.into(),
            spake_message: msg_a,
        },
    )
    .await?;

    // The receiver may refuse immediately (e.g. no window open) without
    // ever sending a challenge.
    let first = read_frame(&mut recv)
        .await
        .map_err(|e| anyhow::anyhow!("read after PairBegin failed: {e}"))?;
    let (reply, rx_confirm) = match first {
        Message::PairResult { .. } => return Ok(first),
        Message::PairChallenge {
            spake_reply,
            receiver_confirmation,
            ..
        } => (spake_reply, receiver_confirmation),
        other => panic!("expected PairChallenge or PairResult, got {other:?}"),
    };
    let key = spake
        .finish(&reply)
        .map_err(|e| anyhow::anyhow!("spake finish failed: {e}"))?;
    // With a wrong code both sides still derive keys — they just disagree.
    // The confirmation check is what actually detects the bad code.
    anyhow::ensure!(
        confirmation(&key, CONFIRM_RX) == rx_confirm,
        "wrong pairing code: receiver confirmation mismatch"
    );

    write_frame(
        &mut send,
        &Message::PairVerify {
            phone_confirmation: confirmation(&key, CONFIRM_PHONE),
        },
    )
    .await?;
    read_frame(&mut recv)
        .await
        .map_err(|e| anyhow::anyhow!("read PairResult failed: {e}"))
}

async fn recv_event(
    events: &mut mpsc::Receiver<ListenerEvent>,
    budget: Duration,
) -> anyhow::Result<ListenerEvent> {
    tokio::time::timeout(budget, events.recv())
        .await
        .map_err(|_| anyhow::anyhow!("timed out waiting for listener event"))?
        .ok_or_else(|| anyhow::anyhow!("event channel closed"))
}

// ---- Shared session helpers ----

/// Pairs for real and returns the issued device token. Waits for the FSM to
/// recycle to Idle and drops stale events from the now-closed pairing
/// connection so later event assertions start from a clean slate.
async fn pair_for_token(rx: &mut TestReceiver, phone_name: &str) -> anyhow::Result<String> {
    let code = rx.pairing.open_window();
    let outcome = match attempt_pair(rx.handle.local_addr(), &code, phone_name).await? {
        Message::PairResult {
            accepted: true,
            outcome: Some(o),
            ..
        } => o,
        other => panic!("pairing should succeed, got {other:?}"),
    };
    wait_for_state(&rx.session, State::Idle, Duration::from_secs(3)).await?;
    flush_events(&mut rx.events);
    Ok(outcome.device_token)
}

async fn connect_authenticated(
    addr: SocketAddr,
    phone_name: &str,
    token: &str,
) -> anyhow::Result<(quinn::Connection, quinn::SendStream, quinn::RecvStream)> {
    let (_ep, conn) = dial(addr).await?;
    let (mut send, mut recv) = conn.open_bi().await?;
    write_frame(
        &mut send,
        &Message::Hello {
            proto_version: Version::CURRENT,
            device_name: phone_name.into(),
            auth_token: Some(token.into()),
        },
    )
    .await?;
    match read_frame(&mut recv).await? {
        Message::HelloAck { .. } => Ok((conn, send, recv)),
        other => panic!("expected HelloAck, got {other:?}"),
    }
}

fn video_offer(width: u16, height: u16, fps: u8, bitrate_kbps: u32) -> VideoOffer {
    VideoOffer {
        codec: VideoCodec::H264Baseline,
        width,
        height,
        fps,
        bitrate_kbps,
    }
}

fn opus_stereo() -> AudioOffer {
    AudioOffer {
        codec: AudioCodec::Opus,
        sample_rate: 48_000,
        channels: 2,
    }
}

async fn exchange_offer(
    send: &mut quinn::SendStream,
    recv: &mut quinn::RecvStream,
    video: VideoOffer,
    audio: AudioOffer,
) -> anyhow::Result<Message> {
    write_frame(send, &Message::SessionOffer { video, audio }).await?;
    Ok(
        tokio::time::timeout(Duration::from_secs(5), read_frame(recv))
            .await
            .map_err(|_| anyhow::anyhow!("no SessionAnswer within 5 s"))??,
    )
}

async fn wait_for_state(
    session: &wd_receiver::session::SessionManager,
    target: State,
    budget: Duration,
) -> anyhow::Result<()> {
    let deadline = tokio::time::Instant::now() + budget;
    while tokio::time::Instant::now() < deadline {
        if session.state() == target {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    anyhow::bail!(
        "timed out waiting for state {target}, stuck at {}",
        session.state()
    )
}

fn flush_events(events: &mut mpsc::Receiver<ListenerEvent>) {
    while events.try_recv().is_ok() {}
}

// ---- Sender-side encoder (mirrors the mock-phone cast pipeline) ----

/// Encodes a synthetic test pattern into RTP/H.264 packets exactly like the
/// planned mock-phone `cast` subcommand: one RTP packet per appsink buffer.
fn encode_test_pattern() -> anyhow::Result<Vec<Vec<u8>>> {
    init_gstreamer();
    let pipeline = gst::parse::launch(
        "videotestsrc num-buffers=120 is-live=true \
         ! video/x-raw,format=I420,width=320,height=240,framerate=30/1 \
         ! openh264enc complexity=0 gop-size=30 \
         ! video/x-h264,stream-format=byte-stream,alignment=au \
         ! rtph264pay pt=96 mtu=1100 config-interval=-1 \
         ! appsink name=sink sync=false",
    )?
    .downcast::<gst::Pipeline>()
    .map_err(|_| anyhow::anyhow!("gst::parse::launch did not yield a pipeline"))?;

    let appsink = pipeline
        .by_name("sink")
        .expect("appsink named sink exists")
        .downcast::<gst_app::AppSink>()
        .map_err(|_| anyhow::anyhow!("sink element is not an appsink"))?;

    let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
    appsink.set_callbacks(
        gst_app::AppSinkCallbacks::builder()
            .new_sample(move |sink| {
                let sample = sink.pull_sample().map_err(|_| gst::FlowError::Error)?;
                let buffer = sample.buffer().ok_or(gst::FlowError::Error)?;
                let map = buffer.map_readable().map_err(|_| gst::FlowError::Error)?;
                tx.send(map.as_slice().to_vec())
                    .map_err(|_| gst::FlowError::Error)?;
                Ok(gst::FlowSuccess::Ok)
            })
            .build(),
    );

    pipeline.set_state(gst::State::Playing)?;

    let bus = pipeline.bus().expect("pipeline has a bus");
    let poll =
        gst::ClockTime::try_from(Duration::from_millis(200)).expect("200 ms is a valid clock time");
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    let mut eos = false;
    while std::time::Instant::now() < deadline {
        if let Some(msg) =
            bus.timed_pop_filtered(poll, &[gst::MessageType::Eos, gst::MessageType::Error])
        {
            match msg.view() {
                gst::MessageView::Eos(_) => {
                    eos = true;
                    break;
                }
                gst::MessageView::Error(err) => {
                    anyhow::bail!(
                        "encode pipeline failed: {} ({:?})",
                        err.error(),
                        err.debug()
                    );
                }
                _ => {}
            }
        }
    }
    drop(appsink);
    drop(pipeline);

    anyhow::ensure!(eos, "encoder did not reach EOS within its time budget");
    let packets: Vec<Vec<u8>> = rx.into_iter().collect();
    anyhow::ensure!(!packets.is_empty(), "appsink produced no RTP packets");
    Ok(packets)
}

// ---- Tests ----

#[tokio::test]
async fn offer_without_media_hooks_is_rejected() -> anyhow::Result<()> {
    let mut rx = spawn_test_receiver(Duration::from_secs(10), false).await?;
    let token = pair_for_token(&mut rx, "Plain Phone").await?;
    let (_conn, mut send, mut recv) =
        connect_authenticated(rx.handle.local_addr(), "Plain Phone", &token).await?;

    let answer = exchange_offer(
        &mut send,
        &mut recv,
        video_offer(1280, 720, 30, 4000),
        opus_stereo(),
    )
    .await?;
    match answer {
        Message::SessionAnswer {
            accepted: false,
            reason: Some(reason),
            ..
        } => {
            assert!(reason.contains("media"), "{reason}");
        }
        other => panic!("expected rejection without media hooks, got {other:?}"),
    }

    rx.handle.shutdown();
    Ok(())
}

#[tokio::test]
async fn test_pattern_reaches_streaming() -> anyhow::Result<()> {
    let mut rx = spawn_test_receiver(Duration::from_secs(30), true).await?;

    // Encode before touching the control stream so the idle timeout can never
    // fire mid-setup. Runs on the blocking pool; GStreamer threads do the work.
    let packets = tokio::task::spawn_blocking(encode_test_pattern)
        .await
        .map_err(|e| anyhow::anyhow!("encoder task panicked: {e}"))??;

    let token = pair_for_token(&mut rx, "Loopback Phone").await?;
    let addr = rx.handle.local_addr();
    let (conn, mut send, mut recv) = connect_authenticated(addr, "Loopback Phone", &token).await?;

    let answer = exchange_offer(
        &mut send,
        &mut recv,
        video_offer(320, 240, 30, 800),
        opus_stereo(),
    )
    .await?;
    assert!(
        matches!(&answer, Message::SessionAnswer { accepted: true, .. }),
        "offer must be accepted, got {answer:?}"
    );

    // One RTP packet per QUIC datagram, lightly throttled in batches of ten.
    let mut sent = 0usize;
    for (i, packet) in packets.iter().enumerate() {
        // Non-blocking: fails only when the QUIC datagram queue is full.
        if conn.send_datagram(packet.clone().into()).is_ok() {
            sent += 1;
        }
        if i % 10 == 9 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }
    assert!(sent > 0, "no datagrams made it into the send queue");

    // The FSM enters Streaming exactly when the first AU leaves the decoder,
    // and only then is MediaFirstFrame emitted — so receiving the event also
    // proves the transition happened.
    let decoder = loop {
        match recv_event(&mut rx.events, STREAMING_BUDGET).await? {
            ListenerEvent::MediaFirstFrame { decoder } => break decoder,
            ListenerEvent::Disconnected { clean, reason } => {
                panic!("disconnected before first frame (clean={clean}): {reason}")
            }
            ListenerEvent::Connected { .. } | ListenerEvent::PairingSucceeded { .. } => {}
        }
    };
    assert!(!decoder.is_empty());
    assert_eq!(rx.session.state(), State::Streaming);

    // Throughput gauges update once per second while streaming.
    let metrics = rx.metrics.as_ref().expect("media hooks installed");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    loop {
        if let Some(MetricValue::Gauge(fps)) = metrics.snapshot().get("net.video_fps") {
            assert!(*fps > 0.0, "net.video_fps gauge stuck at zero");
            break;
        }
        anyhow::ensure!(
            tokio::time::Instant::now() < deadline,
            "net.video_fps never appeared above zero in metrics snapshot"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // Graceful goodbye: receiver echoes Bye and tears down cleanly.
    write_frame(
        &mut send,
        &Message::Bye {
            reason: "loopback complete".into(),
        },
    )
    .await?;
    match read_frame(&mut recv).await {
        Ok(Message::Bye { .. }) | Err(_) => {}
        other => panic!("unexpected reply after bye: {other:?}"),
    }
    loop {
        match recv_event(&mut rx.events, Duration::from_secs(5)).await? {
            ListenerEvent::Disconnected { clean, reason } => {
                assert!(clean, "bye must produce a clean disconnect: {reason}");
                break;
            }
            ListenerEvent::MediaFirstFrame { .. }
            | ListenerEvent::Connected { .. }
            | ListenerEvent::PairingSucceeded { .. } => {}
        }
    }
    wait_for_state(&rx.session, State::Idle, Duration::from_secs(3)).await?;

    rx.handle.shutdown();
    Ok(())
}

#[tokio::test]
async fn double_offer_is_refused() -> anyhow::Result<()> {
    let mut rx = spawn_test_receiver(Duration::from_secs(10), true).await?;
    let token = pair_for_token(&mut rx, "Greedy Phone").await?;
    let (_conn, mut send, mut recv) =
        connect_authenticated(rx.handle.local_addr(), "Greedy Phone", &token).await?;

    let first = exchange_offer(
        &mut send,
        &mut recv,
        video_offer(320, 240, 30, 800),
        opus_stereo(),
    )
    .await?;
    assert!(
        matches!(&first, Message::SessionAnswer { accepted: true, .. }),
        "first offer must be accepted, got {first:?}"
    );

    let second = exchange_offer(
        &mut send,
        &mut recv,
        video_offer(320, 240, 30, 800),
        opus_stereo(),
    )
    .await?;
    match second {
        Message::SessionAnswer {
            accepted: false,
            reason: Some(reason),
            ..
        } => {
            assert!(reason.contains("already active"), "{reason}");
        }
        other => panic!("expected second offer to be refused, got {other:?}"),
    }

    rx.handle.shutdown();
    Ok(())
}
