use std::fs;
use std::path::{Path, PathBuf};

use rcgen::{CertifiedKey, generate_simple_self_signed};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use sha2::{Digest, Sha256};

const CERT_FILE: &str = "identity-cert.der";
const KEY_FILE: &str = "identity-key.pk8";

/// Long-lived receiver identity: a self-signed certificate whose SHA-256
/// fingerprint is shown to users for out-of-band verification.
pub struct Identity {
    cert_der: Vec<u8>,
    key_der: Vec<u8>,
    fingerprint_hex: String,
    host_name: String,
}

impl Identity {
    pub fn load_or_create(dir: &Path) -> anyhow::Result<Self> {
        fs::create_dir_all(dir)?;
        let cert_path = dir.join(CERT_FILE);
        let key_path = dir.join(KEY_FILE);

        let (cert_der, key_der) = if cert_path.exists() && key_path.exists() {
            (
                fs::read(&cert_path)?,
                fs::read(&key_path)?,
            )
        } else {
            tracing::info!(dir = %dir.display(), "generating new receiver identity");
            let host = host_name();
            let CertifiedKey { cert, signing_key } = generate_simple_self_signed(vec![format!(
                "wireless-display@{host}"
            )])?;
            let cert_der = cert.der().to_vec();
            let key_der = signing_key.serialize_der();
            write_private(&key_path, &key_der)?;
            fs::write(&cert_path, &cert_der)?;
            (cert_der, key_der)
        };

        let mut hasher = Sha256::new();
        hasher.update(&cert_der);
        let fingerprint_hex = hex::encode(hasher.finalize());

        Ok(Self {
            cert_der,
            key_der,
            fingerprint_hex,
            host_name: host_name(),
        })
    }

    pub fn fingerprint_hex(&self) -> &str {
        &self.fingerprint_hex
    }

    pub fn fingerprint_short(&self) -> String {
        self.fingerprint_hex.chars().take(12).collect()
    }

    pub fn host_name(&self) -> &str {
        &self.host_name
    }

    pub fn tls_material(&self) -> anyhow::Result<(Vec<CertificateDer<'static>>, PrivateKeyDer<'static>)> {
        let cert = CertificateDer::from(self.cert_der.clone());
        let key = PrivatePkcs8KeyDer::from(self.key_der.clone()).into();
        Ok((vec![cert], key))
    }
}

fn write_private(path: &PathBuf, bytes: &[u8]) -> anyhow::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(bytes)?;
    Ok(())
}

fn host_name() -> String {
    fs::read_to_string("/proc/sys/kernel/hostname")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "linux-desktop".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_is_stable_and_fingerprinted() {
        let dir = tempfile::tempdir().unwrap();
        let id1 = Identity::load_or_create(dir.path()).unwrap();
        let fp1 = id1.fingerprint_hex().to_string();
        assert_eq!(fp1.len(), 64);
        assert!(!id1.host_name().is_empty());
        assert_eq!(id1.fingerprint_short(), fp1[..12]);

        let id2 = Identity::load_or_create(dir.path()).unwrap();
        assert_eq!(fp1, id2.fingerprint_hex(), "identity must persist across restarts");

        let (certs, key) = id2.tls_material().unwrap();
        assert_eq!(certs.len(), 1);
        assert!(!key.secret_der().is_empty());
    }
}
