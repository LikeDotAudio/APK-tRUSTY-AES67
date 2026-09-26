// Part of the APK.audio project — http://APK.audio — made by Anthony Kuzub
// MIT Licence. Free, for everyone, for ever. Full text in LICENSE at the root.
//! The daemon: settings in, a running clock / card / streams out.
//!
//! One process-wide instance ([`global`]) shared by the two agents the runner
//! starts — the bus agent and the web agent — so a change made on /config and
//! one made over `outgoing/command` land in the same place.
//!
//! RECONCILE, DON'T RESTART. `apply` diffs the new settings against the old
//! and touches only what changed: a renamed source restarts that source; a new
//! interface restarts the clock and every stream on it; a different card
//! restarts the card and every stream through it. Receivers that `follow` a
//! SAP session are re-checked every second by `maintain` and re-tuned when the
//! sender's SDP moves.

use crate::audio::{self, Bus, Engine};
use crate::clock::Clock;
use crate::config::{Settings, Sink, Source};
use crate::net::Interface;
use crate::rx::Receiver;
use crate::sap::{self, Browser};
use crate::tx::Transmitter;
use serde_json::{Value, json};
use std::collections::{BTreeMap, VecDeque};
use std::net::UdpSocket;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const EVENT_LOG: usize = 200;

struct Inner {
    settings: Settings,
    iface: Option<Interface>,
    clock: Arc<Clock>,
    engine: Option<Engine>,
    txs: BTreeMap<u32, Transmitter>,
    /// Receivers and the SDP each was started from.
    rxs: BTreeMap<u32, Receiver>,
    rx_errors: BTreeMap<u32, String>,
    sap_socket: Option<UdpSocket>,
    sap_stop: Arc<AtomicBool>,
    next_announce: Instant,
    /// SDPs announced last round, so a removed source gets its deletion.
    announced: BTreeMap<u32, String>,
}

pub struct Daemon {
    inner: Mutex<Inner>,
    pub bus: Arc<Bus>,
    pub browser: Arc<Browser>,
    events: Mutex<VecDeque<(u64, String)>>,
    started: Instant,
    load_error: Option<String>,
}

static DAEMON: OnceLock<Arc<Daemon>> = OnceLock::new();

/// The one daemon, started on first use.
pub fn global() -> Arc<Daemon> {
    Arc::clone(DAEMON.get_or_init(|| {
        let (settings, load_error) = Settings::load();
        let d = Arc::new(Daemon {
            inner: Mutex::new(Inner {
                settings: Settings { sources: vec![], sinks: vec![], ..settings.clone() },
                iface: None,
                clock: Clock::start(crate::clock::Source::System, 0, None),
                engine: None,
                txs: BTreeMap::new(),
                rxs: BTreeMap::new(),
                rx_errors: BTreeMap::new(),
                sap_socket: None,
                sap_stop: Arc::new(AtomicBool::new(true)),
                next_announce: Instant::now(),
                announced: BTreeMap::new(),
            }),
            bus: Arc::new(Bus::default()),
            browser: Arc::new(Browser::default()),
            events: Mutex::new(VecDeque::new()),
            started: Instant::now(),
            load_error: load_error.clone(),
        });
        if let Some(e) = &load_error {
            d.log(format!("saved settings refused, running on config.ini defaults: {e}"));
        }
        {
            let mut inner = d.lock();
            d.bring_up_network(&mut inner, &settings);
            d.bring_up_audio(&mut inner, &settings);
            inner.settings = settings.clone();
            d.reconcile_streams(&mut inner, &settings, true);
        }
        d
    }))
}

fn unix_now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

impl Daemon {
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn log(&self, text: String) {
        println!("🎚️  [aes67] {text}");
        let mut ev = self.events.lock().unwrap_or_else(|e| e.into_inner());
        ev.push_back((unix_now(), text));
        while ev.len() > EVENT_LOG {
            ev.pop_front();
        }
    }

    pub fn settings(&self) -> Settings {
        self.lock().settings.clone()
    }

    /// Validate, save, and make it so. The error is for a person to read.
    pub fn apply(&self, new: Settings) -> Result<(), String> {
        new.validate()?;
        new.save()?;
        let mut inner = self.lock();
        let old = inner.settings.clone();
        let net_changed = old.interface != new.interface || old.clock != new.clock || old.sap != new.sap;
        let audio_changed = old.audio != new.audio || old.sample_rate != new.sample_rate;
        let node_changed = old.node_name != new.node_name;
        if net_changed || audio_changed {
            // Every stream rides on both, so all of them go first.
            self.stop_all_streams(&mut inner);
        }
        if net_changed {
            self.bring_up_network(&mut inner, &new);
        }
        if audio_changed {
            self.bring_up_audio(&mut inner, &new);
        }
        inner.settings = new.clone();
        self.reconcile_streams(&mut inner, &new, net_changed || audio_changed || node_changed);
        self.log(format!("settings applied — {} source(s), {} sink(s)", new.sources.len(), new.sinks.len()));
        Ok(())
    }

    fn stop_all_streams(&self, inner: &mut Inner) {
        inner.txs.clear();
        inner.rxs.clear();
    }

    fn bring_up_network(&self, inner: &mut Inner, s: &Settings) {
        inner.clock.stop();
        inner.sap_stop.store(true, Ordering::Relaxed);
        inner.iface = crate::net::resolve(&s.interface);
        match &inner.iface {
            Some(i) => self.log(format!("interface {} ({})", i.name, i.ipv4)),
            None => self.log(format!(
                "no usable interface{} — nothing can be sent or received",
                if s.interface.is_empty() { String::new() } else { format!(" named {:?}", s.interface) }
            )),
        }
        inner.clock = Clock::start(s.clock_source(), s.clock.domain, inner.iface.as_ref());
        self.log(format!("clock: {:?}, domain {}", s.clock_source(), s.clock.domain));

        inner.sap_socket = inner.iface.as_ref().and_then(|i| crate::net::sender(i.ipv4, s.sap.ttl, 34).ok());
        self.browser.set_timeout(s.sap.timeout_s);
        inner.next_announce = Instant::now() + Duration::from_secs(2);
        let stop = Arc::new(AtomicBool::new(false));
        inner.sap_stop = Arc::clone(&stop);
        if let (true, Some(i)) = (s.sap.listen, &inner.iface) {
            let (browser, ip, group, port) = (Arc::clone(&self.browser), i.ipv4, s.sap.group, s.sap.port);
            std::thread::Builder::new()
                .name("aes67:sap".into())
                .spawn(move || {
                    if let Err(e) = sap::listen(browser, ip, group, port, stop) {
                        eprintln!("⚠️  [aes67] SAP listener: {e}");
                    }
                })
                .ok();
        }
    }

    fn bring_up_audio(&self, inner: &mut Inner, s: &Settings) {
        inner.engine = None; // close the old card before opening the new one
        let engine = Engine::start(&s.audio, s.sample_rate, Arc::clone(&self.bus));
        for side in [&engine.capture, &engine.playback] {
            match &side.error {
                Some(e) => self.log(format!("ALSA {}: {e}", side.requested)),
                None if side.rate != s.sample_rate => self.log(format!(
                    "ALSA {} runs at {} Hz, not {} — resampling",
                    side.device.as_deref().unwrap_or("?"),
                    side.rate,
                    s.sample_rate
                )),
                None => {}
            }
        }
        inner.engine = Some(engine);
    }

    fn node_name(s: &Settings) -> String {
        if !s.node_name.trim().is_empty() {
            return s.node_name.trim().to_string();
        }
        std::fs::read_to_string("/proc/sys/kernel/hostname").map(|h| h.trim().to_string()).unwrap_or_default()
    }

    fn reconcile_streams(&self, inner: &mut Inner, s: &Settings, force: bool) {
        let Some(iface) = inner.iface.clone() else {
            inner.txs.clear();
            inner.rxs.clear();
            return;
        };
        let node = Self::node_name(s);
        let (cap_rate, play_rate) = inner
            .engine
            .as_ref()
            .map(|e| (e.capture.rate, e.playback.rate))
            .unwrap_or((0, 0));

        // Transmitters. A change of card channels alone is re-routed in place
        // (the MATRIX page); anything else about a sender restarts it.
        let wanted: BTreeMap<u32, &Source> = s.sources.iter().filter(|x| x.enabled).map(|x| (x.id, x)).collect();
        if !force {
            for (id, tx) in inner.txs.iter_mut() {
                if let Some(w) = wanted.get(id) {
                    if w.same_stream(&tx.cfg) && **w != tx.cfg {
                        // Announcement flags are not the stream: take them
                        // in place, like a re-route, with no restart.
                        tx.cfg.sap = w.sap;
                        tx.cfg.ravenna = w.ravenna;
                        tx.reroute(w);
                    }
                }
            }
        }
        inner.txs.retain(|id, tx| !force && wanted.get(id).is_some_and(|w| **w == tx.cfg));
        for (id, src) in wanted {
            if !inner.txs.contains_key(&id) {
                let tx = Transmitter::start(
                    src,
                    s.sample_rate,
                    cap_rate,
                    iface.ipv4,
                    &node,
                    Arc::clone(&inner.clock),
                    Arc::clone(&self.bus),
                );
                inner.txs.insert(id, tx);
            }
        }

        // Receivers — by config AND by the SDP they would tune to now.
        let wanted: Vec<&Sink> = s.sinks.iter().filter(|x| x.enabled).collect();
        let effective: BTreeMap<u32, Option<String>> =
            wanted.iter().map(|k| (k.id, self.effective_sdp(k))).collect();
        if !force {
            for (id, rx) in inner.rxs.iter_mut() {
                if let Some(w) = wanted.iter().find(|k| k.id == *id) {
                    if (Sink { map: rx.cfg.map.clone(), ..(*w).clone() }) == rx.cfg && w.map != rx.cfg.map {
                        rx.reroute(w);
                    }
                }
            }
        }
        inner.rxs.retain(|id, rx| {
            !force
                && wanted.iter().any(|k| k.id == *id && **k == rx.cfg)
                && effective.get(id).cloned().flatten().as_deref() == Some(rx.sdp.as_str())
        });
        inner.rx_errors.retain(|id, _| effective.contains_key(id));
        for sink in wanted {
            if inner.rxs.contains_key(&sink.id) {
                continue;
            }
            let Some(sdp_text) = effective.get(&sink.id).cloned().flatten() else {
                inner.rx_errors.insert(sink.id, format!("waiting for SAP session {:?}", sink.follow));
                continue;
            };
            match crate::sdp::parse(&sdp_text) {
                Ok(desc) => {
                    inner.rx_errors.remove(&sink.id);
                    let rx = Receiver::start(
                        sink,
                        &sdp_text,
                        desc,
                        play_rate,
                        iface.ipv4,
                        Arc::clone(&inner.clock),
                        Arc::clone(&self.bus),
                    );
                    inner.rxs.insert(sink.id, rx);
                }
                Err(e) => {
                    inner.rx_errors.insert(sink.id, format!("SDP: {e}"));
                }
            }
        }
    }

    fn effective_sdp(&self, k: &Sink) -> Option<String> {
        if !k.sdp.trim().is_empty() {
            return Some(k.sdp.clone());
        }
        self.browser.find(k.follow.trim())
    }

    /// Once a second from the bus agent: SAP announcements and followers.
    pub fn maintain(&self) {
        let mut inner = self.lock();
        let s = inner.settings.clone();
        if s.sinks.iter().any(|k| k.enabled && k.sdp.trim().is_empty()) {
            self.reconcile_streams(&mut inner, &s, false);
        }
        let origin = inner.iface.as_ref().map(|i| i.ipv4);
        let sap_to = std::net::SocketAddrV4::new(s.sap.group, s.sap.port);
        if let (Some(socket), Some(origin)) = (&inner.sap_socket, origin) {
            let current: BTreeMap<u32, String> = if s.sap.announce {
                inner.txs.iter().filter(|(_, tx)| tx.cfg.sap).map(|(id, tx)| (*id, tx.sdp.clone())).collect()
            } else {
                BTreeMap::new()
            };
            let gone: Vec<String> = inner
                .announced
                .iter()
                .filter(|(id, sdp)| current.get(id) != Some(*sdp))
                .map(|(_, sdp)| sdp.clone())
                .collect();
            if !gone.is_empty() {
                sap::announce(socket, origin, sap_to, &gone, true);
            }
            let changed = current != inner.announced;
            if changed || Instant::now() >= inner.next_announce {
                let sdps: Vec<String> = current.values().cloned().collect();
                sap::announce(socket, origin, sap_to, &sdps, false);
                inner.next_announce = Instant::now() + Duration::from_secs(s.sap.interval_s.clamp(5, 300) as u64);
            }
            inner.announced = current;
        }
        // RAVENNA: the running senders that ask for it, advertised over mDNS
        // and served over RTSP. Stopped senders are withdrawn by the same call.
        let ravenna: Vec<crate::ravenna::Session> = inner
            .txs
            .values()
            .filter(|tx| tx.cfg.ravenna)
            .filter_map(|tx| crate::ravenna::Session::from_sdp(tx.cfg.id, &tx.sdp))
            .collect();
        crate::ravenna::sync(origin, s.rtsp_port, &Self::node_name(&s), ravenna);
    }

    /// The SDP a sender IS, or — when it is not running as configured — the
    /// SDP it WOULD send with `src` exactly as given (the form's unsaved row,
    /// for `/config`'s SDP button). `live` says which. A preview carries this
    /// moment as its `o=` version; the running one keeps its start version.
    pub fn preview_sdp(&self, src: &Source) -> Result<(String, bool), String> {
        self.preview_sdp_with(src, None)
    }

    /// As [`preview_sdp`](Self::preview_sdp), with the node-wide settings the
    /// form holds (`pending`) — sample rate and node name — instead of the
    /// saved ones. The interface address is always the one in use.
    pub fn preview_sdp_with(&self, src: &Source, pending: Option<&Settings>) -> Result<(String, bool), String> {
        let inner = self.lock();
        let saved = inner.settings.clone();
        let s = pending.unwrap_or(&saved);
        src.validate(s.sample_rate).map_err(|e| format!("{}: {e}", src.name))?;
        let node_same = Self::node_name(s) == Self::node_name(&saved) && s.sample_rate == saved.sample_rate;
        if let Some(tx) = inner.txs.get(&src.id).filter(|tx| node_same && tx.cfg.same_stream(src)) {
            return Ok((tx.sdp.clone(), true));
        }
        let ip = inner
            .iface
            .as_ref()
            .map(|i| i.ipv4)
            .ok_or_else(|| "no usable network interface, so no origin address for the SDP".to_string())?;
        let desc = crate::tx::describe(src, s.sample_rate, ip, &Self::node_name(s), inner.clock.refclk());
        Ok((crate::sdp::generate(&desc), false))
    }

    /// Where a RAVENNA receiver would DESCRIBE `src`, if it were advertised.
    pub fn rtsp_url(&self, src: &Source) -> Option<String> {
        let inner = self.lock();
        let ip = inner.iface.as_ref()?.ipv4;
        let node = Self::node_name(&inner.settings);
        let session = if node.is_empty() { src.name.clone() } else { format!("{node} {}", src.name) };
        Some(format!("rtsp://{ip}:{}/by-name/{}", inner.settings.rtsp_port, crate::ravenna::url_encode(&session)))
    }

    /// The SDP of a running source — for `GET /api/sources/{id}/sdp`.
    /// Not running: the SDP its saved settings would produce.
    pub fn source_sdp(&self, id: u32) -> Option<String> {
        if let Some(sdp) = self.lock().txs.get(&id).map(|t| t.sdp.clone()) {
            return Some(sdp);
        }
        let src = self.settings().sources.into_iter().find(|x| x.id == id)?;
        self.preview_sdp(&src).ok().map(|(sdp, _)| sdp)
    }

    /// Is the agent seeing anything? For the contract's `Agent/state`.
    pub fn health(&self) -> (apkaudio_contracts::agent_state::AgentState, String, Vec<String>) {
        use apkaudio_contracts::agent_state::AgentState;
        let inner = self.lock();
        let Some(iface) = &inner.iface else {
            return (AgentState::Unavailable, "no usable network interface".into(), vec![]);
        };
        let clock = inner.clock.status();
        let (cap, play) = inner
            .engine
            .as_ref()
            .map(|e| (e.capture.open, e.playback.open))
            .unwrap_or((false, false));
        let mut missing = Vec::new();
        if clock.error.is_some() {
            missing.push("PTP (could not bind 319/320)".to_string());
        } else if !clock.locked {
            missing.push(format!("PTP {}", clock.state));
        }
        if !cap {
            missing.push("capture device".into());
        }
        if !play {
            missing.push("playback device".into());
        }
        let summary = format!(
            "{} source(s), {} sink(s) on {}; clock {}",
            inner.txs.len(),
            inner.rxs.len(),
            iface.name,
            clock.state
        );
        let state = match (missing.is_empty(), cap || play) {
            (true, _) => AgentState::Listening,
            (false, true) => AgentState::Partial,
            (false, false) => AgentState::Partial,
        };
        let detail = if missing.is_empty() { summary } else { format!("{summary}; missing: {}", missing.join(", ")) };
        (state, detail, vec![iface.name.clone()])
    }

    /// Everything the status page shows, in one document.
    pub fn status(&self) -> Value {
        let inner = self.lock();
        let s = &inner.settings;
        let rate = s.sample_rate;
        let sources: Vec<Value> = s
            .sources
            .iter()
            .map(|cfg| {
                let live = inner.txs.get(&cfg.id).map(|t| t.status(rate));
                json!({ "config": cfg, "live": live })
            })
            .collect();
        let sinks: Vec<Value> = s
            .sinks
            .iter()
            .map(|cfg| {
                let live = inner.rxs.get(&cfg.id).map(|r| r.status());
                json!({ "config": cfg, "live": live, "error": inner.rx_errors.get(&cfg.id) })
            })
            .collect();
        let local_ips: Vec<_> = inner.iface.iter().map(|i| i.ipv4).collect();
        json!({
            "node": {
                "name": Self::node_name(s),
                "interface": inner.iface,
                "sample_rate": rate,
                "uptime_s": self.started.elapsed().as_secs(),
                "version": env!("CARGO_PKG_VERSION"),
                "image_rev": std::env::var("APK_IMAGE_REV").ok(),
                "load_error": self.load_error,
                "settings_file": crate::config::state_path(),
            },
            "clock": inner.clock.status(),
            "audio": inner.engine.as_ref().map(Engine::status),
            "sources": sources,
            "sinks": sinks,
            "sap": {
                "announce": s.sap.announce,
                "listen": s.sap.listen,
                "listening": self.browser.listening.load(Ordering::Relaxed) > 0,
                "sessions": self.browser.list(&local_ips).len(),
                "group": s.sap.group,
                "port": s.sap.port,
            },
            "ravenna": crate::ravenna::status(),
            "events": self.events.lock().unwrap_or_else(|e| e.into_inner()).iter().rev().take(50)
                .map(|(t, text)| json!({"t": t, "text": text})).collect::<Vec<_>>(),
        })
    }

    pub fn browse(&self) -> Value {
        let local: Vec<_> = self.lock().iface.iter().map(|i| i.ipv4).collect();
        let list: Vec<Value> = self
            .browser
            .list(&local)
            .into_iter()
            .map(|r| {
                let parsed = crate::sdp::parse(&r.sdp);
                json!({
                    "name": r.name, "origin": r.origin, "sdp": r.sdp, "last_seen_s": r.last_seen_s,
                    "local": r.local,
                    "stream": parsed.as_ref().ok(),
                    "error": parsed.err(),
                })
            })
            .collect();
        json!(list)
    }

    pub fn devices(&self) -> Value {
        let (mut capture, mut playback) = audio::list_devices();
        // The devices this plugin holds are busy, so enumeration left them
        // out: put them back, marked, with what they opened as.
        if let Some(engine) = self.lock().engine.as_ref() {
            for (list, side, input) in [(&mut capture, &engine.capture, true), (&mut playback, &engine.playback, false)] {
                let Some(name) = side.device.clone().filter(|_| side.open) else { continue };
                match list.iter_mut().find(|d| d.name == name) {
                    Some(d) => d.in_use = true,
                    None => list.insert(0, audio::DeviceInfo {
                        label: audio::friendly_name(&name, input),
                        name,
                        max_channels: side.channels as u16,
                        rates: vec![side.rate],
                        default_rate: Some(side.rate),
                        in_use: true,
                    }),
                }
            }
        }
        json!({ "capture": capture, "playback": playback })
    }

    /// `outgoing/command`, the bus door to the same `apply`. JSON in:
    /// `{"op":"apply","settings":{…}}`, `{"op":"enable"|"disable","kind":"source"|"sink","id":N}`,
    /// `{"op":"reload"}` (re-read the settings file).
    pub fn command(&self, payload: &str) -> Result<String, String> {
        let v: Value = serde_json::from_str(payload).map_err(|e| format!("not JSON: {e}"))?;
        let op = v.get("op").and_then(Value::as_str).unwrap_or("");
        match op {
            "apply" => {
                let s: Settings = serde_json::from_value(v.get("settings").cloned().unwrap_or(Value::Null))
                    .map_err(|e| format!("settings: {e}"))?;
                self.apply(s).map(|_| "applied".into())
            }
            "reload" => {
                let (s, err) = Settings::load();
                if let Some(e) = err {
                    return Err(e);
                }
                self.apply(s).map(|_| "reloaded".into())
            }
            "enable" | "disable" => {
                let id = v.get("id").and_then(Value::as_u64).ok_or("no id")? as u32;
                let on = op == "enable";
                let mut s = self.settings();
                let found = match v.get("kind").and_then(Value::as_str) {
                    Some("source") => s.sources.iter_mut().find(|x| x.id == id).map(|x| x.enabled = on),
                    Some("sink") => s.sinks.iter_mut().find(|x| x.id == id).map(|x| x.enabled = on),
                    _ => return Err("kind must be source or sink".into()),
                };
                found.ok_or(format!("no such id {id}"))?;
                self.apply(s).map(|_| format!("{op}d {id}"))
            }
            other => Err(format!("unknown op {other:?}")),
        }
    }
}
