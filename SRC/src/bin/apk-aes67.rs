// Part of the APK.audio project — http://APK.audio — made by Anthony Kuzub
// MIT Licence. Free, for everyone, for ever. Full text in LICENSE at the root.
//! `apk-aes67` — the AES67 / ST 2110-30 bridge as its own container.
//!
//! Two agents on the runner's watchdog: the daemon (clock, card, streams, SAP,
//! the bus) and the web door (/status, /config, /api). Either returning or
//! panicking restarts the container. NET_BIND_SERVICE comes from the file
//! capability the Dockerfile sets, for PTP's UDP 319/320.

apk_plugin_runner::main!(token = "aes67", lib = apkaudio_aes67, run = [run_daemon_agent, run_web_agent]);
