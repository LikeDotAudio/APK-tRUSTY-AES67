// Part of the APK.audio project — http://APK.audio — made by Anthony Kuzub
// MIT Licence. Free, for everyone, for ever. Full text in LICENSE at the root.
//! RTP (RFC 3550) framing and the two linear PCM payloads AES67 allows.
//!
//! L16 and L24 are big-endian two's complement, interleaved by channel
//! (RFC 3190 / RFC 3551). Samples cross this module as `f32` in [-1, 1] so the
//! resampler and the mixer never see an integer width.

use serde::{Deserialize, Serialize};

/// The fixed RTP header, no CSRCs and no extension — what every AES67 sender
/// emits and the only shape this receiver needs to write.
pub const HEADER_LEN: usize = 12;

/// AES67 §7.2 / ST 2110-10: no RTP payload larger than this.
pub const MAX_PAYLOAD: usize = 1440;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Encoding {
    L16,
    L24,
}

impl Encoding {
    pub fn bytes(self) -> usize {
        match self {
            Encoding::L16 => 2,
            Encoding::L24 => 3,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Encoding::L16 => "L16",
            Encoding::L24 => "L24",
        }
    }

    pub fn parse(word: &str) -> Option<Encoding> {
        match word.trim().to_ascii_uppercase().as_str() {
            "L16" => Some(Encoding::L16),
            "L24" => Some(Encoding::L24),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    pub marker: bool,
    pub payload_type: u8,
    pub sequence: u16,
    pub timestamp: u32,
    pub ssrc: u32,
}

impl Header {
    pub fn write(&self, out: &mut [u8]) {
        out[0] = 0x80; // V=2, no padding, no extension, CC=0
        out[1] = (self.payload_type & 0x7f) | if self.marker { 0x80 } else { 0 };
        out[2..4].copy_from_slice(&self.sequence.to_be_bytes());
        out[4..8].copy_from_slice(&self.timestamp.to_be_bytes());
        out[8..12].copy_from_slice(&self.ssrc.to_be_bytes());
    }

    /// The header and where the payload starts, skipping CSRCs and an
    /// extension if a sender used them; padding is trimmed from the end.
    pub fn parse(packet: &[u8]) -> Option<(Header, std::ops::Range<usize>)> {
        if packet.len() < HEADER_LEN || packet[0] >> 6 != 2 {
            return None;
        }
        let csrc = (packet[0] & 0x0f) as usize;
        let mut start = HEADER_LEN + 4 * csrc;
        if packet[0] & 0x10 != 0 {
            let ext = packet.get(start + 2..start + 4)?;
            start += 4 + 4 * u16::from_be_bytes([ext[0], ext[1]]) as usize;
        }
        let mut end = packet.len();
        if packet[0] & 0x20 != 0 {
            end = end.checked_sub(*packet.last()? as usize)?;
        }
        if start > end {
            return None;
        }
        let header = Header {
            marker: packet[1] & 0x80 != 0,
            payload_type: packet[1] & 0x7f,
            sequence: u16::from_be_bytes([packet[2], packet[3]]),
            timestamp: u32::from_be_bytes([packet[4], packet[5], packet[6], packet[7]]),
            ssrc: u32::from_be_bytes([packet[8], packet[9], packet[10], packet[11]]),
        };
        Some((header, start..end))
    }
}

/// Interleaved `f32` frames to network-order PCM. `out` must hold
/// `samples.len() * encoding.bytes()` bytes.
pub fn encode(encoding: Encoding, samples: &[f32], out: &mut [u8]) {
    match encoding {
        Encoding::L16 => {
            for (s, o) in samples.iter().zip(out.chunks_exact_mut(2)) {
                let v = (s.clamp(-1.0, 1.0) * 32767.0).round() as i16;
                o.copy_from_slice(&v.to_be_bytes());
            }
        }
        Encoding::L24 => {
            for (s, o) in samples.iter().zip(out.chunks_exact_mut(3)) {
                let v = (s.clamp(-1.0, 1.0) * 8_388_607.0).round() as i32;
                let b = v.to_be_bytes();
                o.copy_from_slice(&b[1..4]);
            }
        }
    }
}

/// Network-order PCM to `f32`. Returns how many samples were written.
pub fn decode(encoding: Encoding, payload: &[u8], out: &mut [f32]) -> usize {
    let mut n = 0;
    match encoding {
        Encoding::L16 => {
            for (b, o) in payload.chunks_exact(2).zip(out.iter_mut()) {
                *o = i16::from_be_bytes([b[0], b[1]]) as f32 / 32768.0;
                n += 1;
            }
        }
        Encoding::L24 => {
            for (b, o) in payload.chunks_exact(3).zip(out.iter_mut()) {
                // Sign-extend by landing the 24 bits in the top of an i32.
                let v = i32::from_be_bytes([b[0], b[1], b[2], 0]) >> 8;
                *o = v as f32 / 8_388_608.0;
                n += 1;
            }
        }
    }
    n
}

/// Frames in one packet: `rate × ptime`. 48 at 48 kHz / 1 ms, 6 at 125 µs.
pub fn frames_per_packet(rate: u32, ptime_us: u32) -> usize {
    ((rate as u64 * ptime_us as u64) / 1_000_000) as usize
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_header_round_trips() {
        let h = Header { marker: true, payload_type: 97, sequence: 65535, timestamp: 0xdead_beef, ssrc: 7 };
        let mut buf = [0u8; 12];
        h.write(&mut buf);
        let (back, range) = Header::parse(&buf).unwrap();
        assert_eq!(back, h);
        assert_eq!(range, 12..12);
    }

    #[test]
    fn l24_keeps_its_sign_and_its_resolution() {
        let samples = [0.0f32, 0.5, -0.5, -1.0, 0.999_999];
        let mut bytes = [0u8; 15];
        encode(Encoding::L24, &samples, &mut bytes);
        let mut back = [0f32; 5];
        assert_eq!(decode(Encoding::L24, &bytes, &mut back), 5);
        for (a, b) in samples.iter().zip(back.iter()) {
            assert!((a - b).abs() < 1e-6, "{a} came back as {b}");
        }
    }

    #[test]
    fn l16_round_trips_within_one_step() {
        let samples = [0.25f32, -0.75];
        let mut bytes = [0u8; 4];
        encode(Encoding::L16, &samples, &mut bytes);
        let mut back = [0f32; 2];
        decode(Encoding::L16, &bytes, &mut back);
        assert!((back[0] - 0.25).abs() < 1.0 / 16384.0);
        assert!((back[1] + 0.75).abs() < 1.0 / 16384.0);
    }

    #[test]
    fn packet_sizes_are_the_standard_ones() {
        assert_eq!(frames_per_packet(48_000, 1000), 48);
        assert_eq!(frames_per_packet(48_000, 125), 6);
        assert_eq!(frames_per_packet(96_000, 250), 24);
    }
}
