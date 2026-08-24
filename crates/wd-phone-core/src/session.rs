//! Cast session: connect → authenticate → offer → stream video datagrams.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use tokio::sync::{mpsc, watch};
use wd_protocol::{
    AudioCodec, AudioOffer, Message, VideoCodec, VideoOffer, read_frame, write_frame,
};

use crate::proto;

pub const MTU: usize = 1100;

#[derive(Default)]
pub struct Shared {
    pub state: Mutex<String>,
    pub error: Mutex<Option<String>>,
    pub keyframe: AtomicBool,
    pub bitrate_hint: AtomicU32,
    pub sent: AtomicU64,
    pub dropped: AtomicU64,
}

impl Shared {
    pub fn set_state(&self, s: &str) {
        if let Ok(mut g) = self.state.lock() {
            *g = s.to_string();
        }
    }
    pub fn set_error(&self, e: String) {
        self.set_state("error");
        if let Ok(mut g) = self.error.lock() {
            *g = Some(e);
        }
    }
    pub fn snapshot(&self) -> String {
        let state = self.state.lock().map(|g| g.clone()).unwrap_or_default();
        let error = self.error.lock().ok().and_then(|g| g.clone());
        serde_json::json!({
            "state": state,
            "error": error,
            "sent": self.sent.load(Ordering::Relaxed),
            "dropped": self.dropped.load(Ordering::Relaxed),
            "keyframe": self.keyframe.load(Ordering::Relaxed),
        })
        .to_string()
    }
}

static LAST_CONFIG: Mutex<Option<Vec<u8>>> = Mutex::new(None);

pub fn store_config(csd: &[u8]) -> bool {
    match crate::rtp::avcc_to_annexb(csd) {
        Some(annexb) => {
            if let Ok(mut g) = LAST_CONFIG.lock() {
                *g = Some(annexb);
                return true;
            }
            false
        }
        None => false,
    }
}

fn clear_config() {
    if let Ok(mut g) = LAST_CONFIG.lock() {
        *g = None;
    }
}

fn ssrc_from_time() -> u32 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() ^ (d.as_secs() as u32))
        .unwrap_or(0x1234_5678)
        | 1
}

/// Handle for a running session.
pub struct SessionHandle {
    pub shared: Arc<Shared>,
    pub video_tx: mpsc::Sender<(Vec<u8>, bool, u64)>, // (data, is_codec_config, pts_us)
    pub stop_tx: watch::Sender<bool>,
}

impl SessionHandle {
    /// Push one MediaCodec output buffer into the pipeline.
    pub fn push(&self, data: Vec<u8>, is_config: bool, pts_us: u64) -> bool {
        if is_config {
            let stored = store_config(&data);
            return stored;
        }
        matches!(self.video_tx.try_send((data, false, pts_us)), Ok(()))
    }

    pub fn stop(self) {
        let _ = self.stop_tx.send(true);
    }
}

/// Run a full cast session until stopped or the receiver drops us.
pub async fn run(
    addr: SocketAddr,
    phone_name: &str,
    token: &str,
    width: u16,
    height: u16,
    fps: u8,
    bitrate_kbps: u32,
    shared: Arc<Shared>,
    mut video_rx: mpsc::Receiver<(Vec<u8>, bool, u64)>,
    mut stop_rx: watch::Receiver<bool>,
) -> anyhow::Result<()> {
    clear_config();
    shared.set_state("connecting");
    let conn = proto::connect(addr).await?;
    let (mut send, mut recv) = proto::authenticate(&conn, phone_name, token).await?;

    shared.set_state("offering");
    write_frame(
        &mut send,
        &Message::SessionOffer {
            video: VideoOffer {
                codec: VideoCodec::H264Baseline,
                width,
                height,
                fps,
                bitrate_kbps,
            },
            // Declared but silent in v1 (receiver watchdog keys on video only).
            audio: AudioOffer {
                codec: AudioCodec::Opus,
                sample_rate: 48000,
                channels: 2,
            },
        },
    )
    .await?;

    match read_frame(&mut recv).await? {
        Message::SessionAnswer { accepted: true, .. } => {}
        Message::SessionAnswer {
            accepted: false,
            reason,
            ..
        } => anyhow::bail!(
            "receiver refused the session: {}",
            reason.unwrap_or_else(|| "unknown reason".into())
        ),
        other => anyhow::bail!("expected SessionAnswer, got {}", proto::kind_of(&other)),
    }

    // Video pump task: converts + packetizes + sends datagrams.
    let conn_pump = conn.clone();
    let shared_pump = Arc::clone(&shared);
    let ssrc = ssrc_from_time();
    let mut pump_stop = stop_rx.clone();
    let pump = tokio::spawn(async move {
        loop {
            let item = tokio::select! {
                changed = pump_stop.changed() => {
                    if changed.is_err() || *pump_stop.borrow() {
                        break;
                    }
                    continue;
                }
                item = video_rx.recv() => match item {
                    Some(v) => v,
                    None => break,
                }
            };
            let (data, is_config, pts_us) = item;
            if is_config {
                continue; // config handled via store_config
            }
            let au = crate::rtp::lp_to_annexb(&data).unwrap_or(data);
            // Prepend parameter sets on every IDR for mid-stream decode.
            let au = if crate::rtp::contains_idr(&au) {
                match LAST_CONFIG.lock().ok().and_then(|g| g.clone()) {
                    Some(cfg) => {
                        let mut full = Vec::with_capacity(cfg.len() + au.len());
                        full.extend_from_slice(&cfg);
                        full.extend_from_slice(&au);
                        full
                    }
                    None => au,
                }
            } else {
                au
            };
            let ts90k = ((u64::from(pts_us) * 9 / 100) & 0xffff_ffff) as u32;
            send_packets(&conn_pump, &shared_pump, au, ts90k, ssrc).await;
        }
    });

    shared.set_state("streaming");

    let mut ping = tokio::time::interval(Duration::from_secs(2));
    ping.tick().await;
    let result: anyhow::Result<()> = loop {
        if *stop_rx.borrow() {
            let _ = write_frame(
                &mut send,
                &Message::Bye {
                    reason: "cast stopped".into(),
                },
            )
            .await;
            break Ok(());
        }
        tokio::select! {
            changed = stop_rx.changed() => {
                if changed.is_ok() && *stop_rx.borrow() {
                    let _ = write_frame(
                        &mut send,
                        &Message::Bye { reason: "cast stopped".into() },
                    ).await;
                    break Ok(());
                }
            }
            _ = ping.tick() => {
                if let Err(e) = write_frame(
                    &mut send,
                    &Message::Ping { sender_time_ms: proto::unix_millis() },
                ).await {
                    shared.set_error(format!("control send failed: {e}"));
                    break Err(anyhow::anyhow!(e));
                }
            }
            frame = read_frame(&mut recv) => {
                match frame {
                    Ok(Message::KeyframeRequest) => {
                        shared.keyframe.store(true, Ordering::Relaxed);
                    }
                    Ok(Message::BitrateHint { video_kbps }) => {
                        shared.bitrate_hint.store(video_kbps, Ordering::Relaxed);
                    }
                    Ok(Message::Ping { sender_time_ms }) => {
                        let _ = write_frame(
                            &mut send,
                            &Message::Pong { echoed_time_ms: sender_time_ms },
                        ).await;
                    }
                    Ok(Message::Bye { .. }) => {
                        shared.set_state("stopped");
                        break Ok(());
                    }
                    Ok(_) => {}
                    Err(wd_protocol::CodecError::UnexpectedEof) => {
                        shared.set_state("stopped");
                        break Ok(());
                    }
                    Err(e) => {
                        shared.set_error(format!("control stream error: {e}"));
                        break Err(e.into());
                    }
                }
            }
        }
    };

    pump.abort();
    if matches!(shared.state.lock().map(|g| g.clone()).unwrap_or_default().as_str(), "streaming") {
        shared.set_state("stopped");
    }
    result
}

async fn send_packets(
    conn: &quinn::Connection,
    shared: &Shared,
    au: Vec<u8>,
    ts90k: u32,
    ssrc: u32,
) {
    let mut seq = NEXT_SEQ.lock().map(|mut g| {
        let v = *g;
        *g = g.wrapping_add(0x100);
        v
    }).unwrap_or(0x1234);
    for pkt in crate::rtp::packetize_au(&au, ts90k, ssrc, MTU, &mut seq) {
        match conn.send_datagram(Bytes::from(pkt)) {
            Ok(()) => shared.sent.fetch_add(1, Ordering::Relaxed),
            Err(quinn::SendDatagramError::ConnectionLost(_)) => return,
            Err(_) => shared.dropped.fetch_add(1, Ordering::Relaxed),
        };
    }
}

static NEXT_SEQ: Mutex<u16> = Mutex::new(0x1234);
