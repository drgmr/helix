//! Claude Code IDE integration for Helix.
//!
//! Runs a WebSocket MCP server on localhost that the Claude Code CLI can
//! auto-discover via a lockfile in `~/.claude/ide/<port>.lock`. Exposes the
//! currently open file, selection, diagnostics, workspace folders, and a few
//! file operations as MCP tools; pushes `selection_changed` and `at_mentioned`
//! notifications.

use std::{path::PathBuf, sync::Arc};

use arc_swap::ArcSwap;
use tokio::sync::{broadcast, mpsc};

mod command;
mod lockfile;
mod mcp;
mod notification;
mod protocol;
mod server;
mod snapshot;
mod tools;

pub use command::{Command, DiffEdit, OpenFileArgs};
pub use notification::{AtMention, Notification, SelectionNotification, TextSelection};
pub use snapshot::{
    ActiveSelection, DiagnosticEntry, DiagnosticRange, OpenEditor, Position, SelectionRange,
    Snapshot,
};

/// Compute a unified diff suitable for display as a `diff`-format scratch
/// buffer in Helix.
pub fn unified_diff(old: &str, new: &str, old_label: &str, new_label: &str) -> String {
    let diff = similar::TextDiff::from_lines(old, new);
    let mut out = String::new();
    out.push_str(&format!("--- {old_label}\n+++ {new_label}\n"));
    for hunk in diff.unified_diff().iter_hunks() {
        out.push_str(&hunk.to_string());
    }
    out
}

const DEFAULT_COMMAND_CAP: usize = 64;
const NOTIFICATION_CAP: usize = 256;

/// Configuration for the Claude Code IDE server.
#[derive(Debug, Clone)]
pub struct Config {
    pub bind: std::net::IpAddr,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            bind: std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
        }
    }
}

/// Returned once from [`start`]. Hooks write into `snapshot` and `notifications`;
/// the main event loop drains `commands`. Drop the handle to shut the server down.
pub struct Handle {
    pub snapshot: Arc<ArcSwap<Snapshot>>,
    pub notifications: broadcast::Sender<Notification>,
    pub commands: mpsc::Receiver<Command>,
    pub port: u16,
    pub auth_token: String,
    lockfile: lockfile::LockFileGuard,
    _shutdown: mpsc::Sender<()>,
}

impl Handle {
    pub fn lockfile_path(&self) -> &std::path::Path {
        self.lockfile.path()
    }
}

/// Start the IDE server. Binds a random port synchronously, writes the
/// lockfile, and spawns the accept loop onto the current Tokio runtime
/// (must be called from within a runtime, e.g., from `#[tokio::main]`).
pub fn start(config: Config, workspace_folders: Vec<PathBuf>) -> anyhow::Result<Handle> {
    let auth_token = uuid::Uuid::new_v4().to_string();
    let snapshot = Arc::new(ArcSwap::from_pointee(Snapshot::empty(workspace_folders.clone())));
    let (notif_tx, _) = broadcast::channel(NOTIFICATION_CAP);
    let (cmd_tx, cmd_rx) = mpsc::channel(DEFAULT_COMMAND_CAP);
    let (shutdown_tx, shutdown_rx) = mpsc::channel::<()>(1);

    let std_listener = std::net::TcpListener::bind((config.bind, 0))?;
    std_listener.set_nonblocking(true)?;
    let port = std_listener.local_addr()?.port();
    let listener = tokio::net::TcpListener::from_std(std_listener)?;

    let lockfile = lockfile::write_lockfile(port, &auth_token, &workspace_folders)?;

    let state = Arc::new(server::State {
        snapshot: snapshot.clone(),
        notifications: notif_tx.clone(),
        commands: cmd_tx,
        auth_token: auth_token.clone(),
    });

    tokio::spawn(server::run(listener, state, shutdown_rx));

    log::info!(
        "helix-claude-ide: listening on 127.0.0.1:{port}, lockfile {}",
        lockfile.path().display()
    );

    Ok(Handle {
        snapshot,
        notifications: notif_tx,
        commands: cmd_rx,
        port,
        auth_token,
        lockfile,
        _shutdown: shutdown_tx,
    })
}
