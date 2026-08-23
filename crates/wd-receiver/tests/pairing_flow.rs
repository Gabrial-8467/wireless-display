//! End-to-end tests of the receiver network stack over real QUIC on loopback:
//! SPAKE2 pairing, token re-authentication, framing robustness and drop detection.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use sha2::Digest;
use spake2::{Ed25519Group, Identity as SpakeIdentity, Password, Spake2};
use tokio::sync::mpsc;
use wd_protocol::{Message, Version, read_frame, write_frame};
use wd_receiver::net::{Identity, ListenerEvent, ListenerHandle, NetContext, PairingManager};

const CONFIRM_RX: &[u8] = b"wdl-confirm-receiver";
const CONFIRM_PHONE: &[u8] = b"wdl-confirm-phone";
const PHONE_IDENTITY: &[u8] = b"wdl-phone";
const RECEIVER_IDENTITY: &[u8] = b"wdl-receiver";

struct TestReceiver {
    handle: ListenerHandle,
    pairing: Arc<PairingManager>,
    session: Arc<wd_receiver::session::SessionManager>,
    events: mpsc::Receiver<ListenerEvent>,
    _dir: tempfile::TempDir,
}

fn init_test_logging() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| wd_receiver::diag::init_tracing("debug"));
}

async fn spawn_test_receiver(idle_timeout: Duration) -> anyhow::Result<TestReceiver> {
    init_test_logging();
    let dir = tempfile::tempdir()?;
    let identity = Arc::new(Identity::load_or_create(dir.path())?);
    let pairing = Arc::new(PairingManager::new(&dir.path().join("paired.json")));
    let session = Arc::new(wd_receiver::session::SessionManager::new());
    let (events_tx, events) = mpsc::channel(32);
    let ctx = NetContext {
        identity,
        pairing: pairing.clone(),
        session: session.clone(),
        events_tx,
        idle_timeout,
        media: None,
    };
    // Port 0 lets the OS pick a free port.
    let handle = wd_receiver::net::start_listener(ctx, "127.0.0.1:0".parse()?)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok(TestReceiver {
        handle,
        pairing,
        session,
        events,
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

// ---- Tests ----

#[tokio::test]
async fn wrong_pairing_code_is_rejected_and_nothing_persists() -> anyhow::Result<()> {
    let rx = spawn_test_receiver(Duration::from_secs(5)).await?;
    let real_code = rx.pairing.open_window();
    assert_ne!(
        real_code, "000001",
        "random codes should not be trivially guessable"
    );

    // The client detects the mismatch via the receiver's confirmation hash.
    let result = attempt_pair(rx.handle.local_addr(), "000001", "Sneaky Phone").await;
    assert!(result.is_err(), "wrong code must fail confirmation check");

    // The receiver never registered anything.
    assert!(rx.pairing.list_devices().is_empty());
    // The half-finished window stays open until its TTL expires.
    assert!(rx.pairing.window_is_open());
    rx.handle.shutdown();
    Ok(())
}

#[tokio::test]
async fn pairing_without_open_window_is_refused_cleanly() -> anyhow::Result<()> {
    let rx = spawn_test_receiver(Duration::from_secs(5)).await?;
    let result = attempt_pair(rx.handle.local_addr(), "123456", "Lonely Phone").await?;
    match result {
        Message::PairResult {
            accepted: false,
            reason: Some(reason),
            ..
        } => {
            assert!(reason.contains("no pairing window"), "{reason}");
        }
        other => panic!("expected refusal, got {other:?}"),
    }
    rx.handle.shutdown();
    Ok(())
}

#[tokio::test]
async fn successful_pairing_then_token_reconnect_receives_hello_ack() -> anyhow::Result<()> {
    let rx = spawn_test_receiver(Duration::from_secs(5)).await?;
    let addr = rx.handle.local_addr();

    // Pair for real, capturing the code returned by open_window().
    let code = rx.pairing.open_window();
    let outcome = match attempt_pair(addr, &code, "Integration Phone").await? {
        Message::PairResult {
            accepted: true,
            outcome: Some(o),
            ..
        } => o,
        other => panic!("pairing should succeed, got {other:?}"),
    };
    assert_eq!(outcome.device_token.len(), 64);

    // A fresh connection presenting the issued token is authenticated.
    let (_ep, conn) = dial(addr).await?;
    let (mut send, mut recv) = conn.open_bi().await?;
    write_frame(
        &mut send,
        &Message::Hello {
            proto_version: Version::CURRENT,
            device_name: "Integration Phone".into(),
            auth_token: Some(outcome.device_token),
        },
    )
    .await?;
    match read_frame(&mut recv).await? {
        Message::HelloAck {
            proto_version,
            receiver_name,
        } => {
            assert!(Version::CURRENT.compatible_with(proto_version));
            assert!(!receiver_name.is_empty());
        }
        other => panic!("expected HelloAck, got {other:?}"),
    }

    // Graceful goodbye: receiver answers Bye and FSM returns to Idle.
    write_frame(
        &mut send,
        &Message::Bye {
            reason: "test done".into(),
        },
    )
    .await?;
    match read_frame(&mut recv).await {
        Ok(Message::Bye { .. }) | Err(_) => {}
        other => panic!("unexpected reply after bye: {other:?}"),
    }
    drop(conn);
    rx.handle.shutdown();
    Ok(())
}

#[tokio::test]
async fn stale_token_is_sent_to_pairing_flow_and_refused_without_window() -> anyhow::Result<()> {
    let rx = spawn_test_receiver(Duration::from_secs(5)).await?;
    let (_ep, conn) = dial(rx.handle.local_addr()).await?;
    let (mut send, mut recv) = conn.open_bi().await?;
    write_frame(
        &mut send,
        &Message::Hello {
            proto_version: Version::CURRENT,
            device_name: "Ghost Phone".into(),
            auth_token: Some(hex::encode([0u8; 32])),
        },
    )
    .await?;
    // Unknown token → falls into pairing path → no window open → refusal.
    match read_frame(&mut recv).await? {
        Message::PairResult {
            accepted: false,
            reason: Some(reason),
            ..
        } => {
            assert!(reason.contains("no pairing window"), "{reason}");
        }
        other => panic!("expected pairing refusal, got {other:?}"),
    }
    rx.handle.shutdown();
    Ok(())
}

#[tokio::test]
async fn garbage_after_hello_terminates_connection_without_panic() -> anyhow::Result<()> {
    let rx = spawn_test_receiver(Duration::from_secs(2)).await?;
    let code = rx.pairing.open_window();
    let outcome = match attempt_pair(rx.handle.local_addr(), &code, "Fuzz Phone").await? {
        Message::PairResult {
            accepted: true,
            outcome: Some(o),
            ..
        } => o,
        other => panic!("pairing should succeed, got {other:?}"),
    };

    let (_ep, conn) = dial(rx.handle.local_addr()).await?;
    let (mut send, mut _recv) = conn.open_bi().await?;
    write_frame(
        &mut send,
        &Message::Hello {
            proto_version: Version::CURRENT,
            device_name: "Fuzz Phone".into(),
            auth_token: Some(outcome.device_token),
        },
    )
    .await?;
    // Oversized declared length must make the server hang up, not crash.
    use tokio::io::AsyncWriteExt as _;
    send.write_all(&(u32::MAX).to_le_bytes()).await?;
    send.flush().await?;

    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while tokio::time::Instant::now() < deadline {
        if matches!(rx.session.state(), wd_receiver::session::State::Idle) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(
        rx.session.state(),
        wd_receiver::session::State::Idle,
        "session must recycle to Idle after protocol violation"
    );
    rx.handle.shutdown();
    Ok(())
}

#[tokio::test]
async fn silent_client_hits_idle_timeout_and_disconnect_event_fires() -> anyhow::Result<()> {
    let idle = Duration::from_millis(400);
    let mut rx = spawn_test_receiver(idle).await?;
    let code = rx.pairing.open_window();
    let outcome = match attempt_pair(rx.handle.local_addr(), &code, "Quiet Phone").await? {
        Message::PairResult {
            accepted: true,
            outcome: Some(o),
            ..
        } => o,
        other => panic!("pairing should succeed, got {other:?}"),
    };

    let (_ep, conn) = dial(rx.handle.local_addr()).await?;
    let (mut send, mut hello_recv) = conn.open_bi().await?;
    write_frame(
        &mut send,
        &Message::Hello {
            proto_version: Version::CURRENT,
            device_name: "Quiet Phone".into(),
            auth_token: Some(outcome.device_token),
        },
    )
    .await?;
    let ack = tokio::time::timeout(Duration::from_secs(2), read_frame(&mut hello_recv))
        .await
        .expect("hello ack timeout")?;
    assert!(matches!(ack, Message::HelloAck { .. }));
    drop(send);
    drop(conn); // vanish without Bye

    // Expect a Disconnected event (unclean) within ~8× the idle timeout.
    loop {
        match recv_event(&mut rx.events, idle * 8).await? {
            ListenerEvent::Disconnected { clean, reason } => {
                assert!(!clean, "vanishing client is unclean: {reason}");
                break;
            }
            ListenerEvent::PairingSucceeded { name, .. } => {
                assert_eq!(name, "Quiet Phone");
            }
            // Connected fires right after successful pairing; keep waiting.
            ListenerEvent::Connected { name } => assert_eq!(name, "Quiet Phone"),
            ListenerEvent::MediaFirstFrame { .. } => {}
        }
    }
    rx.handle.shutdown();
    Ok(())
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
