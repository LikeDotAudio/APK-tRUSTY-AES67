# APK:plugin:AES67

AES67 and SMPTE ST 2110-30 **senders and receivers, bridged to an ALSA sound card**, in one container: 16 senders and 16 receivers, each up to 8 channels wide. It has a web page for configuration and another for status.

| | |
|---|---|
| **Recipe** | `Docker/docker-compose.yml`. There is no plugin Dockerfile here: the container runs `apk-aes67` from the shared plugin image that `DISCOVERY.Engine/Docker/Dockerfile` builds. `Docker/Dockerfile.loopback` is only the one-shot `snd-aloop` loader below |
| **Source** | `SRC/` is the crate `apkaudio-aes67` (bin `apk-aes67`, run under `apk-plugin-runner`), a member of `PODS/POD.THICK/DISCOVERY.Engine/SRC/Cargo.toml` |
| **Family / Category** | PLUGINS — audio transport; one of the `sound` plugins with SOUNDCARD_IN/OUT |
| **Started by** | the runner, with the rest of the plugins: `python3 'PODS/POD.manager/MANAGER.Docktor/bin/K8:runner.py' up`. `DISCOVERY.Engine/Docker/docker-compose.yml` `include:`s this compose file, and its `sound`/`aes67` profiles are in DockTor's default role list |
| **Profiles** | `sound` (with the other card plugins), `aes67` (alone) |
| **Web** | `https://apk.local/plugins/aes67/ui/` through the gateway — `status`, `config` (also `senders`, `receivers` — one page, the path picks the tab), `matrix`, `api/*`. On the bench itself also `http://127.0.0.1:8130/…`. The listener binds `APK_AES67_HTTP_BIND` (default `127.0.0.1`); the port comes from `APK_AES67_HTTP_PORT`, else `http_port`. `ui_path` in `config.ini` names the gateway path in the plugin's `incoming/api` |
| **Other ports** | UDP 319/320 (PTP), 9875 (SAP), TCP `rtsp_port` 8554 (RAVENNA DESCRIBE), the RTP multicast groups — all on the host network |
| **Needs** | `/dev/snd`, group `audio`, `NET_BIND_SERVICE` (PTP on 319/320), host networking (multicast) |
| **Keeps** | `apk-audio-aes67-state`, a volume that holds `settings.json`, which is what `/config` saves |

**First run:** on `/config`, pick the capture and playback devices. ALSA's `default` can be a device that won't open, such as an HDMI output with nothing connected. Then enable the senders and receivers you want.

**Wiki:** [Documentation/README.md](Documentation/README.md) — principles (with the code that keeps each), architecture, delivery.

## A rewrite of aes67-linux-daemon, turned inside out

[bondagit/aes67-linux-daemon](https://github.com/bondagit/aes67-linux-daemon) loads the RAVENNA kernel module. That module creates a **virtual** ALSA card, steers the card's clock from a PTP slave in the kernel, and a C++ daemon with a web UI configures it. None of that can run in a plugin container, which has a read-only root, runs as uid 1000 and cannot load kernel modules. So this plugin keeps the function and changes the structure:

| aes67-linux-daemon | this plugin |
|---|---|
| a virtual card that other programs open | opens a **real** card and moves its channels to and from the network |
| PTP slave in the kernel module | a PTPv2 slave in user space (`clock.rs`), or the host's `CLOCK_TAI` |
| the kernel steers the card to PTP | the card runs free, and an **adaptive resampler** (`ring.rs`) absorbs the ppm difference |
| C++ daemon, REST API, React UI | one Rust binary under `apk-plugin-runner`, a REST API, two pages compiled into the binary |
| SAP, mDNS/RTSP (RAVENNA) | SAP (RFC 2974), which Dante-in-AES67-mode, RAVENNA and Lawo all speak; and, per sender, RAVENNA mDNS + an RTSP server that answers `DESCRIBE` only (`ravenna.rs`) |

## What it does

- **Senders.** Each sender takes 1–8 capture-card channels and sends them as L16 or L24 at a packet time of 125, 250 or 333 µs, or 1 or 4 ms. A packet goes out when the media clock passes its last sample, so the network rate follows PTP and not the card's crystal. Each sender is announced over SAP and its SDP is served at `/api/sources/{id}/sdp`.
- **Receivers.** Each receiver takes one stream, either by **following a SAP session by name** (it re-tunes when the sender moves or restarts) or from a pasted SDP. Its channels map onto playback-card channels. Lost packets are replaced by silence at their correct position in time, and late packets are dropped and counted. The playout buffer is `delay_ms`.
- **AES67 and ST 2110-30.** Both profiles write and read `ts-refclk`, `mediaclk:direct=0` and `source-filter`. ST 2110-30 adds `channel-order=SMPTE2110.(…)`, and the plugin enforces its conformance levels: A is 48k/1 ms/≤8 ch, B is 48k/125 µs/≤8 ch, and C and the X levels are also accepted. AM824 (ST 2110-31) is refused by name.
- **Packet-size limit.** No packet may exceed 1440 bytes (AES67 §7.2, `rtp.rs` `MAX_PAYLOAD`). If a sender's settings would, `/config` shows the arithmetic.
- **RAVENNA.** A sender with `ravenna` ticked is advertised over mDNS as `<session>._rtsp._tcp.local.` (subtype `_ravenna_session`) once it is running, and `DESCRIBE rtsp://<node>:<rtsp_port>/by-name/<session>` returns its SDP. No SETUP/PLAY: the stream is multicast and already flowing. A sender that stops is unregistered.

## Clock, stated honestly

The built-in slave uses E2E delay request/response, handles one-step and two-step masters, and runs the best-master comparison over Announce messages. It timestamps in user space, just after `recv`. A lucky-packet filter and a slow PI servo bring it to within tens of µs of the grandmaster, about one sample at 48 kHz. That is enough for adaptive playout. It is not a boundary clock. For sample-accurate alignment, run `ptp4l` + `phc2sys` on the host and set **Clock = System**, which reads `CLOCK_TAI`.

If 319/320 will not bind because the capability is missing, the clock state reads `faulty` with the reason, and the daemon runs on `CLOCK_TAI`.

## Seeing the network in your desktop's sound settings: `snd-aloop`

The original daemon's kernel module **creates** a sound card. This plugin can't load drivers from inside its container, so it **opens** an existing card and connects that card's jacks to the network. With a physical card, the network comes out of the speakers and goes in through the mic. It doesn't show up as a device in your system sound settings.

The kernel's own virtual loopback driver, **`snd-aloop`**, provides that device. It creates a card with two linked sides: what is played into one side can be recorded from the other.

```
   your apps (PipeWire)                        Plugin-AES67
 ┌─────────────────────┐                  ┌──────────────────────┐
 │ hw:CARD=AES67,DEV=0 │ ── play ──────▶  │ hw:CARD=AES67,DEV=1  │ ─▶ Senders  ─▶ network
 │  "Loopback Analog   │ ◀── record ────  │ (capture + playback) │ ◀─ Receivers ◀─ network
 │   Stereo" device    │                  │                      │
 └─────────────────────┘                  └──────────────────────┘
```

The two sides are symmetric. PipeWire takes **DEV=0** as soon as the card appears, so the plugin uses **DEV=1**, the side that's left.

**It is loaded for you at launch.** The compose file runs a one-shot container, `Plugin-AES67-Loopback` (`Docker/Dockerfile.loopback`, `SRC/loopback/aes67-loopback.sh`), **before** the plugin:

- if a card with id `AES67` already exists, it does nothing;
- if `snd-aloop` is already loaded under another id, it leaves it alone and says which id to pick;
- otherwise it runs `modprobe snd-aloop id=AES67 pcm_substreams=1`, using the **host's** `/lib/modules` mounted read-only, so the module always matches the running kernel.

It is the only container here with a kernel capability. It has `SYS_MODULE` and nothing else: no network, a read-only root, and it exits within a second. **It never blocks the launch.** If the host refuses the module, the plugin still starts on your physical card, and the container's log prints the commands to run by hand:

```sh
docker logs Plugin-AES67-Loopback
# by hand, once:
sudo modprobe snd-aloop id=AES67 pcm_substreams=1
# and across reboots:
echo snd-aloop | sudo tee /etc/modules-load.d/aes67.conf
echo 'options snd-aloop id=AES67 pcm_substreams=1' | sudo tee /etc/modprobe.d/aes67.conf
```

`Plugin-AES67` waits for this container to finish (`depends_on: service_completed_successfully`). This ordering matters because Docker resolves `devices: /dev/snd` when a container is **created**, so a card that appears later isn't visible inside it. If you load the module by hand while the plugin is running, recreate the container: `up -d --force-recreate aes67`. A plain restart isn't enough.

**Using it:** on `/config`, set both **Input device** and **Output device** to **"Loopback PCM — Loopback"**, i.e. `hw:CARD=AES67,DEV=1`, the side PipeWire left free. It is the only loopback device the list offers. PipeWire shows the other side as **"Loopback Analog Stereo"**, as both an output and an input. Your apps play into it, and the audio leaves on the senders. What the receivers play comes back into your apps from it. Route the channels on the **MATRIX** tab.

**Limits, stated plainly:**
- A loopback substream carries at most **32 channels**, so at most 4 of the 16 eight-channel streams can pass through it in each direction. For all 128 channels, use a real multichannel card.
- Both sides of a loopback must agree on rate, format and channel count, and **whichever side opens first sets them.** PipeWire's default profile opens it as stereo. For more than 2 channels, switch the Loopback card to the **Pro Audio** profile (`pavucontrol` → Configuration), and give **Input/Output channels to open** the same count on `/config`.

| Variable | Default | |
|---|---|---|
| `APK_AES67_LOOPBACK` | `on` | `off` skips the launcher |
| `APK_AES67_LOOPBACK_ID` | `AES67` | the card id, i.e. `hw:CARD=<id>` |
| `APK_AES67_LOOPBACK_SUBSTREAMS` | `1` | substreams per side (one per program that opens it) |

## Sound card latency

Each buffer grows automatically to at least 1.5 × the largest burst the card has delivered. On a desktop, ALSA's `default` device goes through PipeWire, which delivers 2048-frame (43 ms) bursts, and buffers settle around 65 ms. For low latency, pick the card's `hw:` device on `/config` and set **ALSA period** to 64–256 frames.

## On the bus

| Topic under `APK.audio/System/Protocols/aes67/` | |
|---|---|
| `config`, `status`, `incoming/api`, `incoming/telemetry`, self test | from `apk-plugin-runner`, as for every plugin |
| `Agent/state`, `detail`, `interface` | retained: `listening`, or `partial` with a list of what is missing |
| `Clock`, `Audio` | QoS 0, not retained, restated every 5 s |
| `Stream/Source/<id>`, `Stream/Sink/<id>` | **retained, published only when they change**: configuration and state (`sending` / `receiving` / `waiting` / `disabled`, error, SDP, sender), **no counters or levels**. A removed stream is cleared. The live numbers are on `/status` and `GET /api/status` |
| `outgoing/command` → `CommandResult` | `{"op":"enable"\|"disable","kind":"source"\|"sink","id":1}`, `{"op":"apply","settings":{…}}`, `{"op":"reload"}` (re-read the settings file); the result is published once, not retained |

## Why this plugin has its own port

Plugins normally describe themselves read-only through the node's one gateway. This plugin is configured by a person: 32 slots, device pickers and pasted SDPs. That needs a form and a writable store, which is why it has its own port and volume.

The port is not a door of its own (PLAN-3403.01). It binds the host's loopback, and browsers reach it at `/plugins/aes67/ui/` on the gateway's one published port, behind the gateway's credential for writes. The path from the browser to the page:

1. Portal-Gateway (WEB.Traffic-Router) routes by prefix: `/plugins/<token>/ui/*` strips the prefix and dials the relay socket `<token>.sock` — here `unix//run/apk-frames/aes67.sock`. There is no per-plugin upstream variable any more (`APK_AES67_UPSTREAM` is gone); `APK_PLUGIN_UI_DIR` re-points the socket folder for every plugin at once.
2. NODE.BareMetal's relay (`Node-FramesRelay`, host network) holds that socket and dials `127.0.0.1:8130`. A socket is used because a host firewall drops bridge-to-host connections.
3. `web.rs` answers as it would on `:8130` directly. Every URL in the pages is relative (`api/v1/status`, `style.css`, `config`) and the `/` redirect's `Location` is relative, so the same page works at the root and under the prefix.

Set `APK_AES67_TOKEN` to require `Authorization: Bearer <token>` (or `X-APK-Token: <token>`) on every PUT, POST and DELETE — except `POST /api/sources/sdp`, the SDP preview, which changes nothing.

Every route is under **`/api/v1/`** (the pages call it); the unversioned `/api/…` spelling is the same handler, kept for one release and retired at crate 0.3.0, and any other version (`/api/v2/…`) is refused by name. A change of meaning is a new version beside v1. The settings document carries `schema` (`SETTINGS_SCHEMA`, `config.rs`): a file without it is the v1 shape, a newer one is refused rather than half-read, and every save writes the current number.

| Route (under `/api/v1/`) | |
|---|---|
| `GET /api/status` | the live numbers: clock, audio and every stream's counters |
| `GET` / `PUT /api/settings` | the whole settings document (and the slot limits); a PUT is validated, saved to `APK_AES67_STATE`, and reconciled |
| `GET /api/devices`, `/api/interfaces`, `/api/browse` | ALSA devices, network interfaces, the SAP sessions heard (`sap.rs` `Browser`; the header itself is read by the shared `apkaudio_ravenna::sap`) |
| `GET` / `POST /api/sources`, `GET` / `PUT` / `DELETE /api/sources/{id}` (and the same for `sinks`) | one stream, edited through the same validate-save-reconcile as a full PUT |
| `GET /api/sources/{id}/sdp` | a running sender's SDP |
| `POST /api/sources/sdp` | preview the SDP a form row would produce |
| `GET /api/selftest` | the crate's self test as JSON |

## Build and test

```sh
cd PODS/POD.THICK/DISCOVERY.Engine/SRC
cargo test -p apkaudio-aes67     # 31 unit tests: clock, ring, rtp, sdp (against the shared corpus), sap, ravenna, config (settings schema), web (API versions), net, bus, self test
cargo build -p apkaudio-aes67    # the binary apk-aes67; it binds ports, so run it in its container
```

Checked 2026-10-30: `cargo test -p apkaudio-aes67` — 31 passed.

## Philosophy

- **Keep the function, change the structure.** Where the original design needs what a container cannot have — a kernel module, a virtual card, a kernel PTP slave — this plugin opens a real card, runs PTP in user space and resamples, rather than asking for privilege (`SRC/Cargo.toml` header; `SRC/src/clock.rs`, `SRC/src/ring.rs`).
- **State the limit honestly.** The clock says it is not a boundary clock and points at `ptp4l` + `phc2sys`; a PTP port that will not bind reads `faulty` with the reason and the daemon runs on `CLOCK_TAI` (`SRC/src/clock.rs`, the `faulty` state). AM824 is refused by name rather than half-decoded (the shared `apkaudio_ravenna::sdp::parse_stream`, re-exported by `SRC/src/sdp.rs`).
- **Refuse what no level admits.** An ST 2110-30 sender whose rate × ptime × channels fits no conformance level, a ptime outside 125/250/333/1000/4000 µs, or a packet over 1440 bytes is refused at validation with the arithmetic (`SRC/src/config.rs` `validate`, `apkaudio_ravenna::sdp::conformance_level`, `SRC/src/rtp.rs`).
- **One door for every write.** The web page, the per-stream routes and the bus command all go through `Daemon::apply` — validate, save, reconcile (`SRC/src/daemon.rs`, `SRC/src/web.rs` `item`, `SRC/src/bus.rs`).
- **The bus carries state, not a firehose.** Streams are retained and sent only on change, never with counters; live numbers stay on HTTP; Clock and Audio are QoS 0 and not retained (`SRC/src/bus.rs` header).
- **An announcement is a promise.** Only a running sender is advertised over SAP or RAVENNA; a stopped one says goodbye (`SRC/src/ravenna.rs` header).
- **Defaults are first-boot only.** `config.ini` seeds the settings file once; after that the file the person saved is the truth (`SRC/config.ini` header, `SRC/src/config.rs` `Settings::from_ini`).
- **A deliberate exception to "no port per plugin".** Plugins describe themselves read-only through the node's one gateway; this one is configured by a person, so it has its own port, its own volume and an optional token (section above; `Docker/docker-compose.yml`).
- **Never block the launch.** The `snd-aloop` loader always exits 0; a host that refuses the module still gets the plugin on its physical card (`SRC/loopback/aes67-loopback.sh`, `Docker/docker-compose.yml`).

---

*© 2026 — Written & Maintained by Anthony P. Kuzub. MIT Licence.*

<!-- wiki:smoke -->
**Smoke test** — ✅ PASS 3/3 since 2026-10-30 12:51 EDT — [results](Documentation/%F0%9F%A7%A9Manual%20and%20Artifacts/SMOKE.md)
<!-- /wiki:smoke -->
