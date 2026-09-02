//! Device-agnostic data model and the JSON events every client sees.
//!
//! A device produces [`Observation`]s: one completed result with one or more
//! [`Component`]s (a scale gives weight, maybe height and BMI; a BP cuff gives
//! systolic, diastolic and pulse; a urinalysis strip reader gives ten
//! analytes). Components carry LOINC codes and UCUM units so the EMR can
//! build FHIR `Observation` resources without device-specific knowledge.

use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Bump when the JSON shape of any event changes incompatibly.
pub const WIRE_VERSION: u8 = 1;

/// A measured value: numeric with a unit, or a coded/text result such as a
/// urinalysis "trace" or "2+".
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Value {
    Quantity(f64),
    Text(String),
}

/// One named value inside an observation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Component {
    /// LOINC code, e.g. `29463-7` for body weight.
    pub code: String,
    /// Human label, e.g. `Body weight`.
    pub display: String,
    pub value: Value,
    /// UCUM unit for quantities, e.g. `kg`, `[lb_av]`, `mm[Hg]`. `None` for text values.
    pub unit: Option<String>,
}

impl Component {
    /// A numeric component.
    pub fn quantity(code: &str, display: &str, value: f64, unit: &str) -> Self {
        Self {
            code: code.to_owned(),
            display: display.to_owned(),
            value: Value::Quantity(value),
            unit: Some(unit.to_owned()),
        }
    }

    /// A text or coded component (for strip readers and the like; no driver uses it yet).
    #[allow(dead_code)]
    pub fn text(code: &str, display: &str, value: &str) -> Self {
        Self {
            code: code.to_owned(),
            display: display.to_owned(),
            value: Value::Text(value.to_owned()),
            unit: None,
        }
    }
}

/// One completed result from one device.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Observation {
    /// Random ID so a consumer can accept or discard exactly this result.
    pub id: Uuid,
    /// Stable identifier of the physical device (see [`DeviceInfo::id`]).
    pub device_id: String,
    /// Driver kind, e.g. `healthometer_scale`.
    pub device_kind: String,
    /// When the device first reported this result.
    pub captured_at: Timestamp,
    /// When the driver decided the result was complete.
    pub completed_at: Timestamp,
    /// Anything the device itself said about who this is, such as an ID typed
    /// on a scale keypad. A hint for the clinician, never an identity.
    pub subject_hint: Option<String>,
    pub components: Vec<Component>,
    /// Driver-specific plausibility flags, e.g. `below_minimum`, `single_packet`.
    pub flags: Vec<String>,
    /// How many device packets contributed; useful for judging a fleeting reading.
    pub packets: u32,
}

/// A live, not-yet-final value stream from a device (the scale sends one per second).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Reading {
    pub device_id: String,
    pub device_kind: String,
    pub at: Timestamp,
    pub subject_hint: Option<String>,
    pub components: Vec<Component>,
}

/// Identity of one attached device.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceInfo {
    /// USB serial number when the OS exposes one, else `{host}-{port}`.
    pub id: String,
    /// Driver kind, e.g. `healthometer_scale`.
    pub kind: String,
    /// Driver's human name, e.g. `Health o meter scale`.
    pub display_name: String,
    /// OS port name: `COM3`, `/dev/ttyUSB0`, `demo`.
    pub port: String,
}

/// Current health of one device, sent on connect and whenever it changes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DeviceStatus {
    #[serde(flatten)]
    pub info: DeviceInfo,
    pub connected: bool,
    /// Why the last disconnect happened, if any.
    pub last_error: Option<String>,
    pub last_data_at: Option<Timestamp>,
    /// The device is mid-result (for the scale: someone is on the platform).
    pub active: bool,
}

/// Health of the reporter process itself.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ServerStatus {
    pub host: String,
    pub version: String,
    pub started_at: Timestamp,
    pub devices: Vec<DeviceStatus>,
}

/// Everything that goes over the WebSocket, tagged by `type`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    /// Sent once on connect: the whole device list.
    Server(ServerStatus),
    /// One device changed state.
    Device(DeviceStatus),
    Reading(Reading),
    Observation(Observation),
}

#[derive(Serialize)]
struct Envelope<'a> {
    v: u8,
    #[serde(flatten)]
    event: &'a Event,
}

impl Event {
    /// JSON text with the wire version stamped in.
    #[must_use]
    pub fn to_json(&self) -> String {
        serde_json::to_string(&Envelope {
            v: WIRE_VERSION,
            event: self,
        })
        .unwrap_or_else(|e| {
            tracing::error!(error = %e, "event failed to serialise");
            format!(r#"{{"v":{WIRE_VERSION},"type":"error","message":"serialisation failed"}}"#)
        })
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

    #[test]
    fn values_serialise_untagged() {
        let q = Component::quantity("29463-7", "Body weight", 70.5, "kg");
        let t = Component::text(
            "5792-7",
            "Glucose [Presence] in Urine by Test strip",
            "negative",
        );
        let json = serde_json::to_value([&q, &t]).unwrap();
        assert_eq!(json[0]["value"], 70.5);
        assert_eq!(json[0]["unit"], "kg");
        assert_eq!(json[1]["value"], "negative");
        assert!(json[1]["unit"].is_null());
        let back: Vec<Component> = serde_json::from_value(json).unwrap();
        assert_eq!(back, vec![q, t]);
    }

    #[test]
    fn events_carry_type_and_version() {
        let ev = Event::Device(DeviceStatus {
            info: DeviceInfo {
                id: "abc".to_owned(),
                kind: "healthometer_scale".to_owned(),
                display_name: "Health o meter scale".to_owned(),
                port: "COM3".to_owned(),
            },
            connected: true,
            last_error: None,
            last_data_at: None,
            active: false,
        });
        let json: serde_json::Value = serde_json::from_str(&ev.to_json()).unwrap();
        assert_eq!(json["v"], 1);
        assert_eq!(json["type"], "device");
        assert_eq!(json["id"], "abc", "DeviceInfo is flattened into the status");
        assert_eq!(json["port"], "COM3");
        assert_eq!(json["connected"], true);
    }
}
