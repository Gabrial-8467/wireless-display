# 05 — Performance Plan & Targets

Status: Proposed · Principle: **instrument first, then optimize.** Every number below must be measurable by the diagnostics layer before we tune anything.

## 1. Targets (default mode / low-latency mode)

| Metric | Target | Stretch | Measured via |
|---|---|---|---|
| Glass-to-glass latency (LAN Wi-Fi) | ≤ 120 ms | ≤ 80 ms | capture-timestamp header extension + clock offset estimate |
| A/V drift (sustained) | < ±40 ms | < ±20 ms | sink render-time deltas |
| Video FPS @1080p | 30 steady, 60 capable | — | jitterbuffer/sink pad probes |
| Dropped frames (steady LAN) | < 0.5 % | — | QoS events counter |
| Receiver CPU (hw decode) | < 15 % of one core @60fps | < 8 % | /proc sampling thread |
| Receiver CPU (sw decode) | < 80 % one core @30fps | — | same |
| RSS steady state | < 250 MB | < 150 MB | /proc/self/statm |
| Decode latency | < 10 ms hw | — | decoder element latency query + probes |
| Reconnect after Wi-Fi blip | < 3 s to picture | — | session FSM timestamps |

Latency budget (default mode): encode 5–15 ms → packetize/net 2–10 → jitterbuffer 40 → decode 3–10 → render/vsync 8–16 ≈ **58–91 ms** typical.

## 2. Instrumentation plan (built in Phase 1–3, before tuning)

* `tracing` structured logs with spans per stage (`net.ingest`, `video.decode`, `audio.play`, `session.fsm`) — the §18 log format.
* Metrics registry (`diag::metrics`): counters/gauges exported to UI Diagnostics page and to a debug JSON file on demand:
  fps, decode fps, dropped, RTT, video/audio bitrate, jitterbuffer fill %, loss %,
  A/V drift, clock offset estimate, CPU%, RSS, PipeWire quantum.
* Latency truth-source: sender embeds capture timestamp (custom RTP header extension); receiver computes `now − capture_ts + offset_estimate`. Clock offset refreshed every 10 s (median of 8 exchanges).
* Pad probes on: depay src (arrival), decoder src (decode done), sink (rendered) → per-stage histograms.
* Mock-phone emits its own stats so both sides' numbers can be cross-checked in tests.

## 3. Optimization levers (only after metrics prove need)

1. Jitterbuffer latency auto-tune (40 ms default ↔ floor 15 ms in low-latency mode with keyframe-request-on-loss).
2. Encoder-side (Android): `KEY_LATENCY`, intra-refresh instead of periodic IDR, bitrate ramp via receiver `BitrateHint`.
3. Zero-copy checks: confirm DMABuf path VAAPI→`gtk4paintablesink` (trace GstGL memory ops) before touching render code.
4. QUIC datagram vs UDP-AEAD media path A/B benchmark (Phase 2 exit criterion).
5. Frame pacing: late-frame drop policy vs display-vsync alignment.

## 4. Phase exit criteria (condensed from roadmap)

| Phase | Exit criterion |
|---|---|
| 1 Skeleton | app launches on Wayland/GNOME, device list UI live, logs+config work, zero panics in 24 h idle soak |
| 2 Transport | mock-phone pairs & connects; malformed-packet suite passes; disconnect/reconnect drill green |
| 3 Video | 1080p30 H.264 mock stream renders; hw/sw fallback verified; resolution-change handled live |
| 4 Audio+sync | Opus audio via PipeWire; drift metric < ±40 ms over 1 h soak |
| 5 Performance | targets table met on real phone; metrics page populated end-to-end |
| 6 Security | pairing flow complete; fuzz corpus clean; pinning/re-pair tested incl. hostile-peer negative tests |
| 7 UI polish | settings/diagnostics/error flows done; a11y labels; GNOME HIG review |
| 8 Packaging | RPM builds on Fedora 44; desktop entry+icon; firewalld service file; dependency self-check |
