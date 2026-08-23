# Phase 3 — Media Plane (design)

Status: **approved 2026-08-24** · Implements §4–§7 of [02-system-architecture](02-system-architecture.md)

## Goal

A test-pattern "cast" flows end-to-end through the production media path:
mock-phone encodes → QUIC datagrams → receiver decodes → frames appear in the
GTK window and audio plays through PipeWire. The FSM enters `Streaming` only
when the first video frame is decoded (§23 truthfulness rule).

## Transport decision (ADR-003, interim)

Media packets ride **QUIC unreliable datagrams** (RFC 9221), one datagram =
one RTP packet. Rationale:

* The architecture doc's receive chain (`rtpjitterbuffer` → depayloader) is
  RTP-native; using RTP from day 1 means the Phase 4 Android encoder side and
  any future transport change do not touch pipeline code.
* QUIC gives us encryption for free — no bespoke AEAD layer to build now.
* The `UDP+AEAD` variant stays a Phase 5 benchmark; only the transport leg
  differs, payload above it is identical.

Datagrams are enabled by raising `TransportConfig::datagram_receive_buffer_size`
on both endpoints. Loss handling: jitterbuffer conceals; decoder errors trigger
`KeyframeRequest`.

## Packet format

No custom framing — each datagram is a bare RTP packet.

| Medium | PT | Clock | Payloader (sender) | Depayloader (receiver) |
|---|---|---|---|---|
| Video H.264 | 96 | 90 kHz | `rtph264pay` (mtu≈1100) | `rtph264depay` |
| Audio Opus | 97 | 48 kHz | `rtpopuspay` | `rtpopusdepay` |

Demux at ingest: RTP header byte 1 & 0x7F = payload type → route to the
matching medium queue. SSRC sanity-checked per medium; malformed/oversized
datagrams are dropped with a counter bump, never fatal (§7 error containment).

## Receiver pipelines

```
video: appsrc(application/x-rtp,pt=96,clock-rate=90000)
        ▸ rtpjitterbuffer(latency=40,do-lost=true)
        ▸ rtph264depay ▸ h264parse(caps-limited)
        ▸ decoder ladder: vah264* / vaapih264dec [hw if present]
                          ▸ openh264dec [sw]      ← only working option on this box today
        ▸ videoconvert ▸ gtk4paintablesink(paintable → GTK Picture)

audio: appsrc(application/x-rtp,pt=97,clock-rate=48000)
        ▸ rtpjitterbuffer(40ms) ▸ rtpopusdepay ▸ opusparse
        ▸ opusdec ▸ audioconvert ▸ pipewiresink(sync=false)
```

* Two independent `Gst::Pipeline`s (§5): audio trouble can never stall video.
* Phase 3 A/V sync is best-effort: both sinks run `sync=false`; drift metrics
  and master-clock playout arrive in Phase 5 tuning.
* First decoded frame: pad probe on the decoder src pad fires exactly once →
  `Negotiating→Streaming`, UI swaps in the paintable, fps/kbps counters start.
* Decoder failure at runtime → bus error → rebuild with next candidate;
  final fallback logs truthfully and shows "Video unavailable".

## Session flow additions

```
phone                              receiver
  │ SessionOffer{h264, opus, caps} ─▶│ validate against limits (≤1080p, ≤60fps, ≤25 Mbps)
  │ ◀─ SessionAnswer{accepted=true} ─│ builds pipelines (Paused), starts ingest tasks
  │ ══ RTP datagrams ══════════════▶ │ first frame ⇒ Streaming
  │ ◀─ KeyframeRequest (on decode loss)
  │ BitrateHint{kbps} ──────────────▶│ logged + applied to encoder (Phase 4 sender honours)
  │ Bye ────────────────────────────▶│ stop pipelines, Streaming→Closed
```

Media-idle watchdog: no video datagram for 5 s while Streaming → `Recovering`
→ keyframe request ×1 → still silent after 2 s → `Failed`.

## Sender (mock-phone `cast` subcommand)

```
videotestsrc is-live ! I420,WxH@fps ! openh264enc(bitrate,complexity=low,gop=60)
                     ! rtph264pay(pt=96,mtu=1100) ! appsink → try_send_datagram
audiotestsrc is-live ! S16LE 48k stereo ! opusenc ! rtpopuspay(pt=97) ! appsink → datagrams
```

Dropped datagrams (send buffer full) are counted and reported, never retried.

## Exit criteria

1. Integration test: real encode loopback reaches `Streaming` within 10 s.
2. Live run: moving test pattern visible inside the receiver window; audible
   tone when PipeWire output exists; diagnostics page shows live fps/bitrate.
3. Gate green: fmt/clippy/tests/build + Wayland smoke.
