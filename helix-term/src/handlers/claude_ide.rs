//! Integration layer between `helix-claude-ide` and the editor. Refreshes
//! the shared snapshot from editor events, pushes notifications (debounced)
//! to connected Claude Code CLI clients, and drains the command channel
//! into editor mutations on the main loop.

use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock},
    time::Duration,
};

use anyhow::anyhow;
use arc_swap::ArcSwap;
use helix_claude_ide::{
    AtMention, Command, DiffEdit, Handle, Notification, OpenFileArgs, Position,
    SelectionNotification, Snapshot, TextSelection,
};
use helix_core::Rope;
use helix_event::{register_hook, send_blocking, AsyncHook};
use helix_view::{
    events::{
        DiagnosticsDidChange, DocumentDidChange, DocumentDidClose, DocumentDidOpen,
        DocumentFocusLost, SelectionDidChange,
    },
    Document, DocumentId, Editor,
};
use tokio::{sync::broadcast, time::Instant};

use crate::compositor::Context;
use crate::job::Jobs;

static NOTIFIER: OnceLock<broadcast::Sender<Notification>> = OnceLock::new();

/// Scratch buffers we've opened for `openDiff`, tracked so `closeAllDiffTabs`
/// can close them without false positives on other scratch buffers the user
/// may have created.
static DIFF_BUFFERS: Mutex<Option<HashSet<DocumentId>>> = Mutex::new(None);

fn record_diff_buffer(id: DocumentId) {
    let mut guard = DIFF_BUFFERS.lock().unwrap();
    guard.get_or_insert_with(HashSet::new).insert(id);
}

fn take_diff_buffers() -> Vec<DocumentId> {
    let mut guard = DIFF_BUFFERS.lock().unwrap();
    match guard.as_mut() {
        Some(set) => set.drain().collect(),
        None => Vec::new(),
    }
}

/// Record the broadcast sender so `mention_current_selection` can reach it
/// from a typed command / keybinding context.
pub fn install_notifier(tx: broadcast::Sender<Notification>) {
    let _ = NOTIFIER.set(tx);
}

/// Held on `Application`. Keeps the handle alive (so the lockfile sticks around
/// for the lifetime of the editor) and gives the main loop a drain point.
pub struct ClaudeIde {
    pub handle: Handle,
}

impl ClaudeIde {
    pub fn new(handle: Handle) -> Self {
        Self { handle }
    }
}

/// Called once during `Application::new` after the server is spawned.
/// Registers all synchronous event hooks that feed the shared snapshot +
/// notification broadcast.
pub fn register_hooks(
    snapshot: Arc<ArcSwap<Snapshot>>,
    notifications: broadcast::Sender<Notification>,
) {
    let selection_tx = SelectionDebounce::new(notifications.clone(), snapshot.clone()).spawn();

    let snap = snapshot.clone();
    let tx = selection_tx.clone();
    register_hook!(move |event: &mut SelectionDidChange<'_>| {
        let active = build_active_selection(event.doc, event.view);
        snap.rcu(|s| {
            let mut new = (**s).clone();
            new.active_selection = active.clone();
            new
        });
        if let Some(sel) = active {
            send_blocking(&tx, SelectionEvent::Updated(sel));
        }
        Ok(())
    });

    let snap = snapshot.clone();
    register_hook!(move |event: &mut DocumentDidOpen<'_>| {
        refresh_editors(&snap, event.editor);
        Ok(())
    });

    let snap = snapshot.clone();
    register_hook!(move |event: &mut DocumentDidClose<'_>| {
        refresh_editors(&snap, event.editor);
        Ok(())
    });

    let snap = snapshot.clone();
    register_hook!(move |event: &mut DocumentDidChange<'_>| {
        let path = event.doc.path().cloned();
        snap.rcu(|s| {
            let mut new = (**s).clone();
            if let Some(ref p) = path {
                let p_str = p.display().to_string();
                for entry in new.open_editors.iter_mut() {
                    if entry.file_path == p_str {
                        entry.is_dirty = true;
                    }
                }
            }
            new
        });
        Ok(())
    });

    let snap = snapshot.clone();
    register_hook!(move |event: &mut DiagnosticsDidChange<'_>| {
        refresh_diagnostics(&snap, event.editor);
        Ok(())
    });

    let snap = snapshot.clone();
    register_hook!(move |event: &mut DocumentFocusLost<'_>| {
        snap.rcu(|s| {
            let mut new = (**s).clone();
            new.latest_selection = new.active_selection.clone();
            new
        });
        refresh_editors(&snap, event.editor);
        Ok(())
    });
}

fn refresh_editors(snap: &ArcSwap<Snapshot>, editor: &Editor) {
    let active_doc_id = editor.tree.try_get(editor.tree.focus).map(|v| v.doc);
    let open_editors: Vec<_> = editor
        .documents
        .values()
        .filter_map(|d| {
            let path = d.path()?;
            Some(helix_claude_ide::OpenEditor {
                file_path: path.display().to_string(),
                language_id: d.language_name().unwrap_or("plaintext").to_string(),
                is_dirty: d.is_modified(),
                is_active: Some(d.id()) == active_doc_id,
            })
        })
        .collect();

    snap.rcu(|s| {
        let mut new = (**s).clone();
        new.open_editors = open_editors.clone();
        new
    });
}

fn refresh_diagnostics(snap: &ArcSwap<Snapshot>, editor: &Editor) {
    let list = collect_diagnostics(editor);
    snap.rcu(|s| {
        let mut new = (**s).clone();
        new.diagnostics = list.clone();
        new
    });
}

fn collect_diagnostics(editor: &Editor) -> Vec<helix_claude_ide::DiagnosticEntry> {
    use helix_claude_ide::{DiagnosticEntry, DiagnosticRange};
    let mut out = Vec::new();
    for (uri, items) in editor.diagnostics.iter() {
        let uri_str = uri.to_string();
        for (d, _provider) in items {
            out.push(DiagnosticEntry {
                uri: uri_str.clone(),
                severity: match d.severity {
                    Some(helix_lsp::lsp::DiagnosticSeverity::ERROR) => "error",
                    Some(helix_lsp::lsp::DiagnosticSeverity::WARNING) => "warning",
                    Some(helix_lsp::lsp::DiagnosticSeverity::INFORMATION) => "info",
                    Some(helix_lsp::lsp::DiagnosticSeverity::HINT) => "hint",
                    _ => "unknown",
                }
                .to_string(),
                message: d.message.clone(),
                source: d.source.clone(),
                range: DiagnosticRange {
                    start: Position {
                        line: d.range.start.line as usize,
                        character: d.range.start.character as usize,
                    },
                    end: Position {
                        line: d.range.end.line as usize,
                        character: d.range.end.character as usize,
                    },
                },
            });
        }
    }
    out
}

fn build_active_selection(
    doc: &Document,
    view: helix_view::ViewId,
) -> Option<helix_claude_ide::ActiveSelection> {
    let path = doc.path()?.clone();
    let selection = doc.selection(view);
    let primary = selection.primary();
    let text = doc.text();
    let (start_char, end_char) = (primary.from(), primary.to());

    // Helix always keeps the cursor as at least a 1-char range, so treat
    // anything that small as "just a cursor, no real selection."
    let is_empty = end_char.saturating_sub(start_char) <= 1;
    let selected: String = if is_empty {
        String::new()
    } else {
        text.slice(start_char..end_char).to_string()
    };
    let start = char_to_position(text, start_char);
    let end = char_to_position(text, end_char);
    Some(helix_claude_ide::ActiveSelection {
        file_path: path.display().to_string(),
        file_url: format!("file://{}", path.display()),
        text: selected,
        selection: helix_claude_ide::SelectionRange {
            start,
            end,
            is_empty,
        },
    })
}

fn char_to_position(text: &Rope, char_idx: usize) -> Position {
    let char_idx = char_idx.min(text.len_chars());
    let line = text.char_to_line(char_idx);
    let line_start = text.line_to_char(line);
    Position {
        line,
        character: char_idx - line_start,
    }
}

/// Called by the main event loop when a `Command` arrives from the server.
/// Must run on the editor thread with access to `Context`.
pub fn handle_command(cx: &mut Context<'_>, cmd: Command) {
    match cmd {
        Command::SaveDocument { path, reply } => {
            let result = save_document(cx.editor, &path);
            let _ = reply.send(result);
        }
        Command::OpenFile { args, reply } => {
            let result = open_file(cx.editor, args);
            let _ = reply.send(result);
        }
        Command::OpenDiff { edit, reply } => {
            let result = open_diff(cx.editor, edit);
            let _ = reply.send(result);
        }
        Command::CloseAllDiffTabs { reply } => {
            let n = close_all_diff_tabs(cx.editor);
            let _ = reply.send(Ok(n));
        }
    }
}

fn save_document(editor: &mut Editor, path: &Path) -> anyhow::Result<()> {
    let doc_id = editor
        .documents()
        .find(|d| d.path().map(|p| p == path).unwrap_or(false))
        .map(|d| d.id())
        .ok_or_else(|| anyhow!("document not open: {}", path.display()))?;
    editor.save(doc_id, None::<PathBuf>, false)?;
    Ok(())
}

fn open_file(editor: &mut Editor, args: OpenFileArgs) -> anyhow::Result<()> {
    use helix_view::editor::Action;
    let action = if args.make_frontmost {
        Action::Replace
    } else {
        Action::Load
    };
    let doc_id = editor.open(&args.path, action)?;

    if args.start_text.is_some() {
        let view_id = editor.tree.focus;
        let doc = editor
            .documents
            .get_mut(&doc_id)
            .ok_or_else(|| anyhow!("missing document after open"))?;
        if let Some(range) = locate_range(
            doc.text(),
            args.start_text.as_deref(),
            args.end_text.as_deref(),
            args.select_to_end_of_line,
        ) {
            let (start, end) = range;
            let sel = helix_core::Selection::single(start, end);
            doc.set_selection(view_id, sel);
        }
    }
    Ok(())
}

fn locate_range(
    text: &Rope,
    start_text: Option<&str>,
    end_text: Option<&str>,
    select_to_end_of_line: bool,
) -> Option<(usize, usize)> {
    let full: String = text.to_string();
    let s = start_text?;
    let start_byte = full.find(s)?;
    let start_char = text.byte_to_char(start_byte);
    let end_char = if let Some(e) = end_text {
        let end_byte = full[start_byte + s.len()..].find(e).map(|i| i + start_byte + s.len() + e.len())?;
        text.byte_to_char(end_byte)
    } else {
        text.byte_to_char(start_byte + s.len())
    };
    let end_char = if select_to_end_of_line {
        let line = text.char_to_line(end_char.saturating_sub(1).max(start_char));
        let line_end = text.line_to_char(line + 1).saturating_sub(1);
        line_end.max(end_char)
    } else {
        end_char
    };
    Some((start_char, end_char))
}

fn open_diff(editor: &mut Editor, edit: DiffEdit) -> anyhow::Result<()> {
    use helix_view::editor::Action;

    let old_contents = std::fs::read_to_string(&edit.old_path).unwrap_or_default();
    let diff = helix_claude_ide::unified_diff(
        &old_contents,
        &edit.new_contents,
        &edit.old_path.display().to_string(),
        &edit.new_path.display().to_string(),
    );

    // Replace the current view so the user sees the diff where they already are,
    // rather than splitting. A new scratch buffer is allocated; the previous
    // document stays open in the editor and can be switched back to.
    let doc_id = editor.new_file(Action::Replace);
    let loader = editor.syn_loader.load_full();
    let display_path = diff_display_path(&edit.tab_name);
    let doc = editor
        .documents
        .get_mut(&doc_id)
        .ok_or_else(|| anyhow!("failed to allocate diff scratch buffer"))?;

    let transaction = helix_core::Transaction::change(
        doc.text(),
        std::iter::once((0, doc.text().len_chars(), Some(diff.into()))),
    );
    let view_id = editor.tree.focus;
    doc.apply(&transaction, view_id);

    // Auto-apply the `diff` language so highlighting works without user action.
    if let Err(e) = doc.set_language_by_language_id("diff", &loader) {
        log::debug!("claude-ide: could not set diff language: {e}");
    }

    // Give the buffer a recognizable name visible in `<space>b` / the
    // statusline. Mark it read-only so `:w` doesn't accidentally persist it.
    doc.set_path(Some(&display_path));
    doc.readonly = true;

    record_diff_buffer(doc_id);
    Ok(())
}

fn diff_display_path(tab_name: &str) -> PathBuf {
    let sanitized: String = tab_name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' { c } else { '-' })
        .collect();
    let name = if sanitized.is_empty() {
        "diff".into()
    } else {
        sanitized
    };
    std::env::temp_dir().join(format!("claude-diff-{name}.diff"))
}

fn close_all_diff_tabs(editor: &mut Editor) -> usize {
    let ids = take_diff_buffers();
    let mut closed = 0;
    for id in ids {
        // Force-close: diff buffers are read-only scratch views; the user
        // never expected to save them.
        if editor.close_document(id, true).is_ok() {
            closed += 1;
        }
    }
    closed
}

// ---------- selection debounce (async hook) ----------

#[derive(Debug)]
enum SelectionEvent {
    Updated(helix_claude_ide::ActiveSelection),
}

struct SelectionDebounce {
    pending: Option<helix_claude_ide::ActiveSelection>,
    notifications: broadcast::Sender<Notification>,
    snapshot: Arc<ArcSwap<Snapshot>>,
}

impl SelectionDebounce {
    fn new(
        notifications: broadcast::Sender<Notification>,
        snapshot: Arc<ArcSwap<Snapshot>>,
    ) -> Self {
        Self {
            pending: None,
            notifications,
            snapshot,
        }
    }
}

impl AsyncHook for SelectionDebounce {
    type Event = SelectionEvent;

    fn handle_event(
        &mut self,
        event: Self::Event,
        _existing: Option<Instant>,
    ) -> Option<Instant> {
        match event {
            SelectionEvent::Updated(sel) => {
                self.pending = Some(sel);
                Some(Instant::now() + Duration::from_millis(300))
            }
        }
    }

    fn finish_debounce(&mut self) {
        if let Some(sel) = self.pending.take() {
            // Persist as latest selection too.
            self.snapshot.rcu(|s| {
                let mut new = (**s).clone();
                new.latest_selection = Some(sel.clone());
                new
            });
            let notif = SelectionNotification {
                text: sel.text,
                file_path: sel.file_path,
                file_url: sel.file_url,
                selection: TextSelection {
                    start: sel.selection.start,
                    end: sel.selection.end,
                    is_empty: sel.selection.is_empty,
                },
            };
            let _ = self.notifications.send(Notification::SelectionChanged(notif));
        }
    }
}

// ---------- at-mention command helper ----------

/// Compute the 0-indexed inclusive line range to send with an at-mention.
/// The Claude Code IDE protocol uses 0-indexed lines (LSP convention).
///
/// Returns `(None, None)` when every range looks like a parked cursor
/// (Helix's minimum 1-char range) so Claude treats it as "whole file."
fn mention_line_range(
    text: &Rope,
    selection: &helix_core::Selection,
) -> (Option<usize>, Option<usize>) {
    let has_real_selection = selection.ranges().iter().any(|r| r.to() - r.from() > 1);
    if !has_real_selection {
        return (None, None);
    }
    let mut min_line = usize::MAX;
    let mut max_line = 0usize;
    for r in selection.ranges() {
        if r.to() - r.from() <= 1 {
            continue;
        }
        let s = text.char_to_line(r.from());
        let e = text.char_to_line(r.to().saturating_sub(1).max(r.from()));
        min_line = min_line.min(s);
        max_line = max_line.max(e);
    }
    (Some(min_line), Some(max_line))
}

/// Build an `at_mentioned` notification from the current editor state and send
/// it. Called by the `<space>@` keybinding and the `:claude-ide-mention` cmd.
pub fn mention_current_selection(editor: &mut Editor) -> anyhow::Result<()> {
    let view_id = editor.tree.focus;
    let doc = editor
        .tree
        .try_get(view_id)
        .and_then(|v| editor.documents.get(&v.doc))
        .ok_or_else(|| anyhow!("no active document"))?;
    let path = doc
        .path()
        .ok_or_else(|| anyhow!("document has no path"))?
        .clone();
    let selection = doc.selection(view_id);
    let text = doc.text();
    let (line_start, line_end) = mention_line_range(text, selection);

    let tx = NOTIFIER.get().ok_or_else(|| {
        anyhow!("claude-code IDE server not running (enable with editor.claude-ide.enable)")
    })?;
    let mention = AtMention {
        file_path: path.display().to_string(),
        line_start,
        line_end,
    };
    let _ = tx.send(Notification::AtMentioned(mention));
    editor.set_status("sent selection to Claude Code");
    Ok(())
}

/// Called from `Application::event_loop_until_idle` on each incoming
/// `Command`. Runs on the editor thread.
pub fn drain(editor: &mut Editor, jobs: &mut Jobs, cmd: Command) {
    let mut cx = Context {
        editor,
        jobs,
        scroll: None,
    };
    handle_command(&mut cx, cmd);
}

#[cfg(test)]
mod tests {
    use super::mention_line_range;
    use helix_core::{Range, Rope, Selection};

    fn rope(s: &str) -> Rope {
        Rope::from_str(s)
    }

    /// Helper: line `n` (1-indexed) start char index in `text`.
    fn line_start(text: &Rope, line_1: usize) -> usize {
        text.line_to_char(line_1 - 1)
    }

    #[test]
    fn cursor_only_no_selection() {
        let text = rope("abc\ndef\nghi\n");
        // 1-char cursor parked on 'd' (line 2, col 0).
        let sel = Selection::single(4, 5);
        assert_eq!(mention_line_range(&text, &sel), (None, None));
    }

    #[test]
    fn one_char_selection_treated_as_no_selection() {
        // Single char produced by `f`-style motion onto the next char.
        let text = rope("abc\ndef\nghi\n");
        let sel = Selection::single(4, 5);
        assert_eq!(mention_line_range(&text, &sel), (None, None));
    }

    #[test]
    fn word_selection_within_line_3() {
        let text = rope("aaa\nbbb\nccc ddd\n");
        // Select "ccc" on line 3 (0-indexed line 2).
        let start = line_start(&text, 3);
        let sel = Selection::single(start, start + 3);
        assert_eq!(mention_line_range(&text, &sel), (Some(2), Some(2)));
    }

    #[test]
    fn x_selects_line_3_only() {
        // After `x` on line 3 (0-indexed line 2): [start_line_3, start_line_4).
        let text = rope("aaa\nbbb\nccc\nddd\n");
        let from = line_start(&text, 3);
        let to = line_start(&text, 4);
        let sel = Selection::single(from, to);
        assert_eq!(mention_line_range(&text, &sel), (Some(2), Some(2)));
    }

    #[test]
    fn xx_selects_lines_3_and_4() {
        let text = rope("aaa\nbbb\nccc\nddd\neee\n");
        let from = line_start(&text, 3);
        let to = line_start(&text, 5);
        let sel = Selection::single(from, to);
        assert_eq!(mention_line_range(&text, &sel), (Some(2), Some(3)));
    }

    #[test]
    fn selection_ending_at_col_0_excludes_trailing_line() {
        // Selection from line 2 col 0 through line 4 col 0 — visually,
        // lines 2 and 3 are highlighted; line 4 is just where the cursor parks.
        let text = rope("aaa\nbbb\nccc\nddd\n");
        let from = line_start(&text, 2);
        let to = line_start(&text, 4);
        let sel = Selection::single(from, to);
        assert_eq!(mention_line_range(&text, &sel), (Some(1), Some(2)));
    }

    #[test]
    fn selection_ending_mid_line_includes_that_line() {
        let text = rope("aaa\nbbb\nccc\nddd\n");
        let from = line_start(&text, 2);
        let to = line_start(&text, 4) + 2; // mid line 4
        let sel = Selection::single(from, to);
        assert_eq!(mention_line_range(&text, &sel), (Some(1), Some(3)));
    }

    #[test]
    fn x_on_last_line_with_no_trailing_newline() {
        // File whose last line has no trailing \n. `x` on that final line.
        // Helix's extend_line_below clamps `to` to len_lines(), so
        // selection becomes [start_last, len_chars).
        let text = rope("aaa\nbbb\nccc");
        let from = line_start(&text, 3);
        let to = text.len_chars();
        let sel = Selection::single(from, to);
        assert_eq!(mention_line_range(&text, &sel), (Some(2), Some(2)));
    }

    #[test]
    fn x_on_line_with_multibyte_grapheme_at_eol() {
        let text = rope("aaa\nbb🚀\nccc\n");
        let from = line_start(&text, 2);
        let to = line_start(&text, 3);
        let sel = Selection::single(from, to);
        assert_eq!(mention_line_range(&text, &sel), (Some(1), Some(1)));
    }

    #[test]
    fn multi_cursor_lines_2_and_5_with_real_selections() {
        let text = rope("aaa\nbbb\nccc\nddd\neee\nfff\n");
        let r1_from = line_start(&text, 2);
        let r1_to = line_start(&text, 3);
        let r2_from = line_start(&text, 5);
        let r2_to = line_start(&text, 6);
        let sel = Selection::new(
            smallvec::smallvec![Range::new(r1_from, r1_to), Range::new(r2_from, r2_to)],
            0,
        );
        assert_eq!(mention_line_range(&text, &sel), (Some(1), Some(4)));
    }

    #[test]
    fn matches_users_repro_x_on_line_25_sends_24() {
        // User's repro: cursor on line 25 (1-indexed), `x`, `<space>@`.
        // We must send 0-indexed 24, which the Claude CLI renders as #L25.
        let mut s = String::new();
        for i in 1..=30 {
            s.push_str(&format!("line{i}\n"));
        }
        let text = rope(&s);
        let from = line_start(&text, 25);
        let to = line_start(&text, 26);
        let sel = Selection::single(from, to);
        assert_eq!(mention_line_range(&text, &sel), (Some(24), Some(24)));
    }
}
