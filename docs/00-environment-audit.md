# 00 — Environment Audit

Audited: 2026-08-23 · Target machine: this workstation · All findings below were **verified live** unless marked otherwise.

## Host system

| Component | Finding | Status |
|---|---|---|
| OS | Fedora Linux 44 Workstation | Confirmed |
| Desktop | GNOME Shell 50.4 | Confirmed |
| Display server | Wayland (`XDG_SESSION_TYPE=wayland`) | Confirmed |
| GPU | Intel Alder Lake Iris Xe (8086:46a8), i915/xe driver | Confirmed |
| VAAPI driver | `libva-intel-media-driver` 26.2.4 (iHD) installed → HW decode H.264/H.265 capable | Confirmed |
| Audio server | PipeWire 1.6.8 + WirePlumber running | Confirmed |
| GStreamer runtime | 1.28.6 with plugins base/good/bad-free | Confirmed |
| GTK4 / libadwaita runtime | 4.22.4 / 1.9.3 installed; `-devel` packages available in dnf at same versions | Confirmed |

## GStreamer elements verified present (runtime)

| Element | Role in our design | Status |
|---|---|---|
| `gtk4paintablesink` | Zero-copy-ish video render into GTK4 scene graph (GSK textures), Wayland-native | **Confirmed present** |
| `pipewiresink` | Audio output into PipeWire graph | **Confirmed present** |
| Full RTP suite (`rtpjitterbuffer`, `rtph264depay`, `rtpopusdepay`, …) | Packet handling, jitter buffering, depayloading | **Confirmed present** |
| `dtlssrtpenc/dec` | Available if we ever need SRTP fallback path | Confirmed present |
| `opusdec`, `h264parse`, `avdec_h264`, `openh264dec` | SW decode fallbacks | Confirmed present |
| `vaapih264dec` / `vah264dec` | HW decode | **Not available on stock Fedora 44** (see Phase 1 finding below) |

## Phase 1 finding — decoder reality on Fedora 44 (verified 2026-08-23)

* `gstreamer1-vaapi` is only a **virtual Provide** of `gstreamer1-plugins-bad-free`; installing the name is a no-op.
* The free `libgstva.so` plugin ships VP8/VP9/MPEG2/JPEG VA elements but **H.264/H.265 are compiled out**.
* `gstreamer1-plugin-libav` links against `ffmpeg-free`, which **excludes H.264 decoding entirely** (`avdec_h264` does not exist).
* Net result: the only working H.264 decoder on a stock system is `openh264dec` (Cisco OpenH264, software).
* Consequence for roadmap: Phases 3–4 stream/decode fine with `openh264dec`. Hardware decoding (VAAPI/NVDEC) lands in Phase 5 by optionally enabling RPM Fusion freeworld (`gstreamer1-plugins-bad-freeworld` / full libav), documented as an explicit user choice because it involves a third-party repository.

## Networking / Wi-Fi — decisive finding

```
$ iw list → Supported interface modes: IBSS, managed, AP, AP/VLAN, monitor
```

**No P2P-client / P2P-GO modes.** This Intel Wi-Fi adapter cannot do Wi-Fi Direct,
which is a hard prerequisite for Miracast. `nmcli` shows no `p2p-dev-*` capability either.

Current link: PC is connected to **"Gabrial Mobile hotspot"** (the phone's hotspot).
This is exactly the deployment topology the recommended architecture needs:
phone ↔ PC direct IP connectivity with zero extra infrastructure.

## Toolchain status

| Tool | State | Action |
|---|---|---|
| Rust / Cargo | **Not installed** (dnf offers rust/cargo 1.97.1) | Install in Phase 1 bootstrap |
| GTK4/libadwaita/GStreamer/PipeWire devel | Not installed, all available in dnf | Install in Phase 1 bootstrap |
| clang | 22.1.8 present (useful for some -sys crates) | OK |
| avahi-browse | Present (mDNS debugging aid) | OK |

## Implications for architecture

1. **Miracast (Approach A) is impossible on this hardware today** regardless of software effort — no Wi-Fi Direct.
2. **Companion-app over IP (Approach B) matches the actual usage pattern already in place** (phone hotspot ↔ laptop).
3. The multimedia stack (GStreamer 1.28 + `gtk4paintablesink` + PipeWire) is modern enough to build everything we planned without workarounds.
