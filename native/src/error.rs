use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::fmt;

pub type Result<T> = std::result::Result<T, GatewayError>;

/// Only deliberate public messages cross API/log boundaries. Never wrap an SQL,
/// crypto, deserializer or operating-system error containing user data here.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct GatewayError {
    pub code: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "serde_json::Map::is_empty")]
    pub details: serde_json::Map<String, Value>,
}

impl GatewayError {
    pub fn new(code: &str) -> Self {
        Self::message(code, code)
    }

    pub fn message(code: &str, message: &str) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            details: Default::default(),
        }
    }

    pub fn invalid() -> Self {
        Self::message(
            "INVALID_ARGUMENT",
            "Request does not match the strict input schema",
        )
    }

    pub fn storage() -> Self {
        Self::message("STORAGE_ERROR", "Configuration store unavailable")
    }

    pub fn envelope(&self) -> Value {
        json!({"ok":false, "error":{"code":self.code,"message":self.message}, "details":self.details})
    }
}

impl fmt::Display for GatewayError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for GatewayError {}
