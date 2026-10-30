# Principles — PLUGIN.AES67

The rules this plugin keeps, each pointed at the code that keeps it. Ecosystem rules (one port per node, plugins over the bus, unreachable is dead) live in the monorepo's root `Documentation/🏛Philosophy/` and are cited, not copied. The repo [README](../../README.md#philosophy) has the short list; this page adds where each rule lives and where the code falls short.

← [Philosophy](INDEX.md) · [Wiki home](../README.md)

## Kept

| Principle | Where the code keeps it |
|---|---|
| **Keep the function, change the structure.** No kernel module, no virtual card, no kernel PTP: a real ALSA card is opened, PTP runs in user space and an adaptive resampler absorbs the crystal's drift. | `SRC/Cargo.toml` header; `SRC/src/clock.rs` (`Clock::start`, line 387); `SRC/src/ring.rs` (`DriftReader::read`, line 182) |
| **State the limit honestly.** A PTP port that will not bind reads `faulty` with the reason and the daemon runs on `CLOCK_TAI`. AM824 is refused by name rather than half-decoded. | `SRC/src/clock.rs:613` (`Status`, `faulty`); `SRC/src/sdp.rs:209` |
| **Refuse what no level admits.** Bad ptime, an ST 2110-30 stream that fits no conformance level, or a packet over 1440 bytes is refused at validation, with the arithmetic. | `SRC/src/config.rs:303`, `:387` (`validate`), `:412` (`MAX_PAYLOAD`); `SRC/src/sdp.rs:63` (`conformance_level`) |
| **One door for every write.** The web page, the per-stream routes and the bus command all end in `Daemon::apply`: validate, save, reconcile. | `SRC/src/daemon.rs:120` (`apply`), `:534` (`command`); `SRC/src/web.rs` (`item`, `route`) |
| **Defaults are first-boot only.** `config.ini` seeds `settings.json` once; afterwards the saved file is the truth. | `SRC/src/config.rs:229` (`Settings::from_ini`), `:268` (`Settings::load`) |
| **The bus carries state, not a firehose.** Stream documents are retained and sent only when they change, with no counters; `Clock`/`Audio` are QoS 0, not retained, every 5 s. | `SRC/src/bus.rs:162` (`stream_document`), `:191` (`publish_changed_streams`), `:150` (`restate`) |
| **One topic root for config and live leaves.** The live leaves are built under `config.ini`'s `topic` key, read the way the runner reads it, so the retained `<topic>/config` and `Stream/`, `Clock`, `Audio`, `CommandResult` move together. | `SRC/src/bus.rs:41` (`topic`), test `the_live_leaves_follow_config_ini_topic_key` |
| **A retained command is not a command.** A retained publish on `outgoing/command` is ignored, so it never replays on reconnect. | `SRC/src/bus.rs` (`run_daemon_agent`, the `p.retain` check) |
| **An announcement is a promise.** Only a running sender is advertised over SAP or RAVENNA mDNS; a stopped one is unregistered (an mDNS goodbye). | `SRC/src/ravenna.rs:79` (`sync`), `:133` (`unregister`) |
| **Never block the launch.** The `snd-aloop` loader exits 0 on every path, so a host that refuses the module still gets the plugin on its physical card. | `SRC/loopback/aes67-loopback.sh` (every `exit 0`); `Docker/docker-compose.yml` (`depends_on: service_completed_successfully`) |
| **Least privilege.** The plugin gets `NET_BIND_SERVICE` only (as a file capability on `apk-aes67`), `/dev/snd` and group `audio`; the loader alone gets `SYS_MODULE`, with no network. | `Docker/docker-compose.yml`; `DISCOVERY.Engine/Docker/Dockerfile` (`setcap … apk-aes67`) |

## A deliberate exception

**Its own port.** The ecosystem rule is that plugins describe themselves read-only through the node's one gateway (root memory note *Plugin API over the bus*). This plugin is configured by a person — 32 slots, device pickers, pasted SDPs — so it serves its own pages and REST API on `http_port` (8130), keeps a volume (`apk-audio-aes67-state`), and gates writes with `APK_AES67_TOKEN` when it is set (`SRC/src/web.rs`, `authorised`).

## Where the code falls short

- **The contract's leaves do not follow the `topic` key.** `Agent/*`, `incoming/*` and `outgoing/command` are built by the contracts crate from the module token (`contracts/rust/src/baremetal_io.rs`, `module_topic`; `agent_state.rs`), always under `APK.audio/System/Protocols/aes67`. That is the contract's design (one identity per module); moving `topic` moves `config`, `status` and this plugin's own live leaves, not those.
- **Only the daemon loop ticks** (`SRC/src/bus.rs`, `run_daemon_agent`). The web agent's accept loop waits on an idle port by design and hands each request its own thread, so it has no pass to tick on; a wedged request thread is not caught, and since the runner's tick is one process-wide clock, a tick there would be masked by the daemon's anyway.
- **The clock is not a boundary clock.** User-space timestamps after `recv` get to tens of µs; sample-accurate alignment needs `ptp4l` + `phc2sys` on the host and Clock = System.
