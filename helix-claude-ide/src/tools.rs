//! MCP tool definitions + dispatcher. The registered set mirrors the Claude
//! Code VS Code extension so the CLI can talk to Helix unchanged.

use std::{path::PathBuf, sync::Arc};

use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::oneshot;

use crate::{
    command::{Command, DiffEdit, OpenFileArgs},
    server::State,
};

/// Result of a tool call, already shaped as the MCP `tools/call` response.
pub struct ToolOutput {
    pub content: Value,
    pub is_error: bool,
}

impl ToolOutput {
    pub fn text(s: impl Into<String>) -> Self {
        Self {
            content: json!([{ "type": "text", "text": s.into() }]),
            is_error: false,
        }
    }

    pub fn json(v: &impl serde::Serialize) -> Self {
        let text = serde_json::to_string_pretty(v).unwrap_or_else(|_| "{}".into());
        Self::text(text)
    }

    pub fn error(s: impl Into<String>) -> Self {
        Self {
            content: json!([{ "type": "text", "text": s.into() }]),
            is_error: true,
        }
    }
}

pub fn tool_list() -> Value {
    json!([
        {
            "name": "openDiff",
            "description": "Open a git diff for the file",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "old_file_path": { "type": "string" },
                    "new_file_path": { "type": "string" },
                    "new_file_contents": { "type": "string" },
                    "tab_name": { "type": "string" }
                },
                "required": ["old_file_path", "new_file_path", "new_file_contents", "tab_name"]
            }
        },
        {
            "name": "getDiagnostics",
            "description": "Get language diagnostics from the editor",
            "inputSchema": {
                "type": "object",
                "properties": { "uri": { "type": "string" } }
            }
        },
        {
            "name": "closeAllDiffTabs",
            "description": "Close all diff tabs in the editor",
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "openFile",
            "description": "Open a file in the editor and optionally select a range of text",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "filePath": { "type": "string" },
                    "preview": { "type": "boolean", "default": false },
                    "startText": { "type": "string" },
                    "endText": { "type": "string" },
                    "selectToEndOfLine": { "type": "boolean", "default": false },
                    "makeFrontmost": { "type": "boolean", "default": true }
                },
                "required": ["filePath"]
            }
        },
        {
            "name": "getOpenEditors",
            "description": "Get information about currently open editors",
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "getWorkspaceFolders",
            "description": "Get all workspace folders currently open in the IDE",
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "getCurrentSelection",
            "description": "Get the current text selection in the active editor",
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "checkDocumentDirty",
            "description": "Check if a document has unsaved changes (is dirty)",
            "inputSchema": {
                "type": "object",
                "properties": { "filePath": { "type": "string" } },
                "required": ["filePath"]
            }
        },
        {
            "name": "saveDocument",
            "description": "Save a document with unsaved changes",
            "inputSchema": {
                "type": "object",
                "properties": { "filePath": { "type": "string" } },
                "required": ["filePath"]
            }
        },
        {
            "name": "getLatestSelection",
            "description": "Get the most recent text selection (even if not in the active editor)",
            "inputSchema": { "type": "object", "properties": {} }
        }
    ])
}

pub async fn dispatch(state: &Arc<State>, name: &str, args: Value) -> ToolOutput {
    match name {
        "getCurrentSelection" => {
            let snap = state.snapshot.load();
            match &snap.active_selection {
                Some(sel) => ToolOutput::json(sel),
                None => ToolOutput::text("{}"),
            }
        }
        "getLatestSelection" => {
            let snap = state.snapshot.load();
            match &snap.latest_selection {
                Some(sel) => ToolOutput::json(sel),
                None => ToolOutput::text("{}"),
            }
        }
        "getOpenEditors" => {
            let snap = state.snapshot.load();
            ToolOutput::json(&json!({ "tabs": snap.open_editors }))
        }
        "getWorkspaceFolders" => {
            let snap = state.snapshot.load();
            let folders: Vec<_> = snap
                .workspace_folders
                .iter()
                .map(|p| {
                    json!({
                        "name": p.file_name().map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_else(|| p.display().to_string()),
                        "uri": format!("file://{}", p.display()),
                        "path": p.display().to_string(),
                    })
                })
                .collect();
            ToolOutput::json(&json!({ "folders": folders }))
        }
        "getDiagnostics" => {
            #[derive(Deserialize, Default)]
            struct Args {
                uri: Option<String>,
            }
            let args: Args = serde_json::from_value(args).unwrap_or_default();
            let snap = state.snapshot.load();
            let list: Vec<_> = snap
                .diagnostics
                .iter()
                .filter(|d| args.uri.as_deref().map_or(true, |u| u == d.uri))
                .cloned()
                .collect();
            ToolOutput::json(&list)
        }
        "checkDocumentDirty" => {
            #[derive(Deserialize)]
            struct Args {
                #[serde(rename = "filePath")]
                file_path: String,
            }
            let args: Args = match serde_json::from_value(args) {
                Ok(a) => a,
                Err(e) => return ToolOutput::error(format!("invalid args: {e}")),
            };
            let snap = state.snapshot.load();
            let dirty = snap
                .open_editors
                .iter()
                .find(|e| e.file_path == args.file_path)
                .map(|e| e.is_dirty);
            match dirty {
                Some(d) => ToolOutput::json(&json!({ "isDirty": d })),
                None => ToolOutput::error(format!("document not open: {}", args.file_path)),
            }
        }
        "saveDocument" => {
            #[derive(Deserialize)]
            struct Args {
                #[serde(rename = "filePath")]
                file_path: String,
            }
            let args: Args = match serde_json::from_value(args) {
                Ok(a) => a,
                Err(e) => return ToolOutput::error(format!("invalid args: {e}")),
            };
            let (tx, rx) = oneshot::channel();
            let cmd = Command::SaveDocument {
                path: PathBuf::from(&args.file_path),
                reply: tx,
            };
            send_and_await(state, cmd, rx, "saveDocument").await
        }
        "openFile" => {
            #[derive(Deserialize)]
            struct Args {
                #[serde(rename = "filePath")]
                file_path: String,
                #[serde(default)]
                preview: bool,
                #[serde(rename = "startText", default)]
                start_text: Option<String>,
                #[serde(rename = "endText", default)]
                end_text: Option<String>,
                #[serde(rename = "selectToEndOfLine", default)]
                select_to_end_of_line: bool,
                #[serde(rename = "makeFrontmost", default = "default_true")]
                make_frontmost: bool,
            }
            fn default_true() -> bool {
                true
            }
            let args: Args = match serde_json::from_value(args) {
                Ok(a) => a,
                Err(e) => return ToolOutput::error(format!("invalid args: {e}")),
            };
            let (tx, rx) = oneshot::channel();
            let cmd = Command::OpenFile {
                args: OpenFileArgs {
                    path: PathBuf::from(&args.file_path),
                    preview: args.preview,
                    start_text: args.start_text,
                    end_text: args.end_text,
                    select_to_end_of_line: args.select_to_end_of_line,
                    make_frontmost: args.make_frontmost,
                },
                reply: tx,
            };
            send_and_await(state, cmd, rx, "openFile").await
        }
        "openDiff" => {
            #[derive(Deserialize)]
            struct Args {
                old_file_path: String,
                new_file_path: String,
                new_file_contents: String,
                tab_name: String,
            }
            let args: Args = match serde_json::from_value(args) {
                Ok(a) => a,
                Err(e) => return ToolOutput::error(format!("invalid args: {e}")),
            };
            let (tx, rx) = oneshot::channel();
            let cmd = Command::OpenDiff {
                edit: DiffEdit {
                    old_path: PathBuf::from(args.old_file_path),
                    new_path: PathBuf::from(args.new_file_path),
                    new_contents: args.new_file_contents,
                    tab_name: args.tab_name,
                },
                reply: tx,
            };
            send_and_await(state, cmd, rx, "openDiff").await
        }
        "closeAllDiffTabs" => {
            let (tx, rx) = oneshot::channel();
            let cmd = Command::CloseAllDiffTabs { reply: tx };
            match send_and_await_value(state, cmd, rx, "closeAllDiffTabs").await {
                Ok(n) => ToolOutput::json(&json!({ "closedCount": n })),
                Err(e) => ToolOutput::error(e),
            }
        }
        other => ToolOutput::error(format!("unknown tool: {other}")),
    }
}

async fn send_and_await(
    state: &Arc<State>,
    cmd: Command,
    rx: oneshot::Receiver<anyhow::Result<()>>,
    label: &str,
) -> ToolOutput {
    if state.commands.send(cmd).await.is_err() {
        return ToolOutput::error(format!("{label}: editor unavailable"));
    }
    match rx.await {
        Ok(Ok(())) => ToolOutput::text("ok"),
        Ok(Err(e)) => ToolOutput::error(format!("{label}: {e}")),
        Err(_) => ToolOutput::error(format!("{label}: editor dropped reply")),
    }
}

async fn send_and_await_value<T>(
    state: &Arc<State>,
    cmd: Command,
    rx: oneshot::Receiver<anyhow::Result<T>>,
    label: &str,
) -> Result<T, String> {
    if state.commands.send(cmd).await.is_err() {
        return Err(format!("{label}: editor unavailable"));
    }
    match rx.await {
        Ok(Ok(v)) => Ok(v),
        Ok(Err(e)) => Err(format!("{label}: {e}")),
        Err(_) => Err(format!("{label}: editor dropped reply")),
    }
}
