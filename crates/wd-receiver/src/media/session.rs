use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::time::Duration;

use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;

use super::{JITTER_LATENCY_MS, MediaCounters, MediaEvent, VideoParams};

/// Pre-built sink elements. Production passes the GTK paintable sink (created
/// on the UI thread); tests pass `None` and get fakesinks.
#[derive(Default, Clone)]
pub struct Sinks {
    pub video: Option<gst::Element>,
    pub audio: Option<gst::Element>,
}

pub struct MediaSession {
    pipelines: Vec<gst::Pipeline>,
    stop: Arc<AtomicBool>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl MediaSession {
    /// Starts video+audio receive chains. `video_rx`/`audio_rx` carry raw RTP
    /// packets already routed by payload type.
    pub fn start(
        video: Option<VideoParams>,
        audio_params: Option<(u32, u8)>,
        sinks: Sinks,
        video_rx: mpsc::Receiver<Vec<u8>>,
        audio_rx: mpsc::Receiver<Vec<u8>>,
        counters: Arc<MediaCounters>,
        events: mpsc::Sender<MediaEvent>,
    ) -> anyhow::Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let mut pipelines = Vec::new();
        let mut tasks = Vec::new();

        if let Some(vp) = video {
            let (pipeline, appsrc) =
                build_video_pipeline(vp, sinks.video.clone(), counters.clone(), events.clone())?;
            pipeline.set_state(gst::State::Playing)?;
            pipelines.push(pipeline);
            let src = appsrc;
            tasks.push(tokio::spawn(pump(
                video_rx,
                stop.clone(),
                counters.clone(),
                true,
                move |data| {
                    let _ = src.push_buffer(gstreamer::Buffer::from_slice(data));
                },
            )));
        }

        if let Some((rate, ch)) = audio_params {
            let (pipeline, appsrc) = build_audio_pipeline(rate, ch, sinks.audio.clone())?;
            pipeline.set_state(gst::State::Playing)?;
            pipelines.push(pipeline);
            let src = appsrc;
            tasks.push(tokio::spawn(pump(
                audio_rx,
                stop.clone(),
                counters.clone(),
                false,
                move |data| {
                    let _ = src.push_buffer(gstreamer::Buffer::from_slice(data));
                },
            )));
        }

        for pl in &pipelines {
            let bus = pl.bus().expect("pipeline has bus");
            let name = pl.name().to_string();
            tasks.push(tokio::spawn(bus_watch(bus, name, events.clone())));
        }

        Ok(Self {
            pipelines,
            stop,
            tasks,
        })
    }

    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        for p in &self.pipelines {
            let _ = p.set_state(gst::State::Null);
        }
        for t in self.tasks.splice(.., []) {
            t.abort();
        }
    }
}

impl Drop for MediaSession {
    fn drop(&mut self) {
        self.stop();
    }
}

async fn pump<F>(
    rx: mpsc::Receiver<Vec<u8>>,
    stop: Arc<AtomicBool>,
    counters: Arc<MediaCounters>,
    is_video: bool,
    push: F,
) where
    F: Fn(Vec<u8>) + Send + 'static,
{
    use std::sync::atomic::Ordering::Relaxed;
    loop {
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(data) => {
                if is_video {
                    counters.video_packets.fetch_add(1, Relaxed);
                } else {
                    counters.audio_packets.fetch_add(1, Relaxed);
                    counters.audio_bytes.fetch_add(data.len() as u64, Relaxed);
                }
                push(data);
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
        }
        if stop.load(Ordering::SeqCst) {
            return;
        }
    }
}

fn build_video_pipeline(
    vp: VideoParams,
    sink: Option<gst::Element>,
    counters: Arc<MediaCounters>,
    events: mpsc::Sender<MediaEvent>,
) -> anyhow::Result<(gst::Pipeline, gst_app::AppSrc)> {
    let pipeline = gst::Pipeline::with_name("wdl-video");
    let appsrc = gst_app::AppSrc::builder()
        .caps(
            &gst::Caps::builder("application/x-rtp")
                .field("media", "video")
                .field("payload", 96i32)
                .field("clock-rate", 90_000i32)
                .field("encoding-name", "H264")
                .build(),
        )
        .is_live(true)
        .format(gst::Format::Time)
        .max_bytes(4 * 1024 * 1024)
        .build();

    let jb = gst::ElementFactory::make("rtpjitterbuffer")
        .name("wdl-video-jb")
        .property("latency", JITTER_LATENCY_MS)
        .property_from_str("do-lost", "true")
        .build()?;
    let depay = gst::ElementFactory::make("rtph264depay").build()?;
    let parse = gst::ElementFactory::make("h264parse")
        .property_from_str("config-interval", "-1")
        .build()?;

    let decoder_name;
    let decoder = {
        let candidates = crate::diag::DECODER_CANDIDATES;
        let found = candidates.iter().find_map(|c| {
            gst::ElementFactory::make(c)
                .build()
                .ok()
                .map(|el| (el, (*c).to_string()))
        });
        let (decoder, name) = found.ok_or_else(|| anyhow::anyhow!("no H.264 decoder available"))?;
        decoder_name = name;
        decoder
    };

    let convert = gst::ElementFactory::make("videoconvert").build()?;
    let sink_el = sink.unwrap_or_else(|| {
        gst::ElementFactory::make("fakesink")
            .property("sync", false)
            .build()
            .expect("fakesink always exists")
    });

    pipeline.add_many([
        appsrc.upcast_ref(),
        &jb,
        &depay,
        &parse,
        &decoder,
        &convert,
        &sink_el,
    ])?;
    gst::Element::link_many([
        appsrc.upcast_ref(),
        &jb,
        &depay,
        &parse,
        &decoder,
        &convert,
        &sink_el,
    ])?;

    tracing::debug!(
        width = vp.width,
        height = vp.height,
        fps = vp.fps,
        decoder = %decoder_name,
        "video receive chain built"
    );

    // First-frame probe + per-frame counters on the decoder output.
    let first = Arc::new(AtomicBool::new(false));
    let dec_pad = decoder.static_pad("src").expect("decoder src pad");
    dec_pad.add_probe(gst::PadProbeType::BUFFER, move |_, info| {
        if let Some(gst::PadProbeData::Buffer(ref buf)) = info.data {
            counters.video_frames.fetch_add(1, Ordering::Relaxed);
            counters
                .video_bytes
                .fetch_add(buf.size() as u64, Ordering::Relaxed);
            if !first.swap(true, Ordering::SeqCst) {
                let _ = events.send(MediaEvent::FirstVideoFrame {
                    decoder: decoder_name.clone(),
                });
            }
        }
        gst::PadProbeReturn::Ok
    });

    Ok((pipeline, appsrc))
}

fn build_audio_pipeline(
    rate: u32,
    channels: u8,
    sink: Option<gst::Element>,
) -> anyhow::Result<(gst::Pipeline, gst_app::AppSrc)> {
    let pipeline = gst::Pipeline::with_name("wdl-audio");
    let appsrc = gst_app::AppSrc::builder()
        .caps(
            &gst::Caps::builder("application/x-rtp")
                .field("media", "audio")
                .field("payload", 97i32)
                .field("clock-rate", rate as i32)
                .field("encoding-name", "OPUS")
                .build(),
        )
        .is_live(true)
        .format(gst::Format::Time)
        .max_bytes(1024 * 1024)
        .build();
    let jb = gst::ElementFactory::make("rtpjitterbuffer")
        .name("wdl-audio-jb")
        .property("latency", JITTER_LATENCY_MS)
        .build()?;
    let depay = gst::ElementFactory::make("rtpopusdepay").build()?;
    let parse = gst::ElementFactory::make("opusparse").build()?;
    let dec = gst::ElementFactory::make("opusdec").build()?;
    let convert = gst::ElementFactory::make("audioconvert").build()?;
    let resample = gst::ElementFactory::make("audioresample").build()?;
    let sink_el = sink.unwrap_or_else(|| {
        gst::ElementFactory::make("fakesink")
            .property("sync", false)
            .build()
            .expect("fakesink always exists")
    });
    let _ = channels; // negotiated by caps on first payload

    pipeline.add_many([
        appsrc.upcast_ref(),
        &jb,
        &depay,
        &parse,
        &dec,
        &convert,
        &resample,
        &sink_el,
    ])?;
    gst::Element::link_many([
        appsrc.upcast_ref(),
        &jb,
        &depay,
        &parse,
        &dec,
        &convert,
        &resample,
        &sink_el,
    ])?;

    Ok((pipeline, appsrc))
}

async fn bus_watch(bus: gst::Bus, name: String, events: mpsc::Sender<MediaEvent>) {
    loop {
        if let Some(m) = bus.timed_pop_filtered(
            gst::ClockTime::from_mseconds(250),
            &[gst::MessageType::Error, gst::MessageType::Eos],
        ) {
            match m.view() {
                gst::MessageView::Error(e) => {
                    let reason = format!("{name}: {}", e.error());
                    if name.contains("audio") {
                        let _ = events.send(MediaEvent::AudioError { reason });
                    } else {
                        let _ = events.send(MediaEvent::VideoError { reason });
                    }
                    return;
                }
                gst::MessageView::Eos(_) => return,
                _ => {}
            }
        }
    }
}
