# 03 — Protocol Design (Companion Protocol "WDL/1")

Status: Proposed draft — frozen at Phase 2 implementation, then versioned.
All claims tagged: **[Confirmed]** verified against docs/source · **[Likely]** high confidence, verify at impl time.

## 1. Transport selection

Options evaluated:

| Option | Pros | Cons | Verdict |
|---|---|---|---|
| Raw TCP for everything | simple | HoL blocking kills video latency | rejected |
| RTP/SRTP over UDP (GStreamer dtlssrtp) | standard | DTLS handshake + cert plumbing duplicated with control channel; two security stacks | fallback option |
| WebRTC (libwebrtc / webrtc-rs) | battle-tested stack | 30 MB+ native dep on Android, opaque latency tuning, signaling still custom | rejected for v1 |
| **QUIC (quinn/rustls): reliable streams = control; unreliable datagrams (RFC 9221) = media** | one encrypted transport; no HoL blocking on datagrams; built-in TLS 1.3 + certs; connection migration survives IP change (Wi-Fi↔tethering); congestion control shared | Rust-only mature lib → Android side needs its own QUIC or we keep media on plain UDP+AEAD | **selected**, with documented fallback |

**Android-side reality check [Likely]:** mature Kotlin QUIC clients (Cronet) expose
request APIs, not raw datagram sockets. Therefore the wire format is designed so that:

* Control plane: runs over the QUIC stream **or** a TLS-TCP stream — same messages either way (transport negotiated in `Hello`).
* Media plane: RTP packets carried as QUIC datagrams when both sides support it; otherwise UDP with AES-256-GCM per-packet AEAD using session keys distributed over the control channel. Identical RTP payload layout in both modes.

This keeps the Android implementation on boring, provable primitives (SSLSocket + DatagramChannel + javax.crypto AEAD) while leaving the QUIC fast path open. Decision re-validated by Phase 2 benchmark.

## 2. Discovery

Pluggable trait:

```rust
trait DiscoveryProvider {
    async fn run(&self, sink: mpsc::Sender<DeviceAnnouncement>);
}
```

* **MdnsDiscovery** — advertise `_wdlink._udp.local` (TXT: `v=1`, `name`, `fingerprint=<sha256 of TLS cert>`, `pairing=open|paired`, `caps`). Use pure-Rust `mdns-sd` (no avahi C dependency; works inside Flatpak sandbox). Caveat [Confirmed]: AP/client isolation on some hotspots blocks multicast → hence Manual provider always present.
* **ManualDiscovery** — user types `ip:port` or scans QR from companion app (QR contains ip/port/fingerprint/pairing code).
* Future providers (`WifiDirect`, `CastReceiver`) implement the same trait behind ADR-001's abstraction.

Device identity = SHA-256 fingerprint of receiver TLS certificate. Display name is untrusted metadata → rendered as text, length-capped.

## 3. Pairing (first contact)

```
Phone                          Linux receiver
  │  mDNS/QR: addr + fingerprint   │
  │───────── TLS connect ─────────▶│  (server presents self-signed ECDSA cert)
  │ verify fingerprint vs QR/mDNS  │
  │── PairRequest(pin_proof) ─────▶│  SPAKE2 over 6-digit pairing code shown on BOTH screens;
  │◀──── PairAccept(ephemeral) ────│  code binds session even if fingerprint check is skipped,
  │   register client-cert         │  prevents rogue LAN peer pairing while user confirms
  │◀── paired device stored ───────│  (phone generates key in Android Keystore; receiver pins it)
```

Subsequent connections: mutual TLS with pinned certificates, silent reconnect.
Re-pairing = delete device in settings. All of this lives in `net::security`.

## 4. Control message catalog (reliable channel, length-prefixed, postcard/serde)

Version field first byte-pair (`WDL`, major.minor). Unknown message types are ignored
(forward compatible). Every message bounded (max 4 KiB).

| Message | Direction | Purpose |
|---|---|---|
| `Hello{proto_ver, device_name, capabilities}` | phone→pc | announce |
| `HelloAck{proto_ver, accepted_caps}` | pc→phone | select transport mode, limits |
| `PairRequest/PairChallenge/PairConfirm` | both | §3 |
| `SessionOffer{video:{codec,res,fps,bitrate_max}, audio:{codec,rate,ch}}` | phone→pc | what it can send |
| `SessionAnswer{accepted, constraints{max_w,max_h,max_fps,max_bitrate}, rtp_ports/ssrcs}` | pc→phone | enforce our caps |
| `KeyframeRequest{ssrc}` | pc→phone | packet-loss recovery |
| `BitrateHint{target_kbps}` | pc→phone | congestion feedback |
| `ClockSync{t1,t2,t3,t4}` | both | offset estimation (median of N) |
| `StatsReport{fps,enc_kbps,loss_pct}` | phone→pc | diagnostics mirror |
| `Ping/Pong{nonce}` | both | RTT + liveness watchdog |
| `Bye{reason}` | both | clean teardown |

## 5. Media plane rules

* Video: RTP/AVP, H.264 payloaded per RFC 6184 (single NALU / FU-A; STAP-A allowed), `clock-rate=90000`. MTU-safe ≤1400 B packets enforced by sender.
* Audio: RTP Opus `clock-rate=48000` stereo 10 ms frames; AAC-LC alternative reserved codec id.
* Both sessions share sender capture-clock mapping (see 02 §6). Sequence numbers + SSRC validated at ingest; replay window 128.
* Sender may change resolution/orientation mid-stream via new SPS/PPS in-band (receiver pipeline renegotiates; no protocol round-trip needed).

## 6. Disconnect semantics

| Trigger | Behavior |
|---|---|
| `Bye` received/sent | graceful: stop pipelines, close endpoint, state `Closed` |
| QUIC idle timeout / ICMP unreachable / 5 s media silence | state `Recovering`; attempt reconnect to last addr for 10 s; then `Failed` + dialog `[Reconnect] [Close]` |
| Phone hotspot gone / IP changed | QUIC connection migration attempts; else rediscovery finds new address automatically |

## 7. What is deliberately NOT in v1

Miracast/WFD interop, Google Cast emulation, UIBC input injection (PC→touch), H.265
(structured for later), multi-device concurrent sessions (single-session v1).
