// Part of the APK.audio project — http://APK.audio — made by Anthony Kuzub
// MIT Licence. Free, for everyone, for ever. Full text in LICENSE at the root.
//! `apkaudio-aes67` — AES67 and SMPTE ST 2110-30 senders and receivers,
//! bridged to an ALSA sound card, configured and watched from a web page.
//!
//! A rewrite of what bondagit/aes67-linux-daemon does, shaped for a plugin
//! container: see Cargo.toml for what changed and why. The pieces:
//!
//!   clock    PTPv2 slave (or CLOCK_TAI) → the media clock
//!   audio    one ALSA capture + one playback stream, shared by every stream
//!   ring     lock-free hand-off and the drift-correcting resampler
//!   tx / rx  RTP out / in, L16 and L24, 125 µs … 4 ms packets
//!   sdp/sap  describing streams and finding them (RFC 4566 / RFC 2974)
//!   daemon   settings → running pieces, reconciled on every change
//!   web      /status, /config and the REST API    bus  the MQTT side

pub mod audio;
pub mod bus;
pub mod clock;
pub mod config;
pub mod daemon;
pub mod net;
pub mod ring;
pub mod rtp;
pub mod rx;
pub mod sap;
pub mod sdp;
pub mod self_test;
pub mod tx;
pub mod web;

pub use bus::run_daemon_agent;
pub use web::run_web_agent;
