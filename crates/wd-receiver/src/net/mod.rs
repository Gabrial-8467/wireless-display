pub mod identity;
pub mod listener;
pub mod pairing;

use std::net::SocketAddr;
use std::sync::Arc;

pub use identity::Identity;
pub use listener::{ListenerEvent, ListenerHandle, NetContext, start_listener};
pub use pairing::{PairingManager, PairedDevice};

#[derive(Debug, thiserror::Error)]
pub enum NetError {
    #[error("quinn transport error: {0}")]
    Quic(#[from] quinn::ConnectionError),
    #[error("stream read error: {0}")]
    Read(#[from] quinn::ReadExactError),
    #[error("stream write error: {0}")]
    Write(#[from] quinn::WriteError),
    #[error("protocol error: {0}")]
    Protocol(#[from] wd_protocol::CodecError),
}

pub fn rustls_crypto_provider() -> &'static Arc<rustls::crypto::CryptoProvider> {
    static PROVIDER: std::sync::OnceLock<Arc<rustls::crypto::CryptoProvider>> =
        std::sync::OnceLock::new();
    PROVIDER.get_or_init(|| {
        // Another component (e.g. tests) may already have installed one.
        if let Some(existing) = rustls::crypto::CryptoProvider::get_default() {
            return existing.clone();
        }
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let _ = rustls::crypto::CryptoProvider::install_default(provider.clone());
        provider
    })
}

pub fn endpoint_local_addr(endpoint: &quinn::Endpoint) -> SocketAddr {
    endpoint.local_addr().unwrap_or_else(|_| {
        "0.0.0.0:0"
            .parse()
            .expect("fallback socket addr is always valid")
    })
}
