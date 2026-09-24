// Part of the APK.audio project — http://APK.audio — made by Anthony Kuzub
// MIT Licence. Free, for everyone, for ever. Full text in LICENSE at the root.
//! A transmitter: capture channels → RTP, paced by the media clock.
//!
//! PACING IS BY TIMESTAMP, NOT BY THE CARD. A packet carrying timestamp `ts`
//! leaves once the media clock has passed `ts + frames` — i.e. once its last
//! sample would have been captured by a card locked to PTP. So packets come
//! out at the network's rate whatever the card's crystal is doing, and the
//! DriftReader between the capture ring and this loop takes up the slack. The
//! first timestamp is aligned to a multiple of the packet size, as ST 2110-10
//! receivers that align buffers on it prefer.

use crate::audio::{Bus, Tap};
use crate::clock::{self, Clock};
use crate::config::Source;
use crate::ring::{DriftReader, Meter, ReaderStats, Ring};
use crate::rtp;
use crate::sdp::{self, Profile, StreamDesc};
use serde::Serialize;
use std::hash::{BuildHasher, Hasher};
use std::net::{Ipv4Addr, SocketAddrV4};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Capture-side buffering before a packet: covers an ALSA period of up to
/// ~1000 frames at 48 kHz, which is what cpal's default size tends to be.
const CAPTURE_CUSHION_S: f64 = 0.025;

#[derive(Default)]
pub struct TxStats {
    pub packets: AtomicU64,
    pub bytes: AtomicU64,
    pub late: AtomicU64,
    pub resyncs: AtomicU64,
    pub send_errors: AtomicU64,
}

pub struct Transmitter {
    pub cfg: Source,
    pub desc: StreamDesc,
    pub sdp: String,
    pub stats: Arc<TxStats>,
    pub reader: Arc<ReaderStats>,
    pub meter: Arc<Meter>,
    pub error: Arc<Mutex<Option<String>>>,
    tap: Arc<Tap>,
    bus: Arc<Bus>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

pub fn random_u32() -> u32 {
    std::collections::hash_map::RandomState::new().build_hasher().finish() as u32
}

/// The SDP this source is described by — built once per start, so a receiver
/// comparing `o=` versions sees a restart as a new version.
pub fn describe(cfg: &Source, rate: u32, origin: Ipv4Addr, node: &str, refclk: String) -> StreamDesc {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    std::hash::Hash::hash(&(node, cfg.id), &mut h);
    let version = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(1);
    let name = if node.is_empty() { cfg.name.clone() } else { format!("{node} {}", cfg.name) };
    StreamDesc {
        name,
        info: format!("{} channels, {}", cfg.channels.len(), cfg.profile.as_str()),
        profile: cfg.profile,
        origin_ip: origin,
        session_id: h.finish() & 0x7fff_ffff_ffff,
        session_version: version,
        address: cfg.address,
        port: cfg.port,
        ttl: cfg.ttl,
        payload_type: cfg.payload_type,
        encoding: cfg.encoding,
        rate,
        channels: cfg.channels.len() as u16,
        ptime_us: cfg.ptime_us,
        source_filter: cfg.address.is_multicast().then_some(origin),
        refclk: Some(refclk),
        mediaclk_offset: Some(0),
        channel_order: (cfg.profile == Profile::St2110_30).then(|| sdp::channel_order(cfg.channels.len() as u16)),
    }
}

impl Transmitter {
    pub fn start(
        cfg: &Source,
        rate: u32,
        card_rate: u32,
        iface: Ipv4Addr,
        node: &str,
        clock: Arc<Clock>,
        bus: Arc<Bus>,
    ) -> Transmitter {
        let ch = cfg.channels.len();
        let card_rate = if card_rate == 0 { rate } else { card_rate };
        let cushion = (card_rate as f64 * CAPTURE_CUSHION_S) as usize;
        let tap = Arc::new(Tap {
            channels: cfg.channels.iter().map(|c| *c as usize).collect(),
            ring: Ring::new(cushion * 4 + 16384, ch),
            burst: std::sync::atomic::AtomicUsize::new(0),
        });
        bus.add_tap(Arc::clone(&tap));

        let desc = describe(cfg, rate, iface, node, clock.refclk());
        let sdp = sdp::generate(&desc);
        let stats = Arc::new(TxStats::default());
        let reader = Arc::new(ReaderStats::default());
        let meter = Arc::new(Meter::new(ch));
        let error = Arc::new(Mutex::new(None));
        let stop = Arc::new(AtomicBool::new(false));

        let thread = {
            let (cfg, tap, stats, reader, meter, error, stop) = (
                cfg.clone(),
                Arc::clone(&tap),
                Arc::clone(&stats),
                Arc::clone(&reader),
                Arc::clone(&meter),
                Arc::clone(&error),
                Arc::clone(&stop),
            );
            std::thread::Builder::new()
                .name(format!("aes67:tx{}", cfg.id))
                .spawn(move || {
                    let socket = match crate::net::sender(iface, cfg.ttl, cfg.dscp) {
                        Ok(s) => s,
                        Err(e) => {
                            *error.lock().unwrap_or_else(|e| e.into_inner()) = Some(format!("socket: {e}"));
                            return;
                        }
                    };
                    let drift = DriftReader::new(ch, card_rate, rate, cushion);
                    run(&cfg, rate, socket, SocketAddrV4::new(cfg.address, cfg.port), &clock, &tap, drift, &stats, &reader, &meter, &stop);
                })
                .expect("tx thread")
        };
        println!("📤 [aes67] source {} “{}” → {}:{} ({}, {} ch)", cfg.id, cfg.name, cfg.address, cfg.port, cfg.profile.as_str(), ch);
        Transmitter { cfg: cfg.clone(), desc, sdp, stats, reader, meter, error, tap, bus, stop, thread: Some(thread) }
    }
}

#[allow(clippy::too_many_arguments)]
fn run(
    cfg: &Source,
    rate: u32,
    socket: std::net::UdpSocket,
    dest: SocketAddrV4,
    clock: &Clock,
    tap: &Tap,
    mut drift: DriftReader,
    stats: &TxStats,
    reader: &ReaderStats,
    meter: &Meter,
    stop: &AtomicBool,
) {
    let ch = cfg.channels.len();
    let n = rtp::frames_per_packet(rate, cfg.ptime_us).max(1);
    let n32 = n as u32;
    let bytes = cfg.encoding.bytes();
    let mut samples = vec![0f32; n * ch];
    let mut packet = vec![0u8; rtp::HEADER_LEN + n * ch * bytes];
    let mut sequence = random_u32() as u16;
    let ssrc = random_u32();
    let align = |ts: u32| ts - ts % n32;
    let mut next_ts = align(clock::media_ts(clock.now_ns(), rate)).wrapping_add(n32);

    while !stop.load(Ordering::Relaxed) {
        let now_ts = clock::media_ts(clock.now_ns(), rate);
        // Due once the packet's LAST sample time has passed.
        let ahead = next_ts.wrapping_add(n32).wrapping_sub(now_ts) as i32;
        if ahead > rate as i32 || ahead < -(n as i32 * 50).max(rate as i32 / 20) {
            // The clock stepped (PTP locked, or a new grandmaster): start over
            // from now rather than bursting or waiting out the difference.
            next_ts = align(now_ts).wrapping_add(n32);
            stats.resyncs.fetch_add(1, Ordering::Relaxed);
            continue;
        }
        if ahead > 0 {
            let ns = ahead as u64 * 1_000_000_000 / rate as u64;
            std::thread::sleep(Duration::from_nanos(ns.max(20_000)));
            continue;
        }
        if ahead < -(n as i32 * 2) {
            stats.late.fetch_add(1, Ordering::Relaxed);
        }

        // A card that delivers in bursts bigger than the cushion (PipeWire's
        // default quantum is ~21 ms) would underrun once per burst.
        drift.raise_target(tap.burst.load(Ordering::Relaxed) * 3 / 2 + n * 2);
        drift.read(&tap.ring, &mut samples, reader);
        meter.feed(&samples);
        rtp::Header { marker: false, payload_type: cfg.payload_type, sequence, timestamp: next_ts, ssrc }
            .write(&mut packet);
        rtp::encode(cfg.encoding, &samples, &mut packet[rtp::HEADER_LEN..]);
        match socket.send_to(&packet, dest) {
            Ok(sent) => {
                stats.packets.fetch_add(1, Ordering::Relaxed);
                stats.bytes.fetch_add(sent as u64, Ordering::Relaxed);
            }
            Err(_) => {
                stats.send_errors.fetch_add(1, Ordering::Relaxed);
            }
        }
        sequence = sequence.wrapping_add(1);
        next_ts = next_ts.wrapping_add(n32);
    }
}

#[derive(Serialize)]
pub struct TxStatus {
    pub id: u32,
    pub state: &'static str,
    pub error: Option<String>,
    pub sdp: String,
    pub packets: u64,
    pub bytes: u64,
    pub late: u64,
    pub resyncs: u64,
    pub send_errors: u64,
    pub underruns: u64,
    pub overruns: u64,
    pub buffer_ms: f64,
    pub card_burst_frames: usize,
    pub trim_ppm: f64,
    pub level_dbfs: Vec<f32>,
    pub conformance_level: Option<&'static str>,
}

impl Transmitter {
    pub fn status(&self, rate: u32) -> TxStatus {
        let error = self.error.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let underruns = self.reader.underruns.load(Ordering::Relaxed);
        TxStatus {
            id: self.cfg.id,
            state: if error.is_some() { "error" } else { "sending" },
            error,
            sdp: self.sdp.clone(),
            packets: self.stats.packets.load(Ordering::Relaxed),
            bytes: self.stats.bytes.load(Ordering::Relaxed),
            late: self.stats.late.load(Ordering::Relaxed),
            resyncs: self.stats.resyncs.load(Ordering::Relaxed),
            send_errors: self.stats.send_errors.load(Ordering::Relaxed),
            underruns,
            overruns: self.tap.ring.overruns.load(Ordering::Relaxed),
            buffer_ms: self.reader.fill_x16.load(Ordering::Relaxed) as f64 / 16.0 * 1000.0 / rate.max(1) as f64,
            card_burst_frames: self.tap.burst.load(Ordering::Relaxed),
            trim_ppm: self.reader.trim_ppb.load(Ordering::Relaxed) as i64 as f64 / 1000.0,
            level_dbfs: self.meter.take_dbfs(),
            conformance_level: sdp::conformance_level(rate, self.cfg.ptime_us, self.cfg.channels.len() as u16),
        }
    }
}

impl Drop for Transmitter {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        self.bus.remove_tap(&self.tap);
    }
}
