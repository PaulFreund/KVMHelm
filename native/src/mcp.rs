//! Tool metadata and result packing shared by native HTTP and MCP transports.
use crate::{
    auth::Identity,
    core::{Core, ToolResult},
};
use base64::{Engine, engine::general_purpose::STANDARD};
use rmcp::{ErrorData, RoleServer, ServerHandler, model::*, service::RequestContext};
use serde_json::{Value, json};
use std::{
    borrow::Cow,
    sync::{Arc, OnceLock},
};

pub const IMAGE_PRESENTATION: &str = "When the user asks to see the screen or a visual result, explicitly display the returned image in your user-visible response using your client's supported image attachment/rendering mechanism. An image in a tool result may be visible only to you. Forward the actual image content; do not replace it with a description or invent a URL. If your client cannot display it, say so; never claim an image is shown unless you included it. Do not upload screenshots to an external service merely to display them.";

/// Checked-in public metadata captured from the original SDK registration.
/// A cross-language test compares it against a fresh capture; execution and
/// validation still belong to the native Core, never the TypeScript runtime.
pub fn contract() -> &'static Value {
    static CONTRACT: OnceLock<Value> = OnceLock::new();
    CONTRACT.get_or_init(|| {
        serde_json::from_str(include_str!("mcp-contract.json")).expect("embedded MCP contract")
    })
}
pub fn tools() -> &'static Vec<Tool> {
    static TOOLS: OnceLock<Vec<Tool>> = OnceLock::new();
    TOOLS.get_or_init(|| {
        serde_json::from_value(contract()["tools"].clone()).expect("embedded MCP tools")
    })
}
pub fn known_tool(name: &str) -> bool {
    tools().iter().any(|tool| tool.name == name)
}
pub(crate) fn validate_wire_request(message: &Value) -> std::result::Result<(), ErrorData> {
    if message.get("method").is_none() {
        return Ok(());
    }
    if message.get("id").is_some()
        && !matches!(
            message["method"].as_str(),
            Some("initialize" | "ping" | "tools/list" | "tools/call")
        )
    {
        return Err(ErrorData::new(
            ErrorCode::METHOD_NOT_FOUND,
            "Method not found",
            None,
        ));
    }
    if let Some(params) = message.get("params") {
        let params = params
            .as_object()
            .ok_or_else(|| ErrorData::invalid_params("MCP params must be an object", None))?;
        if params.get("_meta").is_some_and(|value| !value.is_object()) {
            return Err(ErrorData::invalid_params(
                "MCP metadata must be an object",
                None,
            ));
        }
        if let Some(task) = params.get("task") {
            if !task.is_object() {
                return Err(ErrorData::invalid_params(
                    "Task metadata must be an object",
                    None,
                ));
            }
            return Err(ErrorData::internal_error(
                "Server does not support task creation",
                None,
            ));
        }
        if message["method"] == "tools/list"
            && params.get("cursor").is_some_and(|value| !value.is_string())
        {
            return Err(ErrorData::invalid_params("Cursor must be a string", None));
        }
        if message["method"] == "tools/call"
            && params
                .get("arguments")
                .is_some_and(|value| !value.is_object())
        {
            return Err(ErrorData::invalid_params(
                "Tool arguments must be an object",
                None,
            ));
        }
    }
    Ok(())
}
/// rmcp 3 removed the legacy `execution` field from its typed Tool model.
/// Existing 2025 clients still receive the exact original no-tasks declaration.
pub(crate) fn restore_legacy_tool_metadata(message: &mut Value) {
    if let Some(tools) = message
        .pointer_mut("/result/tools")
        .and_then(Value::as_array_mut)
    {
        for tool in tools {
            if let Some(original) = contract()["tools"]
                .as_array()
                .unwrap()
                .iter()
                .find(|original| original["name"] == tool["name"])
                && let Some(execution) = original.get("execution")
            {
                tool["execution"] = execution.clone();
            }
        }
    }
}
pub fn server_info() -> ServerConfig {
    serde_json::from_value(json!({
        "protocolVersion":contract()["protocolVersion"], "capabilities":contract()["capabilities"],
        "serverInfo":contract()["serverInfo"],
        "instructions":contract()["instructions"]
    }))
    .expect("native server info")
}
pub fn legacy_versions() -> Cow<'static, [ProtocolVersion]> {
    Cow::Owned(
        contract()["versions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|version| serde_json::from_value(version.clone()).expect("reference MCP version"))
            .collect(),
    )
}

#[derive(Clone)]
pub(crate) struct Handler {
    pub core: Arc<Core>,
    pub identity: Identity,
}
impl ServerHandler for Handler {
    fn get_info(&self) -> ServerConfig {
        server_info()
    }
    fn supported_protocol_versions(&self) -> Cow<'static, [ProtocolVersion]> {
        legacy_versions()
    }
    fn get_tool(&self, name: &str) -> Option<Tool> {
        tools().iter().find(|t| t.name == name).cloned()
    }
    async fn list_tools(
        &self,
        _: Option<PaginatedRequestParams>,
        _: RequestContext<RoleServer>,
    ) -> std::result::Result<ListToolsResult, ErrorData> {
        self.core
            .auth
            .check(&self.identity, None, None)
            .map_err(|_| ErrorData::invalid_request("Authorization expired", None))?;
        Ok(serde_json::from_value(json!({"tools":tools()})).expect("native tool list"))
    }
    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _: RequestContext<RoleServer>,
    ) -> std::result::Result<CallToolResponse, ErrorData> {
        if !known_tool(&request.name) {
            return Err(ErrorData::invalid_params("Unknown tool", None));
        }
        let arguments = Value::Object(request.arguments.unwrap_or_default());
        crate::contracts::ToolRequest::parse_tool(&request.name, arguments.clone())
            .map_err(|_| ErrorData::invalid_params("Invalid tool arguments", None))?;
        let result = self
            .core
            .tool(&self.identity, &request.name, arguments)
            .await;
        let result: CallToolResult =
            serde_json::from_value(pack(&result)).expect("native tool result");
        Ok(result.into())
    }
}

pub fn pack(result: &ToolResult) -> Value {
    let body = result.metadata();
    let mut content = vec![json!({"type":"text","text":body.to_string()})];
    for frame in &result.images {
        content.push(json!({"type":"image","mimeType":frame.info.mime_type.name(),"data":STANDARD.encode(&frame.data),"annotations":{"audience":["user","assistant"],"priority":1}}));
    }
    if !result.images.is_empty() {
        content.push(json!({"type":"text","text":IMAGE_PRESENTATION,"annotations":{"audience":["assistant"]}}));
    }
    json!({"isError":!result.success(),"structuredContent":body,"content":content})
}
