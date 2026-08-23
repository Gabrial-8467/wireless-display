use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::{Mutex, mpsc};
use std::time::Instant;

use crate::media::MediaCounters;

/// RTP payload types fixed by docs/06-phase3-media-plan.md.
pub const PT_VIDEO_H264: u8 = 96;
pub const PT_AUDIO_OPUS: u8 = 97;

const MIN_RTP_LEN: usize = 12;
const MAX_DGRAM_LEN: usize = 1500;

/// Splits incoming QUIC datagrams (one RTP packet each) into per-medium
/// queues for the GStreamer appsrc pumps. The queue senders are owned here;
/// receivers go to the media session.
pub struct MediaIngest {
    pub counters: Arc<MediaCounters>,
    /// Arrival time of the most recent video datagram (watchdog input).
    pub last_video: Arc<Mutex<Option<Instant>>>,
    _video_tx: mpsc::SyncSender<Vec<u8>>,
    _audio_tx: mpsc::SyncSender<Vec<u8>>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for MediaIngest {
    fn drop(&mut self) {
        self.task.abort();
    }
}

pub fn payload_type(packet: &[u8]) -> Option<u8> {
    if packet.len() < MIN_RTP_LEN {
        return None;
    }
    Some(packet[1] & 0x7F)
}

impl MediaIngest {
    pub fn spawn(
        conn: quinn::Connection,
        video_tx: mpsc::SyncSender<Vec<u8>>,
        audio_tx: mpsc::SyncSender<Vec<u8>>,
        counters: Arc<MediaCounters>,
    ) -> Self {
        let video_q = video_tx.clone();
        let audio_q = audio_tx.clone();
        let last_video = Arc::new(Mutex::new(None::<Instant>));
        let watchdog_stamp = last_video.clone();
        let task_counters = counters.clone();

        let task = tokio::spawn(async move {
            loop {
                match conn.read_datagram().await {
                    Ok(bytes) => {
                        if bytes.len() < MIN_RTP_LEN || bytes.len() > MAX_DGRAM_LEN {
                            task_counters
                                .dropped_datagrams
                                .fetch_add(1, Ordering::Relaxed);
                            continue;
                        }
                        match payload_type(&bytes) {
                            Some(PT_VIDEO_H264) => {
                                *watchdog_stamp.lock().unwrap() = Some(Instant::now());
                                if video_q.try_send(bytes.to_vec()).is_err() {
                                    task_counters
                                        .dropped_datagrams
                                        .fetch_add(1, Ordering::Relaxed);
                                }
                            }
                            Some(PT_AUDIO_OPUS) => {
                                if audio_q.try_send(bytes.to_vec()).is_err() {
                                    task_counters
                                        .dropped_datagrams
                                        .fetch_add(1, Ordering::Relaxed);
                                }
                            }
                            _ => {
                                task_counters
                                    .dropped_datagrams
                                    .fetch_add(1, Ordering::Relaxed);
                            }
                        }
                    }
                    Err(_) => return,
                }
            }
        });

        Self {
            counters,
            last_video,
            _video_tx: video_tx,
            _audio_tx: audio_tx,
            task,
        }
    }
}

/// Raises the QUIC datagram receive window on both endpoints; without this
/// peers negotiate "datagrams unsupported".
pub fn enable_datagrams(cfg: &mut quinn::TransportConfig) {
    cfg.datagram_receive_buffer_size(Some(512 * 1024));
}
