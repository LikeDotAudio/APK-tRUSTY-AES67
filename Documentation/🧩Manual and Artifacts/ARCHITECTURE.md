# Architecture — PLUGIN.AES67

How to use it and how it works inside. The repo [README](../../README.md) has the user-facing tables (routes, topics, loopback card); this page is the map of the code.

← [Manual and Artifacts](INDEX.md) · [Wiki home](../README.md)

## Using it

1. Bring the plugins up: `python3 'PODS/POD.manager/MANAGER.Docktor/bin/K8:runner.py' up` (the `sound` or `aes67` role), or alone: `COMPOSE_PROFILES=aes67 docker compose -f 'PODS/POD.THICK/PLUGINS/PLUGIN.AES67/Docker/docker-compose.yml' up -d --build`.
2. Open `/config` — through the gateway at `https://apk.local/plugins/aes67/ui/config`, or `http://127.0.0.1:8130/config` on the bench.
3. Pick the capture and playback devices (`hw:CARD=AES67,DEV=1` for the `snd-aloop` card), then enable senders and receivers. `/status` shows clock, card and per-stream counters; `/matrix` routes channels.
4. From the bus: publish `{"op":"enable","kind":"source","id":1}` (or `disable`, `apply`, `reload`) on `APK.audio/System/Protocols/aes67/outgoing/command`; the answer comes once on `<topic>/CommandResult`.

## Entry point

`SRC/src/bin/apk-aes67.rs` is one line: `apk_plugin_runner::main!(token = "aes67", lib = apkaudio_aes67, run = [run_daemon_agent, run_web_agent])`. The runner (`DISCOVERY.Engine/SRC/RUNNER`) owns the broker session, publishes `config.ini` retained at `<topic>/config`, sets the last will, answers the self test and exits non-zero if either agent returns or panics.

## The pieces (`SRC/src/`)

| File | Role |
|---|---|
| `daemon.rs` | The one `Daemon` (`global()`): holds `Settings`, starts/stops the clock, the card, every sender and receiver, SAP and RAVENNA. `apply` = validate → save → reconcile; `maintain` runs once a second; `health`, `status`, `command`. |
| `config.rs` | `Settings` and its slots; `from_ini` (first boot), `load`/`save` (`APK_AES67_STATE`, default `/var/lib/apkaudio/aes67/settings.json`), `validate`. |
| `clock.rs` | PTPv2 slave (E2E, one/two-step, BMCA over Announce, PI servo) or `CLOCK_TAI`; `media_ts` turns ns into RTP timestamps. The wire (header, Announce, Delay_Req) is the shared `apkaudio_ravenna::ptp`, the parser PLUGIN.PTP monitors with. |
| `audio.rs` | One ALSA capture + one playback stream (cpal) shared by every stream, with channel routes and taps. |
| `ring.rs` | Lock-free ring, `DriftReader` (adaptive resampler), level meter. |
| `tx.rs` / `rx.rs` | RTP out (packet goes when the media clock passes its last sample) / RTP in (silence for lost packets, late ones dropped). |
| `rtp.rs` | L16/L24 packing, `MAX_PAYLOAD` = 1440. |
| `sdp.rs` / `sap.rs` | Thin over the shared AoIP readers in PLUGIN.RAVENNA's crate (`apkaudio_ravenna::{sdp, sap}`, `default-features = false, features = ["serde"]`): SDP generate/parse (AES67 and ST 2110-30 attributes, conformance levels, the corpus test) / SAP header; `sap.rs` keeps this bridge's announce and browse (`Browser`). `ravenna.rs` takes the `/by-name/` encoding from `apkaudio_ravenna::rtsp`. |
| `ravenna.rs` | Per-sender mDNS `_rtsp._tcp` + `_ravenna_session`, and an RTSP server answering `DESCRIBE` only. |
| `net.rs` | Interface list (getifaddrs), multicast listener and sender sockets (SO_REUSEPORT, SSM). |
| `web.rs` + `web/*.html` | `run_web_agent`: the HTTP listener, `/status`, `/config`, `/matrix`, `/api/*`; the pages are compiled in. |
| `bus.rs` | `run_daemon_agent`: the MQTT session, agent report, retained stream documents, `Clock`/`Audio` restatement, the command door. `topic()` reads `config.ini`'s `topic` key. |
| `self_test.rs` | The runner's self-test door (`self_test_json`). |

## Data flow

```
 ALSA capture ─▶ audio.rs ─▶ ring ─▶ tx.rs ─▶ RTP multicast ─▶ network
                                     ▲ media clock (clock.rs)
 network ─▶ rx.rs ─▶ ring + DriftReader ─▶ audio.rs ─▶ ALSA playback
 SDP: sdp.rs ─▶ sap.rs announce / ravenna.rs mDNS+RTSP     SAP heard ─▶ sap::Browser ─▶ receivers that follow by name
 web / bus command ─▶ Daemon::apply ─▶ settings.json + reconcile
 Daemon::status ─▶ /api/status (live numbers)   and ─▶ bus.rs (state only, on change)
```

## Files outside `src/`

- `SRC/config.ini` — first-boot defaults and the `topic` key; compiled in (`config::CONFIG_INI`) and published by the runner.
- `SRC/loopback/aes67-loopback.sh` + `Docker/Dockerfile.loopback` — the one-shot `snd-aloop` loader (`Plugin-AES67-Loopback`).
- `Docker/docker-compose.yml` — services `aes67-loopback` and `aes67`; `extends:` `DISCOVERY.Engine/Docker/plugin-base.yml`.

## Tests

`cd PODS/POD.THICK/DISCOVERY.Engine/SRC && cargo test -p apkaudio-aes67` — 31 unit tests (clock, ring, rtp, sdp, sap, ravenna, config, net, bus, self test). Checked 2026-10-30: 31 passed.
