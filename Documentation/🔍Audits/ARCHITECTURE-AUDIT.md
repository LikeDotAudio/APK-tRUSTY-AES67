# 🔍 Architecture audit — PLUGIN.AES67 (2026-10-30)

Audited against the APK.audio multi-repo architecture standard: coupling, boundaries, duplication, contracts.
← [Audits](INDEX.md) · Improvements: [🛣Roadmap/IMPROVEMENTS.md](../%F0%9F%9B%A3Roadmap/IMPROVEMENTS.md)

## Bounded context
AES67 / SMPTE ST 2110-30 senders and receivers (16×16, ≤8 ch) bridged to ALSA, with a user-space PTPv2 slave, SAP announce/browse and RAVENNA RTSP DESCRIBE. Rust, ~5.3 k lines in `SRC/src/` (15 modules) plus 4 embedded web pages; its own port 8130 and state volume are a deliberate house exception. Sampled: `Cargo.toml`, `Docker/*`, `bin/apk-aes67.rs`, `bus.rs`, `web.rs`, `config.rs`, `clock.rs`, `sap.rs`, `ravenna.rs` headers, test inventory, CI.

## Context map
| Talks to | How (topic / endpoint / file / import) | Relationship |
|---|---|---|
| APK.plugin.SDK (`apk-plugin-runner`, `apkaudio-contracts`) | git deps `tag = "sdk-v0.5"`; `apk_plugin_runner::main!(token = "aes67", …)` | Customer of a versioned Shared Kernel |
| Bus | `<topic>/Agent/state|detail|interface` (+ `incoming/` twins, retained), `Stream/Source|Sink/<id>` (retained on change), `Clock`, `Audio` (QoS 0), `outgoing/command` → `CommandResult` (`bus.rs` header) | Published Language |
| DISCOVERY.Engine | compose `extends: ../../../DISCOVERY.Engine/Docker/plugin-base.yml` (line 59); binary is built inside the Engine's `plugins` image | Customer, by sibling path |
| Browser / operators | `:8130/status`, `/config`, `/matrix`, `/api/*` (`web.rs`), writes gated by `APK_AES67_TOKEN` | own HTTP (deliberate exception) |
| Host kernel | `aes67-loopback` one-shot, `CAP_SYS_MODULE`, `modprobe snd-aloop` (`Dockerfile.loopback`) | infrastructure |
| PLUGIN.SAP / PLUGIN.RAVENNA / PLUGIN.PTP | none in code — each re-implements the same wire protocols | parallel implementations |

## Scorecard
| Category | Score (0–3) | Finding |
|---|---|---|
| Coupling & Cohesion | 3 | Code depends on the SDK by tag and the shared AoIP readers by `rev`; the loopback image builds from this repo's own `SRC/`. The compose `extends` / engine-built image is the house pattern, kept by design (PRINCIPLES.md). |
| Boundary Integrity | 3 | `rumqttc` only in `bus.rs`, `cpal`/`alsa` only in `audio.rs`, `mdns-sd` only in `ravenna.rs`; wire formats only in the shared readers. |
| Duplication Control | 3 | SAP, SDP, PTPv2 and the RTSP path codec come from `apkaudio_ravenna::{sap, sdp, ptp, rtsp}`, shared with PLUGIN.SAP, RAVENNA, DANTE and PTP; the private `style.css` is kept by design (standalone pages). |
| Testing & Contracts | 3 | 31 unit tests incl. payload-shape, the shared SDP corpus against the pinned rev, API-version routing, settings schema; SDK CI on `plugin-ci.yml@sdk-v0.5`. |
| Change management | 3 | SDK and CI pinned to `sdk-v0.5`; HTTP API `/api/v1/` with a stated alias retirement; settings file `schema` + `migrate`. |
Score key: 3 meets the standard · 2 minor gaps · 1 significant gaps · 0 absent / violated.

## Findings
1. **Good — clean adapters.** Each vendor library sits in exactly one module (grep: `cpal::`/`alsa::` → `audio.rs`; `mdns_sd` → `ravenna.rs`; `rumqttc` → `bus.rs`). Commands from the bus and the web page go through one `Daemon::apply` (`bus.rs` header), so there is one write path.
2. **Good — bus payload discipline.** `bus.rs` publishes configuration and state retained, never counters; live numbers stay on `GET /api/v1/status`. Topic root is read from `config.ini` the same way the runner reads it, so config and live leaves cannot diverge (tested).
3. **Needs owner — standalone build.** The re-pinned `apkaudio-ravenna` rev resolves outside the monorepo only once PLUGIN.RAVENNA is pushed; in the monorepo the engine `[patch]` builds it today.

Delivered items are recorded in [🌍Publishing/DELIVERY.md](../%F0%9F%8C%8DPublishing/DELIVERY.md#delivered-from-the-2026-10-30-audit).
