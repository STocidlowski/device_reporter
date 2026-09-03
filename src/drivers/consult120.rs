//! `McKesson` Consult 120 urine analyzer over its USB serial port.
//!
//! The analyzer has a WCH CH9102 USB-to-UART bridge (`1a86:55d4`) and prints
//! each result as a plain-text report, 9600 8N1, framed by STX (`0x02`) and
//! ETX (`0x03`). Captured from a real unit with `device-reporter sniff`:
//!
//! ```text
//! <STX>
//!  ID:10
//!  Date:15-05-2026 16:51
//!  Operator: 100
//!  No. 100002
//!  LEU       -              neg
//!  NIT       -              neg
//!  URO       -       0.2  mg/dL
//!  PRO       -              neg
//!  pH               6.0
//!  BLO       -              neg
//!  SG         1.010
//!  KET       -              neg
//!  BIL       -              neg
//!  GLU       -              neg
//! <ETX>
//! ```
//!
//! Analyte lines are `[*]LABEL [grade] value [unit]`. A leading `*` marks an
//! abnormal result. The grade column is `-` (negative), `+/-` (trace), `1+`
//! to `4+` for graded analytes, or a bare `+` for positive-only ones such as
//! nitrite; pH and SG have no grade. Level 2 control, verbatim (`pos` and `+-` are
//! normalised to `positive` and `trace`):
//!
//! ```text
//! *LEU      3+       500 Leu/uL
//! *NIT       +              pos
//! *URO      2+        4   mg/dL
//! *PRO      3+       300  mg/dL
//!  pH           7.5
//! *BLO      3+       200 Ery/uL
//!  SG         1.010
//!  KET      +-        5   mg/dL
//! *BIL      2+       2    mg/dL
//! *GLU      3+       1000 mg/dL
//! ```

use crate::driver::{
    DeviceSession, Driver, ObservationDraft, Output, PortCandidate, SerialSettings,
};
use crate::model::{Component, Value};
use jiff::Timestamp;
use std::time::{Duration, Instant};

/// Machine name of this driver.
pub const KIND: &str = "consult120_urinalysis";

/// WCH CH9102 USB-to-UART bridge inside the analyzer. A generic chip, so a
/// clinic with two CH9102 devices needs `--assign`.
pub const CH9102_VID: u16 = 0x1A86;
pub const CH9102_PID: u16 = 0x55D4;

const STX: u8 = 0x02;
const ETX: u8 = 0x03;
/// Longest plausible report; anything larger without an ETX is discarded.
const MAX_REPORT: usize = 4096;
/// A report that started but never finished is dropped after this long.
const REPORT_TIMEOUT: Duration = Duration::from_secs(5);

/// Prefix of the flag listing analytes the analyzer starred as abnormal, e.g. `abnormal:LEU,GLU`.
pub const FLAG_ABNORMAL: &str = "abnormal";
/// An analyte label the driver does not know; it is passed through with the label as its code.
pub const FLAG_UNMAPPED_FIELD: &str = "unmapped_field";

/// Label as printed, LOINC code, LOINC long name, and the unit when the value is bare.
const FIELDS: &[(&str, &str, &str, Option<&str>)] = &[
    (
        "LEU",
        "5799-2",
        "Leukocyte esterase [Presence] in Urine by Test strip",
        None,
    ),
    (
        "NIT",
        "5802-4",
        "Nitrite [Presence] in Urine by Test strip",
        None,
    ),
    (
        "URO",
        "5818-0",
        "Urobilinogen [Mass/volume] in Urine by Test strip",
        Some("mg/dL"),
    ),
    (
        "PRO",
        "5804-0",
        "Protein [Presence] in Urine by Test strip",
        None,
    ),
    ("pH", "5803-2", "pH of Urine by Test strip", Some("[pH]")),
    (
        "BLO",
        "5794-3",
        "Hemoglobin [Presence] in Urine by Test strip",
        None,
    ),
    (
        "SG",
        "5811-5",
        "Specific gravity of Urine by Test strip",
        None,
    ),
    (
        "KET",
        "2514-8",
        "Ketones [Presence] in Urine by Test strip",
        None,
    ),
    (
        "BIL",
        "5770-3",
        "Bilirubin.total [Presence] in Urine by Test strip",
        None,
    ),
    (
        "GLU",
        "5792-7",
        "Glucose [Presence] in Urine by Test strip",
        None,
    ),
    (
        "ASC",
        "5800-8",
        "Ascorbate [Presence] in Urine by Test strip",
        None,
    ),
];

/// One decoded report.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Report {
    /// `ID:` line; whatever was keyed in as the sample or patient ID.
    pub sample_id: Option<String>,
    /// `Date:` line, verbatim (the analyzer's own clock, no time zone).
    pub device_time: Option<String>,
    pub operator: Option<String>,
    /// `No.` line, the analyzer's running sequence number.
    pub sequence: Option<String>,
    pub results: Vec<StripResult>,
}

/// One analyte line.
#[derive(Debug, Clone, PartialEq)]
pub struct StripResult {
    pub label: String,
    /// The analyzer printed a `*` before the label.
    pub abnormal: bool,
    /// Normalised grade: `negative`, `trace`, `1+` ... `4+`, or the raw symbol.
    pub grade: Option<String>,
    pub value: Value,
    pub unit: Option<String>,
}

fn normalise_grade(s: &str) -> String {
    match s {
        "-" => "negative",
        "+/-" | "±" | "+-" => "trace",
        // Positive-only analytes (nitrite) print a bare `+`; graded ones print `1+`..`4+`.
        "+" => "positive",
        "++" => "2+",
        "+++" => "3+",
        "++++" => "4+",
        other => other,
    }
    .to_owned()
}

fn normalise_text(s: &str) -> String {
    match s.to_ascii_lowercase().as_str() {
        "neg" | "negative" => "negative".to_owned(),
        "nor" | "norm" | "normal" => "normal".to_owned(),
        "pos" | "positive" => "positive".to_owned(),
        "trace" => "trace".to_owned(),
        _ => s.to_owned(),
    }
}

fn is_grade(token: &str) -> bool {
    if token.is_empty() {
        return false;
    }
    let symbols_only = token.chars().all(|c| matches!(c, '+' | '-' | '/' | '±'));
    // `1+` .. `4+`
    let digit_plus = token.len() == 2
        && token.chars().next().is_some_and(|c| c.is_ascii_digit())
        && token.ends_with('+');
    symbols_only || digit_plus
}

/// Device unit spellings to UCUM.
fn ucum(unit: &str) -> String {
    match unit {
        "Leu/uL" => "{Leu}/uL".to_owned(),
        "Ery/uL" => "{Ery}/uL".to_owned(),
        other => other.to_owned(),
    }
}

/// Decode the text between STX and ETX.
#[must_use]
pub fn parse_report(text: &str) -> Report {
    let mut report = Report::default();
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(v) = line.strip_prefix("ID:") {
            report.sample_id = Some(v.trim().to_owned()).filter(|s| !s.is_empty());
            continue;
        }
        if let Some(v) = line.strip_prefix("Date:") {
            report.device_time = Some(v.trim().to_owned()).filter(|s| !s.is_empty());
            continue;
        }
        if let Some(v) = line.strip_prefix("Operator:") {
            report.operator = Some(v.trim().to_owned()).filter(|s| !s.is_empty());
            continue;
        }
        if let Some(v) = line.strip_prefix("No.") {
            report.sequence = Some(v.trim().to_owned()).filter(|s| !s.is_empty());
            continue;
        }
        let mut tokens = line.split_whitespace();
        let Some(label) = tokens.next() else { continue };
        let (label, abnormal) = match label.strip_prefix('*') {
            Some(bare) => (bare, true),
            None => (label, false),
        };
        let rest: Vec<&str> = tokens.collect();
        let (grade, rest) = match rest.split_first() {
            Some((first, tail)) if is_grade(first) => (Some(normalise_grade(first)), tail),
            _ => (None, rest.as_slice()),
        };
        let (value, unit) = match rest {
            [] => (Value::Text(String::new()), None),
            [num, unit, ..] if num.parse::<f64>().is_ok() => (
                Value::Quantity(num.parse().unwrap_or(0.0)),
                Some(ucum(unit)),
            ),
            [num] if num.parse::<f64>().is_ok() => {
                (Value::Quantity(num.parse().unwrap_or(0.0)), None)
            }
            words => (Value::Text(normalise_text(&words.join(" "))), None),
        };
        report.results.push(StripResult {
            label: label.to_owned(),
            abnormal,
            grade,
            value,
            unit,
        });
    }
    report
}

/// FHIR-shaped components plus plausibility flags.
#[must_use]
pub fn components(report: &Report) -> (Vec<Component>, Vec<String>) {
    let mut out = Vec::with_capacity(report.results.len());
    let mut flags = Vec::new();
    let abnormal: Vec<&str> = report
        .results
        .iter()
        .filter(|r| r.abnormal)
        .map(|r| r.label.as_str())
        .collect();
    if !abnormal.is_empty() {
        flags.push(format!("{FLAG_ABNORMAL}:{}", abnormal.join(",")));
    }
    for r in &report.results {
        let known = FIELDS.iter().find(|f| f.0.eq_ignore_ascii_case(&r.label));
        let (code, display, default_unit) = if let Some((_, code, display, unit)) = known {
            ((*code).to_owned(), (*display).to_owned(), *unit)
        } else {
            if !flags.iter().any(|f| f == FLAG_UNMAPPED_FIELD) {
                flags.push(FLAG_UNMAPPED_FIELD.to_owned());
            }
            (r.label.clone(), r.label.clone(), None)
        };
        let unit = match (&r.value, &r.unit) {
            (Value::Quantity(_), Some(u)) => Some(u.clone()),
            (Value::Quantity(_), None) => default_unit.map(str::to_owned),
            (Value::Text(_), _) => None,
        };
        // A text value that is only a grade ("neg") is best expressed as the grade itself.
        let value = match (&r.value, &r.grade) {
            (Value::Text(t), Some(g)) if t.is_empty() => Value::Text(g.clone()),
            (v, _) => v.clone(),
        };
        out.push(Component {
            code,
            display,
            value,
            unit,
            interpretation: r.grade.clone(),
        });
    }
    (out, flags)
}

/// The driver.
#[derive(Debug, Clone, Default)]
pub struct Consult120;

impl Driver for Consult120 {
    fn kind(&self) -> &'static str {
        KIND
    }

    fn display_name(&self) -> &'static str {
        "McKesson Consult 120 urinalysis"
    }

    fn matches(&self, port: &PortCandidate) -> bool {
        port.vid == Some(CH9102_VID) && port.pid == Some(CH9102_PID)
    }

    fn serial_settings(&self) -> SerialSettings {
        SerialSettings::eight_n_one(9600)
    }

    fn open_session(&self) -> Box<dyn DeviceSession> {
        Box::new(Session {
            buf: Vec::new(),
            started: None,
        })
    }
}

struct Session {
    buf: Vec<u8>,
    /// When the current STX was seen; drives the incomplete-report timeout.
    started: Option<Instant>,
}

impl DeviceSession for Session {
    fn on_bytes(&mut self, bytes: &[u8], now: Instant, wall: Timestamp) -> Vec<Output> {
        let mut out = Vec::new();
        self.buf.extend_from_slice(bytes);
        loop {
            // Drop anything before the first STX.
            let Some(start) = self.buf.iter().position(|&b| b == STX) else {
                self.buf.clear();
                break;
            };
            if start > 0 {
                self.buf.drain(..start);
            }
            self.started.get_or_insert(now);
            let Some(end) = self.buf.iter().position(|&b| b == ETX) else {
                if self.buf.len() > MAX_REPORT {
                    out.push(Output::Rejected(format!(
                        "report exceeded {MAX_REPORT} bytes without ETX"
                    )));
                    self.buf.clear();
                    self.started = None;
                }
                break;
            };
            let frame: Vec<u8> = self.buf.drain(..=end).collect();
            self.started = None;
            let text = String::from_utf8_lossy(frame.get(1..end).unwrap_or_default());
            tracing::debug!(raw = %text, "consult120 report");
            let report = parse_report(&text);
            if report.results.is_empty() {
                out.push(Output::Rejected("report had no analyte lines".to_owned()));
                continue;
            }
            let (components, mut flags) = components(&report);
            if let Some(t) = &report.device_time {
                flags.push(format!("device_time:{t}"));
            }
            out.push(Output::Complete(ObservationDraft {
                captured_at: wall,
                completed_at: wall,
                subject_hint: report.sample_id.clone(),
                components,
                flags,
                packets: 1,
            }));
        }
        out
    }

    fn on_tick(&mut self, now: Instant, _wall: Timestamp) -> Vec<Output> {
        match self.started {
            Some(t) if now.duration_since(t) > REPORT_TIMEOUT => {
                self.buf.clear();
                self.started = None;
                vec![Output::Rejected("incomplete report timed out".to_owned())]
            }
            _ => Vec::new(),
        }
    }

    fn is_active(&self) -> bool {
        self.started.is_some()
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

    /// Verbatim capture from the clinic's analyzer (see module docs).
    const CAPTURE: &[u8] =
        b"\x02 ID:10\r\n Date:15-05-2026 16:51\r\n Operator: 100\r\n No. 100002\r\n\
 LEU       -              neg\r\n NIT       -              neg\r\n URO       -       0.2  mg/dL\r\n\
 PRO       -              neg\r\n pH               6.0\r\n BLO       -              neg\r\n\
 SG         1.010\r\n KET       -              neg\r\n BIL       -              neg\r\n\
 GLU       -              neg\r\n\x03";

    #[test]
    fn parses_the_captured_report() {
        let text = String::from_utf8_lossy(&CAPTURE[1..CAPTURE.len() - 1]);
        let r = parse_report(&text);
        assert_eq!(r.sample_id.as_deref(), Some("10"));
        assert_eq!(r.device_time.as_deref(), Some("15-05-2026 16:51"));
        assert_eq!(r.operator.as_deref(), Some("100"));
        assert_eq!(r.sequence.as_deref(), Some("100002"));
        let labels: Vec<&str> = r.results.iter().map(|x| x.label.as_str()).collect();
        assert_eq!(
            labels,
            [
                "LEU", "NIT", "URO", "PRO", "pH", "BLO", "SG", "KET", "BIL", "GLU"
            ]
        );

        let uro = &r.results[2];
        assert_eq!(uro.grade.as_deref(), Some("negative"));
        assert_eq!(uro.value, Value::Quantity(0.2));
        assert_eq!(uro.unit.as_deref(), Some("mg/dL"));

        let ph = &r.results[4];
        assert_eq!(ph.grade, None);
        assert_eq!(ph.value, Value::Quantity(6.0));
        assert_eq!(ph.unit, None);

        let sg = &r.results[6];
        assert_eq!(sg.value, Value::Quantity(1.010));

        let glu = &r.results[9];
        assert_eq!(glu.grade.as_deref(), Some("negative"));
        assert_eq!(glu.value, Value::Text("negative".to_owned()));
        assert!(r.results.iter().all(|x| !x.abnormal));
    }

    /// Level 2 control solution, verbatim from the analyzer (captured via the driver's DEBUG log).
    const LEVEL2: &str = " ID:14
 Date:03-09-2026 12:42 pm
 Operator: 100
 No. 000002
*LEU      3+       500 Leu/uL
*NIT       +              pos
*URO      2+        4   mg/dL
*PRO      3+       300  mg/dL
 pH           7.5
*BLO      3+       200 Ery/uL
 SG         1.010
 KET      +-        5   mg/dL
*BIL      2+       2    mg/dL
*GLU      3+       1000 mg/dL
";

    #[test]
    fn parses_a_positive_control() {
        let r = parse_report(LEVEL2);
        assert_eq!(r.sample_id.as_deref(), Some("14"));
        assert_eq!(r.device_time.as_deref(), Some("03-09-2026 12:42 pm"));
        let leu = &r.results[0];
        assert_eq!(leu.label, "LEU");
        assert!(leu.abnormal);
        assert_eq!(leu.grade.as_deref(), Some("3+"));
        assert_eq!(leu.value, Value::Quantity(500.0));
        assert_eq!(leu.unit.as_deref(), Some("{Leu}/uL"));
        let nit = &r.results[1];
        assert_eq!(nit.grade.as_deref(), Some("positive"));
        assert_eq!(nit.value, Value::Text("positive".to_owned()));
        let ket = &r.results[7];
        assert!(!ket.abnormal);
        assert_eq!(ket.grade.as_deref(), Some("trace"));
        assert_eq!(ket.value, Value::Quantity(5.0));
        let glu = &r.results[9];
        assert_eq!(glu.grade.as_deref(), Some("3+"));
        assert_eq!(glu.value, Value::Quantity(1000.0));
        assert_eq!(glu.unit.as_deref(), Some("mg/dL"));

        let (c, flags) = components(&r);
        assert!(
            c.iter().all(|x| x.code.contains('-')),
            "every analyte mapped to LOINC: {c:?}"
        );
        assert_eq!(c[0].interpretation.as_deref(), Some("3+"));
        assert_eq!(c[5].unit.as_deref(), Some("{Ery}/uL"));
        assert_eq!(flags, vec!["abnormal:LEU,NIT,URO,PRO,BLO,BIL,GLU"]);
    }

    #[test]
    fn components_carry_loinc_codes_units_and_grades() {
        let text = String::from_utf8_lossy(&CAPTURE[1..CAPTURE.len() - 1]);
        let (c, flags) = components(&parse_report(&text));
        assert_eq!(c.len(), 10);
        assert_eq!(c[0].code, "5799-2");
        assert_eq!(c[0].value, Value::Text("negative".to_owned()));
        assert_eq!(c[0].interpretation.as_deref(), Some("negative"));
        assert_eq!(c[2].code, "5818-0");
        assert_eq!(c[2].unit.as_deref(), Some("mg/dL"));
        assert_eq!(c[4].code, "5803-2");
        assert_eq!(
            c[4].unit.as_deref(),
            Some("[pH]"),
            "bare pH gets its default unit"
        );
        assert_eq!(c[6].code, "5811-5");
        assert_eq!(c[6].unit, None, "specific gravity is unitless");
        assert!(flags.is_empty(), "a normal strip has no flags: {flags:?}");
    }

    #[test]
    fn positive_grades_and_unknown_labels() {
        let r = parse_report(
            " GLU       ++      100  mg/dL\r\n PRO       +/-           trace\r\n XYZ  -  neg\r\n",
        );
        assert_eq!(
            r.results[0].grade.as_deref(),
            Some("2+"),
            "`++` spelling is still accepted"
        );
        assert_eq!(r.results[0].value, Value::Quantity(100.0));
        assert_eq!(r.results[1].grade.as_deref(), Some("trace"));
        assert_eq!(r.results[1].value, Value::Text("trace".to_owned()));
        let (c, flags) = components(&r);
        assert_eq!(
            c[2].code, "XYZ",
            "unknown labels pass through with the label as code"
        );
        assert!(flags.contains(&FLAG_UNMAPPED_FIELD.to_owned()));
    }

    #[test]
    fn session_frames_on_stx_etx_across_chunks() {
        let mut s = Consult120.open_session();
        let t0 = Instant::now();
        let wall = Timestamp::UNIX_EPOCH;
        // Leading garbage, then the report byte by byte, then a second report in one go.
        let mut outputs = s.on_bytes(b"junk", t0, wall);
        assert!(outputs.is_empty() && !s.is_active());
        for b in CAPTURE {
            outputs.extend(s.on_bytes(&[*b], t0, wall));
        }
        assert_eq!(outputs.len(), 1);
        let Output::Complete(d) = &outputs[0] else {
            panic!("expected complete, got {outputs:?}")
        };
        assert_eq!(d.subject_hint.as_deref(), Some("10"));
        assert_eq!(d.components.len(), 10);
        assert_eq!(d.flags, vec!["device_time:15-05-2026 16:51"]);
        assert!(!s.is_active());

        let two: Vec<u8> = [CAPTURE, CAPTURE].concat();
        assert_eq!(s.on_bytes(&two, t0, wall).len(), 2);
    }

    #[test]
    fn an_unfinished_report_times_out() {
        let mut s = Consult120.open_session();
        let t0 = Instant::now();
        let wall = Timestamp::UNIX_EPOCH;
        assert!(s.on_bytes(b"\x02 ID:10\r\n", t0, wall).is_empty());
        assert!(s.is_active());
        assert!(s.on_tick(t0 + Duration::from_secs(1), wall).is_empty());
        let out = s.on_tick(t0 + Duration::from_secs(6), wall);
        assert!(matches!(out.as_slice(), [Output::Rejected(_)]));
        assert!(!s.is_active());
    }

    #[test]
    fn matches_only_the_ch9102() {
        let d = Consult120;
        let mut p = PortCandidate {
            name: "COM7".to_owned(),
            vid: Some(CH9102_VID),
            pid: Some(CH9102_PID),
            serial_number: Some("56A4065083".to_owned()),
            manufacturer: Some("Microsoft".to_owned()),
            product: Some("USB Serial Device (COM7)".to_owned()),
        };
        assert!(d.matches(&p));
        p.pid = Some(0x7523);
        assert!(!d.matches(&p));
    }
}
