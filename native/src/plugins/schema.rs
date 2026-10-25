//! Actual kvmhelm.plugin/v1 manifest and panel contracts. No executable driver
//! RPC is invented for the manifest's currently declarative kind field.
use crate::{
    GatewayError, Result,
    contracts::{Validate, bounded_text, integer, optional},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Manifest {
    pub id: String,
    pub version: String,
    pub api_version: String,
    pub kind: String,
    pub runtime: String,
    pub entry: String,
    pub permissions: Vec<String>,
    #[serde(default)]
    pub outbound_hosts: Vec<String>,
    #[serde(default)]
    pub secret_refs: Vec<String>,
    #[serde(default)]
    pub ui_slots: Vec<String>,
}
fn identifier(s: &str, max: usize, dot: bool) -> bool {
    !s.is_empty()
        && s.len() <= max
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-') || dot && b == b'.')
}
pub fn slot(s: &str) -> bool {
    matches!(s, "kvm.sidepanel" | "settings.plugins" | "overview.badge")
}
impl Validate for Manifest {
    fn validate(&self) -> Result<()> {
        if !identifier(&self.id, usize::MAX, true)
            || self.version.is_empty()
            || self.api_version != "kvmhelm.plugin/v1"
            || !matches!(self.kind.as_str(), "feature" | "driver" | "media")
            || self.runtime != "external-process"
            || self.permissions.iter().any(|p| {
                !matches!(
                    p.as_str(),
                    "audio:read:from_target" | "video:read" | "ui:panel" | "notifications:publish"
                )
            })
            || self.ui_slots.iter().any(|s| !slot(s))
        {
            return Err(GatewayError::invalid());
        }
        Ok(())
    }
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Record {
    pub id: String,
    pub path: String,
    pub manifest: Manifest,
    pub devices: Vec<String>,
    pub enabled: bool,
    pub revision: i64,
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Highlight {
    #[serde(deserialize_with = "integer")]
    pub start: i64,
    #[serde(deserialize_with = "integer")]
    pub end: i64,
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Choice {
    pub value: String,
    pub label: String,
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Field {
    pub id: String,
    pub label: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub value: Value,
    #[serde(
        default,
        deserialize_with = "optional",
        skip_serializing_if = "Option::is_none"
    )]
    pub options: Option<Vec<Choice>>,
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PanelAction {
    pub id: String,
    pub label: String,
    #[serde(
        default,
        deserialize_with = "optional",
        skip_serializing_if = "Option::is_none"
    )]
    pub disabled: Option<bool>,
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Panel {
    pub id: String,
    pub title: String,
    pub text: String,
    pub slot: String,
    #[serde(
        default,
        deserialize_with = "optional",
        skip_serializing_if = "Option::is_none"
    )]
    pub device_id: Option<String>,
    #[serde(
        default,
        deserialize_with = "optional",
        skip_serializing_if = "Option::is_none"
    )]
    pub highlights: Option<Vec<Highlight>>,
    #[serde(
        default,
        deserialize_with = "optional",
        skip_serializing_if = "Option::is_none"
    )]
    pub fields: Option<Vec<Field>>,
    #[serde(
        default,
        deserialize_with = "optional",
        skip_serializing_if = "Option::is_none"
    )]
    pub actions: Option<Vec<PanelAction>>,
}
fn strings(value: &Value) -> Result<&Vec<Value>> {
    let values = value
        .as_array()
        .filter(|v| v.len() <= 100)
        .ok_or_else(GatewayError::invalid)?;
    for value in values {
        bounded_text(value.as_str().ok_or_else(GatewayError::invalid)?, 0, 200)?;
    }
    Ok(values)
}
impl Field {
    pub fn check_value(&self, value: &Value) -> Result<()> {
        if self.kind == "number" {
            if !value.is_number() {
                return Err(GatewayError::invalid());
            }
        } else {
            let values = strings(value)?;
            if self.kind == "multi-select"
                && values.iter().any(|v| {
                    !self
                        .options
                        .as_ref()
                        .is_some_and(|o| o.iter().any(|o| v == &o.value))
                })
            {
                return Err(GatewayError::invalid());
            }
        }
        Ok(())
    }
}
impl Validate for Panel {
    fn validate(&self) -> Result<()> {
        bounded_text(&self.id, 0, 100)?;
        bounded_text(&self.title, 0, 100)?;
        bounded_text(&self.text, 0, 16000)?;
        if !slot(&self.slot) {
            return Err(GatewayError::invalid());
        }
        if let Some(ranges) = &self.highlights
            && (ranges.len() > 500 || ranges.iter().any(|r| r.start < 0 || r.end <= r.start))
        {
            return Err(GatewayError::invalid());
        }
        if let Some(fields) = &self.fields {
            if fields.len() > 12 {
                return Err(GatewayError::invalid());
            }
            for field in fields {
                if !identifier(&field.id, 64, false)
                    || !matches!(
                        field.kind.as_str(),
                        "string-list" | "number" | "multi-select"
                    )
                {
                    return Err(GatewayError::invalid());
                }
                bounded_text(&field.label, 0, 100)?;
                if let Some(options) = &field.options {
                    if options.len() > 100 {
                        return Err(GatewayError::invalid());
                    }
                    for option in options {
                        bounded_text(&option.value, 1, 50)?;
                        bounded_text(&option.label, 1, 100)?;
                    }
                }
                if field.kind == "multi-select"
                    && field.options.as_ref().is_none_or(|v| v.is_empty())
                {
                    return Err(GatewayError::invalid());
                }
                // The original schema permits a number or string array for
                // other field defaults; submitted values are typed separately.
                if field.kind == "multi-select" || !field.value.is_number() {
                    strings(&field.value)?;
                }
                if field.kind == "multi-select" {
                    field.check_value(&field.value)?;
                }
            }
        }
        if let Some(actions) = &self.actions {
            if actions.len() > 8 {
                return Err(GatewayError::invalid());
            }
            for action in actions {
                if !identifier(&action.id, 64, false) {
                    return Err(GatewayError::invalid());
                }
                bounded_text(&action.label, 0, 100)?;
            }
        }
        Ok(())
    }
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Reference {
    pub panel_id: String,
    pub item_id: String,
    #[serde(deserialize_with = "integer")]
    pub revision: i64,
}
impl Validate for Reference {
    fn validate(&self) -> Result<()> {
        bounded_text(&self.panel_id, 0, 100)?;
        bounded_text(&self.item_id, 0, 200)?;
        if self.revision < 0 {
            return Err(GatewayError::invalid());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn panel_fields_keep_declared_choices_and_type_submitted_values() {
        let panel=Panel::parse(json!({"id":"n","title":"Notes","text":"text","slot":"kvm.sidepanel","fields":[{"id":"languages","label":"Languages","type":"multi-select","value":["de"],"options":[{"value":"de","label":"German"}]},{"id":"number","label":"N","type":"number","value":2},{"id":"terms","label":"Terms","type":"string-list","value":[]}]})).unwrap();
        let f = panel.fields.unwrap();
        assert!(f[0].check_value(&json!(["de"])).is_ok());
        assert!(f[0].check_value(&json!(["en"])).is_err());
        assert!(f[0].check_value(&json!(1)).is_err());
        assert!(f[1].check_value(&json!(1.5)).is_ok());
        assert!(f[1].check_value(&json!("1")).is_err());
        assert!(f[2].check_value(&json!(["text"])).is_ok());
        assert!(f[2].check_value(&json!(3)).is_err());
    }
    #[test]
    fn panel_ranges_unicode_optional_values_and_unknown_fields_obey_the_contract() {
        let panel = json!({"id":"n","title":"😀".repeat(100),"text":"x","slot":"kvm.sidepanel","highlights":[{"start":0.0,"end":1.0}]});
        assert!(Panel::parse(panel.clone()).is_ok());
        for (key, value) in [
            ("title", json!("😀".repeat(101))),
            ("highlights", json!([{"start":1,"end":1}])),
            ("fields", Value::Null),
            ("unexpected", json!(true)),
        ] {
            let mut p = panel.clone();
            p[key] = value;
            assert!(Panel::parse(p).is_err());
        }
    }
    #[test]
    fn manifest_defaults_do_not_invent_executable_driver_protocols_or_secret_access() {
        let value = json!({"id":"local.feature","version":"1","apiVersion":"kvmhelm.plugin/v1","kind":"feature","runtime":"external-process","entry":"index.mjs","permissions":["ui:panel"]});
        let manifest = Manifest::parse(value.clone()).unwrap();
        assert!(manifest.outbound_hosts.is_empty());
        assert!(manifest.secret_refs.is_empty());
        assert!(manifest.ui_slots.is_empty());
        for (key, bad) in [
            ("permissions", json!(["input:write"])),
            ("runtime", json!("in-process")),
            ("secretRefs", Value::Null),
            ("unknown", json!(true)),
        ] {
            let mut v = value.clone();
            v[key] = bad;
            assert!(Manifest::parse(v).is_err());
        }
    }
}
