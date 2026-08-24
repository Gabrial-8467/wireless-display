//! Paired-device persistence (one receiver slot for v1).

use std::path::Path;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceToken {
    pub device_id: String,
    pub device_token: String,
}

fn path_for(store_dir: &Path) -> std::path::PathBuf {
    store_dir.join("wd_device.json")
}

pub fn save(store_dir: &Path, t: &DeviceToken) -> anyhow::Result<()> {
    std::fs::create_dir_all(store_dir)?;
    let data = serde_json::to_vec_pretty(t)?;
    let p = path_for(store_dir);
    let tmp = p.with_extension("json.tmp");
    std::fs::write(&tmp, data)?;
    std::fs::rename(&tmp, &p)?;
    Ok(())
}

pub fn load(store_dir: &Path) -> Option<DeviceToken> {
    let data = std::fs::read(path_for(store_dir)).ok()?;
    serde_json::from_slice(&data).ok()
}
