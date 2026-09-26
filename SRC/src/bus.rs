// Part of the APK.audio project — http://APK.audio — made by Anthony Kuzub
// MIT Licence. Free, for everyone, for ever. Full text in LICENSE at the root.
//! The daemon on the bus: its agent report, its live picture, its command door.
//!
//! WHAT IS PUBLISHED, AND WHEN:
//!   `Agent/state|detail|interface` (+ `incoming/` twins) — retained, as the
//!     contract says; re-sent on change and once a minute.
//!   `Stream/Source/<id>`, `Stream/Sink/<id>` — retained, and ONLY WHEN THEY
//!     CHANGE. Configuration and state, never a counter: no packets, levels,
//!     buffer, drift or jitter — those change every second and would make
//!     every stream a firehose (32 of them). The live numbers are on the web
//!     page and `GET /api/status`. A stream that is removed is cleared (an
//!     empty retained payload); a reconnect re-sends them all, since the
//!     broker may have lost its store.
//!   `Clock`, `Audio` — QoS 0, not retained, restated every 5 s.
//!
//! COMMANDS arrive on `outgoing/command` (a one-shot leaf in the contract) and
//! go through the same `Daemon::apply` the web page uses. The outcome is
//! published, not retained, at `<topic>/CommandResult`.

use crate::daemon;
use apkaudio_contracts::agent_state::AgentReport;
use apkaudio_contracts::baremetal_io;
use rumqttc::{Client, Event, MqttOptions, Packet, QoS};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

pub const MODULE: &str = "aes67";
const TOPIC: &str = "APK.audio/System/Protocols/aes67";
const RESTATE: Duration = Duration::from_secs(5);
/// The agent report is retained, so it is re-sent only when it changes — and
/// once a minute anyway, in case the broker lost its store.
const REPORT_REFRESH: Duration = Duration::from_secs(60);

/// The daemon's own agent: keeps it maintained and tells the bus about it.
/// Never returns — the runner treats a return as a failure.
pub fn run_daemon_agent(host: &str, port: u16) {
    let d = daemon::global();
    let mut options = MqttOptions::new(format!("apk-aes67-daemon-{}", std::process::id()), host, port);
    options.set_keep_alive(Duration::from_secs(30));
    if let Some((u, p)) = apkaudio_contracts::broker_auth::credentials() {
        options.set_credentials(u, p);
    }
    let (client, mut connection) = Client::new(options, 256);
    let command_topic = baremetal_io::module_outgoing(MODULE, "command").map(|(t, _)| t).ok();
    // Set on every (re)connect: the retained stream documents go out again.
    let resend = Arc::new(AtomicBool::new(true));

    {
        let resend = Arc::clone(&resend);
        let client = client.clone();
        let d = std::sync::Arc::clone(&d);
        let command_topic = command_topic.clone();
        std::thread::Builder::new()
            .name("aes67:mqtt".into())
            .spawn(move || {
                let mut backoff = apkaudio_contracts::broker_backoff::Backoff::new();
                for event in connection.iter() {
                    let Some(event) = apkaudio_contracts::broker_backoff::step(MODULE, &mut backoff, event, |e| {
                        matches!(e, rumqttc::ConnectionError::ConnectionRefused(_))
                    }) else {
                        continue;
                    };
                    match event {
                        Event::Incoming(Packet::ConnAck(_)) => {
                            resend.store(true, Ordering::Relaxed);
                            if let Some(t) = &command_topic {
                                let _ = client.try_subscribe(t.clone(), QoS::AtLeastOnce);
                            }
                        }
                        Event::Incoming(Packet::Publish(p)) if Some(&p.topic) == command_topic.as_ref() => {
                            // A retained command would replay on every reconnect.
                            if p.retain {
                                continue;
                            }
                            let text = String::from_utf8_lossy(&p.payload).to_string();
                            let outcome = d.command(&text);
                            d.log(format!("bus command → {outcome:?}"));
                            let body = match outcome {
                                Ok(ok) => json!({"ok": true, "result": ok}),
                                Err(e) => json!({"ok": false, "error": e}),
                            };
                            let _ = client.try_publish(format!("{TOPIC}/CommandResult"), QoS::AtMostOnce, false, body.to_string());
                        }
                        _ => {}
                    }
                }
            })
            .expect("mqtt thread");
    }

    let mut last_report: Option<(String, String)> = None;
    let mut report_at = Instant::now();
    let mut restate_at = Instant::now();
    let mut streams: BTreeMap<String, String> = BTreeMap::new();
    loop {
        d.maintain();
        apk_plugin_runner::tick();

        let (state, detail, interfaces) = d.health();
        let key = (state.as_str().to_string(), detail.clone());
        if last_report.as_ref() != Some(&key) || report_at.elapsed() >= REPORT_REFRESH {
            if let Ok(report) = AgentReport::new(MODULE, state, &detail) {
                for (t, payload, retain) in report.on(interfaces).publications().unwrap_or_default() {
                    let _ = client.try_publish(t, QoS::AtLeastOnce, retain, payload);
                }
            }
            last_report = Some(key);
            report_at = Instant::now();
        }

        let status = d.status();
        if resend.swap(false, Ordering::Relaxed) {
            streams.clear();
        }
        publish_changed_streams(&client, &status, &mut streams);
        if restate_at.elapsed() >= RESTATE {
            restate(&client, &status);
            restate_at = Instant::now();
        }
        std::thread::sleep(Duration::from_secs(1));
    }
}

fn restate(client: &Client, status: &Value) {
    let say = |leaf: &str, v: &Value| {
        let _ = client.try_publish(format!("{TOPIC}/{leaf}"), QoS::AtMostOnce, false, v.to_string());
    };
    say("Clock", &status["clock"]);
    say("Audio", &status["audio"]);
}

/// A stream as the bus carries it: what it is and what state it is in —
/// nothing that ticks. Changes when somebody edits it, when it starts or
/// stops, when a receiver finds or loses its sender, or when an error appears
/// or clears.
pub fn stream_document(kind: &str, item: &Value) -> Value {
    let live = &item["live"];
    let running = !live.is_null();
    let state = if !item["config"]["enabled"].as_bool().unwrap_or(false) {
        "disabled"
    } else if !running {
        if item["error"].is_null() { "starting" } else { "waiting" }
    } else {
        live["state"].as_str().unwrap_or("unknown")
    };
    let error = if live["error"].is_null() { item["error"].clone() } else { live["error"].clone() };
    let mut doc = json!({
        "config": item["config"],
        "state": state,
        "error": error,
    });
    if kind == "Source" {
        doc["sdp"] = live["sdp"].clone();
        doc["conformance_level"] = live["conformance_level"].clone();
    } else {
        doc["stream"] = live["stream"].clone();
        doc["sender"] = live["sender"].clone();
        doc["ssrc"] = live["ssrc"].clone();
    }
    doc
}

/// Publish (retained) each stream document that differs from what was last
/// sent; clear the topics of streams that no longer exist.
fn publish_changed_streams(client: &Client, status: &Value, sent: &mut BTreeMap<String, String>) {
    let mut now: BTreeMap<String, String> = BTreeMap::new();
    for (kind, list) in [("Source", &status["sources"]), ("Sink", &status["sinks"])] {
        for item in list.as_array().into_iter().flatten() {
            if let Some(id) = item["config"]["id"].as_u64() {
                now.insert(format!("{TOPIC}/Stream/{kind}/{id}"), stream_document(kind, item).to_string());
            }
        }
    }
    for (topic, payload) in &now {
        if sent.get(topic) != Some(payload) {
            let _ = client.try_publish(topic.clone(), QoS::AtLeastOnce, true, payload.clone());
        }
    }
    for topic in sent.keys().filter(|t| !now.contains_key(*t)) {
        // An empty retained payload is how MQTT deletes a retained message.
        let _ = client.try_publish(topic.clone(), QoS::AtLeastOnce, true, Vec::<u8>::new());
    }
    *sent = now;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stream_document_carries_no_counters() {
        let item = json!({
            "config": {"id": 1, "enabled": true, "name": "TX 01"},
            "live": {"state": "sending", "packets": 123, "bytes": 9, "level_dbfs": [-3.0],
                     "buffer_ms": 20.1, "trim_ppm": 4.2, "sdp": "v=0", "conformance_level": "A"},
        });
        let doc = stream_document("Source", &item);
        assert_eq!(doc["state"], "sending");
        for counter in ["packets", "bytes", "level_dbfs", "buffer_ms", "trim_ppm"] {
            assert!(doc.get(counter).is_none() && doc["live"].is_null(), "{counter} leaked into {doc}");
        }
        // And two samples that differ only in counters are the same document.
        let mut later = item.clone();
        later["live"]["packets"] = json!(99_999);
        later["live"]["level_dbfs"] = json!([-60.0]);
        assert_eq!(stream_document("Source", &later), doc);
    }
}
