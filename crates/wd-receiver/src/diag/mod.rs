use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Mutex;

use serde::Serialize;

pub fn init_tracing(level: &str) {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        tracing_subscriber::EnvFilter::new(format!("wd_receiver={level},info"))
    });
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(true)
        .with_line_number(false)
        .init();
}

pub const DECODER_CANDIDATES: &[&str] = &[
    "vaapih264dec",
    "vah264dec",
    "nvh264dec",
    "avdec_h264",
    "openh264dec",
];

#[derive(Debug, Clone, Serialize)]
pub struct DecoderProbe {
    pub element: String,
    pub available: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct InterfaceInfo {
    pub name: String,
    pub addr: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct SystemInfo {
    pub app_version: String,
    pub gtk_version: String,
    pub adwaita_version: String,
    pub gstreamer_version: String,
    pub pipewire_sink_available: bool,
    pub decoders: Vec<DecoderProbe>,
    pub interfaces: Vec<InterfaceInfo>,
}

impl SystemInfo {
    pub fn probe() -> Self {
        let decoders = DECODER_CANDIDATES
            .iter()
            .map(|name| DecoderProbe {
                element: (*name).into(),
                available: gst_element_exists(name),
            })
            .collect();
        let interfaces = if_addrs::get_if_addrs()
            .unwrap_or_default()
            .into_iter()
            .filter(|i| !i.is_loopback())
            .map(|i| InterfaceInfo {
                name: i.name.clone(),
                addr: i.ip().to_string(),
            })
            .collect();
        Self {
            app_version: env!("CARGO_PKG_VERSION").into(),
            gtk_version: format!(
                "{}.{}.{}",
                gtk::major_version(),
                gtk::minor_version(),
                gtk::micro_version()
            ),
            adwaita_version: format!(
                "{}.{}.{}",
                adw::major_version(),
                adw::minor_version(),
                adw::micro_version()
            ),
            gstreamer_version: {
                let (maj, min, mic, _) = gstreamer::version();
                format!("{maj}.{min}.{mic}")
            },
            pipewire_sink_available: gst_element_exists("pipewiresink"),
            decoders,
            interfaces,
        }
    }
}

fn gst_element_exists(name: &str) -> bool {
    gstreamer::ElementFactory::find(name).is_some()
}

#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum MetricValue {
    Count(u64),
    Gauge(f64),
    Text(String),
}

#[derive(Default)]
pub struct MetricsRegistry {
    values: Mutex<BTreeMap<String, MetricValue>>,
    tx: tokio::sync::watch::Sender<BTreeMap<String, MetricValue>>,
}

impl MetricsRegistry {
    pub fn new() -> Self {
        let (tx, _) = tokio::sync::watch::channel(BTreeMap::new());
        Self {
            values: Mutex::new(BTreeMap::new()),
            tx,
        }
    }

    pub fn subscribe(&self) -> tokio::sync::watch::Receiver<BTreeMap<String, MetricValue>> {
        self.tx.subscribe()
    }

    pub fn snapshot(&self) -> BTreeMap<String, MetricValue> {
        self.values.lock().expect("metrics lock").clone()
    }

    fn publish(&self) {
        self.tx
            .send_replace(self.values.lock().expect("metrics lock").clone());
    }

    pub fn set_gauge(&self, key: &str, value: f64) {
        self.values
            .lock()
            .expect("metrics lock")
            .insert(key.into(), MetricValue::Gauge(value));
        self.publish();
    }

    pub fn increment(&self, key: &str) {
        let mut guard = self.values.lock().expect("metrics lock");
        let entry = guard.entry(key.into()).or_insert(MetricValue::Count(0));
        if let MetricValue::Count(n) = entry {
            *n += 1;
        }
        drop(guard);
        self.publish();
    }

    pub fn set_text(&self, key: &str, value: impl Into<String>) {
        self.values
            .lock()
            .expect("metrics lock")
            .insert(key.into(), MetricValue::Text(value.into()));
        self.publish();
    }
}

pub fn report_path() -> Result<PathBuf, std::io::Error> {
    let dir = dirs::data_dir()
        .ok_or_else(|| std::io::Error::other("XDG data directory unavailable"))?
        .join("wireless-display")
        .join("reports");
    std::fs::create_dir_all(&dir)?;
    let stamp = chrono_like_stamp();
    Ok(dir.join(format!("diagnostics-{stamp}.txt")))
}

fn chrono_like_stamp() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let days = secs.as_secs() / 86_400;
    let tod = secs.as_secs() % 86_400;
    let (h, m, s) = (tod / 3600, (tod % 3600) / 60, tod % 60);
    let civil = civil_from_days(days as i64);
    format!(
        "{:04}{:02}{:02}-{:02}{:02}{:02}",
        civil.0, civil.1, civil.2, h, m, s
    )
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}
