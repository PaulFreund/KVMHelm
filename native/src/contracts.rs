//! Wire spelling, defaults and limits match shared/contracts.ts. Validation is
//! exposed through Validate::parse at JSON boundaries and validate for typed calls.
use crate::{GatewayError, Result};
use serde::{Deserialize, Deserializer, Serialize, de::DeserializeOwned};
use serde_json::Value;

pub const SCOPES: [&str; 9] = [
    "devices:read",
    "devices:manage",
    "video:read",
    "input:write",
    "audio:read",
    "audio:write",
    "plugins:manage",
    "tokens:manage",
    "control:stop",
];
pub const MAX_SAFE_INTEGER: i64 = 9_007_199_254_740_991;

/// The pinned Zod 4.5 implementation limits strings by Unicode code points.
/// Rust strings contain Unicode scalar values; malformed surrogate JSON is
/// rejected during decoding, before this validation boundary.
pub fn text_length(value: &str) -> usize {
    value.chars().count()
}

pub(crate) fn bounded_text(value: &str, min: usize, max: usize) -> Result<()> {
    let len = text_length(value);
    if (min..=max).contains(&len) {
        Ok(())
    } else {
        Err(GatewayError::invalid())
    }
}

fn range(value: i64, min: i64, max: i64) -> Result<()> {
    if (min..=max).contains(&value) {
        Ok(())
    } else {
        Err(GatewayError::invalid())
    }
}

/// Accept JSON 1.0 exactly as Zod does, but reject fractional/unsafe integers.
pub(crate) fn integer<'de, D: Deserializer<'de>>(d: D) -> std::result::Result<i64, D::Error> {
    let number = serde_json::Number::deserialize(d)?;
    let value = number
        .as_f64()
        .ok_or_else(|| serde::de::Error::custom("integer required"))?;
    if value.fract() != 0.0 || value.abs() > MAX_SAFE_INTEGER as f64 {
        return Err(serde::de::Error::custom("safe integer required"));
    }
    Ok(value as i64)
}

/// Serde's Option normally accepts null; Zod .optional() does not.
pub(crate) fn optional<'de, D, T>(d: D) -> std::result::Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    T::deserialize(d).map(Some)
}

pub trait Validate: DeserializeOwned + Serialize {
    fn validate(&self) -> Result<()>;
    fn validate_input(_input: &Value) -> Result<()> {
        Ok(())
    }
    fn parse(input: Value) -> Result<Self> {
        require_object(&input)?;
        Self::validate_input(&input)?;
        let value: Self = serde_json::from_value(input).map_err(|_| GatewayError::invalid())?;
        value.validate()?;
        Ok(value)
    }
}

fn require_object(value: &Value) -> Result<()> {
    if value.is_object() {
        Ok(())
    } else {
        Err(GatewayError::invalid())
    }
}

fn optional_object(input: &Value, field: &str) -> Result<()> {
    if let Some(value) = input.get(field) {
        require_object(value)?;
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Profile {
    #[default]
    Auto,
    Active,
    Overview,
    Warm,
}

impl Profile {
    pub fn name(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Active => "active",
            Self::Overview => "overview",
            Self::Warm => "warm",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Layout {
    #[default]
    Us,
    De,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Transport {
    #[default]
    Network,
    Simulator,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Tls {
    #[serde(
        default,
        deserialize_with = "optional",
        skip_serializing_if = "Option::is_none"
    )]
    pub ca_ref: Option<String>,
    #[serde(
        default,
        deserialize_with = "optional",
        skip_serializing_if = "Option::is_none"
    )]
    pub fingerprint: Option<String>,
    #[serde(
        default,
        deserialize_with = "optional",
        skip_serializing_if = "Option::is_none"
    )]
    pub insecure: Option<bool>,
}

fn driver_default() -> String {
    "pikvm-v4".into()
}
fn model_default() -> String {
    "PiKVM v4".into()
}
fn firmware_default() -> String {
    "unknown".into()
}
fn yes() -> bool {
    true
}
fn latency_default() -> i64 {
    30
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Device {
    #[serde(
        default,
        deserialize_with = "optional",
        skip_serializing_if = "Option::is_none"
    )]
    pub device_id: Option<String>,
    pub name: String,
    #[serde(default = "driver_default")]
    pub driver_id: String,
    #[serde(default = "model_default")]
    pub model: String,
    #[serde(default = "firmware_default")]
    pub firmware: String,
    #[serde(
        default,
        deserialize_with = "optional",
        skip_serializing_if = "Option::is_none"
    )]
    pub address: Option<String>,
    #[serde(
        default,
        deserialize_with = "optional",
        skip_serializing_if = "Option::is_none"
    )]
    pub secret_ref: Option<String>,
    #[serde(default)]
    pub tls: Tls,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default = "yes")]
    pub enabled: bool,
    #[serde(default)]
    pub media_profile: Profile,
    #[serde(default = "latency_default", deserialize_with = "integer")]
    pub minimum_action_latency_ms: i64,
    #[serde(default)]
    pub keyboard_layout: Layout,
    #[serde(default)]
    pub transport: Transport,
    #[serde(default, deserialize_with = "integer")]
    pub revision: i64,
}

impl Validate for Device {
    fn validate_input(input: &Value) -> Result<()> {
        optional_object(input, "tls")
    }
    fn validate(&self) -> Result<()> {
        bounded_text(&self.name, 1, 100)?;
        range(self.minimum_action_latency_ms, 0, 5000)?;
        range(self.revision, -MAX_SAFE_INTEGER, MAX_SAFE_INTEGER)?;
        if self.tags.len() > 50 {
            return Err(GatewayError::invalid());
        }
        for tag in &self.tags {
            bounded_text(tag, 0, 40)?;
        }
        if let Some(id) = &self.device_id {
            // Zod accepts canonical RFC 9562 UUID versions 1..8, nil and max.
            let bytes = id.as_bytes();
            let canonical = bytes.len() == 36
                && [8, 13, 18, 23].iter().all(|&p| bytes[p] == b'-')
                && bytes
                    .iter()
                    .enumerate()
                    .all(|(p, c)| [8, 13, 18, 23].contains(&p) || c.is_ascii_hexdigit());
            let rfc = canonical
                && (b'1'..=b'8').contains(&bytes[14])
                && matches!(bytes[19].to_ascii_lowercase(), b'8' | b'9' | b'a' | b'b');
            if !rfc
                && id != "00000000-0000-0000-0000-000000000000"
                && id != "ffffffff-ffff-ffff-ffff-ffffffffffff"
            {
                return Err(GatewayError::invalid());
            }
        }
        if let Some(address) = &self.address {
            url::Url::parse(address).map_err(|_| GatewayError::invalid())?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Point {
    #[serde(deserialize_with = "integer")]
    pub x: i64,
    #[serde(deserialize_with = "integer")]
    pub y: i64,
}

impl Point {
    pub fn validate(&self) -> Result<()> {
        range(self.x, 0, MAX_SAFE_INTEGER)?;
        range(self.y, 0, MAX_SAFE_INTEGER)
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Button {
    #[default]
    Left,
    Right,
    Middle,
}

fn wait_default() -> i64 {
    100
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Action {
    Click {
        #[serde(deserialize_with = "integer")]
        x: i64,
        #[serde(deserialize_with = "integer")]
        y: i64,
        #[serde(default)]
        button: Button,
    },
    DoubleClick {
        #[serde(deserialize_with = "integer")]
        x: i64,
        #[serde(deserialize_with = "integer")]
        y: i64,
    },
    Move {
        #[serde(deserialize_with = "integer")]
        x: i64,
        #[serde(deserialize_with = "integer")]
        y: i64,
    },
    Drag {
        path: Vec<Point>,
    },
    Scroll {
        #[serde(deserialize_with = "integer")]
        x: i64,
        #[serde(deserialize_with = "integer")]
        y: i64,
        #[serde(deserialize_with = "integer")]
        scroll_x: i64,
        #[serde(deserialize_with = "integer")]
        scroll_y: i64,
    },
    Keypress {
        keys: Vec<String>,
    },
    Type {
        text: String,
    },
    Wait {
        #[serde(default = "wait_default", deserialize_with = "integer")]
        duration_ms: i64,
    },
    Screenshot {},
}

impl Action {
    pub fn name(&self) -> &'static str {
        match self {
            Self::Click { .. } => "click",
            Self::DoubleClick { .. } => "double_click",
            Self::Move { .. } => "move",
            Self::Drag { .. } => "drag",
            Self::Scroll { .. } => "scroll",
            Self::Keypress { .. } => "keypress",
            Self::Type { .. } => "type",
            Self::Wait { .. } => "wait",
            Self::Screenshot {} => "screenshot",
        }
    }
    pub fn point(&self) -> Option<Point> {
        match *self {
            Self::Click { x, y, .. }
            | Self::DoubleClick { x, y }
            | Self::Move { x, y }
            | Self::Scroll { x, y, .. } => Some(Point { x, y }),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Signal {
    Present,
    Absent,
    Unknown,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct Capabilities {
    pub video: VideoCapabilities,
    pub mouse: MouseCapabilities,
    pub keyboard: KeyboardCapabilities,
    pub audio: AudioCapabilities,
    pub timebase: Timebase,
    pub extensions: serde_json::Map<String, Value>,
}
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct VideoCapabilities {
    pub snapshot: bool,
    pub stream: bool,
    pub formats: Vec<String>,
    pub signal: Signal,
}
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct MouseCapabilities {
    pub absolute: bool,
    pub relative: bool,
    pub buttons: Vec<String>,
    pub horizontal_scroll: bool,
}
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct KeyboardCapabilities {
    pub hid: bool,
    pub layouts: Vec<String>,
    pub text_method: String,
    pub characters: String,
}
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct AudioCapabilities {
    pub from_target: bool,
    pub to_target: bool,
    pub formats: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct Timebase {
    pub source_timestamp: bool,
    pub uncertainty_ms: Option<f64>,
}

impl Validate for Action {
    fn validate_input(input: &Value) -> Result<()> {
        if let Some(points) = input.get("path").and_then(Value::as_array) {
            for point in points {
                require_object(point)?;
            }
        }
        Ok(())
    }
    fn validate(&self) -> Result<()> {
        if let Some(p) = self.point() {
            p.validate()?;
        }
        match self {
            Self::Drag { path } => {
                if !(2..=256).contains(&path.len()) {
                    return Err(GatewayError::invalid());
                }
                for p in path {
                    p.validate()?;
                }
            }
            Self::Scroll {
                scroll_x, scroll_y, ..
            } => {
                range(*scroll_x, -1000, 1000)?;
                range(*scroll_y, -1000, 1000)?;
            }
            Self::Keypress { keys } => {
                if !(1..=6).contains(&keys.len()) {
                    return Err(GatewayError::invalid());
                }
                for key in keys {
                    bounded_text(key, 1, 40)?;
                }
            }
            Self::Type { text } => bounded_text(text, 0, 4096)?,
            Self::Wait { duration_ms } => range(*duration_ms, 0, 5000)?,
            _ => (),
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Reference {
    pub frame_id: String,
    pub view_id: String,
    #[serde(deserialize_with = "integer")]
    pub input_revision: i64,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    Observe,
    Control,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ObservationMode {
    #[default]
    AfterActions,
    None,
}

fn timeout_default() -> i64 {
    1500
}
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Observation {
    #[serde(default)]
    pub mode: ObservationMode,
    #[serde(default = "timeout_default", deserialize_with = "integer")]
    pub timeout_ms: i64,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ControlOperation {
    Acquire,
    Renew,
    Release,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(
    tag = "name",
    content = "args",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum ToolRequest {
    ListComputers(ListComputers),
    OpenComputer(OpenComputer),
    Computer(Computer),
    ComputerScreenshot(Screenshot),
    ComputerControl(Control),
    CloseComputer(Close),
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ListComputers {
    #[serde(
        default,
        deserialize_with = "optional",
        skip_serializing_if = "Option::is_none"
    )]
    pub tags: Option<Vec<String>>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OpenComputer {
    pub computer_id: String,
    pub mode: Mode,
    #[serde(
        default,
        deserialize_with = "optional",
        skip_serializing_if = "Option::is_none"
    )]
    pub request_id: Option<String>,
    #[serde(
        default,
        deserialize_with = "optional",
        skip_serializing_if = "Option::is_none"
    )]
    pub profile: Option<Profile>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Computer {
    pub session_id: String,
    pub request_id: String,
    pub reference: Reference,
    pub actions: Vec<Action>,
    #[serde(
        default,
        deserialize_with = "optional",
        skip_serializing_if = "Option::is_none"
    )]
    pub observation: Option<Observation>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Screenshot {
    pub session_id: String,
    #[serde(default, deserialize_with = "integer")]
    pub max_age_ms: i64,
    #[serde(default = "timeout_default", deserialize_with = "integer")]
    pub timeout_ms: i64,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Control {
    pub session_id: String,
    pub request_id: String,
    pub operation: ControlOperation,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Close {
    pub session_id: String,
}

impl ToolRequest {
    pub fn parse_tool(name: &str, args: Value) -> Result<Self> {
        Self::parse(serde_json::json!({"name": name, "args": args}))
    }
    pub fn normalized_args(&self) -> Value {
        // Serializing this enum always yields an object containing args.
        serde_json::to_value(self).expect("wire request is serializable")["args"].clone()
    }
}

impl Validate for ToolRequest {
    fn validate_input(input: &Value) -> Result<()> {
        let args = input.get("args").ok_or_else(GatewayError::invalid)?;
        require_object(args)?;
        if input.get("name").and_then(Value::as_str) == Some("computer") {
            optional_object(args, "reference")?;
            optional_object(args, "observation")?;
            if let Some(actions) = args.get("actions").and_then(Value::as_array) {
                for action in actions {
                    require_object(action)?;
                    Action::validate_input(action)?;
                }
            }
        }
        Ok(())
    }
    fn validate(&self) -> Result<()> {
        match self {
            Self::OpenComputer(a) => {
                if let Some(id) = &a.request_id {
                    bounded_text(id, 1, 128)?;
                }
            }
            Self::Computer(a) => {
                bounded_text(&a.request_id, 1, 128)?;
                range(
                    a.reference.input_revision,
                    -MAX_SAFE_INTEGER,
                    MAX_SAFE_INTEGER,
                )?;
                if !(1..=32).contains(&a.actions.len()) {
                    return Err(GatewayError::invalid());
                }
                for action in &a.actions {
                    action.validate()?;
                }
                if let Some(o) = &a.observation {
                    range(o.timeout_ms, 50, 10000)?;
                }
            }
            Self::ComputerScreenshot(a) => {
                range(a.max_age_ms, 0, 5000)?;
                range(a.timeout_ms, 50, 10000)?;
            }
            Self::ComputerControl(a) => bounded_text(&a.request_id, 1, 128)?,
            _ => (),
        }
        Ok(())
    }
}
