//! Mapping from the reporter's [`Observation`] to FHIR R5 `Observation` JSON.
//!
//! The FHIR server is the single source of truth, so a device result is
//! forwarded as a **preliminary** Observation with **no subject**: the
//! clinician attaches the patient when they accept it in the chart, flipping
//! `status` to `final`. Unassigned readings must still be treated as sensitive
//! clinical data.
//!
//! Each Observation carries a client-chosen `id` (delivery is `PUT` to that id,
//! see [`crate::forward`]) and the same value as an `identifier` for humans and
//! searches. The device is referenced by identifier so the Pi never needs to
//! know FHIR resource ids; the server resolves `Device` and its `Location`.
//!
//! Shapes follow the conventions of the EMR this feeds: one Observation per
//! vital sign, height in `[in_us]`, BMI never persisted (the chart derives it);
//! a urinalysis strip is one laboratory panel with `component[]`.

use crate::model::{Component, Observation, Value};
use serde_json::{Value as Json, json};

/// Identifier system for `Observation.identifier` (the reporter's observation UUID).
pub const OBSERVATION_ID_SYSTEM: &str = "urn:device-reporter:observation";
/// Identifier system for `Device.identifier` (the reporter's device id).
pub const DEVICE_ID_SYSTEM: &str = "urn:device-reporter:device";
const LOINC: &str = "http://loinc.org";
const UCUM: &str = "http://unitsofmeasure.org";
const CATEGORY_SYSTEM: &str = "http://terminology.hl7.org/CodeSystem/observation-category";
const INTERPRETATION_SYSTEM: &str =
    "http://terminology.hl7.org/CodeSystem/v3-ObservationInterpretation";

/// LOINC 39156-5, body mass index: the EMR derives it at render time and never stores it.
const LOINC_BMI: &str = "39156-5";
/// LOINC panel code for an automated test-strip urinalysis.
const LOINC_URINALYSIS_PANEL: &str = "50556-0";

/// How a device kind's observations are shaped in FHIR.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Shape {
    /// One `vital-signs` Observation per component (weight, height, ...).
    VitalSigns,
    /// One `laboratory` panel Observation with every analyte in `component[]`.
    LaboratoryPanel {
        panel_code: &'static str,
        panel_display: &'static str,
    },
    /// Unknown device: one uncategorised Observation per component.
    Generic,
}

fn shape_for(device_kind: &str) -> Shape {
    match device_kind {
        crate::drivers::healthometer::KIND => Shape::VitalSigns,
        crate::drivers::consult120::KIND => Shape::LaboratoryPanel {
            panel_code: LOINC_URINALYSIS_PANEL,
            panel_display: "Urinalysis complete panel - Urine by Automated test strip",
        },
        _ => Shape::Generic,
    }
}

/// Unit spellings the EMR stores, where they differ from what a device reports.
fn app_unit(code: &str) -> &str {
    match code {
        // The international inch vs the US survey inch differ by 2 ppm; the EMR uses `[in_us]`.
        "[in_i]" => "[in_us]",
        other => other,
    }
}

/// Human label shown as `Quantity.unit`.
fn unit_label(code: &str) -> &str {
    match code {
        "[lb_av]" => "lb",
        "[in_us]" | "[in_i]" => "in",
        "kg/m2" => "kg/m2",
        "[pH]" => "pH",
        "{Leu}/uL" => "Leu/uL",
        "{Ery}/uL" => "Ery/uL",
        other => other,
    }
}

/// Insert into a JSON object; a non-object (never the case here) is left untouched.
fn set(obj: &mut Json, key: &str, value: Json) {
    if let Some(map) = obj.as_object_mut() {
        map.insert(key.to_owned(), value);
    }
}

fn identifier(value: &str) -> Json {
    json!([{ "system": OBSERVATION_ID_SYSTEM, "value": value }])
}

/// FHIR resource id: `[A-Za-z0-9.-]{1,64}`. Built from the reporter UUID plus
/// the LOINC code for per-vital Observations (`/` is not allowed in an id).
fn resource_id(ident: &str) -> String {
    ident
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '.' {
                c
            } else {
                '-'
            }
        })
        .take(64)
        .collect()
}

fn coding(system: &str, code: &str, display: &str) -> Json {
    json!({ "coding": [{ "system": system, "code": code, "display": display }], "text": display })
}

fn value_field(c: &Component) -> (&'static str, Json) {
    match (&c.value, c.unit.as_deref()) {
        (Value::Quantity(q), Some(unit)) => {
            let code = app_unit(unit);
            (
                "valueQuantity",
                json!({ "value": q, "unit": unit_label(code), "system": UCUM, "code": code }),
            )
        }
        (Value::Quantity(q), None) => ("valueQuantity", json!({ "value": q })),
        (Value::Text(t), _) => ("valueString", json!(t)),
    }
}

fn interpretation(grade: &str) -> Json {
    let code = match grade {
        "negative" | "normal" => Some(("NEG", "Negative")),
        "positive" => Some(("POS", "Positive")),
        g if g.ends_with('+') => Some(("A", "Abnormal")),
        _ => None,
    };
    match code {
        Some((c, d)) => {
            json!([{ "coding": [{ "system": INTERPRETATION_SYSTEM, "code": c, "display": d }], "text": grade }])
        }
        None => json!([{ "text": grade }]),
    }
}

fn component_json(c: &Component) -> Json {
    let mut comp = json!({ "code": coding(LOINC, &c.code, &c.display) });
    let (key, v) = value_field(c);
    set(&mut comp, key, v);
    if let Some(g) = &c.interpretation {
        set(&mut comp, "interpretation", interpretation(g));
    }
    comp
}

fn base(o: &Observation, category: Option<(&str, &str)>, ident: &str) -> Json {
    let mut obs = json!({
        "resourceType": "Observation",
        "id": resource_id(ident),
        "identifier": identifier(ident),
        "status": "preliminary",
        "effectiveDateTime": o.captured_at.to_string(),
        "issued": o.completed_at.to_string(),
        "device": {
            "identifier": { "system": DEVICE_ID_SYSTEM, "value": o.device_id },
            "display": o.device_kind,
        },
    });
    if let Some((code, display)) = category {
        set(
            &mut obs,
            "category",
            json!([coding(CATEGORY_SYSTEM, code, display)]),
        );
    }
    let mut notes: Vec<Json> = Vec::new();
    if let Some(hint) = &o.subject_hint {
        notes.push(json!({ "text": format!("Device-entered ID: {hint}") }));
    }
    if !o.flags.is_empty() {
        notes.push(json!({ "text": format!("Device flags: {}", o.flags.join(", ")) }));
    }
    if !notes.is_empty() {
        set(&mut obs, "note", Json::Array(notes));
    }
    obs
}

/// FHIR Observations to post for one device result. May be empty (a scale
/// result whose only component is BMI, say).
#[must_use]
pub fn to_fhir(o: &Observation) -> Vec<Json> {
    match shape_for(&o.device_kind) {
        Shape::VitalSigns => o
            .components
            .iter()
            .filter(|c| c.code != LOINC_BMI)
            .map(|c| {
                // One identifier per Observation: suffix the reporter id with the LOINC code.
                let mut obs = base(
                    o,
                    Some(("vital-signs", "Vital Signs")),
                    &format!("{}/{}", o.id, c.code),
                );
                set(&mut obs, "code", coding(LOINC, &c.code, &c.display));
                let (key, v) = value_field(c);
                set(&mut obs, key, v);
                obs
            })
            .collect(),
        Shape::LaboratoryPanel {
            panel_code,
            panel_display,
        } => {
            let mut obs = base(o, Some(("laboratory", "Laboratory")), &o.id.to_string());
            set(&mut obs, "code", coding(LOINC, panel_code, panel_display));
            set(
                &mut obs,
                "component",
                Json::Array(o.components.iter().map(component_json).collect()),
            );
            vec![obs]
        }
        Shape::Generic => o
            .components
            .iter()
            .map(|c| {
                let mut obs = base(o, None, &format!("{}/{}", o.id, c.code));
                set(&mut obs, "code", coding(LOINC, &c.code, &c.display));
                let (key, v) = value_field(c);
                set(&mut obs, key, v);
                obs
            })
            .collect(),
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
    use jiff::Timestamp;
    use uuid::Uuid;

    fn obs(kind: &str, components: Vec<Component>) -> Observation {
        Observation {
            id: Uuid::nil(),
            device_id: "lab-pi-56A4065083".to_owned(),
            device_kind: kind.to_owned(),
            captured_at: Timestamp::from_second(1_700_000_000).unwrap(),
            completed_at: Timestamp::from_second(1_700_000_005).unwrap(),
            subject_hint: Some("14".to_owned()),
            components,
            flags: vec!["abnormal:GLU".to_owned()],
            packets: 1,
        }
    }

    #[test]
    fn scale_gives_one_vital_per_component_without_bmi_or_subject() {
        let o = obs(
            crate::drivers::healthometer::KIND,
            vec![
                Component::quantity("29463-7", "Body weight", 184.5, "[lb_av]"),
                Component::quantity("8302-2", "Body height", 70.0, "[in_i]"),
                Component::quantity("39156-5", "Body mass index", 26.5, "kg/m2"),
            ],
        );
        let out = to_fhir(&o);
        assert_eq!(out.len(), 2, "BMI is never persisted");
        let w = &out[0];
        assert_eq!(w["resourceType"], "Observation");
        assert_eq!(w["status"], "preliminary");
        assert!(
            w.get("subject").is_none(),
            "no subject until a clinician accepts"
        );
        assert_eq!(w["category"][0]["coding"][0]["code"], "vital-signs");
        assert_eq!(w["code"]["coding"][0]["code"], "29463-7");
        assert_eq!(w["valueQuantity"]["value"], 184.5);
        assert_eq!(w["valueQuantity"]["code"], "[lb_av]");
        assert_eq!(w["valueQuantity"]["unit"], "lb");
        assert_eq!(w["valueQuantity"]["system"], UCUM);
        assert_eq!(w["identifier"][0]["system"], OBSERVATION_ID_SYSTEM);
        assert_eq!(
            w["identifier"][0]["value"],
            "00000000-0000-0000-0000-000000000000/29463-7"
        );
        assert_eq!(w["id"], "00000000-0000-0000-0000-000000000000-29463-7");
        assert!(w.get("meta").is_none(), "server owns resource versions");
        assert_eq!(w["device"]["identifier"]["value"], "lab-pi-56A4065083");
        assert_eq!(w["effectiveDateTime"], "2023-11-14T22:13:20Z");
        assert_eq!(w["issued"], "2023-11-14T22:13:25Z");
        assert_eq!(w["note"][0]["text"], "Device-entered ID: 14");
        let h = &out[1];
        assert_eq!(
            h["valueQuantity"]["code"], "[in_us]",
            "height uses the app's inch"
        );
        assert_eq!(
            h["identifier"][0]["value"],
            "00000000-0000-0000-0000-000000000000/8302-2"
        );
    }

    #[test]
    fn urinalysis_is_one_laboratory_panel_with_components() {
        let mut leu = Component::quantity("5799-2", "Leukocyte esterase", 500.0, "{Leu}/uL");
        leu.interpretation = Some("3+".to_owned());
        let mut nit = Component::text("5802-4", "Nitrite", "positive");
        nit.interpretation = Some("positive".to_owned());
        let mut glu = Component::text("5792-7", "Glucose", "negative");
        glu.interpretation = Some("negative".to_owned());
        let mut ket = Component::quantity("2514-8", "Ketones", 5.0, "mg/dL");
        ket.interpretation = Some("trace".to_owned());
        let ph = Component::quantity("5803-2", "pH", 6.0, "[pH]");
        let o = obs(
            crate::drivers::consult120::KIND,
            vec![leu, nit, glu, ket, ph],
        );

        let out = to_fhir(&o);
        assert_eq!(out.len(), 1);
        let p = &out[0];
        assert_eq!(p["category"][0]["coding"][0]["code"], "laboratory");
        assert_eq!(p["code"]["coding"][0]["code"], LOINC_URINALYSIS_PANEL);
        assert_eq!(
            p["identifier"][0]["value"],
            "00000000-0000-0000-0000-000000000000"
        );
        assert_eq!(p["id"], "00000000-0000-0000-0000-000000000000");
        let c = p["component"].as_array().unwrap();
        assert_eq!(c.len(), 5);
        assert_eq!(c[0]["valueQuantity"]["code"], "{Leu}/uL");
        assert_eq!(c[0]["interpretation"][0]["coding"][0]["code"], "A");
        assert_eq!(c[0]["interpretation"][0]["text"], "3+");
        assert_eq!(c[1]["valueString"], "positive");
        assert_eq!(c[1]["interpretation"][0]["coding"][0]["code"], "POS");
        assert_eq!(c[2]["interpretation"][0]["coding"][0]["code"], "NEG");
        assert_eq!(c[3]["interpretation"][0]["text"], "trace");
        assert!(
            c[3]["interpretation"][0].get("coding").is_none(),
            "trace has no v3 code"
        );
        assert_eq!(c[4]["valueQuantity"]["unit"], "pH");
        assert_eq!(p["note"][1]["text"], "Device flags: abnormal:GLU");
    }

    #[test]
    fn unknown_kinds_fall_back_to_uncategorised_observations() {
        let o = obs(
            "mystery",
            vec![Component::quantity("718-7", "Hemoglobin", 13.2, "g/dL")],
        );
        let out = to_fhir(&o);
        assert_eq!(out.len(), 1);
        assert!(out[0].get("category").is_none());
        assert_eq!(out[0]["valueQuantity"]["code"], "g/dL");
    }
}
