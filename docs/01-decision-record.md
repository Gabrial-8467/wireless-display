# 01 — Architecture Decision Record: Approach A (Miracast sink) vs Approach B (companion app)

Status: **Proposed → awaiting approval** · Date: 2026-08-23 · Decides the primary product architecture.

---

## 1. What each approach actually requires

### Approach A — Miracast receiver ("Cast" / "Smart View" discovers us)

A Miracast *sink* is not one protocol, it is a stack of four:

1. **Wi-Fi Direct (P2P)** — phone and PC form a P2P group (GO/GC negotiation, WSC/WPS key exchange), typically while staying off the normal WLAN. Requires driver P2P support + `wpa_supplicant` built with `CONFIG_P2P` **and** `CONFIG_WIFI_DISPLAY`; NetworkManager ≥ 1.15.2 for management.
2. **Wi-Fi Display discovery** — WFD Information Elements in probe/association frames; source filters sinks by IE presence.
3. **RTSP session (TCP :7236)** — WFD M-series messages (`OPTIONS`, `GET_PARAMETER`, `SET_PARAMETER`) negotiating `wfd_video_formats`, `wfd_audio_codecs`, `wfd_client_rtp_ports`, `wfd_content_protection`.
4. **Media** — MPEG-TS over RTP/UDP carrying H.264 video + AAC/AC3/LPCM audio; many sources demand HDCP 2.x before streaming premium content.

Evidence from the field:

* The reference Linux implementation `gnome-network-displays` self-describes as *"an experimental implementation"*. It is a **source**, not a sink. Its own README documents that P2P setup *"is a relatively complicated process that can fail in a number of different ways"* and that common adapters need patched wpa_supplicant.
* Sink-side attempts on Linux (`miraclecast`) are effectively abandoned; there is no maintained Miracast sink on Linux today. **[Unsupported in practice]**
* Even on supported hardware (Intel AX200), P2P group formation with real-world sources fails intermittently (hostap mailing list, Jan 2024 thread) — radio-level debugging, high maintenance cost.
* **Android-side reality:** Google removed Miracast from stock Android in Android 6.0 (2015). Pixel/stock devices' "Cast" is **Google Cast (Chromecast) only** — they will never discover a Miracast sink. Samsung (Smart View), Xiaomi, ColorOS, EMUI etc. retain Miracast, but it is an OEM-by-OEM lottery that is shrinking over time.

### Approach B — Companion Android application

Uses public, documented, stable Android APIs:

| Concern | API | Notes |
|---|---|---|
| Screen capture | `MediaProjection` + `VirtualDisplay` | User consent dialog each session; foreground service type `mediaProjection` enforced on modern Android |
| Video encode | `MediaCodec` H.264 (surface input) | HW encoder on effectively every device since ~2015; low-latency keys (`KEY_LATENCY`, no B-frames, intra-refresh) on API 30+ |
| Audio capture | `AudioPlaybackCapture` (API 29+) | Captures device audio; apps may opt out (`allowAudioPlaybackCapture=false`), DRM streams excluded — documented limitations we surface honestly in UI |
| Audio encode | `MediaCodec` AAC-LC (universal); Opus where available | |
| Transport | Our protocol over UDP/TLS (see 03-protocol-design) | |

Works over any IP path: home Wi-Fi, **phone hotspot ↔ laptop (the user's current setup)**, USB tethering (near-zero latency, no radio contention).

## 2. Comparison against the required criteria

| Criterion | A: Miracast sink | B: Companion app |
|---|---|---|
| Protocol complexity | Very high: 4 stacked protocols, radio-level state machines, vendor quirks | Moderate: fully under our control, plain IP |
| Android compatibility | Stock/Pixel: **never works**. OEM subset only, shrinking | Any Android 10+ (audio capture limit; Android 7+ if audio optional) |
| Linux compatibility | Driver-dependent; this machine: **impossible (no P2P)** | Any Linux with PipeWire+GStreamer; this machine: ready |
| Latency | Typically 100–300 ms glass-to-glass, untunable by us | Tunable, realistic target ≤120 ms default / ≤80 ms low-latency mode |
| Audio support | AAC/AC3/LPCM in TS; tied to source behavior | Full control incl. capture opt-out handling, latency tuning |
| Video quality | Source decides bitrate/resolution; limited negotiation | We negotiate bitrate/resolution/FPS dynamically |
| Security | WPA2+WPS at link layer; HDCP pressure; no app-layer auth | TLS 1.3 + pinned identities + PAKE pairing (see 04) |
| Development complexity | Extreme (wpa_supplicant D-Bus, WFD RTSP stack, TS demux, HDCP) | High but tractable; all layers testable without radios |
| Hardware acceleration | Same decode options once frames arrive | Identical decode path — **no advantage either way** |
| Network requirements | Needs Wi-Fi Direct radio path | Any IP connectivity incl. hotspot/tethering |
| Future extensibility | Locked to WFD feature set | Protocol versioning ours; new codecs/features freely added |
| Maintenance cost | Chasing OEM/driver/supplicant bugs forever | Normal app maintenance |

## 3. Decision

**Adopt Approach B (companion Android app) as the primary architecture.**
Do **not** build the Miracast sink now. Three independent blockers make it unjustifiable:

1. This machine's Wi-Fi lacks P2P entirely (hardware blocker).
2. Stock-Android phones cannot use it (market blocker).
3. No maintained Linux sink exists to lean on; ecosystem is experimental (ecosystem blocker).

**Preserve optionality:** define a `DisplayProtocol` trait boundary in the receiver
(session lifecycle + media-source interface) so a future `miracast/` or
`google-cast-receiver/` subsystem can plug in behind the same UI without rewrites.
Note: emulating a **Google Cast receiver** (CASTV2/mDNS, plain IP — no Wi-Fi Direct)
is the more viable future path for "built-in Cast button" support than Miracast,
but its core protocol parts are undocumented/proprietary → **Experimental**, out of scope for v1.

## 4. Consequences & accepted risks of B

* We ship and maintain an Android app (Play sideloading/APK distribution initially).
* Users must install something on the phone — acceptable trade-off for reliability, latency, and security; clearly stated in product docs.
* AudioPlaybackCapture limitations (opted-out apps, DRM silence) are surfaced in the companion app's status UI rather than hidden.
* If requirements ever change to "must work with zero phone-side install," revisit ADR with the Google-Cast-emulation spike first, Miracast second.
