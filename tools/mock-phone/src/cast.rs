//! `cast` subcommand: GStreamer test-pattern encoder shipping RTP packets as
//! one-per-datagram QUIC unreliable traffic to a paired receiver.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::Context as _;
use bytes::Bytes;
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use wd_protocol::{
    AudioCodec, AudioOffer, CodecError, Message, VideoCodec, VideoOffer, read_frame, write_frame,
};

use crate::ConnectOpts;
use crate::client;

const VIDEO_SINK: &str = "vidsink";
const AUDIO_SINK: &str = "audsink";
const VIDEO_QUEUE: usize = 256;
const AUDIO_QUEUE: usize = 1024;
const PONG_WAIT: Duration = Duration::from_millis(1000);
const BYE_ACK_WAIT: Duration = Duration::from_millis(300);
const DRAIN_WINDOW: Duration = Duration::from_millis(1);

pub struct CastMedia {
    pub width: u16,
    pub height: u16,
    pub fps: u8,
    pub bitrate_kbps: u32,
    pub with_audio: bool,
}

impl CastMedia {
    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.width > 0 && self.height > 0 && self.fps > 0 && self.bitrate_kbps > 0,
            "--width/--height/--fps/--bitrate-kbps must be positive"
        );
        Ok(())
    }
}

const AUDIO_PIPELINE: &str = "audiotestsrc is-live=true wave=0 samplesperbuffer=960 ! \
     audio/x-raw,format=S16LE,rate=48000,channels=2 ! opusenc ! rtpopuspay pt=97 ! \
     appsink sync=false max-buffers=32 drop=true name=audsink";

fn video_pipeline(width: u16, height: u16, fps: u8, bitrate_kbps: u32) -> String {
    format!(
        "videotestsrc is-live=true ! \
         video/x-raw,format=I420,width={width},height={height},framerate={fps}/1 ! \
         openh264enc bitrate={} complexity=0 gop-size=60 ! \
         video/x-h264,stream-format=byte-stream,alignment=au ! \
         rtph264pay pt=96 mtu=1100 config-interval=-1 ! \
         appsink sync=false max-buffers=4 drop=true name=vidsink",
        bitrate_kbps * 1000
    )
}

#[derive(Default)]
struct FlowStats {
    sent: AtomicU64,
    dropped: AtomicU64,
    bytes: AtomicU64,
}

type Snapshot = (u64, u64, u64);

impl FlowStats {
    fn snapshot(&self) -> Snapshot {
        (
            self.sent.load(Ordering::Relaxed),
            self.dropped.load(Ordering::Relaxed),
            self.bytes.load(Ordering::Relaxed),
        )
    }
}

struct Medium {
    pipeline: gst::Pipeline,
    tx: mpsc::SyncSender<Vec<u8>>,
    stats: Arc<FlowStats>,
    pump: thread::JoinHandle<()>,
}

impl Medium {
    fn shutdown(self) -> thread::Result<()> {
        let _ = self.pipeline.set_state(gst::State::Null);
        drop(self.pipeline);
        drop(self.tx);
        self.pump.join()
    }
}

fn start_medium(
    conn: quinn::Connection,
    desc: &str,
    sink_name: &str,
    queue_depth: usize,
) -> anyhow::Result<Medium> {
    let element = gst::parse::launch(desc).with_context(|| format!("parse pipeline `{desc}`"))?;
    let pipeline = element
        .downcast::<gst::Pipeline>()
        .map_err(|_| anyhow::anyhow!("pipeline description did not yield a pipeline"))?;
    let appsink = pipeline
        .by_name(sink_name)
        .with_context(|| format!("pipeline lacks appsink `{sink_name}`"))?
        .downcast::<gst_app::AppSink>()
        .map_err(|_| anyhow::anyhow!("element `{sink_name}` is not an appsink"))?;

    let (tx, rx) = mpsc::sync_channel(queue_depth);
    let stats = Arc::new(FlowStats::default());
    let callback_tx = tx.clone();
    let callback_stats = Arc::clone(&stats);
    appsink.set_callbacks(
        gst_app::AppSinkCallbacks::builder()
            .new_sample(move |sink| {
                let sample = sink.pull_sample().map_err(|_| gst::FlowError::Error)?;
                let buffer = sample.buffer().ok_or(gst::FlowError::Error)?;
                let map = buffer.map_readable().map_err(|_| gst::FlowError::Error)?;
                if callback_tx.try_send(map.as_slice().to_vec()).is_err() {
                    callback_stats.dropped.fetch_add(1, Ordering::Relaxed);
                }
                Ok(gst::FlowSuccess::Ok)
            })
            .build(),
    );

    pipeline
        .set_state(gst::State::Playing)
        .with_context(|| format!("start pipeline `{desc}`"))?;

    let pump_stats = Arc::clone(&stats);
    Ok(Medium {
        pipeline,
        tx,
        stats,
        pump: spawn_datagram_pump(conn, rx, pump_stats),
    })
}

fn spawn_datagram_pump(
    conn: quinn::Connection,
    rx: mpsc::Receiver<Vec<u8>>,
    stats: Arc<FlowStats>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        while let Ok(packet) = rx.recv() {
            let size = packet.len();
            match conn.send_datagram(Bytes::from(packet)) {
                Ok(()) => {
                    stats.sent.fetch_add(1, Ordering::Relaxed);
                    stats.bytes.fetch_add(size as u64, Ordering::Relaxed);
                }
                Err(quinn::SendDatagramError::ConnectionLost(_)) => break,
                Err(_) => {
                    stats.dropped.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
    })
}

async fn next_reply(recv: &mut quinn::RecvStream) -> Option<Result<Message, CodecError>> {
    tokio::time::timeout(DRAIN_WINDOW, read_frame(recv))
        .await
        .ok()
}

fn log_flow(label: &str, stats: &FlowStats, prev: Snapshot, secs: f64) -> Snapshot {
    let (sent, dropped, bytes) = stats.snapshot();
    let kbps = bytes.saturating_sub(prev.2) as f64 * 8.0 / 1000.0 / secs;
    println!(
        "{label}: sent {sent} (+{}) dropped {dropped} ~{kbps:.0} kbps",
        sent - prev.0
    );
    (sent, dropped, bytes)
}

async fn send_bye_and_await_ack(send: &mut quinn::SendStream, recv: &mut quinn::RecvStream) {
    let _ = write_frame(
        send,
        &Message::Bye {
            reason: "cast stopped".into(),
        },
    )
    .await;
    let _ = tokio::time::timeout(BYE_ACK_WAIT, read_frame(recv)).await;
}

pub async fn run(opts: ConnectOpts, media: CastMedia, token: &str) -> anyhow::Result<()> {
    media.validate()?;
    gst::init().context("gstreamer init failed")?;

    let conn = client::connect(opts.addr()).await?;
    let (mut send, mut recv) = client::authenticate(&conn, &opts.name, token).await?;
    println!("connected to receiver");

    write_frame(
        &mut send,
        &Message::SessionOffer {
            video: VideoOffer {
                codec: VideoCodec::H264Baseline,
                width: media.width,
                height: media.height,
                fps: media.fps,
                bitrate_kbps: media.bitrate_kbps,
            },
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
        other => anyhow::bail!("expected SessionAnswer, got {}", client::kind_of(&other)),
    }

    let video = start_medium(
        conn.clone(),
        &video_pipeline(media.width, media.height, media.fps, media.bitrate_kbps),
        VIDEO_SINK,
        VIDEO_QUEUE,
    )?;
    let audio = if media.with_audio {
        Some(start_medium(
            conn.clone(),
            AUDIO_PIPELINE,
            AUDIO_SINK,
            AUDIO_QUEUE,
        )?)
    } else {
        None
    };

    let mut ping_interval = tokio::time::interval(Duration::from_secs(2));
    let mut report_interval = tokio::time::interval(Duration::from_secs(5));
    report_interval.tick().await;
    let mut last_report = Instant::now();
    let mut last_video = video.stats.snapshot();
    let mut last_audio = audio.as_ref().map(|m| m.stats.snapshot());

    let outcome: anyhow::Result<()> = 'session: loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                send_bye_and_await_ack(&mut send, &mut recv).await;
                break 'session Ok(());
            }
            _ = ping_interval.tick() => {
                if let Err(e) = write_frame(
                    &mut send,
                    &Message::Ping { sender_time_ms: client::unix_millis() },
                )
                .await
                {
                    break 'session Err(anyhow::anyhow!("control send failed: {e}"));
                }
                match tokio::time::timeout(PONG_WAIT, read_frame(&mut recv)).await {
                    Ok(Ok(Message::Pong { echoed_time_ms })) => {
                        println!("rtt {} ms", client::unix_millis().saturating_sub(echoed_time_ms));
                    }
                    Ok(Ok(Message::Bye { reason })) => {
                        println!("bye: {reason}");
                        break 'session Ok(());
                    }
                    Ok(Ok(_)) | Ok(Err(_)) | Err(_) => {}
                }
                while let Some(reply) = next_reply(&mut recv).await {
                    match reply {
                        Ok(Message::Bye { reason }) => {
                            println!("bye: {reason}");
                            break 'session Ok(());
                        }
                        Ok(_) => {}
                        Err(e) => break 'session Err(anyhow::anyhow!("control stream error: {e}")),
                    }
                }
            }
            _ = report_interval.tick() => {
                let secs = last_report.elapsed().as_secs_f64().max(f64::EPSILON);
                last_video = log_flow("video", &video.stats, last_video, secs);
                if let Some((medium, prev)) = audio.as_ref().zip(last_audio) {
                    last_audio = Some(log_flow("audio", &medium.stats, prev, secs));
                }
                last_report = Instant::now();
            }
        }
    };

    let video_totals = video.stats.snapshot();
    let audio_totals = audio.as_ref().map(|m| m.stats.snapshot());
    if let Some(medium) = audio
        && let Err(e) = medium.shutdown()
    {
        eprintln!("audio pump panicked: {e:?}");
    }
    if let Err(e) = video.shutdown() {
        eprintln!("video pump panicked: {e:?}");
    }
    println!(
        "cast finished: video sent {} dropped {}",
        video_totals.0, video_totals.1
    );
    if let Some(a) = audio_totals {
        println!("cast finished: audio sent {} dropped {}", a.0, a.1);
    }
    outcome
}
