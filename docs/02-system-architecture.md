# 02 — System Architecture

Status: Proposed · Companion to ADR-001 (Approach B selected).

## 1. Repository layout

Monorepo, two products + shared protocol crate:

```
wireless-display/
├── Cargo.toml                  # workspace
├── crates/
│   ├── wd-protocol/            # pure protocol types+codec: no UI, no I/O deps → shared with mock & fuzzed
│   └── wd-receiver/            # the Linux application (bin)
│       └── src/
│           ├── main.rs
│           ├── app/            # AdwApplication wiring, lifecycle, global state store
│           ├── ui/             # window, device_list, connection_view, settings, diagnostics (GTK4/libadwaita)
│           ├── session/        # ConnectionManager state machine, event bus
│           ├── discovery/      # DiscoveryProvider trait + mdns + manual providers
│           ├── net/            # listener, QUIC/TLS endpoints, packet framing, limits
│           ├── video/          # pipeline builder, decoder selection (hw→sw), render sink glue
│           ├── audio/          # pipeline builder → pipewiresink, latency handling
│           ├── sync/           # clock offset estimation, drift measurement
│           └── diag/           # tracing setup, metrics registry, self-checks
├── tools/mock-phone/           # dev-only streaming client implementing wd-protocol (clearly non-production)
├── android/                    # companion app (Gradle project, Kotlin) — Phase 3+
├── docs/
└── tests/                      # integration tests driving mock-phone ↔ receiver
```

Rules: `wd-protocol` never depends on GTK/tokio; `ui/` never touches sockets directly;
platform/GStreamer specifics live only under `video/`, `audio/`, `net/`.

## 2. Threading / runtime model

| Domain | Runtime | Notes |
|---|---|---|
| UI | GLib MainContext (GTK main thread) | UI thread never blocks; all work arrives as messages |
| Networking + control | Tokio multi-thread runtime | discovery, QUIC endpoint, session FSM |
| Media decode/render | GStreamer threads | owned by pipelines |
| Audio output | PipeWire graph callback | via `pipewiresink` |

Bridge: tokio side publishes typed events on a broadcast bus (`session::events`);
UI subscribes through an `async-channel` drained on the MainContext. UI commands flow
back through a command queue handled by the session manager. **No shared mutable state**;
state lives in `SessionManager`, mirrored into GTK models via events.

## 3. Session state machine (ConnectionManager)

```
Idle → Discovering → Pairing? → Connecting → Negotiating → Streaming ⇄ Recovering
                        │                        │               │            │
                        └────── Failed ◀─────────┴───────────────┴────────────┘ → Closed
```

* `Negotiating`: exchange Hello/Offer/Answer, verify pinned identity, agree codecs/caps.
* `Streaming`: media flows; watchdogs: RTT pings every 2 s, media-idle timeout 5 s.
* `Recovering`: transient loss → re-request keyframe, re-estimate clocks; escalate to `Failed` after budget exhausted.
* Every transition emits a structured log + user-visible status; failures map to friendly messages + recovery action (§17 of requirements).

## 4. Video data path (Linux receiver)

```
QUIC datagrams (RTP/H264 packets, AES-GCM)
   │  net::MediaIngest  — validate seq/ssrc/size, replay window
   ▼
appsrc caps=application/x-rtp,media=video,payload=96,clock-rate=90000
   ▼
rtpjitterbuffer (latency=40ms adaptive, do-lost=true)
   ▼ rtph264depay → h264parse (caps-limited: profile≤high, w/h/fps bounded)
   ▼ decoder auto-negotiation:
       1. vaapih264dec / vah264dec   (VAAPI — Intel iHD present here)   [hw]
       2. nvh264dec                   (NVIDIA, if present)              [hw]
       3. avdec_h264                  (libav — guaranteed fallback)     [sw]
   ▼
gtk4paintablesink (paintable exported into widget tree; DMABuf/GL textures, Wayland-native,
                   zero memcpy of decoded frames into Cairo)
```

Design points:

* Decoder choice made at pipeline build from a capability probe (`gst_element_factory_list`), recorded in diagnostics; runtime fallback if element fails to link/state-change.
* Resolution/orientation changes arrive as new SPS → `h264parse` renegotiates downstream; paintable resizes naturally. No pipeline teardown required.
* Keyframe recovery: decoder drop events → send `KeyframeRequest` on control stream.

## 5. Audio data path

```
QUIC datagrams (RTP/Opus packets)
   ▼ appsrc(application/x-rtp,clock-rate=48000)
   ▼ rtpjitterbuffer (latency=40ms) → rtpopusdepay → opusparse → opusdec
   ▼ audioconvert/resample (defensive) → pipewiresink (stream moves to default sink;
                                              follows PipeWire device changes automatically)
```

Audio pipeline is fully independent of video pipeline (separate GstPipelines, separate
threads) so audio problems can never stall video and vice-versa; only the *clock* is shared.

## 6. Clock & A/V synchronization strategy

* Sender stamps both RTP sessions from one capture-side monotonic clock (single random RTP offset for A+V — we own the sender, so this is guaranteed).
* At session start, receiver runs NTP-style offset estimation over the reliable stream (T1..T4 exchange, repeated, median filter) to convert capture-time → receiver-time.
* Both pipelines share one `GstSystemClock`; sinks run `sync=true`.
* Audio is the master timeline (PipeWire quantum governs playout); video late frames are dropped, early frames wait — standard AV-sync semantics, drift measured continuously.
* Exposed metrics: estimated glass-to-glass latency, A/V drift ms, jitterbuffer fill.

## 7. Error containment

| Failure | Containment |
|---|---|
| Malformed packet | Parser returns `Err` before any allocation beyond limits; connection reset for that session only |
| Decoder crash | GStreamer bus error → rebuild pipeline with software decoder, emit warning |
| PipeWire unavailable | Video continues; UI shows "Audio unavailable" |
| Network drop | Watchdog → `Recovering` → friendly dialog with `[Reconnect]` after grace period |
| UI freeze risk | All blocking work off MainContext (enforced by design review checklist) |
