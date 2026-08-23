use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Instant;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use spake2::{Ed25519Group, Identity as SpakeIdentity, Password, Spake2};

pub const PAIRING_CODE_TTL_SECS: u64 = 120;
const CONFIRM_RX: &[u8] = b"wdl-confirm-receiver";
const CONFIRM_PHONE: &[u8] = b"wdl-confirm-phone";
const PHONE_IDENTITY: &[u8] = b"wdl-phone";
const RECEIVER_IDENTITY: &[u8] = b"wdl-receiver";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PairedDevice {
    pub device_id: String,
    pub name: String,
    pub token: String,
    pub added_unix_secs: u64,
}

#[derive(Debug)]
enum PairingStage {
    Waiting,
    KeyDerived { key: Vec<u8>, device_name: String },
}

struct ActivePairing {
    code: String,
    expires_at: Instant,
    stage: PairingStage,
}

impl ActivePairing {
    fn expired(&self) -> bool {
        Instant::now() >= self.expires_at
    }
}

#[derive(Debug, thiserror::Error)]
pub enum PairingError {
    #[error("no pairing window is open")]
    NoWindow,
    #[error("pairing window has expired")]
    Expired,
    #[error("pairing handshake out of order")]
    OutOfOrder,
    #[error("pairing code rejected")]
    BadCode,
}

/// Owns the pairing window (6-digit code), the SPAKE2 exchange state and the
/// persistent store of paired devices.
pub struct PairingManager {
    active: Mutex<Option<ActivePairing>>,
    store: Mutex<PairedStore>,
}

#[derive(Default)]
struct PairedStore {
    path: Option<PathBuf>,
    devices: BTreeMap<String, PairedDevice>,
}

impl PairedStore {
    fn load(path: &Path) -> Self {
        let devices = fs::read(path)
            .ok()
            .and_then(|bytes| {
                serde_json::from_slice::<Vec<PairedDevice>>(&bytes)
                    .map_err(|e| tracing::warn!(error = %e, "corrupt paired-devices file"))
                    .ok()
            })
            .unwrap_or_default();
        Self {
            path: Some(path.to_path_buf()),
            devices: devices
                .into_iter()
                .map(|d| (d.device_id.clone(), d))
                .collect(),
        }
    }

    fn persist(&self) {
        let Some(path) = &self.path else { return };
        let list: Vec<&PairedDevice> = self.devices.values().collect();
        if let Err(e) = serde_json::to_vec(&list)
            .map_err(anyhow::Error::from)
            .and_then(|bytes| {
                let tmp = path.with_extension("tmp");
                fs::write(&tmp, &bytes).map_err(anyhow::Error::from)?;
                fs::rename(&tmp, path).map_err(anyhow::Error::from)?;
                Ok(())
            })
        {
            tracing::warn!(error = %e, "could not persist paired devices");
        }
    }
}

pub struct BeginOutcome {
    pub spake_reply: Vec<u8>,
    pub receiver_fingerprint: String,
    pub receiver_confirmation: [u8; 32],
}

impl PairingManager {
    pub fn new(paired_file: &Path) -> Self {
        Self {
            active: Mutex::new(None),
            store: Mutex::new(PairedStore::load(paired_file)),
        }
    }

    /// Opens a fresh pairing window and returns the 6-digit code to display.
    pub fn open_window(&self) -> String {
        let code = generate_pairing_code();
        *self.active.lock().expect("pairing lock") = Some(ActivePairing {
            code: code.clone(),
            expires_at: Instant::now() + std::time::Duration::from_secs(PAIRING_CODE_TTL_SECS),
            stage: PairingStage::Waiting,
        });
        tracing::info!("pairing window opened for {PAIRING_CODE_TTL_SECS}s");
        code
    }

    pub fn close_window(&self) {
        *self.active.lock().expect("pairing lock") = None;
    }

    pub fn window_is_open(&self) -> bool {
        match self.active.lock().expect("pairing lock").as_ref() {
            Some(state) => !state.expired(),
            None => false,
        }
    }

    /// Handles `PairBegin`: verifies the code via SPAKE2 and returns the reply.
    pub fn handle_pair_begin(
        &self,
        device_name: &str,
        phone_message: &[u8],
        receiver_fingerprint: String,
    ) -> Result<BeginOutcome, PairingError> {
        let mut guard = self.active.lock().expect("pairing lock");
        let state = guard.as_mut().ok_or(PairingError::NoWindow)?;
        if state.expired() {
            *guard = None;
            return Err(PairingError::Expired);
        }
        if !matches!(state.stage, PairingStage::Waiting) {
            return Err(PairingError::OutOfOrder);
        }

        let (spake, reply) = Spake2::<Ed25519Group>::start_b(
            &Password::new(&state.code),
            &SpakeIdentity::new(PHONE_IDENTITY),
            &SpakeIdentity::new(RECEIVER_IDENTITY),
        );
        // The receiver already holds the phone's first message, so it can
        // finish its half of the exchange immediately.
        let key = spake
            .finish(phone_message)
            .map_err(|_| PairingError::BadCode)?;
        let receiver_confirmation = confirmation_hash(&key, CONFIRM_RX);

        state.stage = PairingStage::KeyDerived {
            device_name: device_name.to_string(),
            key,
        };

        Ok(BeginOutcome {
            spake_reply: reply,
            receiver_fingerprint,
            receiver_confirmation,
        })
    }

    /// Handles `PairVerify` and on success registers the phone permanently.
    pub fn handle_pair_verify(
        &self,
        phone_confirmation: &[u8; 32],
    ) -> Result<PairedDevice, PairingError> {
        let mut guard = self.active.lock().expect("pairing lock");
        let state = guard.as_ref().ok_or(PairingError::NoWindow)?;
        let PairingStage::KeyDerived {
            key, device_name, ..
        } = &state.stage
        else {
            return Err(PairingError::OutOfOrder);
        };
        if state.expired() {
            *guard = None;
            return Err(PairingError::Expired);
        }

        let expected = confirmation_hash(key, CONFIRM_PHONE);
        if expected != *phone_confirmation {
            *guard = None;
            tracing::warn!("pairing rejected: confirmation mismatch (wrong code?)");
            return Err(PairingError::BadCode);
        }

        let device_id = derive_device_id(device_name, key);
        let token = hex::encode(random_bytes(32));
        let device = PairedDevice {
            device_id: device_id.clone(),
            name: sanitize_name(device_name),
            token,
            added_unix_secs: unix_now(),
        };
        drop(guard);

        let mut store = self.store.lock().expect("store lock");
        store
            .devices
            .insert(device.device_id.clone(), device.clone());
        store.persist();
        self.close_window();
        tracing::info!(device_id = %device.device_id, name = %device.name, "device paired");
        Ok(device)
    }

    pub fn find_by_token(&self, token: &str) -> Option<PairedDevice> {
        let store = self.store.lock().expect("store lock");
        store.devices.values().find(|d| d.token == token).cloned()
    }

    pub fn list_devices(&self) -> Vec<PairedDevice> {
        let store = self.store.lock().expect("store lock");
        store.devices.values().cloned().collect()
    }
}

fn confirmation_hash(key: &[u8], suffix: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(key);
    hasher.update(suffix);
    hasher.finalize().into()
}

fn derive_device_id(name: &str, key: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(name.as_bytes());
    hasher.update(key);
    hex::encode(&hasher.finalize()[..16])
}

fn sanitize_name(name: &str) -> String {
    let cleaned: String = name.trim().chars().take(64).collect();
    if cleaned.is_empty() {
        "Unknown phone".to_string()
    } else {
        cleaned
    }
}

pub fn random_bytes(n: usize) -> Vec<u8> {
    use ring::rand::SecureRandom;
    let rng = ring::rand::SystemRandom::new();
    let mut buf = vec![0u8; n];
    rng.fill(&mut buf).expect("system entropy unavailable");
    buf
}

pub fn generate_pairing_code() -> String {
    // Rejection sampling keeps the distribution uniform over [100_000, 999_999].
    loop {
        let bytes = random_bytes(4);
        let value = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
        let limit = u32::MAX - (u32::MAX % 900_000);
        if value < limit {
            return format!("{}", 100_000 + value % 900_000);
        }
    }
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manager(dir: &Path) -> PairingManager {
        PairingManager::new(&dir.join("paired.json"))
    }

    fn begin(mgr: &PairingManager, code_used: &str, _fingerprint: String) -> ([u8; 32], Vec<u8>) {
        let (spake, msg_a) = Spake2::<Ed25519Group>::start_a(
            &Password::new(code_used),
            &SpakeIdentity::new(PHONE_IDENTITY),
            &SpakeIdentity::new(RECEIVER_IDENTITY),
        );
        let outcome = mgr
            .handle_pair_begin("Test Phone", &msg_a, "a".repeat(64))
            .expect("begin accepted");
        let key = spake.finish(&outcome.spake_reply).expect("client finish");
        (confirmation_hash(&key, CONFIRM_PHONE), outcome.spake_reply)
    }

    #[test]
    fn pairing_codes_are_six_digits_and_random_enough() {
        for _ in 0..32 {
            let code = generate_pairing_code();
            assert_eq!(code.len(), 6);
            let n: u32 = code.parse().unwrap();
            assert!((100_000..1_000_000).contains(&n));
        }
        let codes: std::collections::HashSet<String> =
            (0..16).map(|_| generate_pairing_code()).collect();
        assert!(codes.len() > 1, "codes should vary");
    }

    #[test]
    fn full_pairing_flow_persists_device_and_token_works() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = manager(dir.path());
        let fp = "a".repeat(64);

        mgr.open_window();
        assert!(mgr.window_is_open());
        let (confirm, _reply) = begin(&mgr, &mgr.open_window(), fp.clone());

        let device = mgr.handle_pair_verify(&confirm).expect("verify ok");
        assert_eq!(device.name, "Test Phone");
        assert_eq!(device.token.len(), 64);
        assert!(!mgr.window_is_open(), "window must close after success");

        // Persistence across manager restarts:
        let reloaded = PairingManager::new(&dir.path().join("paired.json"));
        assert_eq!(
            reloaded.find_by_token(&device.token).unwrap().device_id,
            device.device_id
        );
        assert_eq!(reloaded.list_devices().len(), 1);
    }

    #[test]
    fn wrong_code_is_rejected_and_nothing_is_stored() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = manager(dir.path());
        let fp = "b".repeat(64);
        mgr.open_window();
        let (confirm, _) = begin(&mgr, "000001", fp); // window code differs

        assert!(matches!(
            mgr.handle_pair_verify(&confirm),
            Err(PairingError::BadCode)
        ));
        assert!(mgr.list_devices().is_empty());
    }

    #[test]
    fn pair_without_open_window_fails() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = manager(dir.path());
        assert!(
            mgr.handle_pair_begin("Phone", &[0u8; 32], "c".repeat(64))
                .is_err()
        );
    }

    #[test]
    fn second_begin_in_same_window_is_out_of_order() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = manager(dir.path());
        mgr.open_window();
        let fp = "d".repeat(64);
        let (_spake, msg_a) = Spake2::<Ed25519Group>::start_a(
            &Password::new(mgr_window_code(&mgr)),
            &SpakeIdentity::new(PHONE_IDENTITY),
            &SpakeIdentity::new(fp.as_bytes()),
        );
        mgr.handle_pair_begin("P", &msg_a, fp.clone()).unwrap();
        assert!(mgr.handle_pair_begin("P", &msg_a, fp).is_err());
    }

    // The test needs the actual code; open_window returns it, so capture once.
    fn mgr_window_code(mgr: &PairingManager) -> String {
        mgr.open_window()
    }
}
