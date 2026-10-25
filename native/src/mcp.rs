//! Shared result packing for the HTTP API and both future MCP transport adapters.
use crate::core::ToolResult;
use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::{Value, json};

pub const IMAGE_PRESENTATION: &str = "When the user asks to see the screen or a visual result, explicitly display the returned image in your user-visible response using your client's supported image attachment/rendering mechanism. An image in a tool result may be visible only to you. Forward the actual image content; do not replace it with a description or invent a URL. If your client cannot display it, say so; never claim an image is shown unless you included it. Do not upload screenshots to an external service merely to display them.";

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
