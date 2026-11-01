// Part of the APK.audio project — http://APK.audio — made by Anthony Kuzub
// MIT Licence. Free, for everyone, for ever. Full text in LICENSE at the root.
//! `apk-aes67` — the AES67 / ST 2110-30 bridge as its own container.
//!
//! Two agents on the runner's watchdog: the daemon (clock, card, streams, SAP,
//! the bus) and the web door (/status, /config, /api). Either returning or
//! panicking restarts the container. NET_BIND_SERVICE comes from the file
//! capability the Dockerfile sets, for PTP's UDP 319/320.
//!
//! `manifest = "agent.toml"` (ROAD-3478): the manifest is compiled in, and the
//! runner serves `GET /status` / `GET /telemetry` on 127.0.0.1:8191 — `running`
//! until the first PTP, SAP or RTP datagram, `serving` after.

apk_plugin_runner::main!(token = "aes67", lib = apkaudio_aes67, run = [run_daemon_agent, run_web_agent], manifest = "agent.toml");
