//! Health o meter 1100/2000-series scales over their `CP210x` USB serial option.
//!
//! `protocol` decodes bytes into packets; `session` turns the once-per-second
//! packet stream into one observation per weigh-in. This file glues them to
//! the [`Driver`] interface.

pub mod protocol;
pub mod session;

use crate::driver::{DeviceSession, Driver, DriverOptions, Output, PortCandidate, SerialSettings};
use jiff::Timestamp;
use protocol::{Framer, parse_packet};
use session::{SessionConfig, Sessioner, components};
use std::time::{Duration, Instant};

/// Machine name of this driver.
pub const KIND: &str = "healthometer_scale";

/// Silicon Labs `CP210x` USB-to-UART bridge, the chip inside the scale's USB option.
pub const CP210X_VID: u16 = 0x10C4;
/// Product ID of the CP2102/CP2109 family used by Health o meter.
pub const CP210X_PID: u16 = 0xEA60;

/// The driver. Cheap to clone; one instance serves every attached scale.
#[derive(Debug, Clone)]
pub struct HealthometerScale {
    cfg: SessionConfig,
}

impl HealthometerScale {
    /// Build from CLI tunables.
    #[must_use]
    pub fn new(opts: &DriverOptions) -> Self {
        Self {
            cfg: SessionConfig {
                quiet_timeout: Duration::from_millis(opts.scale_quiet_ms),
                weight_tolerance: 0.15,
                min_weight_kg: opts.scale_min_weight_kg,
            },
        }
    }
}

impl Driver for HealthometerScale {
    fn kind(&self) -> &'static str {
        KIND
    }

    fn display_name(&self) -> &'static str {
        "Health o meter scale"
    }

    /// Any `CP210x` bridge is assumed to be the scale: it is the only device in
    /// the clinic using that chip, and a wrong guess only means the parser
    /// rejects every frame (this driver never writes to the port).
    fn matches(&self, port: &PortCandidate) -> bool {
        (port.vid == Some(CP210X_VID) && port.pid == Some(CP210X_PID))
            || port.describes("uart bridge")
            || port.describes("cp210")
    }

    fn serial_settings(&self) -> SerialSettings {
        SerialSettings::eight_n_one(protocol::BAUD)
    }

    fn open_session(&self) -> Box<dyn DeviceSession> {
        Box::new(ScaleSession {
            framer: Framer::new(),
            sessioner: Sessioner::new(self.cfg.clone()),
        })
    }
}

struct ScaleSession {
    framer: Framer,
    sessioner: Sessioner,
}

impl DeviceSession for ScaleSession {
    fn on_bytes(&mut self, bytes: &[u8], now: Instant, wall: Timestamp) -> Vec<Output> {
        let mut out = Vec::new();
        for frame in self.framer.push(bytes) {
            match parse_packet(&frame) {
                Ok(packet) => {
                    out.push(Output::Live {
                        subject_hint: packet.patient_id.clone(),
                        components: components(&packet),
                    });
                    if let Some(draft) = self.sessioner.push(packet, now, wall) {
                        out.push(Output::Complete(draft));
                    }
                }
                Err(e) => out.push(Output::Rejected(e.to_string())),
            }
        }
        out
    }

    fn on_tick(&mut self, now: Instant, wall: Timestamp) -> Vec<Output> {
        self.sessioner
            .tick(now, wall)
            .map(Output::Complete)
            .into_iter()
            .collect()
    }

    fn is_active(&self) -> bool {
        self.sessioner.is_open()
    }
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
    use crate::model::Value;

    fn candidate(vid: u16, pid: u16, product: Option<&str>) -> PortCandidate {
        PortCandidate {
            name: "COM9".to_owned(),
            vid: Some(vid),
            pid: Some(pid),
            serial_number: None,
            manufacturer: None,
            product: product.map(str::to_owned),
        }
    }

    #[test]
    fn matches_by_vid_pid_or_name_only() {
        let d = HealthometerScale::new(&DriverOptions::default());
        assert!(d.matches(&candidate(CP210X_VID, CP210X_PID, None)));
        assert!(d.matches(&candidate(
            0x1234,
            0x5678,
            Some("CP2102 USB to UART Bridge Controller")
        )));
        assert!(!d.matches(&candidate(0x067B, 0x2303, Some("Prolific USB-to-Serial"))));
    }

    #[test]
    fn a_full_weigh_in_yields_live_readings_then_one_observation() {
        let d = HealthometerScale::new(&DriverOptions::default());
        let mut s = d.open_session();
        let t0 = Instant::now();
        let wall = Timestamp::UNIX_EPOCH;
        let packet = b"\x1bR\x1bI0000000000\x1bW184.5\x1bH0.0\x1bB0.0\x1bT0.0\x1bNc\x1bE";

        let mut lives = 0;
        for i in 0..3u64 {
            let out = s.on_bytes(packet, t0 + Duration::from_secs(i), wall);
            assert!(matches!(out.as_slice(), [Output::Live { .. }]));
            lives += 1;
            assert!(s.is_active());
        }
        assert_eq!(lives, 3);
        assert!(s.on_tick(t0 + Duration::from_secs(3), wall).is_empty());

        let out = s.on_tick(t0 + Duration::from_secs(10), wall);
        let [Output::Complete(draft)] = out.as_slice() else {
            panic!("expected one completed draft, got {out:?}")
        };
        assert_eq!(draft.packets, 3);
        assert_eq!(draft.components.len(), 1, "no height/BMI entered");
        assert_eq!(draft.components[0].code, "29463-7");
        assert_eq!(draft.components[0].value, Value::Quantity(184.5));
        assert_eq!(draft.components[0].unit.as_deref(), Some("[lb_av]"));
        assert!(!s.is_active());
    }

    #[test]
    fn garbage_is_rejected_not_forwarded() {
        let d = HealthometerScale::new(&DriverOptions::default());
        let mut s = d.open_session();
        let out = s.on_bytes(b"hello\x1bE", Instant::now(), Timestamp::UNIX_EPOCH);
        assert!(matches!(out.as_slice(), [Output::Rejected(_)]));
    }
}
