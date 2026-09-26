// Part of the APK.audio project — http://APK.audio — made by Anthony Kuzub
// MIT Licence. Free, for everyone, for ever. Full text in LICENSE at the root.
//! The sound card: one ALSA capture stream and one playback stream, shared by
//! every transmitter and receiver.
//!
//! aes67-linux-daemon makes the network LOOK like a sound card (its RAVENNA
//! driver registers a virtual ALSA device other programs open). This does the
//! opposite, which is what a plugin container can do: it opens a card that
//! already exists and moves its channels on and off the network. Two cards, or
//! the same card for both directions, as `capture_device` / `playback_device`
//! say.
//!
//! THE CALLBACKS NEVER BLOCK. They reach the stream lists with `try_lock`
//! (which fails only for the instant /config is re-routing), and the audio
//! itself through lock-free rings. A callback that loses the race plays one
//! buffer of silence; one that waited would be an xrun for every stream.

use crate::config::AudioSettings;
use crate::ring::{DriftReader, Meter, ReaderStats, Ring};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SampleFormat, SizedSample};
use serde::Serialize;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};

/// Frames of one card frame the stack buffer can carry — the most channels a
/// stream may have.
const MAX_CH: usize = 64;

/// A route that is not there: an unrouted sender channel reads silence, an
/// unrouted receiver channel goes nowhere. Also what a sender's `channels`
/// entry of 65535 (the matrix's "no source") becomes.
pub const NO_ROUTE: usize = usize::MAX;

/// A set of routes the MATRIX page can move while audio is flowing: one
/// atomic per stream channel, read by the callback on every buffer, so a
/// crosspoint takes effect on the next buffer without restarting anything.
pub struct Routes(Vec<AtomicUsize>);

impl Routes {
    pub fn new(routes: impl IntoIterator<Item = Option<usize>>) -> Routes {
        Routes(routes.into_iter().map(|r| AtomicUsize::new(r.unwrap_or(NO_ROUTE))).collect())
    }
    pub fn len(&self) -> usize {
        self.0.len()
    }
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
    pub fn get(&self, i: usize) -> Option<usize> {
        self.0.get(i).map(|a| a.load(Ordering::Relaxed)).filter(|r| *r != NO_ROUTE)
    }
    /// Same length only — a different channel COUNT is a different stream.
    pub fn set(&self, routes: impl IntoIterator<Item = Option<usize>>) {
        for (slot, r) in self.0.iter().zip(routes) {
            slot.store(r.unwrap_or(NO_ROUTE), Ordering::Relaxed);
        }
    }
}

/// Capture → transmitter: the card channels a source wants, into its ring.
pub struct Tap {
    pub channels: Routes,
    pub ring: Ring,
    /// The most frames one capture callback has delivered — the transmitter
    /// sizes its cushion from it.
    pub burst: AtomicUsize,
}

/// Receiver → playback: a ring of stream channels, resampled to the card's
/// clock, summed onto the card channels `map` names.
pub struct Feed {
    pub map: Routes,
    pub ring: Arc<Ring>,
    pub stream_rate: u32,
    pub reader: Mutex<DriftReader>,
    /// The reader's target fill, readable without its lock (which only the
    /// callback may hold). Grows if the card's callbacks outgrow `delay_ms`.
    pub target_frames: AtomicUsize,
    pub stats: ReaderStats,
    pub meter: Meter,
}

/// The two lists the callbacks read.
#[derive(Default)]
pub struct Bus {
    pub taps: Mutex<Vec<Arc<Tap>>>,
    pub feeds: Mutex<Vec<Arc<Feed>>>,
}

impl Bus {
    pub fn add_tap(&self, t: Arc<Tap>) {
        self.taps.lock().unwrap_or_else(|e| e.into_inner()).push(t);
    }
    pub fn remove_tap(&self, t: &Arc<Tap>) {
        self.taps.lock().unwrap_or_else(|e| e.into_inner()).retain(|x| !Arc::ptr_eq(x, t));
    }
    pub fn add_feed(&self, f: Arc<Feed>) {
        self.feeds.lock().unwrap_or_else(|e| e.into_inner()).push(f);
    }
    pub fn remove_feed(&self, f: &Arc<Feed>) {
        self.feeds.lock().unwrap_or_else(|e| e.into_inner()).retain(|x| !Arc::ptr_eq(x, f));
    }
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Side {
    pub requested: String,
    pub device: Option<String>,
    pub open: bool,
    pub rate: u32,
    pub channels: usize,
    pub format: String,
    pub error: Option<String>,
}

#[derive(Default)]
pub struct Counters {
    pub capture_callbacks: AtomicU64,
    pub playback_callbacks: AtomicU64,
    pub stream_errors: AtomicU64,
    pub tap_misses: AtomicU64,
    pub feed_misses: AtomicU64,
}

#[derive(Debug, Clone, Serialize)]
pub struct Status {
    pub capture: Side,
    pub playback: Side,
    pub capture_callbacks: u64,
    pub playback_callbacks: u64,
    pub stream_errors: u64,
    pub last_error: Option<String>,
    pub capture_dbfs: Vec<f32>,
    pub playback_dbfs: Vec<f32>,
}

/// An open card. Dropping it closes both streams.
pub struct Engine {
    pub capture: Side,
    pub playback: Side,
    pub counters: Arc<Counters>,
    pub capture_meter: Arc<Meter>,
    pub playback_meter: Arc<Meter>,
    last_error: Arc<Mutex<Option<String>>>,
    stop: Option<mpsc::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Engine {
    /// Open both sides and return once they are running (or have failed —
    /// each side fails alone, and the status says which).
    ///
    /// On their own thread because a cpal `Stream` is not `Send` on every
    /// backend; the thread owns them until [`Engine`] drops.
    pub fn start(settings: &AudioSettings, rate: u32, bus: Arc<Bus>) -> Engine {
        let counters = Arc::new(Counters::default());
        let capture_meter = Arc::new(Meter::new(MAX_CH));
        let playback_meter = Arc::new(Meter::new(MAX_CH));
        let pm = Arc::clone(&playback_meter);
        let last_error = Arc::new(Mutex::new(None));
        let (stop_tx, stop_rx) = mpsc::channel::<()>();
        let (ready_tx, ready_rx) = mpsc::channel::<(Side, Side)>();
        let settings = settings.clone();
        let (c, m, e) = (Arc::clone(&counters), Arc::clone(&capture_meter), Arc::clone(&last_error));

        let thread = std::thread::Builder::new()
            .name("aes67:alsa".into())
            .spawn(move || {
                let host = cpal::default_host();
                let (cap_side, cap_stream) = open_capture(&host, &settings, rate, &bus, &c, &m, &e);
                let (play_side, play_stream) = open_playback(&host, &settings, rate, &bus, &c, &pm, &e);
                let _ = ready_tx.send((cap_side, play_side));
                let _ = stop_rx.recv();
                drop(cap_stream);
                drop(play_stream);
            })
            .expect("alsa thread");

        let (capture, playback) = ready_rx.recv().unwrap_or_default();
        for side in [&capture, &playback] {
            match (&side.error, side.open) {
                (Some(err), _) => eprintln!("⚠️  [aes67] ALSA {}: {err}", side.requested),
                (None, true) => println!(
                    "🔊 [aes67] ALSA {} open: {} Hz × {} ch ({})",
                    side.device.as_deref().unwrap_or("?"),
                    side.rate,
                    side.channels,
                    side.format
                ),
                _ => {}
            }
        }
        Engine { capture, playback, counters, capture_meter, playback_meter, last_error, stop: Some(stop_tx), thread: Some(thread) }
    }

    pub fn status(&self) -> Status {
        let mut dbfs = self.capture_meter.take_dbfs();
        dbfs.truncate(self.capture.channels);
        let mut out_dbfs = self.playback_meter.take_dbfs();
        out_dbfs.truncate(self.playback.channels);
        Status {
            capture: self.capture.clone(),
            playback: self.playback.clone(),
            capture_callbacks: self.counters.capture_callbacks.load(Ordering::Relaxed),
            playback_callbacks: self.counters.playback_callbacks.load(Ordering::Relaxed),
            stream_errors: self.counters.stream_errors.load(Ordering::Relaxed),
            last_error: self.last_error.lock().unwrap_or_else(|e| e.into_inner()).clone(),
            capture_dbfs: dbfs,
            playback_dbfs: out_dbfs,
        }
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        drop(self.stop.take());
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn find_device(host: &cpal::Host, wanted: &str, input: bool) -> Result<cpal::Device, String> {
    let wanted = wanted.trim();
    if wanted.is_empty() {
        let d = if input { host.default_input_device() } else { host.default_output_device() };
        return d.ok_or_else(|| "this host has no default device".to_string());
    }
    let list = if input { host.input_devices() } else { host.output_devices() };
    let list: Vec<cpal::Device> = list.map_err(|e| e.to_string())?.collect();
    if let Some(d) = list.iter().find(|d| d.name().ok().as_deref() == Some(wanted)) {
        return Ok(d.clone());
    }
    // ALSA's enumeration skips a device somebody else holds, so "not in the
    // list" is usually "busy", not "gone" — say both.
    Err(format!(
        "{} device {wanted:?} is unavailable — unplugged, or held by another program (often the host's PipeWire)",
        if input { "capture" } else { "playback" }
    ))
}

/// The config closest to (rate, channels): exact rate if any range has it,
/// the requested channel count if the device offers it, and the best sample
/// format among those. A rate the card cannot do is not an error — the
/// resamplers convert — but it is reported, because it costs quality.
fn pick_config(
    device: &cpal::Device,
    rate: u32,
    channels: u16,
    input: bool,
) -> Result<(cpal::StreamConfig, SampleFormat), String> {
    let ranges: Vec<cpal::SupportedStreamConfigRange> = if input {
        device.supported_input_configs().map_err(|e| e.to_string())?.collect()
    } else {
        device.supported_output_configs().map_err(|e| e.to_string())?.collect()
    };
    let default = if input { device.default_input_config() } else { device.default_output_config() }
        .map_err(|e| e.to_string())?;
    let want_ch = if channels == 0 { default.channels() } else { channels };
    let format_rank = |f: SampleFormat| match f {
        SampleFormat::F32 => 0,
        SampleFormat::I32 => 1,
        SampleFormat::I16 => 2,
        _ => 9,
    };
    let usable: Vec<&cpal::SupportedStreamConfigRange> =
        ranges.iter().filter(|r| format_rank(r.sample_format()) < 9).collect();
    let pick = usable
        .iter()
        .filter(|r| r.channels() == want_ch)
        .min_by_key(|r| {
            let lo = r.min_sample_rate().0;
            let hi = r.max_sample_rate().0;
            let miss = if rate < lo { lo - rate } else { rate.saturating_sub(hi) };
            (miss, format_rank(r.sample_format()))
        })
        .or_else(|| usable.iter().min_by_key(|r| (r.channels().abs_diff(want_ch), format_rank(r.sample_format()))))
        .ok_or("the device offers no F32/I32/I16 configuration")?;
    let r = rate.clamp(pick.min_sample_rate().0, pick.max_sample_rate().0);
    let chosen = (**pick).with_sample_rate(cpal::SampleRate(r));
    Ok((chosen.config(), chosen.sample_format()))
}

fn on_error(counters: &Arc<Counters>, last: &Arc<Mutex<Option<String>>>) -> impl FnMut(cpal::StreamError) + Send + 'static {
    let (c, l) = (Arc::clone(counters), Arc::clone(last));
    move |e| {
        c.stream_errors.fetch_add(1, Ordering::Relaxed);
        *l.lock().unwrap_or_else(|e| e.into_inner()) = Some(e.to_string());
    }
}

fn open_capture(
    host: &cpal::Host,
    s: &AudioSettings,
    rate: u32,
    bus: &Arc<Bus>,
    counters: &Arc<Counters>,
    meter: &Arc<Meter>,
    last: &Arc<Mutex<Option<String>>>,
) -> (Side, Option<cpal::Stream>) {
    let mut side = Side { requested: or_default(&s.capture_device), ..Side::default() };
    let result = (|| -> Result<cpal::Stream, String> {
        let device = find_device(host, &s.capture_device, true)?;
        side.device = device.name().ok();
        let (mut config, format) = pick_config(&device, rate, s.capture_channels, true)?;
        if s.period_frames > 0 {
            config.buffer_size = cpal::BufferSize::Fixed(s.period_frames);
        }
        side.rate = config.sample_rate.0;
        side.channels = config.channels as usize;
        side.format = format!("{format:?}");
        let stream = match format {
            SampleFormat::F32 => build_input::<f32>(&device, &config, bus, counters, meter, last),
            SampleFormat::I32 => build_input::<i32>(&device, &config, bus, counters, meter, last),
            SampleFormat::I16 => build_input::<i16>(&device, &config, bus, counters, meter, last),
            other => return Err(format!("sample format {other:?}")),
        }?;
        stream.play().map_err(|e| e.to_string())?;
        Ok(stream)
    })();
    match result {
        Ok(stream) => {
            side.open = true;
            (side, Some(stream))
        }
        Err(e) => {
            side.error = Some(e);
            (side, None)
        }
    }
}

fn build_input<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    bus: &Arc<Bus>,
    counters: &Arc<Counters>,
    meter: &Arc<Meter>,
    last: &Arc<Mutex<Option<String>>>,
) -> Result<cpal::Stream, String>
where
    T: SizedSample,
    f32: FromSample<T>,
{
    let ch = config.channels as usize;
    let (bus, counters, meter) = (Arc::clone(bus), Arc::clone(counters), Arc::clone(meter));
    let mut scratch: Vec<f32> = Vec::with_capacity(8192);
    let err = on_error(&counters, last);
    device
        .build_input_stream(
            config,
            move |data: &[T], _: &cpal::InputCallbackInfo| {
                counters.capture_callbacks.fetch_add(1, Ordering::Relaxed);
                scratch.clear();
                scratch.extend(data.iter().map(|s| s.to_sample::<f32>()));
                if ch <= MAX_CH {
                    meter.feed(&scratch);
                }
                let Ok(taps) = bus.taps.try_lock() else {
                    counters.tap_misses.fetch_add(1, Ordering::Relaxed);
                    return;
                };
                let mut frame = [0f32; MAX_CH];
                let burst = scratch.len() / ch.max(1);
                for tap in taps.iter() {
                    tap.burst.fetch_max(burst, Ordering::Relaxed);
                    let n = tap.channels.len().min(MAX_CH);
                    for card_frame in scratch.chunks_exact(ch) {
                        for (i, slot) in frame.iter_mut().enumerate().take(n) {
                            *slot = tap.channels.get(i).and_then(|c| card_frame.get(c)).copied().unwrap_or(0.0);
                        }
                        tap.ring.push(&frame[..n]);
                    }
                }
            },
            err,
            None,
        )
        .map_err(|e| e.to_string())
}

fn open_playback(
    host: &cpal::Host,
    s: &AudioSettings,
    rate: u32,
    bus: &Arc<Bus>,
    counters: &Arc<Counters>,
    meter: &Arc<Meter>,
    last: &Arc<Mutex<Option<String>>>,
) -> (Side, Option<cpal::Stream>) {
    let mut side = Side { requested: or_default(&s.playback_device), ..Side::default() };
    let result = (|| -> Result<cpal::Stream, String> {
        let device = find_device(host, &s.playback_device, false)?;
        side.device = device.name().ok();
        let (mut config, format) = pick_config(&device, rate, s.playback_channels, false)?;
        if s.period_frames > 0 {
            config.buffer_size = cpal::BufferSize::Fixed(s.period_frames);
        }
        side.rate = config.sample_rate.0;
        side.channels = config.channels as usize;
        side.format = format!("{format:?}");
        let stream = match format {
            SampleFormat::F32 => build_output::<f32>(&device, &config, bus, counters, meter, last),
            SampleFormat::I32 => build_output::<i32>(&device, &config, bus, counters, meter, last),
            SampleFormat::I16 => build_output::<i16>(&device, &config, bus, counters, meter, last),
            other => return Err(format!("sample format {other:?}")),
        }?;
        stream.play().map_err(|e| e.to_string())?;
        Ok(stream)
    })();
    match result {
        Ok(stream) => {
            side.open = true;
            (side, Some(stream))
        }
        Err(e) => {
            side.error = Some(e);
            (side, None)
        }
    }
}

fn build_output<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    bus: &Arc<Bus>,
    counters: &Arc<Counters>,
    meter: &Arc<Meter>,
    last: &Arc<Mutex<Option<String>>>,
) -> Result<cpal::Stream, String>
where
    T: SizedSample + FromSample<f32>,
{
    let ch = config.channels as usize;
    let out_rate = config.sample_rate.0 as usize;
    let (bus, counters, meter) = (Arc::clone(bus), Arc::clone(counters), Arc::clone(meter));
    let mut mix: Vec<f32> = Vec::with_capacity(8192);
    let mut tmp: Vec<f32> = Vec::with_capacity(8192);
    let err = on_error(&counters, last);
    device
        .build_output_stream(
            config,
            move |data: &mut [T], _: &cpal::OutputCallbackInfo| {
                counters.playback_callbacks.fetch_add(1, Ordering::Relaxed);
                let frames = data.len() / ch.max(1);
                mix.clear();
                mix.resize(data.len(), 0.0);
                if let Ok(feeds) = bus.feeds.try_lock() {
                    for feed in feeds.iter() {
                        let fch = feed.ring.channels();
                        tmp.clear();
                        tmp.resize(frames * fch, 0.0);
                        let Ok(mut reader) = feed.reader.try_lock() else { continue };
                        // One callback's worth, in stream frames, with half
                        // again for jitter: below that every callback underruns.
                        let in_rate = feed.stream_rate as usize;
                        let need = frames * in_rate / out_rate.max(1) * 3 / 2 + 64;
                        if need > reader.target() {
                            reader.raise_target(need);
                            feed.target_frames.store(need, Ordering::Relaxed);
                        }
                        reader.read(&feed.ring, &mut tmp, &feed.stats);
                        drop(reader);
                        feed.meter.feed(&tmp);
                        for sc in 0..feed.map.len().min(fch) {
                            let Some(oc) = feed.map.get(sc) else { continue };
                            if oc >= ch {
                                continue;
                            }
                            for f in 0..frames {
                                mix[f * ch + oc] += tmp[f * fch + sc];
                            }
                        }
                    }
                } else {
                    counters.feed_misses.fetch_add(1, Ordering::Relaxed);
                }
                if ch <= MAX_CH {
                    meter.feed(&mix);
                }
                for (o, m) in data.iter_mut().zip(mix.iter()) {
                    *o = T::from_sample(m.clamp(-1.0, 1.0));
                }
            },
            err,
            None,
        )
        .map_err(|e| e.to_string())
}

fn or_default(name: &str) -> String {
    if name.trim().is_empty() { "default".to_string() } else { name.to_string() }
}

#[derive(Debug, Clone, Serialize)]
pub struct DeviceInfo {
    pub name: String,
    /// What the jack is, for a person: `ALC897 Analog — HD-Audio Generic`,
    /// `HDMI 0 — HDA NVidia`. Empty for ALSA's virtual names (default, dmix…).
    pub label: String,
    pub max_channels: u16,
    pub rates: Vec<u32>,
    pub default_rate: Option<u32>,
    /// This plugin has it open right now. An open device is BUSY, so ALSA's
    /// enumeration skips it; the daemon adds it back with this flag set,
    /// rather than the page calling the device in use "not found".
    pub in_use: bool,
}

/// `hw:CARD=Generic_1,DEV=0` → `ALC897 Analog — HD-Audio Generic`, asked of
/// the card's control device (`/dev/snd/controlC*`). NOT from /proc/asound:
/// Docker masks /proc/asound in every unprivileged container, so a reader
/// there sees nothing — which is how this first shipped blank. A name with a
/// card but no device (`default:CARD=…`) gets the card alone.
pub fn friendly_name(name: &str, input: bool) -> String {
    let field = |key: &str| name.split([':', ',']).find_map(|p| p.strip_prefix(key)).map(str::to_string);
    let Some(card) = field("CARD=") else { return String::new() };
    let Ok(ctl) = alsa::Ctl::new(&format!("hw:{card}"), false) else { return card };
    let card_name = ctl
        .card_info()
        .ok()
        .and_then(|i| i.get_name().ok().map(str::to_string))
        .unwrap_or_else(|| card.clone());
    let dev = field("DEV=").and_then(|d| d.parse::<u32>().ok());
    // ALSA's `hdmi:` names count HDMI PORTS in DEV=, not PCM numbers.
    if name.starts_with("hdmi:") {
        return match dev {
            Some(d) => format!("HDMI {d} — {card_name}"),
            None => card_name,
        };
    }
    let direction = if input { alsa::Direction::Capture } else { alsa::Direction::Playback };
    let pcm = dev
        .and_then(|d| ctl.pcm_info(d, 0, direction).ok())
        .and_then(|i| i.get_name().ok().map(str::to_string));
    match pcm {
        Some(p) => format!("{p} — {card_name}"),
        None => card_name,
    }
}

/// What `/api/devices` lists. A device that will not describe itself (busy,
/// or ALSA's plugin names that only exist on paper) is skipped, not fatal.
pub fn list_devices() -> (Vec<DeviceInfo>, Vec<DeviceInfo>) {
    let host = cpal::default_host();
    let describe = |d: &cpal::Device, input: bool| -> Option<DeviceInfo> {
        let name = d.name().ok()?;
        let ranges: Vec<cpal::SupportedStreamConfigRange> = if input {
            d.supported_input_configs().ok()?.collect()
        } else {
            d.supported_output_configs().ok()?.collect()
        };
        if ranges.is_empty() {
            return None;
        }
        let rates = [44_100u32, 48_000, 88_200, 96_000]
            .into_iter()
            .filter(|r| ranges.iter().any(|x| (x.min_sample_rate().0..=x.max_sample_rate().0).contains(r)))
            .collect();
        let default = if input { d.default_input_config() } else { d.default_output_config() };
        Some(DeviceInfo {
            label: friendly_name(&name, input),
            in_use: false,
            name,
            max_channels: ranges.iter().map(|r| r.channels()).max().unwrap_or(0),
            rates,
            default_rate: default.ok().map(|c| c.sample_rate().0),
        })
    };
    let inputs = host
        .input_devices()
        .map(|it| it.filter_map(|d| describe(&d, true)).collect())
        .unwrap_or_default();
    let outputs = host
        .output_devices()
        .map(|it| it.filter_map(|d| describe(&d, false)).collect())
        .unwrap_or_default();
    (inputs, outputs)
}
