pub mod session;

use std::time::Duration;

/// Codec parameters agreed in `SessionOffer`/`SessionAnswer`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VideoParams {
    pub width: u16,
    pub height: u16,
    pub fps: u8,
    pub bitrate_kbps: u32,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AudioParams {
    pub sample_rate: u32,
    pub channels: u8,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MediaLimits;

impl MediaLimits {
    pub const MAX_WIDTH: u16 = 1920;
    pub const MAX_HEIGHT: u16 = 1080;
    pub const MAX_FPS: u8 = 60;
    pub const MAX_BITRATE_KBPS: u32 = 25_000;
}

pub fn validate_offer(
    video: &wd_protocol::VideoOffer,
    audio: &wd_protocol::AudioOffer,
) -> Result<(), String> {
    use wd_protocol::{AudioCodec, VideoCodec};
    match video.codec {
        VideoCodec::H264Baseline | VideoCodec::H264Main => {}
    }
    if video.width == 0
        || video.height == 0
        || video.width > MediaLimits::MAX_WIDTH
        || video.height > MediaLimits::MAX_HEIGHT
    {
        return Err(format!(
            "resolution {}x{} outside supported bounds",
            video.width, video.height
        ));
    }
    if video.fps == 0 || video.fps > MediaLimits::MAX_FPS {
        return Err(format!("fps {} outside supported bounds", video.fps));
    }
    if video.bitrate_kbps > MediaLimits::MAX_BITRATE_KBPS {
        return Err("bitrate above receiver cap".into());
    }
    if audio.codec != AudioCodec::Opus || audio.sample_rate != 48_000 || audio.channels > 2 {
        return Err("only Opus 48 kHz mono/stereo is supported".into());
    }
    Ok(())
}

/// Live counters shared between pad probes, ingest pumps and metrics tasks.
#[derive(Default)]
pub struct MediaCounters {
    /// Access units leaving the H.264 decoder (pad probe).
    pub video_frames: std::sync::atomic::AtomicU64,
    /// Decoded bytes (pad probe; basis of the video bitrate gauge).
    pub video_bytes: std::sync::atomic::AtomicU64,
    /// RTP video datagrams fed into the pipeline.
    pub video_packets: std::sync::atomic::AtomicU64,
    /// Opus packets fed into the pipeline.
    pub audio_packets: std::sync::atomic::AtomicU64,
    /// RTP audio bytes fed into the pipeline (basis of the audio gauge).
    pub audio_bytes: std::sync::atomic::AtomicU64,
    /// Datagrams dropped by the ingest router or full queues.
    pub dropped_datagrams: std::sync::atomic::AtomicU64,
}

impl MediaCounters {
    pub fn snapshot(&self) -> (u64, u64, u64, u64, u64, u64) {
        use std::sync::atomic::Ordering::Relaxed;
        (
            self.video_frames.load(Relaxed),
            self.video_bytes.load(Relaxed),
            self.video_packets.load(Relaxed),
            self.audio_packets.load(Relaxed),
            self.audio_bytes.load(Relaxed),
            self.dropped_datagrams.load(Relaxed),
        )
    }
}

/// Events flowing from the media layer back to the network/UI layers.
#[derive(Debug, Clone)]
pub enum MediaEvent {
    /// Exactly once, when the first video AU leaves the decoder.
    FirstVideoFrame {
        decoder: String,
    },
    /// Decoder/pipeline error; session should request a keyframe or fail.
    VideoError {
        reason: String,
    },
    AudioError {
        reason: String,
    },
}

pub const JITTER_LATENCY_MS: u32 = 40;
pub const MEDIA_IDLE_TIMEOUT: Duration = Duration::from_secs(5);
