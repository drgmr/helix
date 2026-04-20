//! Read-only snapshot of editor state, refreshed by event hooks and read
//! directly by MCP tools without round-tripping through the main loop.

use std::path::PathBuf;

use serde::Serialize;

/// Line / UTF-16 character position (VS Code's native coordinate system).
#[derive(Debug, Clone, Copy, Serialize)]
pub struct Position {
    pub line: usize,
    pub character: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct OpenEditor {
    #[serde(rename = "filePath")]
    pub file_path: String,
    #[serde(rename = "languageId")]
    pub language_id: String,
    #[serde(rename = "isDirty")]
    pub is_dirty: bool,
    #[serde(rename = "isActive")]
    pub is_active: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct DiagnosticEntry {
    pub uri: String,
    pub severity: String,
    pub message: String,
    pub source: Option<String>,
    pub range: DiagnosticRange,
}

#[derive(Debug, Clone, Serialize)]
pub struct DiagnosticRange {
    pub start: Position,
    pub end: Position,
}

#[derive(Debug, Clone, Serialize)]
pub struct ActiveSelection {
    #[serde(rename = "filePath")]
    pub file_path: String,
    #[serde(rename = "fileUrl")]
    pub file_url: String,
    pub text: String,
    pub selection: SelectionRange,
}

#[derive(Debug, Clone, Serialize)]
pub struct SelectionRange {
    pub start: Position,
    pub end: Position,
    #[serde(rename = "isEmpty")]
    pub is_empty: bool,
}

#[derive(Debug, Clone)]
pub struct Snapshot {
    pub workspace_folders: Vec<PathBuf>,
    pub open_editors: Vec<OpenEditor>,
    /// The active editor's selection (or None if no document is focused).
    pub active_selection: Option<ActiveSelection>,
    /// Most recent non-trivial selection, persisted across focus changes.
    pub latest_selection: Option<ActiveSelection>,
    pub diagnostics: Vec<DiagnosticEntry>,
}

impl Snapshot {
    pub fn empty(workspace_folders: Vec<PathBuf>) -> Self {
        Self {
            workspace_folders,
            open_editors: Vec::new(),
            active_selection: None,
            latest_selection: None,
            diagnostics: Vec::new(),
        }
    }
}
