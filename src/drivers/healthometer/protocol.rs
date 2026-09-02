//! Health o meter serial protocol for the 1100/2000-series large platform scales
//! ("L" and "E" serial-number versions, see `reference/HealthometerProf.CommunicationProtocols*.pdf`).
//!
//! The scale talks 9600 8N1 and, while a locked weight is on the platform,
//! streams one packet per second:
//!
//! ```text
//! <ESC>R<ESC>I1234567890<ESC>W184.5<ESC>H84.0<ESC>B24.1<ESC>T0.0<ESC>Nm<ESC>E
//! ```
//!
//! Every field is `<ESC>` + one leading letter + value. `R` opens the packet
//! and `E` closes it; there is **no** newline, so framing must key on the
//! `<ESC>E` terminator rather than on lines. Continuous-stream packets may be
//! prefixed with a stray `6` before the `R`. Height and BMI are `0.0` until
//! the BMI button is used; `N` is `m` (metric: kg, cm) or `c` (imperial: lb, in).
//!
//! Everything in this module is pure and unit-tested against the packets
//! printed in the manufacturer's PDF.

use serde::{Deserialize, Serialize};
use std::fmt;

/// ASCII escape, the field separator.
pub const ESC: u8 = 0x1B;
/// Bytes that end every packet.
pub const FRAME_END: &[u8] = b"\x1bE";
/// Serial settings from the protocol sheet: 9600 baud, 8 data bits, no parity, 1 stop bit.
pub const BAUD: u32 = 9600;
/// Longest plausible packet; the framer discards its buffer past this to bound memory.
const MAX_FRAME: usize = 256;

/// Which unit system the scale was in when it sent the packet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum UnitSystem {
    /// Kilograms and centimetres.
    Metric,
    /// Pounds (avoirdupois) and inches.
    Imperial,
}

impl UnitSystem {
    /// UCUM code for the weight unit, as FHIR `Observation.valueQuantity.code` expects.
    pub const fn weight_ucum(self) -> &'static str {
        match self {
            Self::Metric => "kg",
            Self::Imperial => "[lb_av]",
        }
    }

    /// UCUM code for the height unit.
    pub const fn height_ucum(self) -> &'static str {
        match self {
            Self::Metric => "cm",
            Self::Imperial => "[in_i]",
        }
    }

    /// Convert a weight in this system to kilograms.
    pub fn weight_to_kg(self, weight: f64) -> f64 {
        match self {
            Self::Metric => weight,
            Self::Imperial => weight * 0.453_592_37,
        }
    }
}

/// One decoded packet from the scale.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Packet {
    /// Patient ID typed into the scale keypad, if any. All-zero IDs are treated as absent.
    pub patient_id: Option<String>,
    /// Locked weight in `units`. Always positive.
    pub weight: f64,
    /// Height entered for BMI mode, in `units`. `None` when the scale reported `0.0`.
    pub height: Option<f64>,
    /// BMI computed by the scale. `None` when the scale reported `0.0`.
    pub bmi: Option<f64>,
    /// Tare weight in `units`. `None` when `0.0`.
    pub tare: Option<f64>,
    /// Unit system for every numeric field.
    pub units: UnitSystem,
}

/// Why a frame could not be decoded into a [`Packet`].
#[derive(Debug, Clone, PartialEq)]
pub enum ParseError {
    /// No `<ESC>R` start marker anywhere in the frame.
    MissingStart,
    /// The frame did not end with the `E` field.
    MissingTerminator,
    /// No `W` field.
    MissingWeight,
    /// No `N` field; refusing to guess between kg and lb.
    MissingUnits,
    /// `N` carried something other than `m` or `c`.
    UnknownUnits(String),
    /// A numeric field did not parse.
    BadNumber { field: char, text: String },
    /// The weight was zero or negative, which the scale never locks on.
    NonPositiveWeight(f64),
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingStart => write!(f, "no <ESC>R start marker"),
            Self::MissingTerminator => write!(f, "no E terminator"),
            Self::MissingWeight => write!(f, "no W (weight) field"),
            Self::MissingUnits => write!(f, "no N (units) field"),
            Self::UnknownUnits(u) => write!(f, "unknown units code {u:?}"),
            Self::BadNumber { field, text } => write!(f, "field {field} is not a number: {text:?}"),
            Self::NonPositiveWeight(w) => write!(f, "non-positive weight {w}"),
        }
    }
}

impl std::error::Error for ParseError {}

/// Accumulates serial bytes and yields complete frames ending in `<ESC>E`.
///
/// A frame is everything up to and including the terminator. The parser
/// then locates the *last* `<ESC>R` inside it, so a partial packet left over
/// from connecting mid-stream (or from dropped bytes) can never be glued to
/// the front of the next packet and misread as its weight.
#[derive(Debug, Default)]
pub struct Framer {
    buf: Vec<u8>,
}

impl Framer {
    /// Create an empty framer.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed bytes in; get back every complete frame they finished.
    pub fn push(&mut self, bytes: &[u8]) -> Vec<Vec<u8>> {
        self.buf.extend_from_slice(bytes);
        let mut frames = Vec::new();
        while let Some(end) = find(&self.buf, FRAME_END) {
            let frame_len = end.saturating_add(FRAME_END.len());
            let frame: Vec<u8> = self.buf.drain(..frame_len).collect();
            frames.push(frame);
        }
        if self.buf.len() > MAX_FRAME {
            tracing::debug!(
                dropped = self.buf.len(),
                "framer buffer overflow without terminator; discarding"
            );
            self.buf.clear();
        }
        frames
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// `R` or `6R`: the field that opens a packet. Anything longer is data.
fn is_read_marker(field: &str) -> bool {
    field.ends_with('R') && field.len() <= 2
}

/// Decode one frame (as produced by [`Framer`]) into a [`Packet`].
pub fn parse_packet(frame: &[u8]) -> Result<Packet, ParseError> {
    let text = String::from_utf8_lossy(frame);
    let fields: Vec<&str> = text.split(ESC as char).map(str::trim).collect();

    // The packet starts at the last `R` field. Single packets send `<ESC>R`,
    // continuous-stream packets a bare `6R`, so match the field, not the bytes.
    let start = fields
        .iter()
        .rposition(|f| is_read_marker(f))
        .ok_or(ParseError::MissingStart)?;

    let mut patient_id = None;
    let mut weight = None;
    let mut height = None;
    let mut bmi = None;
    let mut tare = None;
    let mut units = None;
    let mut terminated = false;

    for field in fields.iter().skip(start.saturating_add(1)) {
        let mut chars = field.chars();
        let Some(letter) = chars.next() else { continue };
        let value = chars.as_str().trim();
        match letter {
            'I' => patient_id = Some(value.to_owned()),
            'W' => weight = Some(number('W', value)?),
            'H' => height = Some(number('H', value)?),
            'B' => bmi = Some(number('B', value)?),
            'T' => tare = Some(number('T', value)?),
            'N' => {
                units = Some(match value {
                    "m" | "M" => UnitSystem::Metric,
                    "c" | "C" => UnitSystem::Imperial,
                    other => return Err(ParseError::UnknownUnits(other.to_owned())),
                });
            }
            'E' => terminated = true,
            _ => tracing::debug!(field, "ignoring unknown field"),
        }
    }

    if !terminated {
        return Err(ParseError::MissingTerminator);
    }
    let weight = weight.ok_or(ParseError::MissingWeight)?;
    if weight <= 0.0 {
        return Err(ParseError::NonPositiveWeight(weight));
    }
    let units = units.ok_or(ParseError::MissingUnits)?;

    Ok(Packet {
        patient_id: patient_id.filter(|id| !id.is_empty() && !id.chars().all(|c| c == '0')),
        weight,
        height: height.filter(|h| *h > 0.0),
        bmi: bmi.filter(|b| *b > 0.0),
        tare: tare.filter(|t| *t > 0.0),
        units,
    })
}

fn number(field: char, text: &str) -> Result<f64, ParseError> {
    text.parse::<f64>()
        .ok()
        .filter(|n| n.is_finite())
        .ok_or_else(|| ParseError::BadNumber {
            field,
            text: text.to_owned(),
        })
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

    /// Sample packet from the "L" version page of the protocol PDF.
    const L_SAMPLE: &[u8] = b"\x1bR\x1bI1234567890\x1bW184.5\x1bH84.0\x1bB24.1\x1bT0.0\x1bNm\x1bE";
    /// Continuous-stream packet: the same, prefixed with the stray `6`.
    const L_STREAM: &[u8] = b"6R\x1bI1234567890\x1bW184.5\x1bH84.0\x1bB24.1\x1bT0.0\x1bNm\x1bE";
    /// Imperial packet with no patient ID and no BMI entered.
    const NO_BMI: &[u8] = b"\x1bR\x1bI0000000000\x1bW123.4\x1bH0.0\x1bB0.0\x1bT0.0\x1bNc\x1bE";

    #[test]
    fn parses_pdf_sample() {
        let p = parse_packet(L_SAMPLE).unwrap();
        assert_eq!(p.patient_id.as_deref(), Some("1234567890"));
        assert_eq!(p.weight, 184.5);
        assert_eq!(p.height, Some(84.0));
        assert_eq!(p.bmi, Some(24.1));
        assert_eq!(p.tare, None);
        assert_eq!(p.units, UnitSystem::Metric);
    }

    #[test]
    fn stray_six_prefix_is_ignored() {
        assert_eq!(
            parse_packet(L_STREAM).unwrap(),
            parse_packet(L_SAMPLE).unwrap()
        );
    }

    #[test]
    fn zero_fields_become_none() {
        let p = parse_packet(NO_BMI).unwrap();
        assert_eq!(p.patient_id, None, "all-zero patient id means no id");
        assert_eq!(p.height, None);
        assert_eq!(p.bmi, None);
        assert_eq!(p.units, UnitSystem::Imperial);
        assert_eq!(p.weight, 123.4);
    }

    #[test]
    fn fragment_glued_to_next_packet_uses_the_real_packet() {
        // Connected mid-stream: the tail of one packet arrives, then a full one.
        let mut glued = b"\x1bR\x1bI0000000000\x1bW18".to_vec();
        glued.extend_from_slice(L_SAMPLE);
        let p = parse_packet(&glued).unwrap();
        assert_eq!(p.weight, 184.5, "must not read the fragment's W18 as 18 lb");
    }

    #[test]
    fn leftover_tail_without_start_is_rejected() {
        let tail = b"4.5\x1bH84.0\x1bB24.1\x1bT0.0\x1bNm\x1bE";
        assert_eq!(parse_packet(tail), Err(ParseError::MissingStart));
    }

    #[test]
    fn missing_weight_or_units_is_rejected() {
        assert_eq!(
            parse_packet(b"\x1bR\x1bI0000000000\x1bH0.0\x1bNc\x1bE"),
            Err(ParseError::MissingWeight)
        );
        assert_eq!(
            parse_packet(b"\x1bR\x1bW100.0\x1bE"),
            Err(ParseError::MissingUnits)
        );
        assert_eq!(
            parse_packet(b"\x1bR\x1bW100.0\x1bNc"),
            Err(ParseError::MissingTerminator)
        );
        assert_eq!(
            parse_packet(b"\x1bR\x1bW0.0\x1bNc\x1bE"),
            Err(ParseError::NonPositiveWeight(0.0))
        );
        assert_eq!(
            parse_packet(b"\x1bR\x1bWabc\x1bNc\x1bE"),
            Err(ParseError::BadNumber {
                field: 'W',
                text: "abc".to_owned()
            })
        );
        assert_eq!(
            parse_packet(b"\x1bR\x1bW1.0\x1bNx\x1bE"),
            Err(ParseError::UnknownUnits("x".to_owned()))
        );
    }

    #[test]
    fn framer_splits_on_terminator_regardless_of_chunking() {
        let mut two = L_SAMPLE.to_vec();
        two.extend_from_slice(L_STREAM);
        let mut framer = Framer::new();
        let mut frames = Vec::new();
        // Feed one byte at a time to simulate the worst-case serial timing.
        for b in two {
            frames.extend(framer.push(&[b]));
        }
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0], L_SAMPLE);
        assert_eq!(frames[1], L_STREAM);
        assert!(framer.buf.is_empty());
    }

    #[test]
    fn framer_yields_multiple_frames_from_one_read() {
        let mut three = L_SAMPLE.to_vec();
        three.extend_from_slice(L_SAMPLE);
        three.extend_from_slice(NO_BMI);
        let frames = Framer::new().push(&three);
        assert_eq!(frames.len(), 3);
    }

    #[test]
    fn framer_bounds_garbage() {
        let mut framer = Framer::new();
        let garbage = vec![b'x'; MAX_FRAME * 2];
        assert!(framer.push(&garbage).is_empty());
        assert!(
            framer.buf.is_empty(),
            "overflow without terminator is discarded"
        );
    }

    #[test]
    fn unit_conversions() {
        assert!((UnitSystem::Imperial.weight_to_kg(220.0) - 99.79).abs() < 0.01);
        assert_eq!(UnitSystem::Metric.weight_to_kg(70.0), 70.0);
        assert_eq!(UnitSystem::Imperial.weight_ucum(), "[lb_av]");
        assert_eq!(UnitSystem::Metric.height_ucum(), "cm");
    }
}
