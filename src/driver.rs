//! The driver abstraction: how one kind of device is recognised, opened and decoded.
//!
//! Adding a device means implementing [`Driver`] (identification and serial
//! settings) and [`DeviceSession`] (a stateful decoder for one connection),
//! then listing it in [`registry`]. Nothing outside `drivers/` needs to know
//! a device's protocol; everything speaks [`Output`].
//!
//! Sessions are synchronous because they run on the blocking serial thread.
//! They must not sleep or block; timing comes in through `now`/`wall` so the
//! decoders stay deterministic under test.

use crate::model::Component;
use jiff::Timestamp;
use serialport::{DataBits, Parity, SerialPortInfo, SerialPortType, StopBits};
use std::sync::Arc;
use std::time::Instant;

/// A serial port as seen by enumeration, decoupled from `serialport`'s types
/// so matching logic is unit-testable without hardware.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortCandidate {
    /// `COM3`, `/dev/ttyUSB0`, ...
    pub name: String,
    pub vid: Option<u16>,
    pub pid: Option<u16>,
    pub serial_number: Option<String>,
    pub manufacturer: Option<String>,
    pub product: Option<String>,
}

impl PortCandidate {
    /// True when the OS exposed USB descriptors for this port.
    #[must_use]
    pub const fn has_usb_info(&self) -> bool {
        self.vid.is_some()
    }

    /// Case-insensitive substring test over product and manufacturer strings.
    #[must_use]
    pub fn describes(&self, needle: &str) -> bool {
        let needle = needle.to_ascii_lowercase();
        [&self.product, &self.manufacturer]
            .into_iter()
            .flatten()
            .any(|s| s.to_ascii_lowercase().contains(&needle))
    }
}

impl From<&SerialPortInfo> for PortCandidate {
    fn from(info: &SerialPortInfo) -> Self {
        let usb = match &info.port_type {
            SerialPortType::UsbPort(u) => Some(u),
            _ => None,
        };
        Self {
            name: info.port_name.clone(),
            vid: usb.map(|u| u.vid),
            pid: usb.map(|u| u.pid),
            serial_number: usb.and_then(|u| u.serial_number.clone()),
            manufacturer: usb.and_then(|u| u.manufacturer.clone()),
            product: usb.and_then(|u| u.product.clone()),
        }
    }
}

/// Line settings a driver needs for its device.
#[derive(Debug, Clone, Copy)]
pub struct SerialSettings {
    pub baud: u32,
    pub data_bits: DataBits,
    pub parity: Parity,
    pub stop_bits: StopBits,
}

impl SerialSettings {
    /// The near-universal default: N baud, 8 data bits, no parity, 1 stop bit.
    #[must_use]
    pub const fn eight_n_one(baud: u32) -> Self {
        Self {
            baud,
            data_bits: DataBits::Eight,
            parity: Parity::None,
            stop_bits: StopBits::One,
        }
    }
}

/// A completed result before the manager stamps identity onto it.
#[derive(Debug, Clone, PartialEq)]
pub struct ObservationDraft {
    pub captured_at: Timestamp,
    pub completed_at: Timestamp,
    pub subject_hint: Option<String>,
    pub components: Vec<Component>,
    pub flags: Vec<String>,
    pub packets: u32,
}

/// What a session hands back after consuming bytes or a clock tick.
#[derive(Debug, Clone, PartialEq)]
pub enum Output {
    /// A live, provisional value (the scale's once-per-second stream).
    Live {
        subject_hint: Option<String>,
        components: Vec<Component>,
    },
    /// A finished result.
    Complete(ObservationDraft),
    /// Bytes arrived that did not decode; logged, never forwarded.
    Rejected(String),
    /// Bytes to write to the device (request/response protocols; no driver sends yet).
    #[allow(dead_code)]
    Send(Vec<u8>),
}

/// One kind of device.
pub trait Driver: Send + Sync {
    /// Stable machine name used in events and CLI flags, e.g. `healthometer_scale`.
    fn kind(&self) -> &'static str;
    /// Human name, e.g. `Health o meter scale`.
    fn display_name(&self) -> &'static str;
    /// Does this port look like our device? Only called when USB descriptors are available.
    fn matches(&self, port: &PortCandidate) -> bool;
    fn serial_settings(&self) -> SerialSettings;
    /// A fresh decoder for a newly opened port.
    fn open_session(&self) -> Box<dyn DeviceSession>;
}

/// Stateful decoder for one open connection.
pub trait DeviceSession: Send {
    /// Called once after the port opens; return `Output::Send` to kick off a request/response device.
    fn on_connect(&mut self) -> Vec<Output> {
        Vec::new()
    }
    /// Bytes arrived.
    fn on_bytes(&mut self, bytes: &[u8], now: Instant, wall: Timestamp) -> Vec<Output>;
    /// Called roughly twice a second whether or not bytes arrived, for timeouts.
    fn on_tick(&mut self, now: Instant, wall: Timestamp) -> Vec<Output>;
    /// The device is mid-result (someone is on the scale, a strip is being read).
    fn is_active(&self) -> bool {
        false
    }
}

/// Tunables for the built-in drivers, filled from the CLI.
#[derive(Debug, Clone)]
pub struct DriverOptions {
    pub scale_quiet_ms: u64,
    pub scale_min_weight_kg: f64,
}

impl Default for DriverOptions {
    fn default() -> Self {
        Self {
            scale_quiet_ms: 2500,
            scale_min_weight_kg: 1.0,
        }
    }
}

/// Every driver this build knows about.
#[must_use]
pub fn registry(opts: &DriverOptions) -> Vec<Arc<dyn Driver>> {
    vec![Arc::new(
        crate::drivers::healthometer::HealthometerScale::new(opts),
    )]
}

/// Look a driver up by its `kind`.
#[must_use]
pub fn find_driver<'a>(registry: &'a [Arc<dyn Driver>], kind: &str) -> Option<&'a Arc<dyn Driver>> {
    registry
        .iter()
        .find(|d| d.kind().eq_ignore_ascii_case(kind))
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

    #[test]
    fn describes_is_case_insensitive_over_both_strings() {
        let p = PortCandidate {
            name: "COM3".to_owned(),
            vid: Some(1),
            pid: Some(2),
            serial_number: None,
            manufacturer: Some("Silicon Labs".to_owned()),
            product: Some("CP210x USB to UART Bridge".to_owned()),
        };
        assert!(p.describes("uart BRIDGE"));
        assert!(p.describes("silicon"));
        assert!(!p.describes("prolific"));
        assert!(p.has_usb_info());
    }

    #[test]
    fn registry_contains_the_scale_and_lookup_is_case_insensitive() {
        let reg = registry(&DriverOptions::default());
        assert!(find_driver(&reg, "HEALTHOMETER_SCALE").is_some());
        assert!(find_driver(&reg, "nope").is_none());
    }
}
