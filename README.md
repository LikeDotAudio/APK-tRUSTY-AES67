# APK:plugin:AES67

AES67 and SMPTE ST 2110-30 **senders and receivers, bridged to an ALSA sound card**, in one container: 16 senders and 16 receivers, each up to 8 channels wide. It has a web page for configuration and another for status.

| | |
|---|---|
| **Recipe** | `Docker/docker-compose.yml`. There is no Dockerfile here: the container runs `apk-aes67` from the shared plugin image that `discovery` builds |
| **Source** | `SRC/` is the crate `apkaudio-aes67`, a member of `discovery/SRC/Cargo.toml` |
| **Started by** | the runner, with the rest of the plugins: `python3 'PODS/K8:runner.py' up`. The discovery stack `include:`s this compose file, and its `sound`/`aes67` profiles are in DockTor's default role list |
| **Profiles** | `sound` (with the other card plugins), `aes67` (alone) |
| **Web** | `http://<node>:8130/status`, `/config`, `/api/*`. The port comes from `APK_AES67_HTTP_PORT` |
| **Needs** | `/dev/snd`, group `audio`, `NET_BIND_SERVICE` (PTP on 319/320), host networking (multicast) |
| **Keeps** | `apk-audio-aes67-state`, a volume that holds `settings.json`, which is what `/config` saves |

**First run:** on `/config`, pick the capture and playback devices. ALSA's `default` can be a device that won't open, such as an HDMI output with nothing connected. Then enable the senders and receivers you want.

## A rewrite of aes67-linux-daemon, turned inside out

[bondagit/aes67-linux-daemon](https://github.com/bondagit/aes67-linux-daemon) loads the RAVENNA kernel module. That module creates a **virtual** ALSA card, steers the card's clock from a PTP slave in the kernel, and a C++ daemon with a web UI configures it. None of that can run in a plugin container, which has a read-only root, runs as uid 1000 and cannot load kernel modules. So this plugin keeps the function and changes the structure:

| aes67-linux-daemon | this plugin |
|---|---|
| a virtual card that other programs open | opens a **real** card and moves its channels to and from the network |
| PTP slave in the kernel module | a PTPv2 slave in user space (`clock.rs`), or the host's `CLOCK_TAI` |
| the kernel steers the card to PTP | the card runs free, and an **adaptive resampler** (`ring.rs`) absorbs the ppm difference |
| C++ daemon, REST API, React UI | one Rust binary under `apk-plugin-runner`, a REST API, two pages compiled into the binary |
| SAP, mDNS/RTSP (RAVENNA) | SAP (RFC 2974), which Dante-in-AES67-mode, RAVENNA and Lawo all speak |

## What it does

- **Senders.** Each sender takes 1–8 capture-card channels and sends them as L16 or L24 at a packet time of 125, 250 or 333 µs, or 1 or 4 ms. A packet goes out when the media clock passes its last sample, so the network rate follows PTP and not the card's crystal. Each sender is announced over SAP and its SDP is served at `/api/sources/{id}/sdp`.
- **Receivers.** Each receiver takes one stream, either by **following a SAP session by name** (it re-tunes when the sender moves or restarts) or from a pasted SDP. Its channels map onto playback-card channels. Lost packets are replaced by silence at their correct position in time, and late packets are dropped and counted. The playout buffer is `delay_ms`.
- **AES67 and ST 2110-30.** Both profiles write and read `ts-refclk`, `mediaclk:direct=0` and `source-filter`. ST 2110-30 adds `channel-order=SMPTE2110.(…)`, and the plugin enforces its conformance levels: A is 48k/1 ms/≤8 ch, B is 48k/125 µs/≤8 ch, and C and the X levels are also accepted. AM824 (ST 2110-31) is refused by name.
- **Packet-size limit.** No packet may exceed 1440 bytes (AES67 §7.2). If a sender's settings would, `/config` shows the arithmetic.

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
| `outgoing/command` → `CommandResult` | `{"op":"enable","kind":"source","id":1}`, `{"op":"apply","settings":{…}}`, `{"op":"reload"}` |

## Why this plugin has its own port

Plugins normally describe themselves read-only through the node's one gateway. This plugin is configured by a person: 32 slots, device pickers and pasted SDPs. That needs a form and a writable store, which is why it has its own port and volume. Set `APK_AES67_TOKEN` to require `Authorization: Bearer <token>` on every write.

---

*© 2026 — Written & Maintained by Anthony P. Kuzub. MIT Licence.*
