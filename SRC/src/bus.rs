// Part of the APK.audio project — http://APK.audio — made by Anthony Kuzub
// MIT Licence. Free, for everyone, for ever. Full text in LICENSE at the root.
//! The daemon on the bus: its agent report, its live picture, its command door.
//!
//! WHAT IS KEPT AND WHAT IS NOT, the PTP plugin's rule: the agent report
//! (`Agent/state|detail|interface` and their `incoming/` twins) is retained
//! because the contract says so; everything else — `Clock`, `Audio`,
//! `Stream/Source/<id>`, `Stream/Sink/<id>` — is QoS 0, not retained, restated
//! every 5 s. A stream is true only while the daemon is up saying so.
//!
//! COMMANDS arrive on `outgoing/command` (a one-shot leaf in the contract) and
//! go through the same `Daemon::apply` the web page uses. The outcome is
//! published, not retained, at `<topic>/CommandResult`.

use crate::daemon;
use apkaudio_contracts::agent_state::AgentReport;
use apkaudio_contracts::baremetal_io;
use rumqttc::{Client, Event, MqttOptions, Packet, QoS};
use serde_json::json;
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

    {
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

        if restate_at.elapsed() >= RESTATE {
            restate(&client, &d.status());
            restate_at = Instant::now();
        }
        std::thread::sleep(Duration::from_secs(1));
    }
}

fn restate(client: &Client, status: &serde_json::Value) {
    let say = |leaf: String, v: &serde_json::Value| {
        let _ = client.try_publish(format!("{TOPIC}/{leaf}"), QoS::AtMostOnce, false, v.to_string());
    };
    say("Clock".into(), &status["clock"]);
    say("Audio".into(), &status["audio"]);
    for (kind, list) in [("Source", &status["sources"]), ("Sink", &status["sinks"])] {
        for item in list.as_array().into_iter().flatten() {
            if let Some(id) = item["config"]["id"].as_u64() {
                say(format!("Stream/{kind}/{id}"), item);
            }
        }
    }
}
