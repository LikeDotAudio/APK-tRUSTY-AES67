// Part of the APK.audio project — http://APK.audio — made by Anthony Kuzub
// MIT Licence. Free, for everyone, for ever. Full text in LICENSE at the root.
//! The media clock: a user-space PTPv2 slave, or the host's CLOCK_TAI.
//!
//! AES67 §5 and ST 2110-10 §7 both define the RTP timestamp as the PTP time
//! (TAI, since the PTP epoch) times the sample rate, modulo 2³² — plus the
//! `mediaclk:direct=` offset, which a sender here always declares as 0. So all
//! the rest of the daemon needs from this module is "what time is it on the
//! grandmaster", and [`Clock::now_ns`] answers it.
//!
//! WHAT THE ORIGINAL DAEMON DID AND WHY THIS IS DIFFERENT. aes67-linux-daemon
//! runs its PTP slave inside the RAVENNA kernel module, which timestamps in the
//! driver and steers the virtual card's sample clock. Here nothing is steered:
//! CLOCK_MONOTONIC_RAW free-runs, and a *virtual* clock — an offset and a
//! frequency ratio over it — is disciplined to the master by a PI servo. The
//! card keeps its own crystal and the resamplers in `audio.rs` absorb the rest.
//!
//! PRECISION, STATED: timestamps are taken in user space right after `recv`,
//! so each carries tens of microseconds of scheduling jitter. A lucky-packet
//! filter (the minimum over a window — queueing only ever makes a packet late)
//! and a slow servo bring the virtual clock to within a few tens of µs of the
//! grandmaster, i.e. about a sample at 48 kHz. That is ample for the adaptive
//! playout this bridge does; it is not a boundary clock and does not pretend to
//! be. For better, run ptp4l + phc2sys on the host and set `clock_source =
//! system`, which reads CLOCK_TAI instead.
//!
//! E2E delay request/response, two-step and one-step masters, a best-master
//! choice over Announce (IEEE 1588-2008 §9.3.2, without the topology tiebreak a
//! slave-only port never needs). No unicast negotiation, no P2P.

use serde::Serialize;
use std::collections::{HashMap, VecDeque};
use std::net::{Ipv4Addr, SocketAddrV4, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const PTP_PRIMARY: Ipv4Addr = Ipv4Addr::new(224, 0, 1, 129);
pub const EVENT_PORT: u16 = 319;
pub const GENERAL_PORT: u16 = 320;

const MSG_SYNC: u8 = 0x0;
const MSG_DELAY_REQ: u8 = 0x1;
const MSG_FOLLOW_UP: u8 = 0x8;
const MSG_DELAY_RESP: u8 = 0x9;
const MSG_ANNOUNCE: u8 = 0xB;

/// A master unheard for this long is no longer a candidate (3 × a 2 s
/// announce interval, with slack).
const MASTER_TIMEOUT: Duration = Duration::from_secs(8);
/// A phase error beyond this is stepped, not slewed.
const STEP_NS: f64 = 2_000_000.0;
/// Within this for LOCK_COUNT consecutive updates is "locked".
const LOCK_NS: f64 = 100_000.0;
const LOCK_COUNT: u32 = 4;
/// Syncs per servo update — the lucky-packet window.
const WINDOW: usize = 4;
const KP: f64 = 0.1;
const KI: f64 = 0.01;
const MAX_FREQ: f64 = 500e-6;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    Ptp,
    System,
}

/// Nanoseconds on CLOCK_MONOTONIC_RAW — never slewed or stepped by anybody.
pub fn mono_raw_ns() -> i128 {
    read_clock(libc::CLOCK_MONOTONIC_RAW)
}

/// Nanoseconds on CLOCK_TAI — PTP time, if the host keeps it.
pub fn tai_ns() -> i128 {
    read_clock(libc::CLOCK_TAI)
}

fn read_clock(id: libc::clockid_t) -> i128 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: a valid clock id and a timespec we own.
    unsafe { libc::clock_gettime(id, &mut ts) };
    ts.tv_sec as i128 * 1_000_000_000 + ts.tv_nsec as i128
}

/// RTP timestamp at PTP time `ns`: rate × seconds since the epoch, mod 2³².
pub fn media_ts(ns: i128, rate: u32) -> u32 {
    ((ns.max(0) as u128 * rate as u128) / 1_000_000_000) as u32
}

// ----------------------------------------------------------------- the servo

#[derive(Debug, Default, Clone)]
struct Servo {
    init: bool,
    base_local: i128,
    base_ptp: i128,
    freq: f64,
    last_local: i128,
    /// The first sample after a step, kept to measure frequency outright.
    anchor: Option<(i128, i128)>,
    in_lock: u32,
    offset_ns: f64,
    updates: u64,
    steps: u64,
}

impl Servo {
    fn virtual_at(&self, local: i128) -> i128 {
        self.base_ptp + ((local - self.base_local) as f64 * (1.0 + self.freq)) as i128
    }

    fn step(&mut self, local: i128, master: i128) {
        self.init = true;
        self.base_local = local;
        self.base_ptp = master;
        self.last_local = local;
        self.anchor = Some((local, master));
        self.in_lock = 0;
        self.offset_ns = 0.0;
        self.steps += 1;
    }

    /// One (local, master) pair describing the same instant.
    fn sample(&mut self, local: i128, master: i128) {
        self.updates += 1;
        if !self.init {
            self.step(local, master);
            return;
        }
        // The second sample after a step measures frequency directly, a
        // second or so of baseline instead of waiting on the integrator.
        if let Some((l0, m0)) = self.anchor.take() {
            let dl = (local - l0) as f64;
            if dl > 2e8 {
                self.freq = ((master - m0) as f64 / dl - 1.0).clamp(-MAX_FREQ, MAX_FREQ);
                self.step(local, master);
                self.anchor = None;
                return;
            }
            self.anchor = Some((l0, m0));
        }
        let e = (self.virtual_at(local) - master) as f64;
        self.offset_ns = e;
        if e.abs() > STEP_NS {
            self.step(local, master);
            return;
        }
        let dt = (local - self.last_local) as f64;
        if dt > 0.0 {
            self.freq = (self.freq - KI * e / dt).clamp(-MAX_FREQ, MAX_FREQ);
        }
        self.base_ptp = self.virtual_at(local) - (KP * e) as i128;
        self.base_local = local;
        self.last_local = local;
        self.in_lock = if e.abs() < LOCK_NS { self.in_lock + 1 } else { 0 };
    }

    fn locked(&self) -> bool {
        self.init && self.in_lock >= LOCK_COUNT
    }
}

// --------------------------------------------------------------- wire format

type PortId = [u8; 10];

#[derive(Debug, Clone, Copy)]
struct Header {
    msg_type: u8,
    version: u8,
    domain: u8,
    two_step: bool,
    correction_ns: i128,
    source: PortId,
    sequence: u16,
    log_interval: i8,
}

fn parse_header(b: &[u8]) -> Option<Header> {
    if b.len() < 34 {
        return None;
    }
    let mut source = [0u8; 10];
    source.copy_from_slice(&b[20..30]);
    Some(Header {
        msg_type: b[0] & 0x0f,
        version: b[1] & 0x0f,
        domain: b[4],
        two_step: b[6] & 0x02 != 0,
        correction_ns: (i64::from_be_bytes(b[8..16].try_into().ok()?) >> 16) as i128,
        source,
        sequence: u16::from_be_bytes([b[30], b[31]]),
        log_interval: b[33] as i8,
    })
}

fn parse_timestamp(b: &[u8]) -> Option<i128> {
    let t = b.get(0..10)?;
    let secs = u64::from_be_bytes([0, 0, t[0], t[1], t[2], t[3], t[4], t[5]]) as i128;
    let nanos = u32::from_be_bytes([t[6], t[7], t[8], t[9]]) as i128;
    Some(secs * 1_000_000_000 + nanos)
}

#[derive(Debug, Clone, Serialize)]
pub struct Announce {
    pub grandmaster: String,
    pub priority1: u8,
    pub clock_class: u8,
    pub clock_accuracy: u8,
    pub variance: u16,
    pub priority2: u8,
    pub steps_removed: u16,
    pub utc_offset: i16,
    pub time_source: u8,
    #[serde(skip)]
    gm_raw: [u8; 8],
}

impl Announce {
    /// The dataset comparison of §9.3.4, smaller is better.
    fn rank(&self) -> (u8, u8, u8, u16, u8, [u8; 8], u16) {
        (
            self.priority1,
            self.clock_class,
            self.clock_accuracy,
            self.variance,
            self.priority2,
            self.gm_raw,
            self.steps_removed,
        )
    }
}

fn parse_announce(b: &[u8]) -> Option<Announce> {
    if b.len() < 64 {
        return None;
    }
    let mut gm = [0u8; 8];
    gm.copy_from_slice(&b[53..61]);
    Some(Announce {
        grandmaster: clock_id_text(&gm),
        priority1: b[47],
        clock_class: b[48],
        clock_accuracy: b[49],
        variance: u16::from_be_bytes([b[50], b[51]]),
        priority2: b[52],
        steps_removed: u16::from_be_bytes([b[61], b[62]]),
        utc_offset: i16::from_be_bytes([b[44], b[45]]),
        time_source: b[63],
        gm_raw: gm,
    })
}

/// `39-A7-94-FF-FE-07-CB-D0` — the spelling `a=ts-refclk` uses (RFC 7273).
pub fn clock_id_text(id: &[u8]) -> String {
    id.iter().map(|b| format!("{b:02X}")).collect::<Vec<_>>().join("-")
}

fn port_text(p: &PortId) -> String {
    format!("{}:{}", clock_id_text(&p[..8]), u16::from_be_bytes([p[8], p[9]]))
}

fn build_delay_req(domain: u8, me: &PortId, seq: u16) -> [u8; 44] {
    let mut b = [0u8; 44];
    b[0] = MSG_DELAY_REQ;
    b[1] = 2;
    b[2..4].copy_from_slice(&44u16.to_be_bytes());
    b[4] = domain;
    b[20..30].copy_from_slice(me);
    b[30..32].copy_from_slice(&seq.to_be_bytes());
    b[32] = 1; // control: Delay_Req
    b[33] = 0x7f;
    b
}

/// EUI-64 from the interface MAC; a fixed port number so a ptp4l on the same
/// host (which uses the same EUI-64 with port 1) is never answered for us.
fn my_port_id(mac: Option<&str>) -> PortId {
    let mut m = [0x02u8, 0xa6, 0x7a, 0, 0, 1];
    if let Some(mac) = mac {
        let parsed: Vec<u8> = mac.split(':').filter_map(|h| u8::from_str_radix(h, 16).ok()).collect();
        if parsed.len() == 6 {
            m.copy_from_slice(&parsed);
        }
    }
    [m[0], m[1], m[2], 0xff, 0xfe, m[3], m[4], m[5], 0x0a, 0x67]
}

// ---------------------------------------------------------------- the slave

#[derive(Default)]
struct Slave {
    masters: HashMap<PortId, (Announce, Instant)>,
    selected: Option<PortId>,
    /// Sync seen, waiting for its Follow_Up: (seq, t2 local, correction).
    pending: Option<(u16, i128, i128)>,
    /// The latest complete pair, for the next delay computation.
    last_pair: Option<(i128, i128)>,
    window: Vec<(i128, i128)>,
    delay_req: Option<(u16, i128)>,
    delay_seq: u16,
    delay_log_interval: i8,
    delays: VecDeque<i128>,
    mean_delay: Option<i128>,
    servo: Servo,
    sync_log_interval: i8,
    counts: Counts,
}

#[derive(Debug, Default, Clone, Serialize)]
pub struct Counts {
    pub sync: u64,
    pub follow_up: u64,
    pub announce: u64,
    pub delay_req: u64,
    pub delay_resp: u64,
    pub foreign_domain: u64,
}

impl Slave {
    fn choose_master(&mut self) {
        self.masters.retain(|_, (_, at)| at.elapsed() < MASTER_TIMEOUT);
        let best = self.masters.iter().min_by_key(|(_, (a, _))| a.rank()).map(|(p, _)| *p);
        if best != self.selected {
            self.selected = best;
            self.servo = Servo::default();
            self.pending = None;
            self.last_pair = None;
            self.window.clear();
            self.delays.clear();
            self.mean_delay = None;
        }
    }

    /// A complete (t1 master, t2 local) pair: feed the lucky-packet window.
    fn pair(&mut self, t1: i128, t2: i128) {
        self.last_pair = Some((t1, t2));
        let master_at_t2 = t1 + self.mean_delay.unwrap_or(0);
        self.window.push((t2, master_at_t2));
        let needed = if self.servo.init { WINDOW } else { 1 };
        if self.window.len() >= needed {
            // The least-delayed packet: smallest (local − master).
            let best = *self.window.iter().min_by_key(|(l, m)| {
                if self.servo.init { self.servo.virtual_at(*l) - m } else { l - m }
            }).expect("window is not empty");
            self.window.clear();
            self.servo.sample(best.0, best.1);
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Status {
    pub source: Source,
    pub state: &'static str,
    pub locked: bool,
    pub domain: u8,
    pub interface: String,
    pub my_port: String,
    pub master_port: Option<String>,
    pub grandmaster: Option<Announce>,
    pub masters_seen: usize,
    pub offset_ns: f64,
    pub mean_path_delay_ns: Option<i128>,
    pub freq_ppm: f64,
    pub sync_interval_log: i8,
    pub servo_updates: u64,
    pub servo_steps: u64,
    pub counts: Counts,
    pub error: Option<String>,
    pub ptp_time_ns: Option<i128>,
}

pub struct Clock {
    pub source: Source,
    pub domain: u8,
    iface_name: String,
    my_port: PortId,
    slave: Mutex<Slave>,
    error: Mutex<Option<String>>,
    stop: Arc<AtomicBool>,
}

impl Clock {
    /// Start the slave on `iface` (or read CLOCK_TAI for `System`).
    pub fn start(source: Source, domain: u8, iface: Option<&crate::net::Interface>) -> Arc<Clock> {
        let clock = Arc::new(Clock {
            source,
            domain,
            iface_name: iface.map(|i| i.name.clone()).unwrap_or_default(),
            my_port: my_port_id(iface.and_then(|i| i.mac.as_deref())),
            slave: Mutex::new(Slave::default()),
            error: Mutex::new(None),
            stop: Arc::new(AtomicBool::new(false)),
        });
        if source == Source::Ptp {
            match iface {
                None => clock.fail("no network interface for PTP".into()),
                Some(i) => clock.spawn(i.ipv4),
            }
        }
        clock
    }

    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }

    fn fail(&self, why: String) {
        eprintln!("⚠️  [aes67] PTP: {why}");
        *self.error.lock().unwrap_or_else(|e| e.into_inner()) = Some(why);
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Slave> {
        self.slave.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn spawn(self: &Arc<Self>, ip: Ipv4Addr) {
        let open = |port| crate::net::multicast_listener(PTP_PRIMARY, port, ip, None);
        let (event, general) = match (open(EVENT_PORT), open(GENERAL_PORT)) {
            (Ok(e), Ok(g)) => (e, g),
            (Err(e), _) | (_, Err(e)) => {
                self.fail(format!(
                    "could not bind UDP {EVENT_PORT}/{GENERAL_PORT} ({e}) — needs NET_BIND_SERVICE; \
                     falling back to CLOCK_TAI"
                ));
                return;
            }
        };
        let sock = socket2::SockRef::from(&event);
        let _ = sock.set_multicast_if_v4(&ip);
        let _ = sock.set_multicast_ttl_v4(1);
        let _ = sock.set_tos_v4(46 << 2); // EF, AES67 §8.4 for PTP events

        let me = Arc::clone(self);
        std::thread::Builder::new()
            .name("aes67:ptp-event".into())
            .spawn(move || me.event_loop(event))
            .expect("ptp event thread");
        let me = Arc::clone(self);
        std::thread::Builder::new()
            .name("aes67:ptp-general".into())
            .spawn(move || me.general_loop(general))
            .expect("ptp general thread");
    }

    fn event_loop(&self, socket: UdpSocket) {
        let mut buf = [0u8; 1500];
        let mut next_delay_req = Instant::now() + Duration::from_secs(2);
        while !self.stop.load(Ordering::Relaxed) {
            if let Ok((n, _)) = socket.recv_from(&mut buf) {
                let t2 = mono_raw_ns();
                self.on_event(&buf[..n], t2);
            }
            if Instant::now() >= next_delay_req {
                let interval = self.send_delay_req(&socket);
                next_delay_req = Instant::now() + interval;
            }
        }
    }

    fn general_loop(&self, socket: UdpSocket) {
        let mut buf = [0u8; 1500];
        while !self.stop.load(Ordering::Relaxed) {
            if let Ok((n, _)) = socket.recv_from(&mut buf) {
                self.on_general(&buf[..n]);
            }
        }
    }

    fn on_event(&self, b: &[u8], t2: i128) {
        let Some(h) = parse_header(b) else { return };
        if h.version != 2 || h.msg_type != MSG_SYNC {
            return;
        }
        let mut s = self.lock();
        if h.domain != self.domain {
            s.counts.foreign_domain += 1;
            return;
        }
        s.counts.sync += 1;
        if s.selected != Some(h.source) {
            return;
        }
        s.sync_log_interval = h.log_interval;
        if h.two_step {
            s.pending = Some((h.sequence, t2, h.correction_ns));
        } else if let Some(t1) = parse_timestamp(&b[34..]) {
            s.pair(t1 + h.correction_ns, t2);
        }
    }

    fn on_general(&self, b: &[u8]) {
        let Some(h) = parse_header(b) else { return };
        if h.version != 2 {
            return;
        }
        let mut s = self.lock();
        if h.domain != self.domain {
            s.counts.foreign_domain += 1;
            return;
        }
        match h.msg_type {
            MSG_ANNOUNCE => {
                s.counts.announce += 1;
                if let Some(a) = parse_announce(b) {
                    s.masters.insert(h.source, (a, Instant::now()));
                    s.choose_master();
                }
            }
            MSG_FOLLOW_UP => {
                s.counts.follow_up += 1;
                if s.selected != Some(h.source) {
                    return;
                }
                if let (Some((seq, t2, corr)), Some(t1)) = (s.pending, parse_timestamp(&b[34..])) {
                    if seq == h.sequence {
                        s.pending = None;
                        s.pair(t1 + corr + h.correction_ns, t2);
                    }
                }
            }
            MSG_DELAY_RESP => {
                if b.len() < 54 || b[44..54] != self.my_port {
                    return;
                }
                s.counts.delay_resp += 1;
                s.delay_log_interval = h.log_interval;
                let (Some((seq, t3)), Some(t4)) = (s.delay_req, parse_timestamp(&b[34..])) else { return };
                if seq != h.sequence || !s.servo.init {
                    return;
                }
                s.delay_req = None;
                let t4 = t4 - h.correction_ns;
                let Some((t1, t2)) = s.last_pair else { return };
                // Both local stamps through the virtual clock, so the drift
                // between the Sync and the Delay_Req cancels.
                let delay = ((s.servo.virtual_at(t2) - t1) + (t4 - s.servo.virtual_at(t3))) / 2;
                if (0..10_000_000).contains(&delay) {
                    s.delays.push_back(delay);
                    if s.delays.len() > 9 {
                        s.delays.pop_front();
                    }
                    let mut sorted: Vec<i128> = s.delays.iter().copied().collect();
                    sorted.sort_unstable();
                    s.mean_delay = Some(sorted[sorted.len() / 2]);
                }
            }
            _ => {}
        }
    }

    /// Send one Delay_Req and say when the next is due (the master's
    /// logMinDelayReqInterval, jittered so a room of slaves does not align).
    fn send_delay_req(&self, socket: &UdpSocket) -> Duration {
        let (seq, has_master, log) = {
            let mut s = self.lock();
            s.choose_master();
            s.delay_seq = s.delay_seq.wrapping_add(1);
            (s.delay_seq, s.selected.is_some() && s.servo.init, s.delay_log_interval)
        };
        let interval = Duration::from_secs_f64(2f64.powi(log.clamp(-4, 4) as i32).max(0.25));
        if !has_master {
            return Duration::from_secs(1);
        }
        let packet = build_delay_req(self.domain, &self.my_port, seq);
        let t3 = mono_raw_ns();
        if socket.send_to(&packet, SocketAddrV4::new(PTP_PRIMARY, EVENT_PORT)).is_ok() {
            let mut s = self.lock();
            s.delay_req = Some((seq, t3));
            s.counts.delay_req += 1;
        }
        let jitter = (t3 as u64 % 200) as u64;
        interval + Duration::from_millis(jitter)
    }

    /// PTP (TAI) time now. The disciplined virtual clock once PTP has a
    /// master; CLOCK_TAI before that, for `system`, or if the slave failed.
    pub fn now_ns(&self) -> i128 {
        if self.source == Source::Ptp {
            let s = self.lock();
            if s.servo.init {
                return s.servo.virtual_at(mono_raw_ns());
            }
        }
        tai_ns()
    }

    pub fn locked(&self) -> bool {
        match self.source {
            Source::Ptp => self.lock().servo.locked(),
            Source::System => true,
        }
    }

    /// `a=ts-refclk:` — the grandmaster we follow, or `traceable` when the
    /// clock is the host's and we cannot name who disciplines it.
    pub fn refclk(&self) -> String {
        let s = self.lock();
        match s.selected.and_then(|p| s.masters.get(&p)) {
            Some((a, _)) => format!("ptp=IEEE1588-2008:{}:{}", a.grandmaster, self.domain),
            None => "ptp=IEEE1588-2008:traceable".to_string(),
        }
    }

    pub fn status(&self) -> Status {
        let s = self.lock();
        let error = self.error.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let gm = s.selected.and_then(|p| s.masters.get(&p)).map(|(a, _)| a.clone());
        let state = match self.source {
            Source::System => "system",
            Source::Ptp if error.is_some() => "faulty",
            Source::Ptp if s.selected.is_none() => "listening",
            Source::Ptp if s.servo.locked() => "slave",
            Source::Ptp => "uncalibrated",
        };
        Status {
            source: self.source,
            state,
            locked: self.source == Source::System || s.servo.locked(),
            domain: self.domain,
            interface: self.iface_name.clone(),
            my_port: port_text(&self.my_port),
            master_port: s.selected.as_ref().map(port_text),
            grandmaster: gm,
            masters_seen: s.masters.len(),
            offset_ns: s.servo.offset_ns,
            mean_path_delay_ns: s.mean_delay,
            freq_ppm: s.servo.freq * 1e6,
            sync_interval_log: s.sync_log_interval,
            servo_updates: s.servo.updates,
            servo_steps: s.servo.steps,
            counts: s.counts.clone(),
            error,
            ptp_time_ns: Some(if self.source == Source::Ptp && s.servo.init {
                s.servo.virtual_at(mono_raw_ns())
            } else {
                tai_ns()
            }),
        }
    }
}

impl Drop for Clock {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn media_time_is_rate_times_ptp_seconds() {
        assert_eq!(media_ts(1_000_000_000, 48_000), 48_000);
        assert_eq!(media_ts(0, 48_000), 0);
        // And wraps at 2³² rather than saturating.
        let big = (1i128 << 32) * 1_000_000_000 / 48_000 + 1_000_000_000;
        let wrapped = media_ts(big, 48_000);
        assert!((47_999..=48_000).contains(&wrapped), "{wrapped}");
    }

    #[test]
    fn the_servo_converges_on_a_fast_master() {
        // A master running 40 ppm fast, sampled every 125 ms.
        let mut s = Servo::default();
        let mut worst_late = 0f64;
        for i in 0..400i128 {
            let local = 1_000_000_000 + i * 125_000_000;
            let master = 5_000_000_000_000 + (local as f64 * (1.0 + 40e-6)) as i128;
            s.sample(local, master);
            if i > 300 {
                worst_late = worst_late.max(s.offset_ns.abs());
            }
        }
        assert!(s.locked(), "never locked: offset {}", s.offset_ns);
        assert!(worst_late < 1_000.0, "settled to {worst_late} ns");
        assert!((s.freq - 40e-6).abs() < 1e-6, "freq {}", s.freq);
    }

    #[test]
    fn a_delay_req_is_the_shape_a_master_answers() {
        let me = my_port_id(Some("00:1d:c1:11:22:33"));
        let b = build_delay_req(3, &me, 77);
        let h = parse_header(&b).unwrap();
        assert_eq!((h.msg_type, h.version, h.domain, h.sequence), (MSG_DELAY_REQ, 2, 3, 77));
        assert_eq!(&h.source[..8], &[0x00, 0x1d, 0xc1, 0xff, 0xfe, 0x11, 0x22, 0x33]);
    }

    #[test]
    fn the_better_announce_wins() {
        let mut raw = [0u8; 64];
        raw[47] = 128;
        raw[48] = 248;
        raw[52] = 128;
        let worse = parse_announce(&raw).unwrap();
        raw[47] = 100;
        let better = parse_announce(&raw).unwrap();
        assert!(better.rank() < worse.rank());
    }
}
