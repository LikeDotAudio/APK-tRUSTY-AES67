// Part of the APK.audio project — http://APK.audio — made by Anthony Kuzub
// MIT Licence. Free, for everyone, for ever. Full text in LICENSE at the root.
//! The daemon's settings: what it sends, what it receives, and through what.
//!
//! TWO SOURCES, ONE WINNER. `config.ini` is compiled in and published by the
//! runner like every plugin's; it supplies first-boot defaults. The web page
//! writes a JSON file on the plugin's own named volume, and once that file
//! exists it is the truth. The container root is read-only (plugin-base.yml),
//! so the volume is the only place a setting can survive a restart.

use crate::rtp::Encoding;
use crate::sdp::Profile;
use serde::{Deserialize, Serialize};
use std::net::Ipv4Addr;

pub const CONFIG_INI: &str = include_str!("../config.ini");

/// The slot plan: sixteen senders and sixteen receivers, each up to eight
/// channels wide — 128 channels each way, the shape the owner asked for. Every
/// 8-channel stream fits one packet at 48 kHz / 1 ms / L24 (1152 bytes) and is
/// ST 2110-30 level A.
pub const MAX_SOURCES: usize = 16;
pub const MAX_SINKS: usize = 16;
pub const MAX_STREAM_CHANNELS: usize = 8;

/// Where the settings file lives. `APK_AES67_STATE` moves it.
pub fn state_path() -> String {
    std::env::var("APK_AES67_STATE").unwrap_or_else(|_| "/var/lib/apkaudio/aes67/settings.json".to_string())
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub node_name: String,
    pub interface: String,
    pub sample_rate: u32,
    pub clock: ClockSettings,
    pub audio: AudioSettings,
    pub sap: SapSettings,
    pub http_port: u16,
    pub sources: Vec<Source>,
    pub sinks: Vec<Sink>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ClockSettings {
    /// `ptp` or `system`.
    pub source: String,
    pub domain: u8,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AudioSettings {
    pub capture_device: String,
    pub playback_device: String,
    /// 0 = the device's default.
    pub capture_channels: u16,
    pub playback_channels: u16,
    /// ALSA period in frames; 0 = the device's default.
    pub period_frames: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SapSettings {
    pub announce: bool,
    pub listen: bool,
    pub interval_s: u32,
}

/// A transmitter: some capture channels of the card, out as one RTP stream.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Source {
    pub id: u32,
    pub enabled: bool,
    pub name: String,
    pub profile: Profile,
    pub address: Ipv4Addr,
    pub port: u16,
    pub ttl: u8,
    /// DSCP for the media: 34 (AF41) is the AES67 §8.4 recommendation.
    pub dscp: u8,
    pub payload_type: u8,
    pub encoding: Encoding,
    pub ptime_us: u32,
    /// Capture-card channel (0-based) feeding each stream channel, in order.
    pub channels: Vec<u16>,
}

/// A receiver: one RTP stream, its channels onto playback channels of the card.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Sink {
    pub id: u32,
    pub enabled: bool,
    pub name: String,
    /// The stream's SDP, verbatim. When empty, `follow` names a SAP session.
    pub sdp: String,
    /// Follow a SAP-announced session by its `s=` name, re-tuning when its
    /// SDP changes (a sender that moved address, or restarted).
    pub follow: String,
    /// Playout buffer, i.e. link offset budget.
    pub delay_ms: f32,
    /// Playback-card channel (0-based) for each stream channel; null = unrouted.
    pub map: Vec<Option<u16>>,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            node_name: String::new(),
            interface: String::new(),
            sample_rate: 48_000,
            clock: ClockSettings::default(),
            audio: AudioSettings::default(),
            sap: SapSettings::default(),
            http_port: 8130,
            sources: Vec::new(),
            sinks: Vec::new(),
        }
    }
}

impl Default for ClockSettings {
    fn default() -> Self {
        ClockSettings { source: "ptp".into(), domain: 0 }
    }
}

impl Default for SapSettings {
    fn default() -> Self {
        SapSettings { announce: true, listen: true, interval_s: 30 }
    }
}

impl Default for Source {
    fn default() -> Self {
        Source {
            id: 0,
            enabled: true,
            name: String::new(),
            profile: Profile::Aes67,
            address: Ipv4Addr::new(239, 69, 0, 1),
            port: 5004,
            ttl: 32,
            dscp: 34,
            payload_type: 97,
            encoding: Encoding::L24,
            ptime_us: 1000,
            channels: vec![0, 1],
        }
    }
}

impl Default for Sink {
    fn default() -> Self {
        Sink {
            id: 0,
            enabled: true,
            name: String::new(),
            sdp: String::new(),
            follow: String::new(),
            delay_ms: 10.0,
            map: vec![Some(0), Some(1)],
        }
    }
}

impl Sink {
    /// Receiver slot with id `id + 1`, disabled and untuned, routed onto
    /// playback channels 8j..8j+7 for its position j among the sinks.
    pub fn slot(id: usize) -> Sink {
        let j = id - MAX_SOURCES;
        let base = (j * MAX_STREAM_CHANNELS) as u16;
        Sink {
            id: id as u32 + 1,
            enabled: false,
            name: format!("RX {:02}", j + 1),
            map: (base..base + MAX_STREAM_CHANNELS as u16).map(Some).collect(),
            ..Sink::default()
        }
    }
}

impl Settings {
    /// First-boot defaults from the `[aes67]` section of `config.ini`.
    pub fn from_ini(text: &str) -> Settings {
        let ini = apk_plugin_runner::ini_to_json(text);
        let mut s = Settings::default();
        let Some(sect) = ini.get("aes67") else { return s };
        let get = |k: &str| sect.get(k).and_then(|v| v.as_str()).map(str::trim).unwrap_or("").to_string();
        let flag = |k: &str, d: bool| match get(k).to_ascii_lowercase().as_str() {
            "true" | "yes" | "1" | "on" => true,
            "false" | "no" | "0" | "off" => false,
            _ => d,
        };
        s.node_name = get("node_name");
        s.interface = get("interface");
        s.sample_rate = get("sample_rate").parse().unwrap_or(48_000);
        if !get("clock_source").is_empty() {
            s.clock.source = get("clock_source");
        }
        s.clock.domain = get("ptp_domain").parse().unwrap_or(0);
        s.audio.capture_device = get("capture_device");
        s.audio.playback_device = get("playback_device");
        s.audio.capture_channels = get("capture_channels").parse().unwrap_or(0);
        s.audio.playback_channels = get("playback_channels").parse().unwrap_or(0);
        s.sap.announce = flag("sap_announce", true);
        s.sap.listen = flag("sap_listen", true);
        s.sap.interval_s = get("sap_interval_s").parse().unwrap_or(30);
        s.http_port = get("http_port").parse().unwrap_or(8130);
        let slots = |k: &str, max: usize| get(k).parse::<usize>().unwrap_or(max).min(max);
        s.sources = (0..slots("source_slots", MAX_SOURCES)).map(Source::slot).collect();
        s.sinks = (0..slots("sink_slots", MAX_SINKS)).map(|i| Sink::slot(MAX_SOURCES + i)).collect();
        s
    }

    /// The file if there is one, else config.ini. The error, if the file
    /// exists and does not parse, is returned beside the defaults so the
    /// status page can say the saved settings were refused rather than lost.
    pub fn load() -> (Settings, Option<String>) {
        let defaults = Settings::from_ini(CONFIG_INI);
        match std::fs::read_to_string(state_path()) {
            Err(_) => (defaults, None),
            Ok(text) => match serde_json::from_str::<Settings>(&text) {
                Ok(s) => match s.validate() {
                    Ok(()) => (s, None),
                    Err(e) => (defaults, Some(format!("{}: {e}", state_path()))),
                },
                Err(e) => (defaults, Some(format!("{}: {e}", state_path()))),
            },
        }
    }

    /// Write-then-rename, so a crash mid-save leaves the old file whole.
    pub fn save(&self) -> Result<(), String> {
        let path = state_path();
        if let Some(dir) = std::path::Path::new(&path).parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        }
        let tmp = format!("{path}.tmp");
        let text = serde_json::to_string_pretty(self).map_err(|e| e.to_string())?;
        std::fs::write(&tmp, text).map_err(|e| format!("{tmp}: {e}"))?;
        std::fs::rename(&tmp, &path).map_err(|e| format!("{path}: {e}"))
    }

    pub fn clock_source(&self) -> crate::clock::Source {
        if self.clock.source.eq_ignore_ascii_case("system") {
            crate::clock::Source::System
        } else {
            crate::clock::Source::Ptp
        }
    }

    /// Everything a person could get wrong, as one sentence naming the item.
    pub fn validate(&self) -> Result<(), String> {
        if ![44_100, 48_000, 88_200, 96_000].contains(&self.sample_rate) {
            return Err(format!("sample rate {} is not 44100, 48000, 88200 or 96000", self.sample_rate));
        }
        if !matches!(self.clock.source.to_ascii_lowercase().as_str(), "ptp" | "system") {
            return Err(format!("clock source {:?} is not ptp or system", self.clock.source));
        }
        if self.http_port == 0 {
            return Err("http_port is 0".into());
        }
        if self.sources.len() > MAX_SOURCES {
            return Err(format!("{} sources; at most {MAX_SOURCES}", self.sources.len()));
        }
        if self.sinks.len() > MAX_SINKS {
            return Err(format!("{} sinks; at most {MAX_SINKS}", self.sinks.len()));
        }
        let mut ids = std::collections::BTreeSet::new();
        for s in &self.sources {
            if !ids.insert(("source", s.id)) {
                return Err(format!("source id {} is used twice", s.id));
            }
            s.validate(self.sample_rate).map_err(|e| format!("source {} ({}): {e}", s.id, s.name))?;
        }
        for k in &self.sinks {
            if !ids.insert(("sink", k.id)) {
                return Err(format!("sink id {} is used twice", k.id));
            }
            // An empty, disabled slot is legal — that is what a slot is.
            if k.enabled && k.sdp.trim().is_empty() && k.follow.trim().is_empty() {
                return Err(format!("sink {} ({}): needs an SDP or a session to follow", k.id, k.name));
            }
            if !k.sdp.trim().is_empty() {
                let d = crate::sdp::parse(&k.sdp).map_err(|e| format!("sink {} ({}): SDP: {e}", k.id, k.name))?;
                if d.channels as usize > MAX_STREAM_CHANNELS {
                    return Err(format!(
                        "sink {} ({}): the stream has {} channels; a receiver takes at most {MAX_STREAM_CHANNELS}",
                        k.id, k.name, d.channels
                    ));
                }
            }
            if k.map.len() > MAX_STREAM_CHANNELS {
                return Err(format!("sink {} ({}): {} routes; at most {MAX_STREAM_CHANNELS}", k.id, k.name, k.map.len()));
            }
            if !(0.5..=2000.0).contains(&k.delay_ms) {
                return Err(format!("sink {} ({}): delay {} ms is outside 0.5..2000", k.id, k.name, k.delay_ms));
            }
        }
        Ok(())
    }

    /// The next free id in either list.
    pub fn next_id(&self) -> u32 {
        self.sources.iter().map(|s| s.id).chain(self.sinks.iter().map(|s| s.id)).max().unwrap_or(0) + 1
    }
}

impl Source {
    /// Slot `i` of the first-boot plan: disabled, 8 channels from card
    /// channels 8i..8i+7 (channels the card lacks send silence), to
    /// 239.69.0.<i+1>. Named so /config reads as a patch list.
    pub fn slot(i: usize) -> Source {
        let base = (i * MAX_STREAM_CHANNELS) as u16;
        Source {
            id: i as u32 + 1,
            enabled: false,
            name: format!("TX {:02}", i + 1),
            address: Ipv4Addr::new(239, 69, 0, i as u8 + 1),
            channels: (base..base + MAX_STREAM_CHANNELS as u16).collect(),
            ..Source::default()
        }
    }

    pub fn validate(&self, rate: u32) -> Result<(), String> {
        if self.name.trim().is_empty() {
            return Err("has no name".into());
        }
        if self.channels.is_empty() || self.channels.len() > MAX_STREAM_CHANNELS {
            return Err(format!("{} channels is outside 1..{MAX_STREAM_CHANNELS}", self.channels.len()));
        }
        if self.address.is_unspecified() || self.address.is_broadcast() {
            return Err(format!("{} is not a destination", self.address));
        }
        if self.port == 0 {
            return Err("port is 0".into());
        }
        if !(96..=127).contains(&self.payload_type) {
            return Err(format!("payload type {} is not dynamic (96..127)", self.payload_type));
        }
        if self.dscp > 63 {
            return Err(format!("DSCP {} is over 63", self.dscp));
        }
        // AES67 §7.2: 125 µs, 250 µs, 333 µs, 1 ms and 4 ms; 1 ms is mandatory.
        if ![125, 250, 333, 1000, 4000].contains(&self.ptime_us) {
            return Err(format!("ptime {} µs is not 125, 250, 333, 1000 or 4000", self.ptime_us));
        }
        let frames = crate::rtp::frames_per_packet(rate, self.ptime_us);
        let bytes = frames * self.channels.len() * self.encoding.bytes();
        if bytes > crate::rtp::MAX_PAYLOAD {
            return Err(format!(
                "{} channels × {frames} frames × {} bytes = {bytes} bytes, over the {}-byte packet limit — \
                 use fewer channels or a shorter ptime",
                self.channels.len(),
                self.encoding.bytes(),
                crate::rtp::MAX_PAYLOAD
            ));
        }
        if self.profile == Profile::St2110_30
            && crate::sdp::conformance_level(rate, self.ptime_us, self.channels.len() as u16).is_none()
        {
            return Err(format!(
                "ST 2110-30 has no conformance level for {} Hz, {} µs, {} channels \
                 (A: 48k/1ms/≤8, B: 48k/125µs/≤8, C: 48k/125µs/≤64, AX/BX/CX at 96k)",
                rate,
                self.ptime_us,
                self.channels.len()
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_compiled_in_ini_gives_valid_defaults() {
        let s = Settings::from_ini(CONFIG_INI);
        assert_eq!(s.sample_rate, 48_000);
        assert_eq!(s.http_port, 8130);
        assert_eq!(s.clock_source(), crate::clock::Source::Ptp);
        assert_eq!((s.sources.len(), s.sinks.len()), (MAX_SOURCES, MAX_SINKS));
        assert!(s.sources.iter().all(|x| x.channels.len() == 8 && !x.enabled));
        s.validate().unwrap();
    }

    #[test]
    fn an_oversized_packet_is_refused_with_the_arithmetic() {
        let src = Source { name: "big".into(), channels: (0..8).collect(), ptime_us: 4000, ..Source::default() };
        let err = src.validate(48_000).unwrap_err();
        assert!(err.contains("4608 bytes"), "{err}");
        let wide = Source { name: "wide".into(), channels: (0..9).collect(), ..Source::default() };
        assert!(wide.validate(48_000).unwrap_err().contains("1..8"));
    }

    #[test]
    fn st2110_levels_are_enforced() {
        let mut src = Source { name: "c".into(), channels: (0..8).collect(), ..Source::default() };
        src.profile = Profile::St2110_30;
        src.validate(48_000).unwrap();
        src.ptime_us = 125;
        src.validate(48_000).unwrap(); // level B
    }

    #[test]
    fn settings_round_trip_through_json() {
        let mut s = Settings::default();
        s.sources.push(Source { id: 1, name: "a".into(), ..Source::default() });
        s.sinks.push(Sink { id: 2, name: "b".into(), follow: "x".into(), ..Sink::default() });
        let back: Settings = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        assert_eq!(back, s);
        assert_eq!(s.next_id(), 3);
    }
}
