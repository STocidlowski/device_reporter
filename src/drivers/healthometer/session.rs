//! Turns the once-per-second packet stream into one observation per weigh-in.
//!
//! The scale locks a weight when someone stands still and then repeats the
//! same packet every second until they step off. Downstream (the web page,
//! the EMR's pending-vitals queue) wants a single event per person, not
//! thirty identical ones, so a *session* opens on the first packet, absorbs
//! every packet reporting the same locked weight, and closes into an
//! [`ObservationDraft`] once the stream has been quiet for `quiet_timeout`.
//!
//! A packet with a different weight (or different units) closes the current
//! session immediately and opens a new one, so a child hopping on right after
//! a parent yields two observations rather than one blended value.
//!
//! No clocks or I/O here: the caller passes `Instant`s for timing and a
//! wall-clock `Timestamp` for the record, which keeps it deterministic under test.

use super::protocol::Packet;
use crate::driver::ObservationDraft;
use crate::model::Component;
use jiff::Timestamp;
use std::time::{Duration, Instant};

/// LOINC 29463-7.
pub const LOINC_BODY_WEIGHT: &str = "29463-7";
/// LOINC 8302-2.
pub const LOINC_BODY_HEIGHT: &str = "8302-2";
/// LOINC 39156-5.
pub const LOINC_BMI: &str = "39156-5";

/// Below the configured minimum weight: likely a bag, a foot, or a toddler.
pub const FLAG_BELOW_MINIMUM: &str = "below_minimum";
/// Only one packet was seen. RECALL and UNITS resend the previous weight
/// exactly once, so this may be a re-display rather than a new weigh-in.
pub const FLAG_SINGLE_PACKET: &str = "single_packet";

/// Tuning for session detection.
#[derive(Debug, Clone)]
pub struct SessionConfig {
    /// How long without packets before an open session is finalised.
    /// Packets arrive every ~1 s, so 2.5 s is one missed packet plus slack.
    pub quiet_timeout: Duration,
    /// Weights within this many display units of each other count as the same reading.
    pub weight_tolerance: f64,
    /// Weights below this (in kilograms) are flagged, not dropped; the clinician decides.
    pub min_weight_kg: f64,
}

/// The FHIR-shaped components of one packet: weight, plus height and BMI when entered.
#[must_use]
pub fn components(p: &Packet) -> Vec<Component> {
    let mut c = vec![Component::quantity(
        LOINC_BODY_WEIGHT,
        "Body weight",
        p.weight,
        p.units.weight_ucum(),
    )];
    if let Some(h) = p.height {
        c.push(Component::quantity(
            LOINC_BODY_HEIGHT,
            "Body height",
            h,
            p.units.height_ucum(),
        ));
    }
    if let Some(b) = p.bmi {
        c.push(Component::quantity(
            LOINC_BMI,
            "Body mass index",
            b,
            "kg/m2",
        ));
    }
    c
}

#[derive(Debug)]
struct OpenSession {
    latest: Packet,
    captured_at: Timestamp,
    last_seen: Instant,
    packets: u32,
}

/// Stateful session detector. Feed it packets and ticks; it hands back drafts.
#[derive(Debug)]
pub struct Sessioner {
    cfg: SessionConfig,
    open: Option<OpenSession>,
}

impl Sessioner {
    /// A detector with no open session.
    #[must_use]
    pub const fn new(cfg: SessionConfig) -> Self {
        Self { cfg, open: None }
    }

    /// Whether a weigh-in is in progress.
    #[must_use]
    pub const fn is_open(&self) -> bool {
        self.open.is_some()
    }

    /// Record a packet. Returns a finished draft when this packet's weight
    /// differs from the open session's, closing that session.
    pub fn push(
        &mut self,
        packet: Packet,
        now: Instant,
        wall: Timestamp,
    ) -> Option<ObservationDraft> {
        let mut finished = None;
        if let Some(open) = &self.open
            && !same_reading(&open.latest, &packet, self.cfg.weight_tolerance)
        {
            finished = self.open.take().map(|o| self.finish(o, wall));
        }

        match &mut self.open {
            Some(open) => {
                // Height/BMI may appear mid-session when the BMI button is used;
                // the newest packet carries the fullest picture.
                open.latest = packet;
                open.last_seen = now;
                open.packets = open.packets.saturating_add(1);
            }
            None => {
                self.open = Some(OpenSession {
                    latest: packet,
                    captured_at: wall,
                    last_seen: now,
                    packets: 1,
                });
            }
        }
        finished
    }

    /// Call periodically. Closes the open session once the stream has gone quiet.
    pub fn tick(&mut self, now: Instant, wall: Timestamp) -> Option<ObservationDraft> {
        let quiet = self
            .open
            .as_ref()
            .is_some_and(|o| now.duration_since(o.last_seen) >= self.cfg.quiet_timeout);
        if quiet {
            self.open.take().map(|o| self.finish(o, wall))
        } else {
            None
        }
    }

    fn finish(&self, open: OpenSession, completed_at: Timestamp) -> ObservationDraft {
        let p = open.latest;
        let mut flags = Vec::new();
        if p.units.weight_to_kg(p.weight) < self.cfg.min_weight_kg {
            flags.push(FLAG_BELOW_MINIMUM.to_owned());
        }
        if open.packets == 1 {
            flags.push(FLAG_SINGLE_PACKET.to_owned());
        }
        ObservationDraft {
            captured_at: open.captured_at,
            completed_at,
            subject_hint: p.patient_id.clone(),
            components: components(&p),
            flags,
            packets: open.packets,
        }
    }
}

fn same_reading(a: &Packet, b: &Packet, tolerance: f64) -> bool {
    a.units == b.units && (a.weight - b.weight).abs() <= tolerance
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::assert_is_empty,
    clippy::similar_names
)]
mod tests {
    use super::*;
    use crate::drivers::healthometer::protocol::UnitSystem;
    use crate::model::Value;

    fn cfg() -> SessionConfig {
        SessionConfig {
            quiet_timeout: Duration::from_millis(2500),
            weight_tolerance: 0.15,
            min_weight_kg: 1.0,
        }
    }

    fn packet(weight: f64, units: UnitSystem) -> Packet {
        Packet {
            patient_id: None,
            weight,
            height: None,
            bmi: None,
            tare: None,
            units,
        }
    }

    fn wall() -> Timestamp {
        Timestamp::UNIX_EPOCH
    }

    fn weight_of(d: &ObservationDraft) -> f64 {
        match d.components.first().map(|c| &c.value) {
            Some(Value::Quantity(w)) => *w,
            other => panic!("expected a weight quantity, got {other:?}"),
        }
    }

    #[test]
    fn repeated_packets_collapse_into_one_observation() {
        let mut s = Sessioner::new(cfg());
        let t0 = Instant::now();
        for i in 0..10u64 {
            let at = t0 + Duration::from_secs(i);
            assert!(
                s.push(packet(184.5, UnitSystem::Imperial), at, wall())
                    .is_none()
            );
            assert!(s.tick(at, wall()).is_none());
        }
        assert!(
            s.tick(t0 + Duration::from_secs(10), wall()).is_none(),
            "still open 1 s after last packet"
        );
        let d = s
            .tick(t0 + Duration::from_millis(9000 + 2500), wall())
            .unwrap();
        assert_eq!(weight_of(&d), 184.5);
        assert_eq!(d.components[0].unit.as_deref(), Some("[lb_av]"));
        assert_eq!(d.packets, 10);
        assert!(d.flags.is_empty());
        assert!(!s.is_open());
    }

    #[test]
    fn a_different_weight_closes_the_session_immediately() {
        let mut s = Sessioner::new(cfg());
        let t0 = Instant::now();
        for i in 0..5u64 {
            s.push(
                packet(184.5, UnitSystem::Imperial),
                t0 + Duration::from_secs(i),
                wall(),
            );
        }
        // Parent steps off, child steps on within the quiet window.
        let d = s
            .push(
                packet(45.0, UnitSystem::Imperial),
                t0 + Duration::from_secs(6),
                wall(),
            )
            .unwrap();
        assert_eq!(weight_of(&d), 184.5);
        assert_eq!(d.packets, 5);
        assert!(s.is_open());
        let d2 = s.tick(t0 + Duration::from_secs(20), wall()).unwrap();
        assert_eq!(weight_of(&d2), 45.0);
        assert_eq!(d2.flags, vec![FLAG_SINGLE_PACKET]);
    }

    #[test]
    fn a_units_change_is_a_new_session() {
        let mut s = Sessioner::new(cfg());
        let t0 = Instant::now();
        s.push(packet(100.0, UnitSystem::Imperial), t0, wall());
        let d = s
            .push(
                packet(45.4, UnitSystem::Metric),
                t0 + Duration::from_secs(1),
                wall(),
            )
            .unwrap();
        assert_eq!(d.components[0].unit.as_deref(), Some("[lb_av]"));
        assert!(s.is_open());
    }

    #[test]
    fn bmi_entered_mid_session_is_kept() {
        let mut s = Sessioner::new(cfg());
        let t0 = Instant::now();
        s.push(packet(184.5, UnitSystem::Imperial), t0, wall());
        let mut with_bmi = packet(184.5, UnitSystem::Imperial);
        with_bmi.height = Some(70.0);
        with_bmi.bmi = Some(26.5);
        assert!(
            s.push(with_bmi, t0 + Duration::from_secs(1), wall())
                .is_none()
        );
        let d = s.tick(t0 + Duration::from_secs(10), wall()).unwrap();
        let codes: Vec<&str> = d.components.iter().map(|c| c.code.as_str()).collect();
        assert_eq!(codes, vec![LOINC_BODY_WEIGHT, LOINC_BODY_HEIGHT, LOINC_BMI]);
        assert_eq!(d.components[1].unit.as_deref(), Some("[in_i]"));
        assert_eq!(d.packets, 2);
    }

    #[test]
    fn tiny_weights_are_flagged_not_dropped() {
        let mut s = Sessioner::new(cfg());
        let t0 = Instant::now();
        s.push(packet(1.5, UnitSystem::Imperial), t0, wall());
        s.push(
            packet(1.5, UnitSystem::Imperial),
            t0 + Duration::from_secs(1),
            wall(),
        );
        let d = s.tick(t0 + Duration::from_secs(10), wall()).unwrap();
        assert_eq!(d.flags, vec![FLAG_BELOW_MINIMUM]);
    }

    #[test]
    fn small_jitter_within_tolerance_stays_in_session() {
        let mut s = Sessioner::new(cfg());
        let t0 = Instant::now();
        s.push(packet(184.5, UnitSystem::Imperial), t0, wall());
        assert!(
            s.push(
                packet(184.6, UnitSystem::Imperial),
                t0 + Duration::from_secs(1),
                wall()
            )
            .is_none()
        );
    }

    #[test]
    fn patient_id_becomes_subject_hint() {
        let mut s = Sessioner::new(cfg());
        let mut p = packet(70.0, UnitSystem::Metric);
        p.patient_id = Some("1234567890".to_owned());
        let t0 = Instant::now();
        s.push(p, t0, wall());
        let d = s.tick(t0 + Duration::from_secs(10), wall()).unwrap();
        assert_eq!(d.subject_hint.as_deref(), Some("1234567890"));
        assert_eq!(d.components[0].unit.as_deref(), Some("kg"));
    }
}
