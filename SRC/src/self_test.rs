// Part of the APK.audio project — http://APK.audio — made by Anthony Kuzub
// MIT Licence. Free, for everyone, for ever. Full text in LICENSE at the root.
//! aes67's self test — the codecs, the SDP both profiles write, the slot plan.
//!
//! Own-depth checks touch nothing outside the process. The two Device-depth
//! checks look at the host (sound cards, interfaces) and are `skipped` unless
//! the run asked for the hardware, by the contract rather than by this file.

use apkaudio_contracts::self_test::{Check, Depth, Report};

pub const MODULE: &str = "aes67";

const CONFIG_INI: &str = include_str!("../config.ini");

pub fn self_test(depth: Depth) -> Report {
    let mut report = Report::new(MODULE, depth);

    report.record(Check::from_result(
        "landing_spots_build",
        Depth::Own,
        "both inputs and all three readbacks address a legal topic",
        landing_spots(),
    ));
    report.record(Check::from_result(
        "config_section_still_names_this_module",
        Depth::Own,
        "config.ini declares this module under the name the bus knows it by",
        if CONFIG_INI.lines().any(|l| l.trim() == format!("[{MODULE}]")) {
            Ok(())
        } else {
            Err(format!("config.ini does not declare [{MODULE}]"))
        },
    ));
    report.record(Check::from_result(
        "first_boot_settings_validate",
        Depth::Own,
        "config.ini gives 16 senders and 16 receivers of 8 channels, all valid",
        {
            let s = crate::config::Settings::from_ini(CONFIG_INI);
            s.validate().and_then(|_| {
                if s.sources.len() == crate::config::MAX_SOURCES && s.sinks.len() == crate::config::MAX_SINKS {
                    Ok(())
                } else {
                    Err(format!("{} sources, {} sinks", s.sources.len(), s.sinks.len()))
                }
            })
        },
    ));
    report.record(Check::from_result("l24_round_trips", Depth::Own, "L24 keeps sign and resolution", {
        let x = [0.5f32, -0.25, -1.0];
        let mut b = [0u8; 9];
        crate::rtp::encode(crate::rtp::Encoding::L24, &x, &mut b);
        let mut y = [0f32; 3];
        crate::rtp::decode(crate::rtp::Encoding::L24, &b, &mut y);
        if x.iter().zip(y.iter()).all(|(a, b)| (a - b).abs() < 1e-6) { Ok(()) } else { Err(format!("{y:?}")) }
    }));
    for (name, source) in [
        ("aes67_sdp_round_trips", crate::config::Source::slot(0)),
        (
            "st2110_30_sdp_round_trips",
            crate::config::Source { profile: crate::sdp::Profile::St2110_30, ..crate::config::Source::slot(1) },
        ),
    ] {
        report.record(Check::from_result(name, Depth::Own, "a generated SDP parses back to the same stream", {
            let d = crate::tx::describe(&source, 48_000, "10.0.0.1".parse().unwrap(), "test", "ptp=IEEE1588-2008:traceable".into());
            match crate::sdp::parse(&crate::sdp::generate(&d)) {
                Ok(back) if back == d => Ok(()),
                Ok(back) => Err(format!("came back as {back:?}")),
                Err(e) => Err(e),
            }
        }));
    }
    report.record(Check::from_result("sap_round_trips", Depth::Own, "a SAP packet decodes to what was encoded", {
        let p = crate::sap::Packet { deletion: false, msg_id_hash: 9, origin: "10.0.0.1".parse().unwrap(), sdp: "v=0\r\n".into() };
        if crate::sap::decode(&crate::sap::encode(&p)).as_ref() == Some(&p) { Ok(()) } else { Err("SAP packet did not round-trip") }
    }));

    // Only LOOK at the host when the run asked for it: the contract would
    // file these as skipped anyway, but enumerating ALSA is itself a touch.
    if !depth.covers(Depth::Device) {
        report.record(Check::skipped("a_sound_card_is_visible", Depth::Device, "not asked for the hardware"));
        report.record(Check::skipped("a_network_interface_resolves", Depth::Device, "not asked for the hardware"));
        return report;
    }
    report.record(Check::from_result(
        "a_sound_card_is_visible",
        Depth::Device,
        "ALSA lists at least one capture and one playback device",
        {
            let (i, o) = crate::audio::list_devices();
            if !i.is_empty() && !o.is_empty() {
                Ok(())
            } else {
                Err(format!("{} capture, {} playback devices — is /dev/snd mapped?", i.len(), o.len()))
            }
        },
    ));
    report.record(Check::from_result(
        "a_network_interface_resolves",
        Depth::Device,
        "an up, multicast-capable interface exists for PTP and RTP",
        crate::net::resolve("").map(|_| ()).ok_or("no usable interface"),
    ));
    report
}

fn landing_spots() -> Result<(), String> {
    use apkaudio_contracts::self_test as st;
    st::engage_topic(MODULE).map_err(|e| format!("{e:?}"))?;
    st::run_topic(MODULE).map_err(|e| format!("{e:?}"))?;
    apkaudio_contracts::baremetal_io::module_incoming(MODULE, st::LEAF_RESULT).map_err(|e| format!("{e:?}"))?;
    apkaudio_contracts::baremetal_io::module_incoming(MODULE, st::LEAF_REPORT).map_err(|e| format!("{e:?}"))?;
    apkaudio_contracts::baremetal_io::module_outgoing(MODULE, "command").map_err(|e| format!("{e:?}"))?;
    Ok(())
}

pub fn self_test_json(depth_word: &str) -> String {
    apkaudio_contracts::self_test::run_as_json(MODULE, depth_word, self_test)
}

#[cfg(test)]
mod tests {
    use super::*;
    use apkaudio_contracts::self_test::{Outcome, Verdict};

    #[test]
    fn the_self_test_passes_against_this_build() {
        let report = self_test(Depth::Own);
        assert_eq!(report.verdict(), Verdict::Pass, "{}", report.headline());
    }

    #[test]
    fn a_self_depth_run_reaches_no_hardware() {
        for check in self_test(Depth::Own).checks() {
            if check.depth == Depth::Device {
                assert_eq!(check.outcome, Outcome::Skipped, "{} ran at self depth", check.name);
            }
        }
    }
}
