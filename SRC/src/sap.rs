// Part of the APK.audio project — http://APK.audio — made by Anthony Kuzub
// MIT Licence. Free, for everyone, for ever. Full text in LICENSE at the root.
//! SAP (RFC 2974): announcing our sources and browsing everybody else's.
//!
//! AES67 §10 leaves discovery open; SAP on 239.255.255.255:9875 is what Dante
//! (in AES67 mode), RAVENNA, Merging, Lawo and the original aes67-linux-daemon
//! all speak, so it is the one this bridge speaks. The browser keeps what it
//! hears so /config can offer "receive this" without anybody typing an SDP.

use serde::Serialize;
use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddrV4, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const GROUP: Ipv4Addr = Ipv4Addr::new(239, 255, 255, 255);
pub const PORT: u16 = 9875;
const MIME: &[u8] = b"application/sdp\0";

/// Silence this long and a remote session is forgotten (RFC 2974 §4 suggests
/// ten announce intervals or an hour; senders here announce every 30 s).
const FORGET_AFTER: Duration = Duration::from_secs(300);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Packet {
    pub deletion: bool,
    pub msg_id_hash: u16,
    pub origin: Ipv4Addr,
    pub sdp: String,
}

pub fn encode(p: &Packet) -> Vec<u8> {
    let mut out = Vec::with_capacity(8 + MIME.len() + p.sdp.len());
    // V=1, IPv4, not encrypted, not compressed; T is the deletion bit.
    out.push(0x20 | if p.deletion { 0x04 } else { 0 });
    out.push(0); // no authentication data
    out.extend_from_slice(&p.msg_id_hash.to_be_bytes());
    out.extend_from_slice(&p.origin.octets());
    out.extend_from_slice(MIME);
    out.extend_from_slice(p.sdp.as_bytes());
    out
}

pub fn decode(buf: &[u8]) -> Option<Packet> {
    if buf.len() < 8 || buf[0] >> 5 != 1 {
        return None;
    }
    let flags = buf[0];
    if flags & 0x10 != 0 || flags & 0x02 != 0 || flags & 0x01 != 0 {
        // IPv6 origin, encrypted or compressed: none of which anybody sends.
        return None;
    }
    let auth_len = buf[1] as usize * 4;
    let origin = Ipv4Addr::new(buf[4], buf[5], buf[6], buf[7]);
    let mut rest = buf.get(8 + auth_len..)?;
    // The payload type is optional; a payload that starts `v=0` has none.
    if !rest.starts_with(b"v=0") {
        let nul = rest.iter().position(|b| *b == 0)?;
        if &rest[..nul] != b"application/sdp" {
            return None;
        }
        rest = &rest[nul + 1..];
    }
    Some(Packet {
        deletion: flags & 0x04 != 0,
        msg_id_hash: u16::from_be_bytes([buf[2], buf[3]]),
        origin,
        sdp: String::from_utf8_lossy(rest).to_string(),
    })
}

/// A 16-bit hash of the SDP, so a change of version is a new message id and a
/// receiver that caches by (origin, hash) notices it (RFC 2974 §5).
pub fn msg_id_hash(sdp: &str) -> u16 {
    let mut h: u32 = 0x811c_9dc5;
    for b in sdp.bytes() {
        h = (h ^ b as u32).wrapping_mul(0x0100_0193);
    }
    (h ^ (h >> 16)) as u16
}

#[derive(Debug, Clone, Serialize)]
pub struct Remote {
    pub name: String,
    pub origin: Ipv4Addr,
    pub sdp: String,
    pub last_seen_s: u64,
    /// One of ours, heard back over multicast loopback.
    pub local: bool,
}

struct Seen {
    name: String,
    origin: Ipv4Addr,
    sdp: String,
    at: Instant,
}

/// What the network is offering. Keyed by (origin, hash) as RFC 2974 says.
#[derive(Default)]
pub struct Browser {
    sessions: Mutex<HashMap<(Ipv4Addr, u16), Seen>>,
    /// Listener threads running — a count, because a re-tune starts the new
    /// one before the old one has noticed its stop flag.
    pub listening: AtomicUsize,
}

impl Browser {
    pub fn hear(&self, p: Packet) {
        let mut map = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        if p.deletion {
            map.remove(&(p.origin, p.msg_id_hash));
            return;
        }
        let name = p
            .sdp
            .lines()
            .find_map(|l| l.trim_end_matches('\r').strip_prefix("s="))
            .unwrap_or("")
            .trim()
            .to_string();
        // A new version of the same session replaces the old one rather than
        // listing it twice: same origin, same name, different hash.
        map.retain(|(o, _), s| !(*o == p.origin && s.name == name));
        map.insert((p.origin, p.msg_id_hash), Seen { name, origin: p.origin, sdp: p.sdp, at: Instant::now() });
    }

    pub fn list(&self, local_ips: &[Ipv4Addr]) -> Vec<Remote> {
        let mut map = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        map.retain(|_, s| s.at.elapsed() < FORGET_AFTER);
        let mut out: Vec<Remote> = map
            .values()
            .map(|s| Remote {
                name: s.name.clone(),
                origin: s.origin,
                sdp: s.sdp.clone(),
                last_seen_s: s.at.elapsed().as_secs(),
                local: local_ips.contains(&s.origin),
            })
            .collect();
        out.sort_by(|a, b| a.name.cmp(&b.name).then(a.origin.cmp(&b.origin)));
        out
    }

    /// The SDP of a session by its `s=` name — how a sink follows a sender.
    pub fn find(&self, name: &str) -> Option<String> {
        let map = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        map.values()
            .filter(|s| s.name == name && s.at.elapsed() < FORGET_AFTER)
            .max_by_key(|s| std::cmp::Reverse(s.at.elapsed()))
            .map(|s| s.sdp.clone())
    }
}

/// Listen until `stop`. Returns the bind error, if there was one.
pub fn listen(browser: Arc<Browser>, iface: Ipv4Addr, stop: Arc<AtomicBool>) -> std::io::Result<()> {
    let socket = crate::net::multicast_listener(GROUP, PORT, iface, None)?;
    browser.listening.fetch_add(1, Ordering::Relaxed);
    let mut buf = vec![0u8; 65536];
    while !stop.load(Ordering::Relaxed) {
        match socket.recv_from(&mut buf) {
            Ok((n, _)) => {
                if let Some(p) = decode(&buf[..n]) {
                    browser.hear(p);
                }
            }
            Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {}
            Err(e) => {
                eprintln!("⚠️  [aes67] SAP receive: {e}");
                std::thread::sleep(Duration::from_secs(1));
            }
        }
    }
    browser.listening.fetch_sub(1, Ordering::Relaxed);
    Ok(())
}

/// Send one announcement (or deletion) for each SDP.
pub fn announce(socket: &UdpSocket, origin: Ipv4Addr, sdps: &[String], deletion: bool) {
    let to = SocketAddrV4::new(GROUP, PORT);
    for sdp in sdps {
        let packet = encode(&Packet { deletion, msg_id_hash: msg_id_hash(sdp), origin, sdp: sdp.clone() });
        if let Err(e) = socket.send_to(&packet, to) {
            eprintln!("⚠️  [aes67] SAP announce: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_packet_round_trips() {
        let p = Packet {
            deletion: false,
            msg_id_hash: 0xabcd,
            origin: Ipv4Addr::new(10, 1, 2, 3),
            sdp: "v=0\r\ns=x\r\n".into(),
        };
        assert_eq!(decode(&encode(&p)), Some(p));
    }

    #[test]
    fn a_payload_without_a_mime_type_is_still_sdp() {
        let mut raw = vec![0x20, 0, 0, 1, 10, 0, 0, 1];
        raw.extend_from_slice(b"v=0\r\ns=bare\r\n");
        assert_eq!(decode(&raw).unwrap().sdp, "v=0\r\ns=bare\r\n");
    }

    #[test]
    fn a_new_version_replaces_the_old_listing() {
        let b = Browser::default();
        let o = Ipv4Addr::new(10, 0, 0, 9);
        b.hear(Packet { deletion: false, msg_id_hash: 1, origin: o, sdp: "v=0\ns=Mix\no=- 1 1".into() });
        b.hear(Packet { deletion: false, msg_id_hash: 2, origin: o, sdp: "v=0\ns=Mix\no=- 1 2".into() });
        assert_eq!(b.list(&[]).len(), 1);
        b.hear(Packet { deletion: true, msg_id_hash: 2, origin: o, sdp: String::new() });
        assert!(b.list(&[]).is_empty());
    }
}
