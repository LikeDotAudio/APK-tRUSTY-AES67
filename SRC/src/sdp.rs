// Part of the APK.audio project — http://APK.audio — made by Anthony Kuzub
// MIT Licence. Free, for everyone, for ever. Full text in LICENSE at the root.
//! SDP (RFC 4566) for AES67 and SMPTE ST 2110-30, both directions.
//!
//! The two profiles describe the same RTP stream. What ST 2110-30 adds on top
//! of AES67 is small and all of it is here: the `channel-order` fmtp parameter
//! (ST 2110-30 §6.2.2), the conformance level that bounds channels × ptime
//! (§6.2.1, levels A/AX/B/BX/C/CX), and the rule — shared with AES67 §8 but
//! optional in practice there — that `ts-refclk` and `mediaclk:direct=` are
//! present. A receiver here accepts either, and also plain RFC 3190 SDP
//! without the clock lines, which it plays with adaptive timing.

use crate::rtp::Encoding;
use serde::{Deserialize, Serialize};
use std::net::Ipv4Addr;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Profile {
    #[serde(rename = "aes67")]
    Aes67,
    #[serde(rename = "st2110-30")]
    St2110_30,
}

impl Profile {
    pub fn as_str(self) -> &'static str {
        match self {
            Profile::Aes67 => "aes67",
            Profile::St2110_30 => "st2110-30",
        }
    }
}

/// One stream, as either end of it needs to know it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StreamDesc {
    pub name: String,
    pub info: String,
    pub profile: Profile,
    pub origin_ip: Ipv4Addr,
    pub session_id: u64,
    pub session_version: u64,
    pub address: Ipv4Addr,
    pub port: u16,
    pub ttl: u8,
    pub payload_type: u8,
    pub encoding: Encoding,
    pub rate: u32,
    pub channels: u16,
    pub ptime_us: u32,
    /// `a=source-filter: incl` — the one sender a receiver should accept.
    pub source_filter: Option<Ipv4Addr>,
    /// `a=ts-refclk:` verbatim after the colon, e.g. `ptp=IEEE1588-2008:…:0`.
    pub refclk: Option<String>,
    /// `a=mediaclk:direct=<offset>` — RTP timestamp at the PTP epoch.
    pub mediaclk_offset: Option<u32>,
    /// `channel-order=` from fmtp, ST 2110-30 only.
    pub channel_order: Option<String>,
}

/// ST 2110-30 §6.2.1 — the level a (rate, ptime, channels) triple needs.
/// `None` means no level admits it, which a 2110 sender must not emit.
pub fn conformance_level(rate: u32, ptime_us: u32, channels: u16) -> Option<&'static str> {
    match (rate, ptime_us) {
        (48_000, 1000) if channels <= 8 => Some("A"),
        (96_000, 1000) if channels <= 4 => Some("AX"),
        (48_000, 125) if channels <= 8 => Some("B"),
        (96_000, 125) if channels <= 8 => Some("BX"),
        (48_000, 125) if channels <= 64 => Some("C"),
        (96_000, 125) if channels <= 32 => Some("CX"),
        _ => None,
    }
}

/// `SMPTE2110.(ST)` for a pair, `(M)` for one, `(U08)` otherwise — the
/// "undefined grouping" symbol, which claims nothing about the channels that
/// this bridge cannot know about the card they came from.
pub fn channel_order(channels: u16) -> String {
    match channels {
        1 => "SMPTE2110.(M)".to_string(),
        2 => "SMPTE2110.(ST)".to_string(),
        n => format!("SMPTE2110.(U{n:02})"),
    }
}

fn ptime_text(ptime_us: u32) -> String {
    if ptime_us.is_multiple_of(1000) {
        format!("{}", ptime_us / 1000)
    } else {
        let s = format!("{:.3}", ptime_us as f64 / 1000.0);
        s.trim_end_matches('0').to_string()
    }
}

/// The SDP a sender publishes (SAP, NMOS, `GET /api/sources/{id}/sdp`).
pub fn generate(d: &StreamDesc) -> String {
    let mut s = String::new();
    s.push_str("v=0\r\n");
    s.push_str(&format!("o=- {} {} IN IP4 {}\r\n", d.session_id, d.session_version, d.origin_ip));
    s.push_str(&format!("s={}\r\n", d.name));
    if !d.info.is_empty() {
        s.push_str(&format!("i={}\r\n", d.info));
    }
    s.push_str(&format!("c=IN IP4 {}/{}\r\n", d.address, d.ttl));
    s.push_str("t=0 0\r\n");
    if d.profile == Profile::St2110_30 {
        s.push_str("a=recvonly\r\n");
    }
    s.push_str(&format!("m=audio {} RTP/AVP {}\r\n", d.port, d.payload_type));
    s.push_str(&format!("c=IN IP4 {}/{}\r\n", d.address, d.ttl));
    if let Some(src) = d.source_filter {
        s.push_str(&format!("a=source-filter: incl IN IP4 {} {}\r\n", d.address, src));
    }
    s.push_str(&format!("a=rtpmap:{} {}/{}/{}\r\n", d.payload_type, d.encoding.as_str(), d.rate, d.channels));
    if let Some(order) = &d.channel_order {
        s.push_str(&format!("a=fmtp:{} channel-order={}\r\n", d.payload_type, order));
    }
    s.push_str(&format!("a=ptime:{}\r\n", ptime_text(d.ptime_us)));
    s.push_str(&format!("a=maxptime:{}\r\n", ptime_text(d.ptime_us)));
    if let Some(refclk) = &d.refclk {
        s.push_str(&format!("a=ts-refclk:{refclk}\r\n"));
    }
    s.push_str(&format!("a=mediaclk:direct={}\r\n", d.mediaclk_offset.unwrap_or(0)));
    if d.profile == Profile::Aes67 {
        s.push_str("a=recvonly\r\n");
    }
    s
}

/// Parse the first audio media section of an SDP. Errors name the line that
/// was missing or wrong, because they are shown to a person on /config.
pub fn parse(text: &str) -> Result<StreamDesc, String> {
    let mut d = StreamDesc {
        name: String::new(),
        info: String::new(),
        profile: Profile::Aes67,
        origin_ip: Ipv4Addr::UNSPECIFIED,
        session_id: 0,
        session_version: 0,
        address: Ipv4Addr::UNSPECIFIED,
        port: 0,
        ttl: 0,
        payload_type: 255,
        encoding: Encoding::L24,
        rate: 0,
        channels: 0,
        ptime_us: 1000,
        source_filter: None,
        refclk: None,
        mediaclk_offset: None,
        channel_order: None,
    };
    let mut in_audio = false;
    let mut seen_audio = false;
    let mut session_c: Option<(Ipv4Addr, u8)> = None;
    let mut media_c: Option<(Ipv4Addr, u8)> = None;
    let mut rtpmap_seen = false;

    for raw in text.lines() {
        let line = raw.trim_end_matches('\r').trim();
        let Some((kind, value)) = line.split_once('=') else { continue };
        match kind {
            "o" if !seen_audio => {
                let f: Vec<&str> = value.split_whitespace().collect();
                if f.len() >= 6 {
                    d.session_id = f[1].parse().unwrap_or(0);
                    d.session_version = f[2].parse().unwrap_or(0);
                    d.origin_ip = f[5].parse().unwrap_or(Ipv4Addr::UNSPECIFIED);
                }
            }
            "s" if !seen_audio => d.name = value.trim().to_string(),
            "i" if !seen_audio => d.info = value.trim().to_string(),
            "c" => {
                let c = parse_connection(value)?;
                if in_audio {
                    media_c = Some(c);
                } else if !seen_audio {
                    session_c = Some(c);
                }
            }
            "m" => {
                if seen_audio {
                    // Only the first audio section; a second is a different stream.
                    in_audio = false;
                    continue;
                }
                let f: Vec<&str> = value.split_whitespace().collect();
                if f.first() == Some(&"audio") && f.len() >= 4 {
                    in_audio = true;
                    seen_audio = true;
                    d.port = f[1].split('/').next().unwrap_or("0").parse().map_err(|_| format!("bad port in m={value}"))?;
                    d.payload_type = f[3].parse().map_err(|_| format!("bad payload type in m={value}"))?;
                } else {
                    in_audio = false;
                }
            }
            "a" if in_audio || !seen_audio => {
                let (attr, arg) = value.split_once(':').unwrap_or((value, ""));
                match attr {
                    "rtpmap" if in_audio => {
                        let (pt, spec) = arg.split_once(' ').ok_or("a=rtpmap without an encoding")?;
                        if pt.trim().parse::<u8>().ok() != Some(d.payload_type) {
                            continue;
                        }
                        let mut parts = spec.trim().split('/');
                        let enc = parts.next().unwrap_or("");
                        d.encoding = match Encoding::parse(enc) {
                            Some(e) => e,
                            None if enc.eq_ignore_ascii_case("AM824") => {
                                return Err("AM824 (ST 2110-31) is not supported; use L16 or L24 (ST 2110-30)".into())
                            }
                            None => return Err(format!("encoding {enc} is not L16 or L24")),
                        };
                        d.rate = parts.next().and_then(|r| r.parse().ok()).ok_or("a=rtpmap without a rate")?;
                        d.channels = parts.next().and_then(|c| c.parse().ok()).unwrap_or(1);
                        rtpmap_seen = true;
                    }
                    "fmtp" if in_audio => {
                        if let Some((_, params)) = arg.split_once(' ') {
                            for p in params.split(';') {
                                if let Some(order) = p.trim().strip_prefix("channel-order=") {
                                    d.channel_order = Some(order.to_string());
                                    d.profile = Profile::St2110_30;
                                }
                            }
                        }
                    }
                    "ptime" => {
                        let ms: f64 = arg.trim().parse().map_err(|_| format!("bad a=ptime:{arg}"))?;
                        d.ptime_us = (ms * 1000.0).round() as u32;
                    }
                    "source-filter" => {
                        // a=source-filter: incl IN IP4 <dest> <src>...
                        let f: Vec<&str> = arg.split_whitespace().collect();
                        if f.first() == Some(&"incl") && f.len() >= 5 {
                            d.source_filter = f[4].parse().ok();
                        }
                    }
                    "ts-refclk" => d.refclk = Some(arg.trim().to_string()),
                    "mediaclk" => {
                        if let Some(off) = arg.trim().strip_prefix("direct=") {
                            d.mediaclk_offset = off.split_whitespace().next().and_then(|o| o.parse().ok());
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    if !seen_audio {
        return Err("no m=audio section".into());
    }
    if !rtpmap_seen {
        return Err(format!("no a=rtpmap for payload type {}", d.payload_type));
    }
    let (address, ttl) = media_c.or(session_c).ok_or("no c= connection line")?;
    d.address = address;
    d.ttl = ttl;
    if d.channels == 0 || d.channels > 64 {
        return Err(format!("{} channels is outside 1..64", d.channels));
    }
    if d.ptime_us == 0 {
        return Err("ptime is zero".into());
    }
    Ok(d)
}

fn parse_connection(value: &str) -> Result<(Ipv4Addr, u8), String> {
    let f: Vec<&str> = value.split_whitespace().collect();
    if f.len() < 3 || f[1] != "IP4" {
        return Err(format!("c={value} is not IN IP4"));
    }
    let mut parts = f[2].split('/');
    let ip = parts.next().unwrap_or("").parse().map_err(|_| format!("bad address in c={value}"))?;
    let ttl = parts.next().and_then(|t| t.parse().ok()).unwrap_or(0);
    Ok((ip, ttl))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(profile: Profile) -> StreamDesc {
        StreamDesc {
            name: "APK stereo".into(),
            info: "2 channels".into(),
            profile,
            origin_ip: "192.168.1.20".parse().unwrap(),
            session_id: 1234,
            session_version: 5,
            address: "239.69.1.2".parse().unwrap(),
            port: 5004,
            ttl: 32,
            payload_type: 97,
            encoding: Encoding::L24,
            rate: 48_000,
            channels: 2,
            ptime_us: 125,
            source_filter: Some("192.168.1.20".parse().unwrap()),
            refclk: Some("ptp=IEEE1588-2008:00-1D-C1-FF-FE-11-22-33:0".into()),
            mediaclk_offset: Some(0),
            channel_order: (profile == Profile::St2110_30).then(|| channel_order(2)),
        }
    }

    #[test]
    fn both_profiles_round_trip() {
        for p in [Profile::Aes67, Profile::St2110_30] {
            let d = sample(p);
            let back = parse(&generate(&d)).unwrap();
            assert_eq!(back, d, "{}", generate(&d));
        }
    }

    #[test]
    fn a_dante_style_session_level_connection_is_accepted() {
        let sdp = "v=0\no=- 1 2 IN IP4 10.0.0.5\ns=Dante TX\nc=IN IP4 239.1.2.3/32\nt=0 0\n\
                   m=audio 5004 RTP/AVP 97\na=rtpmap:97 L24/48000/8\na=ptime:1\n";
        let d = parse(sdp).unwrap();
        assert_eq!(d.address, "239.1.2.3".parse::<Ipv4Addr>().unwrap());
        assert_eq!((d.channels, d.ptime_us, d.profile), (8, 1000, Profile::Aes67));
    }

    #[test]
    fn am824_is_refused_by_name() {
        let sdp = "v=0\nc=IN IP4 239.1.2.3/32\nm=audio 5004 RTP/AVP 97\na=rtpmap:97 AM824/48000/2\n";
        assert!(parse(sdp).unwrap_err().contains("2110-31"));
    }

    #[test]
    fn levels_bound_channels_by_ptime() {
        assert_eq!(conformance_level(48_000, 1000, 8), Some("A"));
        assert_eq!(conformance_level(48_000, 125, 64), Some("C"));
        assert_eq!(conformance_level(48_000, 1000, 16), None);
    }
}
