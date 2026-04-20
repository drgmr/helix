//! MCP request dispatcher: initialize / tools/list / tools/call.

use std::sync::Arc;

use serde_json::{json, Value};

use crate::{
    protocol::{codes, JsonRpcRequest, JsonRpcResponse},
    server::State,
    tools,
};

pub async fn handle(state: &Arc<State>, req: JsonRpcRequest) -> Option<JsonRpcResponse> {
    let id = req.id.clone()?; // notifications (no id) produce no response
    match req.method.as_str() {
        "initialize" => Some(JsonRpcResponse::success(
            id,
            json!({
                "protocolVersion": "2024-11-05",
                "capabilities": { "tools": {} },
                "serverInfo": { "name": "helix-claude-ide", "version": env!("CARGO_PKG_VERSION") }
            }),
        )),
        "tools/list" => Some(JsonRpcResponse::success(
            id,
            json!({ "tools": tools::tool_list() }),
        )),
        "tools/call" => {
            #[derive(serde::Deserialize)]
            struct Params {
                name: String,
                #[serde(default)]
                arguments: Value,
            }
            let params: Params = match serde_json::from_value(req.params) {
                Ok(p) => p,
                Err(e) => {
                    return Some(JsonRpcResponse::error(
                        id,
                        codes::INVALID_PARAMS,
                        format!("invalid params: {e}"),
                    ));
                }
            };
            let out = tools::dispatch(state, &params.name, params.arguments).await;
            Some(JsonRpcResponse::success(
                id,
                json!({ "content": out.content, "isError": out.is_error }),
            ))
        }
        "ping" => Some(JsonRpcResponse::success(id, json!({}))),
        _ => Some(JsonRpcResponse::error(
            id,
            codes::METHOD_NOT_FOUND,
            format!("method not found: {}", req.method),
        )),
    }
}
