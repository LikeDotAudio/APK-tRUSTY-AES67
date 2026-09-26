// Part of the APK.audio project — http://APK.audio — made by Anthony Kuzub
// MIT Licence. Free, for everyone, for ever. Full text in LICENSE at the root.
//! The hand-off between a network thread and a sound-card callback.
//!
//! [`Ring`] is single-producer single-consumer and lock-free, because one end
//! of it is always an ALSA callback, which must not block: a callback that
//! waits on a lock the network thread holds is an xrun that takes the whole
//! card down, every stream on it with it.
//!
//! [`DriftReader`] is the consumer end, and the reason this bridge works with a
//! real sound card at all. The card runs off its own crystal; the network runs
//! off PTP. They differ by tens of ppm, which is a sample every second or so —
//! a click a second if nothing absorbs it. The reader resamples by a ratio a
//! PI loop trims to hold the ring's fill level at its target, so the card and
//! the network agree on average and neither underruns. Linear interpolation:
//! transparent at ratios within a few hundred ppm of 1, audibly soft at 44.1 ↔
//! 48 kHz — convert the card, not the stream, when that matters.

use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};

pub struct Ring {
    buf: Box<[UnsafeCell<f32>]>,
    frames: usize,
    channels: usize,
    /// Frames ever written / read. Never wrap on a 64-bit target.
    head: AtomicUsize,
    tail: AtomicUsize,
    pub overruns: AtomicU64,
}

// SAFETY: the producer writes only slots in [head, tail + frames) and the
// consumer reads only [tail, head); Release/Acquire on the indices orders the
// sample writes before the reads. One producer and one consumer, by contract.
unsafe impl Sync for Ring {}
unsafe impl Send for Ring {}

impl Ring {
    pub fn new(frames: usize, channels: usize) -> Ring {
        let channels = channels.max(1);
        let frames = frames.max(16);
        Ring {
            buf: (0..frames * channels).map(|_| UnsafeCell::new(0.0)).collect(),
            frames,
            channels,
            head: AtomicUsize::new(0),
            tail: AtomicUsize::new(0),
            overruns: AtomicU64::new(0),
        }
    }

    pub fn channels(&self) -> usize {
        self.channels
    }

    pub fn capacity(&self) -> usize {
        self.frames
    }

    pub fn fill(&self) -> usize {
        self.head.load(Ordering::Acquire).saturating_sub(self.tail.load(Ordering::Acquire))
    }

    /// PRODUCER. One interleaved frame; `false` (and an overrun) when full.
    pub fn push(&self, frame: &[f32]) -> bool {
        let head = self.head.load(Ordering::Relaxed);
        let tail = self.tail.load(Ordering::Acquire);
        if head - tail >= self.frames {
            self.overruns.fetch_add(1, Ordering::Relaxed);
            return false;
        }
        let base = (head % self.frames) * self.channels;
        for c in 0..self.channels {
            // SAFETY: this slot is outside the consumer's readable range.
            unsafe { *self.buf[base + c].get() = frame.get(c).copied().unwrap_or(0.0) };
        }
        self.head.store(head + 1, Ordering::Release);
        true
    }

    /// CONSUMER. One frame into `out`; `false` when empty.
    pub fn pop(&self, out: &mut [f32]) -> bool {
        let tail = self.tail.load(Ordering::Relaxed);
        let head = self.head.load(Ordering::Acquire);
        if head == tail {
            return false;
        }
        let base = (tail % self.frames) * self.channels;
        for (c, o) in out.iter_mut().enumerate().take(self.channels) {
            // SAFETY: this slot is inside the consumer's readable range.
            *o = unsafe { *self.buf[base + c].get() };
        }
        self.tail.store(tail + 1, Ordering::Release);
        true
    }

    /// CONSUMER. Drop `n` frames (or all there are).
    pub fn skip(&self, n: usize) {
        let tail = self.tail.load(Ordering::Relaxed);
        let head = self.head.load(Ordering::Acquire);
        self.tail.store(tail + n.min(head - tail), Ordering::Release);
    }
}

/// Live numbers a reader exposes to the status page without a lock.
#[derive(Default)]
pub struct ReaderStats {
    pub underruns: AtomicU64,
    pub resyncs: AtomicU64,
    /// Ratio trim in parts per billion, as an i64 in a u64.
    pub trim_ppb: AtomicU64,
    /// Smoothed fill in frames, ×16.
    pub fill_x16: AtomicU64,
}

/// The consumer end, resampling by `nominal × (1 + trim)`.
///
/// THE LOOP, in continuous time: fill error `e` (input frames) moves at
/// `R·(δ − trim)` for a rate mismatch δ, and `trim = kp·e + ki·∫e`. With
/// `kp = 2ζω/R`, `ki = ω²/R` that is a critically damped second-order loop of
/// natural frequency ω — 0.3 rad/s here: a 100 ppm mismatch costs a few frames
/// of transient and is absorbed in ~15 s, and the trim wobbles by well under a
/// cent from block-sized fill jitter, which the smoothing pole (τ) takes out.
pub struct DriftReader {
    /// Input frames consumed per output frame at zero trim.
    nominal: f64,
    target: f64,
    kp: f64,
    ki: f64,
    /// Output frames per second, to turn a call's frame count into time.
    out_rate: f64,
    trim: f64,
    integral: f64,
    smoothed: f64,
    frac: f64,
    prev: Vec<f32>,
    cur: Vec<f32>,
    /// Waiting for the ring to refill to target before playing again.
    priming: bool,
}

const LOOP_OMEGA: f64 = 0.3;
const LOOP_ZETA: f64 = 1.0;
const SMOOTH_TAU_S: f64 = 0.5;

impl DriftReader {
    /// `in_rate` / `out_rate` in frames per second; `target` is the fill, in
    /// input frames, the loop holds the ring at.
    pub fn new(channels: usize, in_rate: u32, out_rate: u32, target: usize) -> DriftReader {
        let r = in_rate.max(1) as f64;
        DriftReader {
            nominal: r / out_rate.max(1) as f64,
            target: target.max(1) as f64,
            kp: 2.0 * LOOP_ZETA * LOOP_OMEGA / r,
            ki: LOOP_OMEGA * LOOP_OMEGA / r,
            out_rate: out_rate.max(1) as f64,
            trim: 0.0,
            integral: 0.0,
            smoothed: target as f64,
            frac: 0.0,
            prev: vec![0.0; channels],
            cur: vec![0.0; channels],
            priming: true,
        }
    }

    pub fn target(&self) -> usize {
        self.target as usize
    }

    /// Grow the target to cover `frames` — the card's burst turned out bigger
    /// than the configured cushion, and a target under one burst underruns on
    /// every burst. Never shrinks: a burst size seen once can come again.
    pub fn raise_target(&mut self, frames: usize) {
        if frames as f64 > self.target {
            self.target = frames as f64;
        }
    }

    /// Fill `out` (interleaved, `out.len() / channels` frames) from `ring`.
    /// Returns false for a buffer that was (partly) silence.
    pub fn read(&mut self, ring: &Ring, out: &mut [f32], stats: &ReaderStats) -> bool {
        let ch = self.cur.len();
        let frames = out.len() / ch.max(1);
        let fill = ring.fill() as f64;

        if self.priming {
            if fill < self.target {
                out.fill(0.0);
                return false;
            }
            self.priming = false;
            self.smoothed = fill;
        }
        // Far too full — a sender restarted, or we were descheduled. Drop to
        // target in one move rather than playing seconds of backlog fast.
        if fill > self.target * 3.0 + 256.0 {
            ring.skip((fill - self.target) as usize);
            stats.resyncs.fetch_add(1, Ordering::Relaxed);
            self.smoothed = self.target;
        }

        let dt = frames as f64 / self.out_rate;
        self.smoothed += (ring.fill() as f64 - self.smoothed) * (dt / SMOOTH_TAU_S).min(1.0);
        let e = self.smoothed - self.target;
        self.integral = (self.integral + self.ki * e * dt).clamp(-500e-6, 500e-6);
        self.trim = (self.kp * e + self.integral).clamp(-1000e-6, 1000e-6);
        let ratio = self.nominal * (1.0 + self.trim);
        stats.trim_ppb.store((self.trim * 1e9) as i64 as u64, Ordering::Relaxed);
        stats.fill_x16.store((self.smoothed.max(0.0) * 16.0) as u64, Ordering::Relaxed);

        for f in 0..frames {
            while self.frac >= 1.0 {
                std::mem::swap(&mut self.prev, &mut self.cur);
                if !ring.pop(&mut self.cur) {
                    // Underrun: silence from here and prime again.
                    stats.underruns.fetch_add(1, Ordering::Relaxed);
                    out[f * ch..].fill(0.0);
                    self.cur.fill(0.0);
                    self.prev.fill(0.0);
                    self.frac = 0.0;
                    self.priming = true;
                    return false;
                }
                self.frac -= 1.0;
            }
            let t = self.frac as f32;
            for c in 0..ch {
                out[f * ch + c] = self.prev[c] + (self.cur[c] - self.prev[c]) * t;
            }
            self.frac += ratio;
        }
        true
    }
}

/// Per-channel peak since the last read, lock-free. Positive `f32` bit
/// patterns order like the numbers, so `fetch_max` on the bits is a max.
pub struct Meter {
    peaks: Vec<AtomicU32>,
    /// The last reading and when it was taken. Several readers (the status
    /// page, the matrix, the bus restate) share one meter; without this each
    /// read would reset the peaks and the others would see silence.
    last: std::sync::Mutex<(Option<std::time::Instant>, Vec<f32>)>,
}

/// Reads closer together than this share one reading.
const METER_SHARE: std::time::Duration = std::time::Duration::from_millis(250);

impl Meter {
    pub fn new(channels: usize) -> Meter {
        Meter {
            peaks: (0..channels).map(|_| AtomicU32::new(0)).collect(),
            last: std::sync::Mutex::new((None, vec![-120.0; channels])),
        }
    }

    pub fn feed(&self, interleaved: &[f32]) {
        let ch = self.peaks.len();
        if ch == 0 {
            return;
        }
        for (c, peak) in self.peaks.iter().enumerate() {
            let mut m = 0f32;
            for s in interleaved.iter().skip(c).step_by(ch) {
                m = m.max(s.abs());
            }
            peak.fetch_max(m.to_bits(), Ordering::Relaxed);
        }
    }

    /// dBFS per channel since the last take, and reset — or that same
    /// reading again, to a reader arriving within METER_SHARE of it. −120 is
    /// silence. Only readers touch the lock; the audio callback never does.
    pub fn take_dbfs(&self) -> Vec<f32> {
        let mut last = self.last.lock().unwrap_or_else(|e| e.into_inner());
        if last.0.is_some_and(|at| at.elapsed() < METER_SHARE) {
            return last.1.clone();
        }
        let reading: Vec<f32> = self
            .peaks
            .iter()
            .map(|p| {
                let v = f32::from_bits(p.swap(0, Ordering::Relaxed));
                if v <= 1e-6 { -120.0 } else { (20.0 * v.log10()).max(-120.0) }
            })
            .collect();
        *last = (Some(std::time::Instant::now()), reading.clone());
        reading
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_come_out_in_the_order_they_went_in() {
        let r = Ring::new(16, 2);
        for i in 0..10 {
            assert!(r.push(&[i as f32, -(i as f32)]));
        }
        let mut f = [0f32; 2];
        for i in 0..10 {
            assert!(r.pop(&mut f));
            assert_eq!(f, [i as f32, -(i as f32)]);
        }
        assert!(!r.pop(&mut f));
    }

    #[test]
    fn a_full_ring_counts_the_overrun_and_keeps_the_old_audio() {
        let r = Ring::new(16, 1);
        for _ in 0..16 {
            assert!(r.push(&[1.0]));
        }
        assert!(!r.push(&[2.0]));
        assert_eq!(r.overruns.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn the_reader_holds_a_slow_producer_at_target() {
        // Producer 100 ppm slower than the consumer's clock: without the loop
        // this drains 4.8 frames a second and underruns; with it, it must not.
        let ring = Ring::new(4096, 1);
        let stats = ReaderStats::default();
        let mut reader = DriftReader::new(1, 48_000, 48_000, 480);
        let mut produced = 0f64;
        let mut pushed = 0usize;
        let mut out = [0f32; 48];
        for _ in 0..60_000 {
            produced += 48.0 * (1.0 - 100e-6);
            while (pushed as f64) < produced + 480.0 {
                ring.push(&[0.5]);
                pushed += 1;
            }
            reader.read(&ring, &mut out, &stats);
        }
        assert_eq!(stats.underruns.load(Ordering::Relaxed), 0);
        assert_eq!(stats.resyncs.load(Ordering::Relaxed), 0);
        let trim = stats.trim_ppb.load(Ordering::Relaxed) as i64;
        assert!((-120_000..-80_000).contains(&trim), "trim {trim} ppb");
    }

    #[test]
    fn meters_read_peaks_in_dbfs() {
        let m = Meter::new(2);
        m.feed(&[0.5, -1.0, 0.25, 0.0]);
        let db = m.take_dbfs();
        assert!((db[0] + 6.02).abs() < 0.05 && db[1].abs() < 0.01, "{db:?}");
        // A second reader right behind the first sees the same reading…
        assert_eq!(m.take_dbfs(), db);
        // …and a later one sees what came since, which is nothing.
        std::thread::sleep(METER_SHARE + std::time::Duration::from_millis(20));
        assert_eq!(m.take_dbfs(), vec![-120.0, -120.0]);
    }
}
