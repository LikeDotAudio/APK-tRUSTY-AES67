// Part of the APK.audio project — http://APK.audio — made by Anthony Kuzub
// MIT Licence. Free, for everyone, for ever. Full text in LICENSE at the root.
//! SDP (RFC 4566) for AES67 and SMPTE ST 2110-30, both directions.
//!
//! The reader and writer are the shared AoIP SDP module, `apkaudio_ravenna::sdp`
//! (PLUGIN.RAVENNA, `default-features = false`): one implementation that this
//! bridge sends and receives with and that PLUGIN.RAVENNA, PLUGIN.SAP and
//! PLUGIN.DANTE file discovered streams with, so a fix to how a line is read
//! reaches all four. This module keeps the names the rest of the crate uses:
//! [`parse`] is the strict reader (`parse_stream` there) — first audio section
//! only, errors that name the line, shown to a person on /config — and
//! [`generate`] its inverse. A receiver here accepts AES67, ST 2110-30, and
//! plain RFC 3190 SDP without the clock lines, which it plays with adaptive
//! timing.
pub use apkaudio_ravenna::sdp::{Profile, StreamDesc, channel_order, conformance_level, generate, parse_stream as parse};

#[cfg(test)]
mod tests {
    use super::*;

    /// The shared corpus, run against the rev this crate pins: every playable
    /// announcement in it is one this receiver accepts, every refusal names why.
    #[test]
    fn the_pinned_reader_still_reads_the_corpus() {
        for case in apkaudio_ravenna::sdp::corpus() {
            match (parse(case.sdp), case.stream) {
                (Ok(d), Ok((rate, ch, ptime))) => assert_eq!((d.rate, d.channels, d.ptime_us), (rate, ch, ptime), "{}", case.name),
                (Err(e), Err(start)) => assert!(e.contains(start), "{}: {e}", case.name),
                (got, want) => panic!("{}: {got:?} vs corpus {want:?}", case.name),
            }
        }
    }

    /// What this bridge announces, the profile and level survive the trip.
    #[test]
    fn a_2110_sender_round_trips_with_its_level() {
        let case = apkaudio_ravenna::sdp::corpus().into_iter().find(|c| c.name == "st2110-30-level-b.sdp").unwrap();
        let d = parse(case.sdp).unwrap();
        assert_eq!(d.profile, Profile::St2110_30);
        assert_eq!(conformance_level(d.rate, d.ptime_us, d.channels), Some("B"));
        assert_eq!(parse(&generate(&d)).unwrap(), d);
    }
}
