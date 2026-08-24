//! QUIC client-side protocol logic (mirrors tools/mock-phone client.rs).

use std::net::SocketAddr;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Context as _;
use sha2::Digest;
use spake2::{Ed25519Group, Identity as SpakeIdentity, Password, Spake2};
use wd_protocol::{Message, PairingOutcomeInfo, Version, read_frame, write_frame};

const CONFIRM_RX: &[u8] = b"wdl-confirm-receiver";
const CONFIRM_PHONE: &[u8] = b"wdl-confirm-phone";
const PHONE_IDENTITY: &[u8] = b"wdl-phone";
const RECEIVER_IDENTITY: &[u8] = b"wdl-receiver";

/// The pairing code is the security anchor (SPAKE2); the server cert is
/// unauthenticated by design.
#[derive(Debug)]
struct SkipServerVerification;

impl rustls::client::danger::ServerCertVerifier for SkipServerVerification {
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

pub async fn connect(addr: SocketAddr) -> anyhow::Result<quinn::Connection> {
    let provider = rustls::crypto::ring::default_provider();
    let _ = provider.install_default();

    let mut crypto = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(std::sync::Arc::new(SkipServerVerification))
        .with_no_client_auth();
    crypto.alpn_protocols = vec![b"wdl/1".to_vec()];

    let client_config = quinn::ClientConfig::new(std::sync::Arc::new(
        quinn::crypto::rustls::QuicClientConfig::try_from(crypto)
            .context("quic rejected client tls config")?,
    ));
    let mut endpoint =
        quinn::Endpoint::client("0.0.0.0:0".parse().context("bind ephemeral port")?)?;
    endpoint.set_default_client_config(client_config);

    let connecting = endpoint
        .connect(addr, "wdl")
        .context("QUIC connect failed")?;
    let conn = tokio::time::timeout(Duration::from_secs(6), connecting)
        .await
        .context("connection attempt timed out")?
        .map_err(|e| anyhow::anyhow!("QUIC handshake failed: {e}"))?;
    Ok(conn)
}

fn confirmation(key: &[u8], suffix: &[u8]) -> [u8; 32] {
    let mut hasher = sha2::Sha256::new();
    hasher.update(key);
    hasher.update(suffix);
    hasher.finalize().into()
}

pub async fn pair_with_receiver(
    conn: &quinn::Connection,
    phone_name: &str,
    code: &str,
) -> anyhow::Result<PairingOutcomeInfo> {
    let (mut send, mut recv) = conn.open_bi().await.context("open control stream")?;

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
        &Password::new(code),
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

    let (spake_reply, rx_confirmation) = match read_frame(&mut recv).await? {
        Message::PairChallenge {
            spake_reply,
            receiver_confirmation,
            ..
        } => (spake_reply, receiver_confirmation),
        other => anyhow::bail!("expected PairChallenge, got {:?}", kind_of(&other)),
    };

    let key = spake
        .finish(&spake_reply)
        .map_err(|_| anyhow::anyhow!("pairing code rejected by receiver"))?;

    if confirmation(&key, CONFIRM_RX) != rx_confirmation {
        anyhow::bail!("receiver failed confirmation check");
    }
    write_frame(
        &mut send,
        &Message::PairVerify {
            phone_confirmation: confirmation(&key, CONFIRM_PHONE),
        },
    )
    .await?;

    match read_frame(&mut recv).await? {
        Message::PairResult {
            accepted: true,
            outcome: Some(outcome),
            ..
        } => match read_frame(&mut recv).await? {
            Message::HelloAck { .. } => Ok(outcome),
            other => anyhow::bail!(
                "expected HelloAck after pairing, got {:?}",
                kind_of(&other)
            ),
        },
        Message::PairResult {
            accepted: false,
            reason,
            ..
        } => anyhow::bail!(
            "receiver refused pairing: {}",
            reason.unwrap_or_else(|| "unknown reason".into())
        ),
        other => anyhow::bail!("expected PairResult, got {:?}", kind_of(&other)),
    }
}

pub async fn authenticate(
    conn: &quinn::Connection,
    phone_name: &str,
    token: &str,
) -> anyhow::Result<(quinn::SendStream, quinn::RecvStream)> {
    let (mut send, mut recv) = conn.open_bi().await.context("open control stream")?;
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
        Message::HelloAck {
            receiver_name: _,
            proto_version,
        } => {
            anyhow::ensure!(
                Version::CURRENT.compatible_with(proto_version),
                "receiver speaks incompatible protocol {proto_version}"
            );
            Ok((send, recv))
        }
        other => anyhow::bail!("expected HelloAck, got {:?}", kind_of(&other)),
    }
}

pub fn kind_of(message: &Message) -> &'static str {
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

pub fn unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
