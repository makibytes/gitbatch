//! Worktree data and background actions: parsing `git worktree list
//! --porcelain` output into display rows, resolving a selected worktree's
//! context (is it the primary checkout? is it locked?), and running a
//! worktree action entirely off the key-handler path. None of this touches
//! `App` directly — see `App::spawn_worktree_op` for the caller.

use std::path::{Path, PathBuf};

use crate::{Result, git::GitRunner};

/// A worktree action, executed entirely off the synchronous key-handler
/// path (see `App::spawn_worktree_op`). `Remove` and `ToggleLock` first
/// resolve the selected worktree's context (is it the primary checkout? is
/// it locked?) — a step `worktree_context` used to run inline in the key
/// handler, blocking the UI for the round-trip and, on error, unwinding out
/// of `handle_key` and killing the whole session.
#[derive(Clone)]
pub(super) enum WorktreeOp {
    Remove,
    ToggleLock,
    Prune,
    Add { dest: PathBuf, branch: String },
}

impl WorktreeOp {
    pub(super) fn label(&self) -> &'static str {
        match self {
            Self::Remove => "worktree removed",
            Self::ToggleLock => "worktree lock toggled",
            Self::Prune => "worktree pruned",
            Self::Add { .. } => "new worktree done",
        }
    }
}

/// Run `op` against the worktree at `selected`. `Remove`/`ToggleLock`
/// resolve `worktree_context` first and refuse on the primary checkout,
/// matching the guard the old synchronous handlers enforced inline.
pub(super) async fn run_worktree_op(
    runner: &GitRunner,
    selected: &Path,
    op: WorktreeOp,
) -> Result<String> {
    match op {
        WorktreeOp::Prune => runner.worktree_prune(selected).await,
        WorktreeOp::Add { dest, branch } => {
            runner.worktree_add(selected, &dest, &branch, true).await
        }
        WorktreeOp::Remove => {
            let ctx = worktree_context(runner, selected).await?;
            if ctx.selected_is_primary {
                return Err(crate::error::AppError::Config(
                    "cannot remove primary worktree".into(),
                ));
            }
            runner.worktree_remove(&ctx.primary_path, selected).await
        }
        WorktreeOp::ToggleLock => {
            let ctx = worktree_context(runner, selected).await?;
            if ctx.selected_is_primary {
                return Err(crate::error::AppError::Config(
                    "cannot lock primary worktree".into(),
                ));
            }
            if ctx.selected_is_locked {
                runner.worktree_unlock(&ctx.primary_path, selected).await
            } else {
                runner.worktree_lock(&ctx.primary_path, selected).await
            }
        }
    }
}

// ── Worktree display rows ─────────────────────────────────────────────────────

pub(super) struct WtDisplayRow {
    pub(super) path: PathBuf,
    pub(super) display_name: String,
    pub(super) wt_label: String,
    /// The owning repo's *own* path (i.e. `RepoView::snapshot.path`), not an
    /// index into `App::repos` — an index captured when worktrees were
    /// loaded silently goes stale the next time `repos` is re-sorted (e.g.
    /// the periodic refresh), pairing this row with the wrong repo. A path
    /// stays a valid, stable key across any reordering.
    pub(super) repo_path: PathBuf,
}

pub(super) struct WtEntry {
    pub(super) path: PathBuf,
    pub(super) is_primary: bool,
}

pub(super) fn parse_worktree_listing(text: &str) -> Vec<WtEntry> {
    let mut entries: Vec<WtEntry> = Vec::new();
    let mut current_path: Option<PathBuf> = None;

    for line in text.lines() {
        if let Some(path_str) = line.strip_prefix("worktree ") {
            if let Some(path) = current_path.take() {
                let is_primary = entries.is_empty();
                entries.push(WtEntry { path, is_primary });
            }
            current_path = Some(PathBuf::from(path_str));
        }
    }
    if let Some(path) = current_path {
        let is_primary = entries.is_empty();
        entries.push(WtEntry { path, is_primary });
    }
    entries
}

// ── Worktree context ──────────────────────────────────────────────────────────

struct WorktreeContext {
    primary_path: PathBuf,
    selected_is_primary: bool,
    selected_is_locked: bool,
}

async fn worktree_context(runner: &GitRunner, selected: &Path) -> Result<WorktreeContext> {
    let listing = runner.worktree_list(selected).await?;
    let selected_norm = normalize_path(selected);
    let mut primary_path = selected.to_path_buf();
    let mut current_path: Option<PathBuf> = None;
    let mut selected_is_locked = false;
    let mut first = true;

    for line in listing.lines() {
        if let Some(path_str) = line.strip_prefix("worktree ") {
            let path = PathBuf::from(path_str);
            if first {
                primary_path = path.clone();
                first = false;
            }
            current_path = Some(path);
        } else if line.trim() == "locked"
            && current_path
                .as_ref()
                .is_some_and(|p| normalize_path(p) == selected_norm)
        {
            selected_is_locked = true;
        }
    }

    let selected_is_primary = normalize_path(&primary_path) == selected_norm;

    Ok(WorktreeContext {
        primary_path,
        selected_is_primary,
        selected_is_locked,
    })
}

fn normalize_path(path: &Path) -> String {
    path.canonicalize()
        .unwrap_or_else(|_| path.to_path_buf())
        .to_string_lossy()
        .into_owned()
}

/// Default sibling path for a new linked worktree: `<repo>.<branch-sanitized>`
/// next to the repo it's created from.
pub(super) fn default_worktree_path(repo_path: &Path, branch: &str) -> PathBuf {
    let repo_name = repo_path
        .file_name()
        .map_or_else(|| "worktree".into(), |n| n.to_string_lossy().into_owned());
    let sanitized: String = branch
        .chars()
        .map(|c| match c {
            'a'..='z' | 'A'..='Z' | '0'..='9' => c,
            _ => '-',
        })
        .collect::<String>()
        .trim_matches('-')
        .to_string();
    repo_path
        .parent()
        .unwrap_or(repo_path)
        .join(format!("{repo_name}.{sanitized}"))
}
