# Linux Wireless Display — Phase 0 Documents

A production-grade receiver that mirrors an Android phone's screen + audio to a
Fedora/GNOME/Wayland desktop via a companion app (primary) with protocol
abstraction for future built-in-cast protocols.

## Reading order

| Doc | Contents |
|---|---|
| [00-environment-audit.md](00-environment-audit.md) | Verified findings on this machine (stack versions, Wi-Fi P2P absence, what's missing) |
| [01-decision-record.md](01-decision-record.md) | **ADR-001: Approach A (Miracast sink) vs B (companion app)** — comparison, decision, consequences |
| [02-system-architecture.md](02-system-architecture.md) | Repo layout, modules, threading model, session FSM, A/V data paths |
| [03-protocol-design.md](03-protocol-design.md) | Transport choice, discovery, pairing, control messages, media plane |
| [04-security-model.md](04-security-model.md) | Threat model, crypto design, input-validation rules |
| [05-performance-plan.md](05-performance-plan.md) | Targets, instrumentation plan, phase exit criteria |

## Current status

* **Phase 0 complete** — architecture decided (companion-app primary). ADR-001 approved 2026-08-23.
* **Phase 1 complete** — receiver skeleton builds clean (0 clippy lints / 0 warnings),
  GTK4/libadwaita app runs on Wayland with session FSM, structured `tracing` logs,
  TOML config with safe regeneration, diagnostics page with truthful capability probes.
  Notable finding recorded in [00-environment-audit.md](00-environment-audit.md): stock Fedora 44 ships no
  hardware H.264 decoder element; software `openh264dec` is the working fallback until Phase 5's optional
  RPM Fusion freeworld enablement.
* **Phase 2 complete (receiver side)** — QUIC/TLS transport live (`net::listener`, quinn + rustls,
  control plane on a bi-directional stream), TLS identity with SHA-256 fingerprints (`net::identity`),
  SPAKE2 pairing with 6-digit codes, paired-device store + token reconnect (`net::pairing`),
  mDNS advertisement of `_wdlink._udp.local.` (`discovery`), GTK pairing dialog + known-devices list,
  `tools/mock-phone` CLI client. Gate green: rustfmt clean, 0 clippy lints, 32 tests pass
  (8 protocol · 18 receiver units · 6 end-to-end pairing flows incl. wrong-code refusal, stale-token
  refusal, malformed-frame termination, idle-timeout drop detection).
  Open items: ADR-003 media-path benchmark (QUIC datagrams vs UDP+AEAD) and real-phone validation drill.
* Next: **Phase 3** — H.264 video pipeline (GStreamer → `gtk4paintablesink`), hw/sw decoder fallback.

## Decision log

| ADR | Decision | Status |
|---|---|---|
| ADR-001 | Companion app primary; Miracast deferred behind `DisplayProtocol` trait | **Approved** |
| ADR-002 | GStreamer for media pipelines; `gtk4paintablesink` render; `pipewiresink` audio | Approved (Phase 0) |
| ADR-003 | QUIC datagrams (RFC 9221) carry RTP media; UDP+AEAD variant benchmarked in Phase 5 | Implemented Phase 3 |
