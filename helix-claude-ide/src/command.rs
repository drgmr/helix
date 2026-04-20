//! Commands the server sends back to the main editor loop. Each carries a
//! oneshot reply channel so the MCP tool call can `await` the result.

use std::path::PathBuf;

use tokio::sync::oneshot;

pub type Reply<T> = oneshot::Sender<anyhow::Result<T>>;

pub struct OpenFileArgs {
    pub path: PathBuf,
    pub preview: bool,
    pub start_text: Option<String>,
    pub end_text: Option<String>,
    pub select_to_end_of_line: bool,
    pub make_frontmost: bool,
}

#[derive(Debug, Clone)]
pub struct DiffEdit {
    pub old_path: PathBuf,
    pub new_path: PathBuf,
    pub new_contents: String,
    pub tab_name: String,
}

pub enum Command {
    OpenFile {
        args: OpenFileArgs,
        reply: Reply<()>,
    },
    SaveDocument {
        path: PathBuf,
        reply: Reply<()>,
    },
    OpenDiff {
        edit: DiffEdit,
        reply: Reply<()>,
    },
    CloseAllDiffTabs {
        reply: Reply<usize>,
    },
}
