//! Editor-to-CLI JSON-RPC notifications.

use serde::Serialize;

use crate::snapshot::Position;

#[derive(Debug, Clone, Serialize)]
pub struct TextSelection {
    pub start: Position,
    pub end: Position,
    #[serde(rename = "isEmpty")]
    pub is_empty: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct SelectionNotification {
    pub text: String,
    #[serde(rename = "filePath")]
    pub file_path: String,
    #[serde(rename = "fileUrl")]
    pub file_url: String,
    pub selection: TextSelection,
}

#[derive(Debug, Clone, Serialize)]
pub struct AtMention {
    #[serde(rename = "filePath")]
    pub file_path: String,
    #[serde(rename = "lineStart", skip_serializing_if = "Option::is_none")]
    pub line_start: Option<usize>,
    #[serde(rename = "lineEnd", skip_serializing_if = "Option::is_none")]
    pub line_end: Option<usize>,
}

#[derive(Debug, Clone)]
pub enum Notification {
    SelectionChanged(SelectionNotification),
    AtMentioned(AtMention),
}

impl Notification {
    pub(crate) fn method(&self) -> &'static str {
        match self {
            Notification::SelectionChanged(_) => "selection_changed",
            Notification::AtMentioned(_) => "at_mentioned",
        }
    }

    pub(crate) fn params(&self) -> serde_json::Value {
        match self {
            Notification::SelectionChanged(s) => serde_json::to_value(s).unwrap_or_default(),
            Notification::AtMentioned(s) => serde_json::to_value(s).unwrap_or_default(),
        }
    }
}
