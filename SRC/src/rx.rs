// Part of the APK.audio project — http://APK.audio — made by Anthony Kuzub
// MIT Licence. Free, for everyone, for ever. Full text in LICENSE at the root.
//! A receiver: RTP → a ring → the playback card, on the channels `map` names.
//!
//! LOSS IS CONCEALED IN TIME, NOT IN SAMPLES. A gap in the sequence numbers
//! becomes that many packets of silence, so what follows plays at the right
//! moment and the buffer does not creep; a packet arriving behind the one
//! already played is dropped and counted, never played late.
//!
//! PLAYOUT IS ADAPTIVE: the ring is held at `delay_ms` by the DriftReader on
//! the card side. Where both ends share a PTP grandmaster the link offset —
//! how far behind the media clock the stream arrives — is measured and shown
//! beside it, which is the number to size `delay_ms` from.

use crate::audio::{Bus, Feed};
use crate::clock::{self, Clock};
use crate::config::Sink;
use crate::ring::{DriftReader, Meter, ReaderStats, Ring};
use crate::rtp;
use crate::sdp::StreamDesc;
use serde::Serialize;
use std::net::{IpAddr, Ipv4Addr};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// A sequence jump bigger than this is a sender restart, not loss.
const MAX_GAP: i32 = 100;

#[derive(Default)]
pub struct RxStats {
    pub packets: AtomicU64,
    pub bytes: AtomicU64,
    pub lost: AtomicU64,
    pub out_of_order: AtomicU64,
    pub wrong_payload: AtomicU64,
    pub foreign_source: AtomicU64,
    pub restarts: AtomicU64,
    /// SSRC in the low 32 bits, bit 32 set once one is locked.
    pub ssrc: AtomicU64,
    pub sender: AtomicU64,
    /// CLOCK_MONOTONIC_RAW ns of the last accepted packet.
    pub last_packet: AtomicI64,
    /// Link offset in samples, ×16, smoothed.
    pub link_offset_x16: AtomicI64,
    /// RFC 3550 interarrival jitter in samples, ×16.
    pub jitter_x16: AtomicI64,
}

pub struct Receiver {
    pub cfg: Sink,
    pub desc: StreamDesc,
    pub sdp: String,
    pub stats: Arc<RxStats>,
    pub feed: Arc<Feed>,
    pub error: Arc<Mutex<Option<String>>>,
    bus: Arc<Bus>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Receiver {
    pub fn start(
        cfg: &Sink,
        sdp: &str,
        desc: StreamDesc,
        card_rate: u32,
        iface: Ipv4Addr,
        clock: Arc<Clock>,
        bus: Arc<Bus>,
    ) -> Receiver {
        let ch = desc.channels as usize;
        let rate = desc.rate;
        let card_rate = if card_rate == 0 { rate } else { card_rate };
        let target = ((cfg.delay_ms as f64 / 1000.0) * rate as f64) as usize;
        let ring = Arc::new(Ring::new((target * 4).max(rate as usize / 2), ch));
        let mut map: Vec<Option<usize>> = cfg.map.iter().map(|m| m.map(|c| c as usize)).collect();
        map.resize(ch, None);
        let feed = Arc::new(Feed {
            map,
            ring: Arc::clone(&ring),
            reader: Mutex::new(DriftReader::new(ch, rate, card_rate, target)),
            target_frames: std::sync::atomic::AtomicUsize::new(target),
            stream_rate: rate,
            stats: ReaderStats::default(),
            meter: Meter::new(ch),
        });
        bus.add_feed(Arc::clone(&feed));

        let stats = Arc::new(RxStats::default());
        let error = Arc::new(Mutex::new(None));
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let (desc, stats, error, stop) = (desc.clone(), Arc::clone(&stats), Arc::clone(&error), Arc::clone(&stop));
            std::thread::Builder::new()
                .name(format!("aes67:rx{}", cfg.id))
                .spawn(move || {
                    let group = if desc.address.is_multicast() { desc.address } else { Ipv4Addr::UNSPECIFIED };
                    let socket = match crate::net::multicast_listener(group, desc.port, iface, desc.source_filter) {
                        Ok(s) => s,
                        // An SSM join a switch or kernel refuses: fall back to
                        // any-source and filter the sender in user space.
                        Err(_) if desc.source_filter.is_some() => {
                            match crate::net::multicast_listener(group, desc.port, iface, None) {
                                Ok(s) => s,
                                Err(e) => return fail(&error, e),
                            }
                        }
                        Err(e) => return fail(&error, e),
                    };
                    run(&desc, socket, &ring, &clock, &stats, &stop);
                })
                .expect("rx thread")
        };
        println!(
            "📥 [aes67] sink {} “{}” ← {}:{} ({}, {} ch, {} Hz)",
            cfg.id, cfg.name, desc.address, desc.port, desc.profile.as_str(), ch, rate
        );
        Receiver { cfg: cfg.clone(), desc, sdp: sdp.to_string(), stats, feed, error, bus, stop, thread: Some(thread) }
    }
}

fn fail(error: &Mutex<Option<String>>, e: std::io::Error) {
    *error.lock().unwrap_or_else(|e| e.into_inner()) = Some(format!("socket: {e}"));
}

fn run(desc: &StreamDesc, socket: std::net::UdpSocket, ring: &Ring, clock: &Clock, stats: &RxStats, stop: &AtomicBool) {
    let ch = desc.channels as usize;
    let rate = desc.rate;
    let n = rtp::frames_per_packet(rate, desc.ptime_us).max(1);
    let offset = desc.mediaclk_offset.unwrap_or(0);
    let mut buf = vec![0u8; 9000];
    let mut samples = vec![0f32; 64 * 1500];
    let silence = vec![0f32; ch];
    let mut expected: Option<u16> = None;
    let mut locked_ssrc: Option<u32> = None;
    let mut last_transit: Option<i64> = None;
    let mut have_offset = false;

    while !stop.load(Ordering::Relaxed) {
        let (len, from) = match socket.recv_from(&mut buf) {
            Ok(x) => x,
            Err(_) => continue, // the 200 ms timeout: look at `stop` again
        };
        let IpAddr::V4(from_ip) = from.ip() else { continue };
        if desc.source_filter.is_some_and(|src| src != from_ip) {
            stats.foreign_source.fetch_add(1, Ordering::Relaxed);
            continue;
        }
        let Some((h, range)) = rtp::Header::parse(&buf[..len]) else { continue };
        if h.payload_type != desc.payload_type {
            stats.wrong_payload.fetch_add(1, Ordering::Relaxed);
            continue;
        }
        let now_raw = clock::mono_raw_ns();
        // One sender per stream. Another SSRC is ignored while the locked one
        // is live, and taken over once it has been silent for half a second.
        if locked_ssrc != Some(h.ssrc) {
            let silent_ns = now_raw - stats.last_packet.load(Ordering::Relaxed) as i128;
            if locked_ssrc.is_some() && silent_ns < 500_000_000 {
                stats.foreign_source.fetch_add(1, Ordering::Relaxed);
                continue;
            }
            if locked_ssrc.is_some() {
                stats.restarts.fetch_add(1, Ordering::Relaxed);
            }
            locked_ssrc = Some(h.ssrc);
            expected = None;
            last_transit = None;
            stats.ssrc.store((1 << 32) | h.ssrc as u64, Ordering::Relaxed);
            stats.sender.store(u32::from(from_ip) as u64, Ordering::Relaxed);
        }

        if let Some(exp) = expected {
            let gap = h.sequence.wrapping_sub(exp) as i16 as i32;
            if gap < 0 {
                stats.out_of_order.fetch_add(1, Ordering::Relaxed);
                continue;
            }
            if gap > MAX_GAP {
                stats.restarts.fetch_add(1, Ordering::Relaxed);
            } else if gap > 0 {
                stats.lost.fetch_add(gap as u64, Ordering::Relaxed);
                for _ in 0..gap as usize * n {
                    ring.push(&silence);
                }
            }
        }
        expected = Some(h.sequence.wrapping_add(1));

        let payload = &buf[range];
        let count = rtp::decode(desc.encoding, payload, &mut samples);
        for frame in samples[..count - count % ch.max(1)].chunks_exact(ch.max(1)) {
            ring.push(frame);
        }
        stats.packets.fetch_add(1, Ordering::Relaxed);
        stats.bytes.fetch_add(len as u64, Ordering::Relaxed);
        stats.last_packet.store(now_raw as i64, Ordering::Relaxed);

        // Link offset: how far the media clock has run past this packet's
        // last sample. Meaningful when both ends follow one grandmaster.
        let media_now = clock::media_ts(clock.now_ns(), rate);
        let pkt_end = h.timestamp.wrapping_sub(offset).wrapping_add((count / ch.max(1)) as u32);
        let link = media_now.wrapping_sub(pkt_end) as i32 as i64;
        let prev = stats.link_offset_x16.load(Ordering::Relaxed);
        let next = if have_offset { prev + (link * 16 - prev) / 32 } else { link * 16 };
        have_offset = true;
        stats.link_offset_x16.store(next, Ordering::Relaxed);

        // RFC 3550 §6.4.1 jitter, in samples.
        let transit = media_now.wrapping_sub(h.timestamp) as i32 as i64;
        if let Some(last) = last_transit {
            let d = (transit - last).abs() * 16;
            let j = stats.jitter_x16.load(Ordering::Relaxed);
            stats.jitter_x16.store(j + (d - j) / 16, Ordering::Relaxed);
        }
        last_transit = Some(transit);
    }
}

#[derive(Serialize)]
pub struct RxStatus {
    pub id: u32,
    pub state: &'static str,
    pub error: Option<String>,
    pub stream: StreamDesc,
    pub sender: Option<Ipv4Addr>,
    pub ssrc: Option<u32>,
    pub packets: u64,
    pub bytes: u64,
    pub lost: u64,
    pub out_of_order: u64,
    pub wrong_payload: u64,
    pub foreign_source: u64,
    pub restarts: u64,
    pub underruns: u64,
    pub overruns: u64,
    pub resyncs: u64,
    pub buffer_ms: f64,
    pub target_ms: f64,
    pub trim_ppm: f64,
    pub link_offset_ms: Option<f64>,
    pub jitter_ms: f64,
    pub level_dbfs: Vec<f32>,
}

impl Receiver {
    pub fn status(&self) -> RxStatus {
        let s = &self.stats;
        let rate = self.desc.rate.max(1) as f64;
        let error = self.error.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let last = s.last_packet.load(Ordering::Relaxed) as i128;
        let live = last != 0 && clock::mono_raw_ns() - last < 1_000_000_000;
        let ssrc = s.ssrc.load(Ordering::Relaxed);
        let target = self.feed.target_frames.load(Ordering::Relaxed);
        RxStatus {
            id: self.cfg.id,
            state: if error.is_some() { "error" } else if live { "receiving" } else { "waiting" },
            error,
            stream: self.desc.clone(),
            sender: (ssrc >> 32 != 0).then(|| Ipv4Addr::from(s.sender.load(Ordering::Relaxed) as u32)),
            ssrc: (ssrc >> 32 != 0).then_some(ssrc as u32),
            packets: s.packets.load(Ordering::Relaxed),
            bytes: s.bytes.load(Ordering::Relaxed),
            lost: s.lost.load(Ordering::Relaxed),
            out_of_order: s.out_of_order.load(Ordering::Relaxed),
            wrong_payload: s.wrong_payload.load(Ordering::Relaxed),
            foreign_source: s.foreign_source.load(Ordering::Relaxed),
            restarts: s.restarts.load(Ordering::Relaxed),
            underruns: self.feed.stats.underruns.load(Ordering::Relaxed),
            overruns: self.feed.ring.overruns.load(Ordering::Relaxed),
            resyncs: self.feed.stats.resyncs.load(Ordering::Relaxed),
            buffer_ms: self.feed.stats.fill_x16.load(Ordering::Relaxed) as f64 / 16.0 * 1000.0 / rate,
            target_ms: target as f64 * 1000.0 / rate,
            trim_ppm: self.feed.stats.trim_ppb.load(Ordering::Relaxed) as i64 as f64 / 1000.0,
            link_offset_ms: (s.packets.load(Ordering::Relaxed) > 0)
                .then(|| s.link_offset_x16.load(Ordering::Relaxed) as f64 / 16.0 * 1000.0 / rate),
            jitter_ms: s.jitter_x16.load(Ordering::Relaxed) as f64 / 16.0 * 1000.0 / rate,
            level_dbfs: self.feed.meter.take_dbfs(),
        }
    }
}

impl Drop for Receiver {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        self.bus.remove_feed(&self.feed);
    }
}
