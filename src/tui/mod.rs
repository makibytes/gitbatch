use std::{
    cmp::Ordering,
    collections::{HashSet, VecDeque},
    fmt::Write as _,
    io,
    path::{Path, PathBuf},
    process::Command,
    time::{Duration, Instant, SystemTime},
};

use crossterm::{
    event::{
        self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEventKind,
        KeyModifiers,
    },
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use futures::{StreamExt, future, stream};
use ratatui::{
    DefaultTerminal,
    prelude::*,
    widgets::{Block, Borders, Cell, Clear, Gauge, Paragraph, Row, Table},
};
use tokio::sync::mpsc;

use crate::{
    Result,
    config::AppConfig,
    git::{Credentials, GitRunner, RemoteAction, RepositorySnapshot},
    mode::Mode,
};

// ── Constants ─────────────────────────────────────────────────────────────────

const MIN_WIDTH: u16 = 50;
const MIN_HEIGHT: u16 = 8;
const TICK_MS: u64 = 80;
const REFRESH_SECS: u64 = 30;
const MAIN_PAGE_JUMP: isize = 10;

/// Concurrency cap for background git operations: `available_parallelism * 4`, min 4.
/// Matches the Go reference's `runtime.GOMAXPROCS(0) * 4` semaphore.
fn worker_limit() -> usize {
    std::thread::available_parallelism()
        .map_or(4, |n| n.get().saturating_mul(4))
        .max(4)
}

const SPINNER: [&str; 8] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧"];
const ICON_QUEUED: &str = "●";
const ICON_SUCCESS: &str = "✓";
const ICON_FAIL: &str = "✗";
const ICON_CONFLICT: &str = "⚠";
const ICON_LOCAL: &str = "~";
const ICON_AHEAD: &str = "↖";
const ICON_BEHIND: &str = "↘";
const ICON_CURSOR: &str = "›";

const SYM_PULL: &str = "↓";
const SYM_MERGE: &str = "↣";
const SYM_REBASE: &str = "↯";
const SYM_PUSH: &str = "↑";
const SYM_FETCH: &str = "↺";

// ── Color palette ─────────────────────────────────────────────────────────────

const C_BORDER: Color = Color::Rgb(70, 70, 90);
const C_TITLE_BG: Color = Color::Rgb(62, 35, 150);
const C_TITLE_FG: Color = Color::White;

const C_MODE_PULL_BG: Color = Color::Rgb(21, 101, 192);
const C_MODE_MERGE_BG: Color = Color::Rgb(0, 150, 136);
const C_MODE_REBASE_BG: Color = Color::Rgb(46, 125, 50);
const C_MODE_PUSH_BG: Color = Color::Rgb(230, 190, 40);
const C_MODE_FETCH_BG: Color = Color::Rgb(21, 101, 192);
const C_MODE_DARK_FG: Color = Color::Rgb(27, 27, 27);
const C_MODE_LIGHT_FG: Color = Color::White;

const C_SUCCESS_FG: Color = Color::Rgb(102, 187, 106);
const C_FAIL_FG: Color = Color::Rgb(239, 83, 80);
const C_QUEUED_FG: Color = Color::Rgb(66, 165, 245);
const C_WORKING_FG: Color = Color::Rgb(38, 198, 218);
const C_CONFLICT_FG: Color = Color::Rgb(180, 115, 55);
const C_LOCAL_FG: Color = Color::Rgb(255, 213, 79);
const C_NO_UPSTREAM_FG: Color = Color::Rgb(110, 110, 130);
const C_CREDS_FG: Color = Color::Rgb(171, 71, 188);
const C_HELP_FG: Color = Color::Rgb(140, 140, 160);
const C_DIM_FG: Color = Color::Rgb(90, 90, 110);
const C_NET_FG: Color = Color::Rgb(130, 140, 150);

const C_SEL_DEFAULT_BG: Color = Color::Rgb(21, 101, 192);
const C_SEL_SUCCESS_BG: Color = Color::Rgb(27, 94, 32);
const C_SEL_FAIL_BG: Color = Color::Rgb(183, 28, 28);
const C_SEL_CONFLICT_BG: Color = Color::Rgb(120, 68, 0);
const C_SEL_LOCAL_BG: Color = Color::Rgb(130, 100, 0);
const C_SEL_NO_UPSTREAM_BG: Color = Color::Rgb(55, 55, 70);
const C_SEL_CREDS_BG: Color = Color::Rgb(74, 20, 140);
const C_SEL_WORKING_BG: Color = Color::Rgb(0, 105, 120);
const C_SEL_QUEUED_BG: Color = Color::Rgb(13, 71, 161);

const C_POPUP_BG: Color = Color::Rgb(18, 18, 28);
const C_POPUP_BORDER: Color = Color::Rgb(100, 60, 200);
const C_POPUP_TITLE: Color = Color::Rgb(180, 140, 255);
const C_SECTION_HDR: Color = Color::Rgb(130, 100, 230);
const C_KEY_FG: Color = Color::Rgb(100, 180, 255);
const C_BRANCH_CUR: Color = Color::Rgb(77, 182, 172);
const C_GAUGE_FG: Color = Color::Rgb(100, 60, 200);
const C_GAUGE_BG: Color = Color::Rgb(40, 35, 55);

const C_AHEAD_FG: Color = Color::Rgb(102, 217, 131); // vibrant green — commits to push
const C_BEHIND_FG: Color = Color::Rgb(255, 183, 77); // amber — commits to pull
const C_AGE_FG: Color = Color::Rgb(95, 95, 115); // muted — relative age column
const C_STASH_FG: Color = Color::Rgb(150, 100, 200); // muted purple — stash count badge

// ── Local (non-remote) action types ──────────────────────────────────────────

#[derive(Clone)]
enum LocalAction {
    Commit(String),
    CreateBranch(String),
    Checkout(String),
    CheckoutRemote(String),
    DeleteBranch(String),
    ForceDeleteBranch(String),
    DeleteRemoteBranch(String),
    StashPush(Option<String>),
    StashPop,
    StashDrop,
    ResetUpstream { hard: bool },
    SetUpstream { remote: String, branch: String },
}

impl LocalAction {
    fn label(&self) -> &'static str {
        match self {
            Self::Commit(_) => "commit",
            Self::CreateBranch(_) => "new branch",
            Self::Checkout(_) => "checkout",
            Self::CheckoutRemote(_) => "checkout remote",
            Self::DeleteBranch(_) => "delete branch",
            Self::ForceDeleteBranch(_) => "force delete branch",
            Self::DeleteRemoteBranch(_) => "delete remote branch",
            Self::StashPush(_) => "stash push",
            Self::StashPop => "stash pop",
            Self::StashDrop => "stash drop",
            Self::ResetUpstream { hard: true } => "hard reset",
            Self::ResetUpstream { hard: false } => "reset to upstream",
            Self::SetUpstream { .. } => "set upstream",
        }
    }
}

// ── Background event types ────────────────────────────────────────────────────

enum BgEvent {
    RepoLoaded(RepositorySnapshot),
    RepoSkipped,
    OpResult {
        path: PathBuf,
        action: RemoteAction,
        result: crate::Result<String>,
        snapshot: Option<RepositorySnapshot>,
    },
    RefreshComplete {
        snapshots: Vec<(PathBuf, RepositorySnapshot)>,
    },
    WorktreesReady(Vec<WtDisplayRow>),
    /// Result of a spawned local (non-remote) git operation.
    LocalResult {
        path: PathBuf,
        label: &'static str,
        result: crate::Result<String>,
        snapshot: Option<RepositorySnapshot>,
    },
    /// Per-repo result of startup background fetch + FF-safety check.
    RepoFetchDone {
        path: PathBuf,
        snapshot: Option<RepositorySnapshot>,
        queue: bool,
        fetch_error: Option<String>,
    },
    /// Results of a background fast-forward dry-run check triggered after a
    /// fetch (manual or periodic) increased a repo's `behind` count.
    PullSafetyChecked(Vec<(PathBuf, bool)>),
}

// ── Worktree display rows ─────────────────────────────────────────────────────

struct WtDisplayRow {
    path: PathBuf,
    display_name: String,
    wt_label: String,
    repo_idx: usize,
}

struct WtEntry {
    path: PathBuf,
    is_primary: bool,
}

fn parse_worktree_listing(text: &str) -> Vec<WtEntry> {
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

// ── Branch-name parsing ──────────────────────────────────────────────────────

/// Extract the branch name from one line of `git branch --all -vv` output.
/// Strips leading markers (`*`, `+`, spaces) and returns the first
/// whitespace-delimited token (the ref name). Returns `None` for header /
/// arrow lines (e.g. `remotes/origin/HEAD -> origin/main`).
fn parse_branch_name(line: &str) -> Option<&str> {
    let trimmed = line.trim_start_matches(['*', '+', ' ']);
    let name = trimmed.split_whitespace().next()?;
    // Skip symbolic refs like `remotes/origin/HEAD`
    if trimmed.contains(" -> ") {
        return None;
    }
    Some(name)
}

/// Collect the set of branch names from `git branch [-r] -vv` output.
fn parse_branch_names(output: &str) -> HashSet<String> {
    output
        .lines()
        .filter_map(parse_branch_name)
        .map(ToString::to_string)
        .collect()
}

/// For a `git branch --all` row naming a remote-tracking branch
/// (`remotes/origin/x`), return the `origin/x` form used by the remote flows.
fn remote_short_ref(name: &str) -> Option<&str> {
    name.strip_prefix("remotes/")
}

/// From one local-branch line of `git branch --all -vv --no-abbrev` output,
/// extract the upstream short ref (`origin/x`) out of the tracking bracket
/// that follows the SHA, e.g. `[origin/x]` or `[origin/x: ahead 1]`.
fn upstream_of_local_line(line: &str) -> Option<&str> {
    let name = parse_branch_name(line)?;
    if name.starts_with("remotes/") {
        return None;
    }
    // Tokens: name, 40-char SHA, then optionally `[upstream...`.
    let mut tokens = line.trim_start_matches(['*', '+', ' ']).split_whitespace();
    tokens.next()?; // name
    let sha = tokens.next()?;
    if sha.len() != 40 || !sha.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let bracket = tokens.next()?.strip_prefix('[')?;
    Some(bracket.trim_end_matches([']', ':']))
}

/// Drop `remotes/origin/x` rows from `git branch --all -vv` output when a
/// local branch in the same output already tracks `origin/x` — the tracking
/// info shown on the local line makes the remote row redundant.
fn filter_tracked_remotes(output: &str) -> String {
    let tracked: HashSet<&str> = output.lines().filter_map(upstream_of_local_line).collect();
    output
        .lines()
        .filter(|line| {
            parse_branch_name(line)
                .and_then(remote_short_ref)
                .is_none_or(|short| !tracked.contains(short))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

// ── Prompt editing helpers ────────────────────────────────────────────────────

/// Byte offset of the char boundary before `idx` (0 if already at the start).
fn prev_char_boundary(s: &str, idx: usize) -> usize {
    s[..idx]
        .chars()
        .next_back()
        .map_or(0, |c| idx - c.len_utf8())
}

/// Byte offset of the char boundary after `idx` (unchanged if at the end).
fn next_char_boundary(s: &str, idx: usize) -> usize {
    s[idx..].chars().next().map_or(idx, |c| idx + c.len_utf8())
}

/// Start of the "word" preceding `cursor`: skips separators backwards, then
/// the run of non-separator chars — so `feat/my-branch` deletes one segment
/// at a time.
fn word_back_start(s: &str, cursor: usize) -> usize {
    let is_sep = |c: char| matches!(c, '/' | '-' | '_' | '.' | ' ');
    let char_at = |idx: usize| s[idx..].chars().next();
    let mut idx = cursor;
    loop {
        let prev = prev_char_boundary(s, idx);
        if prev == idx || !char_at(prev).is_some_and(is_sep) {
            break;
        }
        idx = prev;
    }
    loop {
        let prev = prev_char_boundary(s, idx);
        if prev == idx || char_at(prev).is_some_and(is_sep) {
            break;
        }
        idx = prev;
    }
    idx
}

/// Approximate `git check-ref-format --branch`: catch obviously invalid names
/// before handing them to git.
fn validate_branch_name(name: &str) -> std::result::Result<(), &'static str> {
    if name.is_empty() {
        return Err("branch name is empty");
    }
    if name == "@" {
        return Err("'@' is not a valid branch name");
    }
    if name.starts_with('-') {
        return Err("branch name must not start with '-'");
    }
    if name.starts_with('/') || name.ends_with('/') || name.contains("//") {
        return Err("invalid '/' placement in branch name");
    }
    if name.ends_with('.') || name.contains("..") {
        return Err("invalid '.' placement in branch name");
    }
    if name.contains("@{") {
        return Err("branch name must not contain '@{'");
    }
    if name
        .split('/')
        .any(|seg| seg.starts_with('.') || seg.ends_with(".lock"))
    {
        return Err("segment must not start with '.' or end with '.lock'");
    }
    if name.chars().any(|c| {
        c.is_whitespace() || c.is_control() || matches!(c, '~' | '^' | ':' | '?' | '*' | '[' | '\\')
    }) {
        return Err("branch name contains invalid characters");
    }
    Ok(())
}

// ── Per-repo operation state ──────────────────────────────────────────────────

#[derive(Default)]
enum RepoOpState {
    #[default]
    Idle,
    Queued,
    Working,
    Success(String),
    Fail {
        message: String,
        kind: FailKind,
    },
}

/// Why an operation failed — drives per-row icon and color.
#[derive(Debug, Clone, Copy, PartialEq)]
enum FailKind {
    Generic,
    Credentials,
    ForcePush,
    Conflict,
    Network,
}

/// Categorize a git error (force-push suggestion is push-specific and
/// overlaid separately by the remote-op result handler).
fn fail_kind(e: &crate::error::AppError) -> FailKind {
    if e.requires_credentials() {
        FailKind::Credentials
    } else if e.is_merge_conflict() || e.is_rebase_conflict() {
        FailKind::Conflict
    } else if e.is_network_error() {
        FailKind::Network
    } else {
        FailKind::Generic
    }
}

impl RepoOpState {
    fn icon(&self, tick: u64) -> &str {
        match self {
            Self::Idle => " ",
            Self::Queued => ICON_QUEUED,
            Self::Working => SPINNER[(tick / 2) as usize % SPINNER.len()],
            Self::Success(_) => ICON_SUCCESS,
            Self::Fail {
                kind: FailKind::Credentials,
                ..
            } => "?",
            Self::Fail {
                kind: FailKind::ForcePush,
                ..
            } => "!",
            Self::Fail {
                kind: FailKind::Conflict,
                ..
            } => ICON_CONFLICT,
            Self::Fail { .. } => ICON_FAIL,
        }
    }

    fn has_result(&self) -> bool {
        matches!(self, Self::Success(_) | Self::Fail { .. })
    }

    fn row_style(
        &self,
        conflict: bool,
        has_local: bool,
        no_upstream: bool,
        selected: bool,
    ) -> Style {
        if selected {
            let bg = match self {
                Self::Success(_) => C_SEL_SUCCESS_BG,
                Self::Fail {
                    kind: FailKind::Credentials,
                    ..
                } => C_SEL_CREDS_BG,
                Self::Fail {
                    kind: FailKind::Conflict,
                    ..
                } => C_SEL_CONFLICT_BG,
                Self::Fail { .. } => C_SEL_FAIL_BG,
                Self::Queued => C_SEL_QUEUED_BG,
                Self::Working => C_SEL_WORKING_BG,
                Self::Idle if no_upstream => C_SEL_NO_UPSTREAM_BG,
                Self::Idle if has_local => C_SEL_LOCAL_BG,
                Self::Idle if conflict => C_SEL_CONFLICT_BG,
                Self::Idle => C_SEL_DEFAULT_BG,
            };
            Style::default()
                .fg(Color::White)
                .bg(bg)
                .add_modifier(Modifier::BOLD)
        } else {
            let fg = match self {
                Self::Success(_) => C_SUCCESS_FG,
                Self::Fail {
                    kind: FailKind::Credentials,
                    ..
                } => C_CREDS_FG,
                Self::Fail {
                    kind: FailKind::Conflict,
                    ..
                } => C_CONFLICT_FG,
                Self::Fail {
                    kind: FailKind::Network,
                    ..
                } => C_NET_FG,
                Self::Fail { .. } => C_FAIL_FG,
                Self::Queued => C_QUEUED_FG,
                Self::Working => C_WORKING_FG,
                Self::Idle if no_upstream => C_NO_UPSTREAM_FG,
                Self::Idle if has_local => C_LOCAL_FG,
                Self::Idle if conflict => C_CONFLICT_FG,
                Self::Idle => Color::Reset,
            };
            if fg == Color::Reset {
                Style::default()
            } else {
                Style::default().fg(fg)
            }
        }
    }

    fn status_icon(&self, conflict: bool, has_local: bool, _no_upstream: bool, tick: u64) -> &str {
        match self {
            Self::Idle if has_local => ICON_LOCAL,
            Self::Idle if conflict => ICON_CONFLICT,
            Self::Idle => " ",
            _ => self.icon(tick),
        }
    }
}

/// Flatten a git result into a one-line status message.
fn result_message(result: &Result<String>, ok_fallback: &str) -> String {
    match result {
        Ok(m) if !m.trim().is_empty() => m.replace('\n', " | "),
        Ok(_) => ok_fallback.to_string(),
        Err(e) => format!("{e}"),
    }
}

/// Success/Fail state from a git result (without credential/force-push flags).
fn op_state_from_result(result: &Result<String>, ok_fallback: &str) -> RepoOpState {
    let message = result_message(result, ok_fallback);
    if result.is_ok() {
        RepoOpState::Success(message)
    } else {
        let kind = result.as_ref().err().map_or(FailKind::Generic, fail_kind);
        RepoOpState::Fail { message, kind }
    }
}

// ── Repo view ─────────────────────────────────────────────────────────────────

struct RepoView {
    snapshot: RepositorySnapshot,
    state: RepoOpState,
    last_action: Option<RemoteAction>,
    /// True while a background `git fetch` is running for this repo at startup.
    fetching: bool,
    /// Whether incoming commits (if any) can be pulled without conflicts, per the
    /// startup fast-forward dry-run. Irrelevant when `branch.behind == 0`.
    pull_safe: bool,
    /// Error message from the startup background fetch, if it failed. Shown as
    /// a muted marker so the repo isn't painted as a hard failure.
    fetch_error: Option<String>,
}

impl RepoView {
    fn new(snapshot: RepositorySnapshot) -> Self {
        Self {
            snapshot,
            state: RepoOpState::default(),
            last_action: None,
            fetching: false,
            pull_safe: true,
            fetch_error: None,
        }
    }

    fn is_queued(&self) -> bool {
        matches!(self.state, RepoOpState::Queued)
    }

    fn toggle_queue(&mut self) {
        self.state = match self.state {
            RepoOpState::Idle | RepoOpState::Success(_) | RepoOpState::Fail { .. } => {
                RepoOpState::Queued
            }
            RepoOpState::Queued => RepoOpState::Idle,
            RepoOpState::Working => return,
        };
    }

    fn branch_display(&self) -> String {
        let b = &self.snapshot.branch;
        let mut s = b.name.clone();
        if b.ahead > 0 || b.behind > 0 {
            s.push(' ');
            if b.ahead > 0 {
                let _ = write!(s, "{}{}", ICON_AHEAD, b.ahead);
            }
            if b.behind > 0 {
                if b.ahead > 0 {
                    s.push(' ');
                }
                let _ = write!(s, "{}{}", ICON_BEHIND, b.behind);
            }
        }
        s
    }
}

// ── Panel ─────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq)]
enum PanelKind {
    Branches,
    Commits,
    Status,
}

struct PanelState {
    kind: PanelKind,
    title: String,
    lines: Vec<String>,
    cursor: usize,
    scroll: usize,
}

impl PanelState {
    fn new_navigable(kind: PanelKind, title: String, content: String) -> Self {
        let lines = content.lines().map(ToString::to_string).collect();
        Self {
            kind,
            title,
            lines,
            cursor: 0,
            scroll: 0,
        }
    }

    fn new_text(kind: PanelKind, title: String, content: String) -> Self {
        let lines = content.lines().map(ToString::to_string).collect();
        Self {
            kind,
            title,
            lines,
            cursor: usize::MAX,
            scroll: 0,
        }
    }

    fn is_navigable(&self) -> bool {
        self.cursor != usize::MAX
    }

    fn selected_text(&self) -> Option<&str> {
        if self.is_navigable() {
            self.lines.get(self.cursor).map(|s| s.trim())
        } else {
            None
        }
    }

    fn move_cursor(&mut self, delta: isize, viewport: usize) {
        if !self.is_navigable() || self.lines.is_empty() {
            return;
        }
        let max = self.lines.len().saturating_sub(1);
        self.cursor = (self.cursor as isize + delta).clamp(0, max as isize) as usize;
        self.ensure_cursor_visible(viewport);
    }

    fn cursor_to(&mut self, idx: usize, viewport: usize) {
        if !self.is_navigable() || self.lines.is_empty() {
            return;
        }
        self.cursor = idx.min(self.lines.len() - 1);
        self.ensure_cursor_visible(viewport);
    }

    fn ensure_cursor_visible(&mut self, viewport: usize) {
        if viewport == 0 {
            return;
        }
        if self.cursor < self.scroll {
            self.scroll = self.cursor;
        } else if self.cursor >= self.scroll + viewport {
            self.scroll = self.cursor.saturating_sub(viewport.saturating_sub(1));
        }
    }

    fn scroll_text(&mut self, delta: isize, viewport: usize) {
        let max = self.lines.len().saturating_sub(viewport);
        self.scroll = (self.scroll as isize + delta).clamp(0, max as isize) as usize;
    }
}

// ── Prompt ────────────────────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq)]
enum PromptKind {
    Commit,
    Branch,
    Stash,
    WorktreeBranch,
    CheckoutBranch,
    DeleteBranch,
    ForceDeleteBranch,
    CheckoutRemoteBranch,
    DeleteRemoteBranch,
    SetUpstream,
}

impl PromptKind {
    fn label(self) -> &'static str {
        match self {
            Self::Commit => "Summary",
            Self::Branch => "New branch name",
            Self::Stash => "Stash message (optional)",
            Self::WorktreeBranch => "New worktree branch",
            Self::CheckoutBranch => "Checkout branch",
            Self::DeleteBranch => "Delete branch (safe)",
            Self::ForceDeleteBranch => "Force delete branch",
            Self::DeleteRemoteBranch => "Delete remote branch",
            Self::CheckoutRemoteBranch => "Checkout remote branch",
            Self::SetUpstream => "Upstream (remote or remote/branch)",
        }
    }

    fn title(self) -> &'static str {
        match self {
            Self::Commit => "Commit",
            Self::Branch | Self::WorktreeBranch => "New Branch",
            Self::Stash => "Stash",
            Self::CheckoutBranch | Self::CheckoutRemoteBranch => "Checkout",
            Self::DeleteBranch => "Delete Branch",
            Self::ForceDeleteBranch => "Force Delete Branch",
            Self::DeleteRemoteBranch => "Delete Remote Branch",
            Self::SetUpstream => "Set Upstream",
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum CommitField {
    Subject,
    Description,
}

struct PromptState {
    kind: PromptKind,
    input: String,
    /// Byte offset of the edit cursor within `input` (always on a char boundary).
    cursor: usize,
    /// Validation error shown under the input; cleared on the next edit.
    error: Option<&'static str>,
    /// Commit description (body). Only used when kind == Commit. Append-only —
    /// cursor editing applies to the single-line `input` field only.
    description: String,
    /// Which field is active in the commit prompt.
    commit_field: CommitField,
}

// ── Auth / force prompts ──────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq)]
enum CredentialField {
    Username,
    Password,
}

struct AuthPromptState {
    repo_path: PathBuf,
    repo_name: Option<String>,
    action: RemoteAction,
    username: String,
    password: String,
    input: String,
    field: CredentialField,
}

/// What a confirm dialog executes when the user accepts it.
enum ConfirmAction {
    ForcePush {
        path: PathBuf,
    },
    StashDrop {
        paths: Vec<PathBuf>,
    },
    DeleteBranch {
        name: String,
        paths: Vec<PathBuf>,
    },
    ForceDeleteBranch {
        name: String,
        paths: Vec<PathBuf>,
    },
    DeleteRemoteBranch {
        name: String,
        paths: Vec<PathBuf>,
    },
    /// y/Enter = mixed reset (keeps working tree); H = hard reset.
    ResetToUpstream {
        paths: Vec<PathBuf>,
    },
}

struct ConfirmPromptState {
    title: &'static str,
    /// Bold first line: repo name, or "'branch' in N repos".
    subject: String,
    /// Red warning line describing the irreversible consequence.
    warning: String,
    action: ConfirmAction,
}

// ── Sort ──────────────────────────────────────────────────────────────────────

#[derive(Clone, Copy)]
enum SortMode {
    Name,
    Modified,
}

/// Run a remote action, optionally wrapped in an auto-stash (the tracked
/// changes of a dirty tree are stashed first and restored afterwards).
async fn run_remote_with_autostash(
    runner: &GitRunner,
    path: &Path,
    action: RemoteAction,
    creds: Option<&Credentials>,
    stash: bool,
) -> Result<String> {
    if stash {
        runner
            .stash_push_include_untracked(path, Some("gitbatch auto-stash"))
            .await?;
    }
    let result = match creds {
        Some(c) => {
            runner
                .run_remote_action_with_credentials(path, action, c)
                .await
        }
        None => runner.run_remote_action(path, action).await,
    };
    if !stash {
        return result;
    }
    match result {
        Ok(msg) => match runner.stash_pop(path).await {
            Ok(_) => Ok(format!("{msg}\nauto-stash restored")),
            Err(_) => Ok(format!(
                "{msg}\nauto-stash kept (pop failed — resolve manually)"
            )),
        },
        Err(e) => {
            // Restore the user's tree; if this fails the "gitbatch auto-stash"
            // entry stays visible in the stash badge.
            let _ = runner.stash_pop(path).await;
            Err(e)
        }
    }
}

// ── App ───────────────────────────────────────────────────────────────────────

struct App {
    repos: Vec<RepoView>,
    cursor: usize,
    table_offset: usize,
    loading: bool,
    load_done: usize,
    load_total: usize,
    mode: Mode,
    auto_stash: bool,
    show_help: bool,
    help_scroll: usize,
    sort_mode: SortMode,
    panel: Option<PanelState>,
    prompt: Option<PromptState>,
    worktree_mode: bool,
    wt_rows: Vec<WtDisplayRow>,
    wt_cursor: usize,
    wt_offset: usize,
    msg_scroll: usize,
    auth_prompt: Option<AuthPromptState>,
    auth_prompt_queue: VecDeque<AuthPromptState>,
    confirm_prompt: Option<ConfirmPromptState>,
    confirm_prompt_queue: VecDeque<ConfirmPromptState>,
    tick: u64,
    runner: GitRunner,
    event_tx: mpsc::UnboundedSender<BgEvent>,
    event_rx: mpsc::UnboundedReceiver<BgEvent>,
    last_refresh: Instant,
    needs_full_redraw: bool,
}

impl App {
    fn new(runner: GitRunner, mode: Mode, auto_stash: bool, load_total: usize) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        Self {
            repos: Vec::with_capacity(load_total),
            cursor: 0,
            table_offset: 0,
            loading: true,
            load_done: 0,
            load_total,
            mode,
            auto_stash,
            show_help: false,
            help_scroll: 0,
            sort_mode: SortMode::Modified,
            panel: None,
            prompt: None,
            worktree_mode: false,
            wt_rows: Vec::new(),
            wt_cursor: 0,
            wt_offset: 0,
            msg_scroll: 0,
            auth_prompt: None,
            auth_prompt_queue: VecDeque::new(),
            confirm_prompt: None,
            confirm_prompt_queue: VecDeque::new(),
            tick: 0,
            runner,
            event_tx: tx,
            event_rx: rx,
            last_refresh: Instant::now(),
            needs_full_redraw: false,
        }
    }

    fn start_loading(&mut self, paths: Vec<PathBuf>) {
        if paths.is_empty() {
            self.loading = false;
            return;
        }
        let tx = self.event_tx.clone();
        let runner = self.runner.clone();
        let cap = worker_limit();
        tokio::spawn(async move {
            let mut stream = stream::iter(paths)
                .map(|path| {
                    let runner = runner.clone();
                    async move {
                        match runner.status_snapshot(&path).await {
                            Ok(snap) => BgEvent::RepoLoaded(snap),
                            Err(_) => BgEvent::RepoSkipped,
                        }
                    }
                })
                .buffer_unordered(cap);
            while let Some(ev) = stream.next().await {
                if tx.send(ev).is_err() {
                    break;
                }
            }
        });
    }

    fn check_load_complete(&mut self) {
        if self.loading && self.load_done >= self.load_total {
            self.post_load();
        }
    }

    fn post_load(&mut self) {
        self.loading = false;
        self.sort();
        self.auto_select();
        self.last_refresh = Instant::now();
        self.spawn_background_fetch();
    }

    fn auto_select(&mut self) {
        // Prefer repos that need attention: incoming commits first, then dirty
        for (i, repo) in self.repos.iter().enumerate() {
            if repo.snapshot.branch.behind > 0 {
                self.cursor = i;
                return;
            }
        }
        for (i, repo) in self.repos.iter().enumerate() {
            if repo.snapshot.dirty {
                self.cursor = i;
                return;
            }
        }
        self.cursor = 0;
    }

    fn current(&self) -> Option<&RepoView> {
        self.repos.get(self.cursor)
    }

    fn current_path(&self) -> Option<PathBuf> {
        if self.worktree_mode {
            self.wt_rows.get(self.wt_cursor).map(|r| r.path.clone())
        } else {
            self.current().map(|r| r.snapshot.path.clone())
        }
    }

    fn current_mut(&mut self) -> Option<&mut RepoView> {
        if self.worktree_mode {
            let idx = self.wt_rows.get(self.wt_cursor)?.repo_idx;
            self.repos.get_mut(idx)
        } else {
            self.repos.get_mut(self.cursor)
        }
    }

    fn panel_kind(&self) -> Option<PanelKind> {
        self.panel.as_ref().map(|p| p.kind)
    }

    fn repo_name_for_path(&self, path: &Path) -> Option<String> {
        self.repos
            .iter()
            .find(|r| r.snapshot.path == path)
            .map(|r| r.snapshot.name.clone())
    }

    fn move_cursor(&mut self, delta: isize) {
        self.msg_scroll = 0;
        if self.worktree_mode {
            if self.wt_rows.is_empty() {
                return;
            }
            let max = self.wt_rows.len() as isize - 1;
            self.wt_cursor = (self.wt_cursor as isize + delta).clamp(0, max) as usize;
        } else {
            if self.repos.is_empty() {
                return;
            }
            let max = self.repos.len() as isize - 1;
            self.cursor = (self.cursor as isize + delta).clamp(0, max) as usize;
        }
    }

    fn toggle_queue(&mut self) {
        if let Some(repo) = self.current_mut() {
            repo.toggle_queue();
        }
    }

    fn queue_all(&mut self) {
        let mode = self.mode;
        for repo in &mut self.repos {
            let b = &repo.snapshot.branch;
            // No upstream or detached HEAD: remote ops are impossible.
            if b.no_upstream || b.detached {
                continue;
            }
            match mode {
                Mode::Push => {
                    // Push doesn't bring in remote changes, but skip dirty trees to
                    // avoid confusion (uncommitted work mixed with a push session).
                    if repo.snapshot.dirty || b.ahead == 0 {
                        continue;
                    }
                }
                _ => {
                    // Pull / Merge / Rebase: skip if nothing incoming, or if the
                    // dry-run (or active unresolved conflict) says pulling would fail.
                    // A dirty tree the dry-run cleared as conflict-free is fine to queue.
                    let pull_conflict = b.behind > 0 && !repo.pull_safe;
                    if b.behind == 0 || repo.snapshot.has_conflicts || pull_conflict {
                        continue;
                    }
                }
            }
            if matches!(repo.state, RepoOpState::Idle) {
                repo.state = RepoOpState::Queued;
            }
        }
    }

    /// `A`: clear the queue AND all finished results/markers in one stroke —
    /// the bulk counterpart to per-repo Esc.
    fn clear_queue(&mut self) {
        for repo in &mut self.repos {
            if matches!(repo.state, RepoOpState::Queued) || repo.state.has_result() {
                repo.state = RepoOpState::Idle;
            }
            repo.fetch_error = None;
        }
    }

    // ── Multi-repo target resolution ─────────────────────────────────────────

    /// Mutable view of the repo at `path`, if it's still listed.
    fn repo_mut(&mut self, path: &Path) -> Option<&mut RepoView> {
        self.repos.iter_mut().find(|r| r.snapshot.path == path)
    }

    /// Paths of tagged (Queued) repos, or just the cursor repo when none are
    /// tagged, filtered by an additional per-repo predicate.
    fn target_paths_where(&self, keep: impl Fn(&RepoView) -> bool) -> Vec<PathBuf> {
        let queued: Vec<PathBuf> = self
            .repos
            .iter()
            .filter(|r| r.is_queued())
            .map(|r| r.snapshot.path.clone())
            .collect();
        let candidates: Vec<PathBuf> = if !queued.is_empty() {
            queued
        } else {
            self.current_path().into_iter().collect()
        };
        candidates
            .into_iter()
            .filter(|p| self.repos.iter().any(|r| r.snapshot.path == *p && keep(r)))
            .collect()
    }

    /// Paths of tagged (Queued) repos, or just the cursor repo when none are tagged.
    fn target_paths(&self) -> Vec<PathBuf> {
        self.target_paths_where(|_| true)
    }

    /// Like `target_paths`, but only repos with local (dirty) changes.
    fn target_paths_dirty(&self) -> Vec<PathBuf> {
        self.target_paths_where(|r| r.snapshot.dirty)
    }

    /// Like `target_paths`, but only repos that have stash entries.
    fn target_paths_with_stash(&self) -> Vec<PathBuf> {
        self.target_paths_where(|r| r.snapshot.stash_count > 0)
    }

    /// True when more than one repo is targeted (queued).
    fn has_multi_target(&self) -> bool {
        self.repos.iter().filter(|r| r.is_queued()).count() > 1
    }

    /// Number of repos that would be targeted (for prompt title display).
    fn target_count(&self) -> usize {
        let q = self.repos.iter().filter(|r| r.is_queued()).count();
        if q > 0 { q } else { 1 }
    }

    fn sort(&mut self) {
        let selected_path = self.current_path();
        match self.sort_mode {
            SortMode::Name => {
                self.repos
                    .sort_by(|a, b| a.snapshot.name.cmp(&b.snapshot.name));
            }
            SortMode::Modified => {
                self.repos.sort_by(|a, b| {
                    match (a.snapshot.last_modified, b.snapshot.last_modified) {
                        (Some(l), Some(r)) => r.cmp(&l),
                        (Some(_), None) => Ordering::Less,
                        (None, Some(_)) => Ordering::Greater,
                        (None, None) => a.snapshot.name.cmp(&b.snapshot.name),
                    }
                });
            }
        }
        if let Some(path) = selected_path
            && let Some(idx) = self.repos.iter().position(|r| r.snapshot.path == path)
        {
            self.cursor = idx;
        }
    }

    fn toggle_sort(&mut self) {
        self.sort_mode = match self.sort_mode {
            SortMode::Name => SortMode::Modified,
            SortMode::Modified => SortMode::Name,
        };
        self.sort();
    }

    // ── Background task management ────────────────────────────────────────────

    fn spawn_op(&mut self, path: PathBuf, action: RemoteAction) {
        self.spawn_op_inner(path, action, None);
    }

    fn spawn_op_with_credentials(
        &mut self,
        path: PathBuf,
        action: RemoteAction,
        creds: Credentials,
    ) {
        self.spawn_op_inner(path, action, Some(creds));
    }

    fn spawn_op_inner(&mut self, path: PathBuf, action: RemoteAction, creds: Option<Credentials>) {
        // Auto-stash only wraps ops that refuse to run on a dirty tree.
        let stash = self.auto_stash
            && matches!(
                action,
                RemoteAction::PullFfOnly | RemoteAction::Rebase | RemoteAction::Merge
            )
            && self
                .repos
                .iter()
                .find(|r| r.snapshot.path == path)
                .is_some_and(|r| r.snapshot.dirty);
        if let Some(repo) = self.repo_mut(&path) {
            repo.state = RepoOpState::Working;
            repo.last_action = Some(action.clone());
        }
        let tx = self.event_tx.clone();
        let runner = self.runner.clone();
        let action_copy = action.clone();
        tokio::spawn(async move {
            let result =
                run_remote_with_autostash(&runner, &path, action, creds.as_ref(), stash).await;
            let snapshot = runner.status_snapshot(&path).await.ok();
            let _ = tx.send(BgEvent::OpResult {
                path,
                action: action_copy,
                result,
                snapshot,
            });
        });
    }

    /// Runs `fast_forward_safe` for repos whose `behind` count just increased.
    /// `candidates` is `(path, upstream, is_clean)`.
    fn spawn_pull_safety_check(&self, candidates: Vec<(PathBuf, String, bool)>) {
        if candidates.is_empty() {
            return;
        }
        let tx = self.event_tx.clone();
        let runner = self.runner.clone();
        tokio::spawn(async move {
            let cap = worker_limit();
            let results: Vec<(PathBuf, bool)> = stream::iter(candidates)
                .map(|(path, upstream, clean)| {
                    let runner = runner.clone();
                    async move {
                        let safe = runner.fast_forward_safe(&path, &upstream, clean).await;
                        (path, safe)
                    }
                })
                .buffer_unordered(cap)
                .collect()
                .await;
            let _ = tx.send(BgEvent::PullSafetyChecked(results));
        });
    }

    /// Spawn a local (non-remote) git operation in the background.
    fn spawn_local_op(&mut self, path: PathBuf, action: LocalAction) {
        let label = action.label();
        if let Some(repo) = self.repo_mut(&path) {
            repo.state = RepoOpState::Working;
        }
        let tx = self.event_tx.clone();
        let runner = self.runner.clone();
        tokio::spawn(async move {
            let result = match action {
                LocalAction::Commit(ref msg) => runner.commit_all(&path, msg).await,
                LocalAction::CreateBranch(ref name) => runner.create_branch(&path, name).await,
                LocalAction::Checkout(ref name) => runner.checkout_reference(&path, name).await,
                LocalAction::CheckoutRemote(ref name) => {
                    runner.checkout_remote_branch(&path, name).await
                }
                LocalAction::DeleteBranch(ref name) => {
                    runner.delete_local_branch(&path, name).await
                }
                LocalAction::ForceDeleteBranch(ref name) => {
                    runner.force_delete_local_branch(&path, name).await
                }
                LocalAction::DeleteRemoteBranch(ref name) => {
                    runner.delete_remote_branch(&path, name).await
                }
                LocalAction::StashPush(ref msg) => runner.stash_push(&path, msg.as_deref()).await,
                LocalAction::StashPop => runner.stash_pop(&path).await,
                LocalAction::StashDrop => runner.stash_drop(&path).await,
                LocalAction::ResetUpstream { hard: true } => {
                    runner.hard_reset_to_upstream(&path).await
                }
                LocalAction::ResetUpstream { hard: false } => runner.reset_to_upstream(&path).await,
                LocalAction::SetUpstream {
                    ref remote,
                    ref branch,
                } => runner.set_upstream(&path, remote, branch).await,
            };
            let snapshot = runner.status_snapshot(&path).await.ok();
            let _ = tx.send(BgEvent::LocalResult {
                path,
                label,
                result,
                snapshot,
            });
        });
    }

    fn spawn_background_fetch(&mut self) {
        let candidates: Vec<PathBuf> = self
            .repos
            .iter()
            // Include repos whose upstream is configured but gone (e.g. the
            // remote branch was renamed): fetching is how they recover.
            .filter(|r| !r.snapshot.branch.no_upstream || r.snapshot.branch.upstream.is_some())
            .map(|r| r.snapshot.path.clone())
            .collect();

        if candidates.is_empty() {
            return;
        }

        for repo in self.repos.iter_mut() {
            if candidates.contains(&repo.snapshot.path) {
                repo.fetching = true;
            }
        }

        let tx = self.event_tx.clone();
        let runner = self.runner.clone();
        tokio::spawn(async move {
            stream::iter(candidates)
                .for_each_concurrent(Some(worker_limit()), |path| {
                    let runner = runner.clone();
                    let tx = tx.clone();
                    async move {
                        let fetch_error = runner
                            .run(&path, ["fetch", "--prune"])
                            .await
                            .err()
                            .map(|e| e.to_string().replace('\n', " | "));
                        let Ok(snapshot) = runner.status_snapshot(&path).await else {
                            let _ = tx.send(BgEvent::RepoFetchDone {
                                path,
                                snapshot: None,
                                queue: false,
                                fetch_error,
                            });
                            return;
                        };
                        let b = &snapshot.branch;
                        let queue = match &b.upstream {
                            Some(up)
                                if b.behind > 0 && !b.no_upstream && !snapshot.has_conflicts =>
                            {
                                runner.fast_forward_safe(&path, up, !snapshot.dirty).await
                            }
                            _ => false,
                        };
                        let _ = tx.send(BgEvent::RepoFetchDone {
                            path,
                            snapshot: Some(snapshot),
                            queue,
                            fetch_error,
                        });
                    }
                })
                .await;
        });
    }

    fn spawn_refresh(&mut self) {
        let paths: Vec<PathBuf> = self.repos.iter().map(|r| r.snapshot.path.clone()).collect();
        let tx = self.event_tx.clone();
        let runner = self.runner.clone();
        let cap = worker_limit();
        self.last_refresh = Instant::now();
        tokio::spawn(async move {
            let snapshots: Vec<(PathBuf, RepositorySnapshot)> = stream::iter(paths)
                .map(|path| {
                    let runner = runner.clone();
                    async move {
                        runner
                            .status_snapshot(&path)
                            .await
                            .ok()
                            .map(|snap| (path, snap))
                    }
                })
                .buffer_unordered(cap)
                .filter_map(future::ready)
                .collect()
                .await;
            let _ = tx.send(BgEvent::RefreshComplete { snapshots });
        });
    }

    fn spawn_load_worktrees(&self) {
        let repos: Vec<_> = self
            .repos
            .iter()
            .enumerate()
            .map(|(i, r)| (i, r.snapshot.path.clone(), r.snapshot.name.clone()))
            .collect();
        let tx = self.event_tx.clone();
        let runner = self.runner.clone();
        let cap = worker_limit();
        tokio::spawn(async move {
            let mut results: Vec<(usize, String, String)> = stream::iter(repos)
                .map(|(repo_idx, path, repo_name)| {
                    let runner = runner.clone();
                    async move {
                        runner
                            .worktree_list(&path)
                            .await
                            .ok()
                            .map(|listing| (repo_idx, repo_name, listing))
                    }
                })
                .buffer_unordered(cap)
                .filter_map(future::ready)
                .collect()
                .await;
            results.sort_by_key(|(idx, ..)| *idx);

            let mut wt_rows = Vec::new();
            for (repo_idx, repo_name, listing) in results {
                let worktrees = parse_worktree_listing(&listing);
                if worktrees.len() <= 1 {
                    let path = worktrees
                        .into_iter()
                        .next()
                        .map(|e| e.path)
                        .unwrap_or_default();
                    wt_rows.push(WtDisplayRow {
                        path,
                        display_name: repo_name.clone(),
                        wt_label: repo_name.clone(),
                        repo_idx,
                    });
                } else {
                    for wt in worktrees {
                        let wt_label = if wt.is_primary {
                            "[main]".to_string()
                        } else {
                            let base = wt
                                .path
                                .file_name()
                                .map(|n| n.to_string_lossy().into_owned())
                                .unwrap_or_default();
                            format!("[{base}]")
                        };
                        wt_rows.push(WtDisplayRow {
                            path: wt.path,
                            display_name: repo_name.clone(),
                            wt_label,
                            repo_idx,
                        });
                    }
                }
            }
            let _ = tx.send(BgEvent::WorktreesReady(wt_rows));
        });
    }

    fn drain_events(&mut self) {
        while let Ok(event) = self.event_rx.try_recv() {
            self.apply_bg_event(event);
        }
    }

    fn apply_bg_event(&mut self, event: BgEvent) {
        match event {
            BgEvent::RepoLoaded(snapshot) => {
                self.repos.push(RepoView::new(snapshot));
                self.load_done += 1;
                self.check_load_complete();
            }
            BgEvent::RepoSkipped => {
                self.load_done += 1;
                self.check_load_complete();
            }
            BgEvent::OpResult {
                path,
                action,
                result,
                snapshot,
            } => {
                let requires_creds = result
                    .as_ref()
                    .err()
                    .is_some_and(|e| e.requires_credentials());
                let suggest_force = result
                    .as_ref()
                    .err()
                    .is_some_and(|e| action.is_push() && e.suggests_force_push());

                let mut recheck: Vec<(PathBuf, String, bool)> = Vec::new();
                if let Some(repo) = self.repo_mut(&path) {
                    // The op result is authoritative state — never leave the
                    // startup-fetch spinner running past it.
                    repo.fetching = false;
                    let message =
                        result_message(&result, &format!("{} successful", display_action(&action)));
                    repo.state = if result.is_ok() {
                        // A successful remote op supersedes a stale startup-fetch failure.
                        repo.fetch_error = None;
                        RepoOpState::Success(message)
                    } else {
                        let kind = if suggest_force {
                            FailKind::ForcePush
                        } else {
                            result.as_ref().err().map_or(FailKind::Generic, fail_kind)
                        };
                        RepoOpState::Fail { message, kind }
                    };
                    if let Some(snap) = snapshot {
                        let old_behind = repo.snapshot.branch.behind;
                        repo.snapshot = snap;
                        // After a successful fetch, recheck FF safety if new commits arrived.
                        let b = &repo.snapshot.branch;
                        if result.is_ok()
                            && action == RemoteAction::Fetch
                            && b.behind > old_behind
                            && !b.no_upstream
                            && !repo.snapshot.has_conflicts
                            && let Some(up) = b.upstream.clone()
                        {
                            recheck.push((path.clone(), up, !repo.snapshot.dirty));
                        }
                    }
                }
                self.spawn_pull_safety_check(recheck);

                if requires_creds {
                    self.enqueue_auth_prompt(path, action);
                } else if suggest_force {
                    let subject = self.repo_name_for_path(&path).unwrap_or_else(|| "?".into());
                    self.enqueue_confirm(ConfirmPromptState {
                        title: "Force Push?",
                        subject,
                        warning: "This will rewrite remote history.".into(),
                        action: ConfirmAction::ForcePush { path },
                    });
                }
            }
            BgEvent::LocalResult {
                path,
                label,
                result,
                snapshot,
            } => {
                if let Some(repo) = self.repo_mut(&path) {
                    repo.fetching = false;
                    repo.state = op_state_from_result(&result, &format!("{label} done"));
                    if let Some(snap) = snapshot {
                        repo.snapshot = snap;
                    }
                }
            }
            BgEvent::RefreshComplete { snapshots } => {
                let mut recheck: Vec<(PathBuf, String, bool)> = Vec::new();
                for (path, snap) in snapshots {
                    if let Some(repo) = self.repo_mut(&path) {
                        // A refresh recalculated this repo's state; a stale
                        // fetch flag must not keep the spinner alive.
                        repo.fetching = false;
                        let old_behind = repo.snapshot.branch.behind;
                        repo.snapshot = snap;
                        let b = &repo.snapshot.branch;
                        if b.behind > old_behind
                            && !b.no_upstream
                            && !repo.snapshot.has_conflicts
                            && let Some(up) = b.upstream.clone()
                        {
                            recheck.push((path, up, !repo.snapshot.dirty));
                        }
                    }
                }
                self.sort();
                self.spawn_pull_safety_check(recheck);
            }
            BgEvent::WorktreesReady(rows) => {
                self.wt_rows = rows;
                self.wt_cursor = self.wt_cursor.min(self.wt_rows.len().saturating_sub(1));
            }
            BgEvent::RepoFetchDone {
                path,
                snapshot,
                queue,
                fetch_error,
            } => {
                if let Some(repo) = self.repo_mut(&path) {
                    repo.fetching = false;
                    repo.fetch_error = fetch_error;
                    if let Some(snap) = snapshot
                        && matches!(repo.state, RepoOpState::Idle)
                    {
                        repo.snapshot = snap;
                        repo.pull_safe = queue;
                        if queue {
                            repo.state = RepoOpState::Queued;
                        }
                    }
                }
            }
            BgEvent::PullSafetyChecked(results) => {
                for (path, safe) in results {
                    if let Some(repo) = self.repo_mut(&path) {
                        repo.pull_safe = safe;
                    }
                }
            }
        }
    }

    // ── Operations ────────────────────────────────────────────────────────────

    fn run_current_mode(&mut self) {
        let paths = self.target_paths();
        if paths.is_empty() {
            return;
        }
        let action = RemoteAction::from_mode(self.mode);
        for path in paths {
            self.spawn_op(path, action.clone());
        }
    }

    /// Run a remote action (fetch/pull/push) on all target repos.
    fn run_action_on_targets(&mut self, mode: Mode) {
        let paths = self.target_paths();
        let action = RemoteAction::from_mode(mode);
        for path in paths {
            self.spawn_op(path, action.clone());
        }
    }

    // ── Panel handling ────────────────────────────────────────────────────────

    async fn open_panel(&mut self, kind: PanelKind) -> Result<()> {
        let Some(path) = self.current_path() else {
            return Ok(());
        };
        let runner = &self.runner;

        let multi = self.has_multi_target();

        // For Branches with multiple tagged repos: show only branches
        // common to ALL targets (the intersection).
        if multi && kind == PanelKind::Branches {
            let paths = self.target_paths();
            let n = paths.len();
            let runner = runner.clone();

            // Query all repos concurrently — a serial loop would stall the UI
            // for the sum of the git round-trips.
            let outputs = future::try_join_all(paths.iter().map(|p| {
                let runner = runner.clone();
                async move { runner.branch_list(p).await }
            }))
            .await?;
            let name_sets: Vec<HashSet<String>> = outputs
                .iter()
                .map(|o| parse_branch_names(&filter_tracked_remotes(o)))
                .collect();

            // Intersect all sets.
            let common = if let Some(first) = name_sets.first() {
                let mut acc = first.clone();
                for s in &name_sets[1..] {
                    acc.retain(|name| s.contains(name));
                }
                acc
            } else {
                HashSet::new()
            };

            let mut names: Vec<&str> = common.iter().map(std::string::String::as_str).collect();
            names.sort_unstable();

            let title = format!("Branches ({n} repos)");
            let content = names.join("\n");

            self.panel = Some(PanelState::new_navigable(kind, title, content));
            return Ok(());
        }

        let (title, content) = match kind {
            PanelKind::Branches => (
                "Branches".into(),
                filter_tracked_remotes(&runner.branch_list(&path).await?),
            ),
            PanelKind::Commits => ("Commits".into(), runner.commit_log(&path).await?),
            PanelKind::Status => ("Status".into(), runner.status_text(&path).await?),
        };

        self.panel = Some(match kind {
            PanelKind::Branches => PanelState::new_navigable(kind, title, content),
            _ => PanelState::new_text(kind, title, content),
        });
        Ok(())
    }

    async fn refresh_panel(&mut self) -> Result<()> {
        let Some(kind) = self.panel_kind() else {
            return Ok(());
        };
        self.open_panel(kind).await
    }

    // ── Prompt submission ─────────────────────────────────────────────────────

    async fn submit_prompt(&mut self, prompt: PromptState) -> Result<()> {
        let input = prompt.input.trim().to_string();
        if input.is_empty() && prompt.kind != PromptKind::Stash {
            return Ok(());
        }

        // Determine the local action and the set of target repos.
        // Batchable actions fan out over all targets; worktree ops stay single.
        let (action, paths): (LocalAction, Vec<PathBuf>) = match prompt.kind {
            PromptKind::Commit => {
                let desc = prompt.description.trim().to_string();
                let full_msg = if desc.is_empty() {
                    input
                } else {
                    format!("{input}\n\n{desc}")
                };
                (LocalAction::Commit(full_msg), self.target_paths_dirty())
            }
            PromptKind::Branch => (LocalAction::CreateBranch(input), self.target_paths()),
            PromptKind::Stash => {
                let msg = if input.is_empty() { None } else { Some(input) };
                (LocalAction::StashPush(msg), self.target_paths_dirty())
            }
            PromptKind::CheckoutBranch => (LocalAction::Checkout(input), self.target_paths()),
            PromptKind::DeleteBranch => (LocalAction::DeleteBranch(input), self.target_paths()),
            PromptKind::ForceDeleteBranch => {
                (LocalAction::ForceDeleteBranch(input), self.target_paths())
            }
            PromptKind::CheckoutRemoteBranch => {
                (LocalAction::CheckoutRemote(input), self.target_paths())
            }
            PromptKind::DeleteRemoteBranch => {
                (LocalAction::DeleteRemoteBranch(input), self.target_paths())
            }
            PromptKind::SetUpstream => {
                // "origin" pairs each repo with its own current branch;
                // "origin/main" sets the same fixed target everywhere.
                let (remote, fixed_branch) = match input.split_once('/') {
                    Some((r, b)) => (r.to_string(), Some(b.to_string())),
                    None => (input, None),
                };
                for path in self.target_paths() {
                    let branch = fixed_branch.clone().or_else(|| {
                        self.repos
                            .iter()
                            .find(|r| r.snapshot.path == path)
                            .map(|r| r.snapshot.branch.name.clone())
                    });
                    if let Some(branch) = branch {
                        self.spawn_local_op(
                            path,
                            LocalAction::SetUpstream {
                                remote: remote.clone(),
                                branch,
                            },
                        );
                    }
                }
                return Ok(());
            }
            PromptKind::WorktreeBranch => {
                // Worktree operations stay single-repo (not meaningful to batch).
                let Some(path) = self.current_path() else {
                    return Ok(());
                };
                let dest = default_worktree_path(&path, prompt.input.trim());
                let result = self
                    .runner
                    .worktree_add(&path, &dest, prompt.input.trim(), true)
                    .await;
                let snap = self.runner.status_snapshot(&path).await?;
                if let Some(repo) = self.repo_mut(&path) {
                    repo.state = op_state_from_result(&result, "new worktree done");
                    repo.snapshot = snap;
                }
                return self.refresh_panel().await;
            }
        };

        if paths.is_empty() {
            return Ok(());
        }

        // Fan out as background tasks.
        for path in paths {
            self.spawn_local_op(path, action.clone());
        }
        self.panel = None;
        Ok(())
    }

    // ── Auth prompt ───────────────────────────────────────────────────────────

    fn enqueue_auth_prompt(&mut self, path: PathBuf, action: RemoteAction) {
        if self
            .auth_prompt
            .as_ref()
            .is_some_and(|p| p.repo_path == path)
            || self.auth_prompt_queue.iter().any(|p| p.repo_path == path)
        {
            return;
        }
        let prompt = AuthPromptState {
            repo_name: self.repo_name_for_path(&path),
            repo_path: path,
            action,
            username: String::new(),
            password: String::new(),
            input: String::new(),
            field: CredentialField::Username,
        };
        if self.auth_prompt.is_none() {
            self.auth_prompt = Some(prompt);
        } else {
            self.auth_prompt_queue.push_back(prompt);
        }
    }

    fn advance_auth_prompt(&mut self) {
        self.auth_prompt = self.auth_prompt_queue.pop_front();
    }

    fn submit_auth_prompt(&mut self, mut prompt: AuthPromptState) -> Result<()> {
        match prompt.field {
            CredentialField::Username => {
                prompt.username = prompt.input.trim().to_string();
                prompt.field = CredentialField::Password;
                prompt.input = String::new();
                self.auth_prompt = Some(prompt);
                return Ok(());
            }
            CredentialField::Password => {
                prompt.password = prompt.input.clone();
            }
        }
        let creds = Credentials {
            username: prompt.username.trim().to_string(),
            password: prompt.password.clone(),
        };
        self.spawn_op_with_credentials(prompt.repo_path, prompt.action, creds);
        self.advance_auth_prompt();
        Ok(())
    }

    fn cancel_auth_prompt(&mut self) {
        if let Some(prompt) = self.auth_prompt.take()
            && let Some(repo) = self
                .repos
                .iter_mut()
                .find(|r| r.snapshot.path == prompt.repo_path)
        {
            repo.state = RepoOpState::Fail {
                message: "credentials cancelled".into(),
                kind: FailKind::Generic,
            };
        }
        self.advance_auth_prompt();
    }

    // ── Confirm prompt ────────────────────────────────────────────────────────

    fn enqueue_confirm(&mut self, prompt: ConfirmPromptState) {
        // Repeated push failures must not stack duplicate force-push dialogs.
        if let ConfirmAction::ForcePush { path } = &prompt.action {
            let same = |p: &ConfirmPromptState| matches!(&p.action, ConfirmAction::ForcePush { path: other } if other == path);
            if self.confirm_prompt.as_ref().is_some_and(same)
                || self.confirm_prompt_queue.iter().any(same)
            {
                return;
            }
        }
        if self.confirm_prompt.is_none() {
            self.confirm_prompt = Some(prompt);
        } else {
            self.confirm_prompt_queue.push_back(prompt);
        }
    }

    fn dismiss_confirm(&mut self) {
        self.confirm_prompt = self.confirm_prompt_queue.pop_front();
    }

    fn execute_confirm(&mut self, hard: bool) {
        let Some(prompt) = self.confirm_prompt.take() else {
            return;
        };
        self.confirm_prompt = self.confirm_prompt_queue.pop_front();
        match prompt.action {
            ConfirmAction::ForcePush { path } => {
                self.spawn_op(path, RemoteAction::Push { force: true });
            }
            ConfirmAction::StashDrop { paths } => {
                for path in paths {
                    self.spawn_local_op(path, LocalAction::StashDrop);
                }
            }
            ConfirmAction::DeleteBranch { name, paths } => {
                for path in paths {
                    self.spawn_local_op(path, LocalAction::DeleteBranch(name.clone()));
                }
            }
            ConfirmAction::ForceDeleteBranch { name, paths } => {
                for path in paths {
                    self.spawn_local_op(path, LocalAction::ForceDeleteBranch(name.clone()));
                }
            }
            ConfirmAction::DeleteRemoteBranch { name, paths } => {
                for path in paths {
                    self.spawn_local_op(path, LocalAction::DeleteRemoteBranch(name.clone()));
                }
            }
            ConfirmAction::ResetToUpstream { paths } => {
                for path in paths {
                    self.spawn_local_op(path, LocalAction::ResetUpstream { hard });
                }
            }
        }
    }

    /// Subject line for a confirm dialog: the repo name, or the batch size.
    fn confirm_subject(&self, paths: &[PathBuf]) -> String {
        if paths.len() == 1 {
            self.repo_name_for_path(&paths[0])
                .unwrap_or_else(|| "1 repo".into())
        } else {
            format!("{} repos", paths.len())
        }
    }

    // ── Worktree helpers ──────────────────────────────────────────────────────

    async fn remove_selected_worktree(&mut self) -> Result<()> {
        let Some(path) = self.current_path() else {
            return Ok(());
        };
        let ctx = worktree_context(&self.runner, &path).await?;
        if ctx.selected_is_primary {
            if let Some(repo) = self.current_mut() {
                repo.state = RepoOpState::Fail {
                    message: "cannot remove primary worktree".into(),
                    kind: FailKind::Generic,
                };
            }
            return Ok(());
        }
        let result = self.runner.worktree_remove(&ctx.primary_path, &path).await;
        if let Some(repo) = self.current_mut() {
            repo.state = op_state_from_result(&result, "worktree removed");
        }
        self.spawn_refresh();
        Ok(())
    }

    async fn toggle_worktree_lock(&mut self) -> Result<()> {
        let Some(path) = self.current_path() else {
            return Ok(());
        };
        let ctx = worktree_context(&self.runner, &path).await?;
        if ctx.selected_is_primary {
            if let Some(repo) = self.current_mut() {
                repo.state = RepoOpState::Fail {
                    message: "cannot lock primary worktree".into(),
                    kind: FailKind::Generic,
                };
            }
            return Ok(());
        }
        let result = if ctx.selected_is_locked {
            self.runner.worktree_unlock(&ctx.primary_path, &path).await
        } else {
            self.runner.worktree_lock(&ctx.primary_path, &path).await
        };
        if let Some(repo) = self.current_mut() {
            repo.state = op_state_from_result(&result, "worktree lock toggled");
        }
        Ok(())
    }

    // ── Key handling ──────────────────────────────────────────────────────────

    async fn handle_key(&mut self, code: KeyCode, mods: KeyModifiers) -> Result<bool> {
        if code == KeyCode::Char('c') && mods.contains(KeyModifiers::CONTROL) {
            return Ok(true);
        }

        // Allow quit during loading
        if self.loading {
            return Ok(code == KeyCode::Char('q') || code == KeyCode::Char('Q'));
        }

        if self.auth_prompt.is_some() {
            return self.handle_auth_key(code).await;
        }
        if self.confirm_prompt.is_some() {
            return self.handle_confirm_key(code);
        }
        if self.prompt.is_some() {
            return self.handle_prompt_key(code, mods).await;
        }
        if self.show_help {
            match code {
                KeyCode::Char('c') if mods.contains(KeyModifiers::CONTROL) => return Ok(true),
                KeyCode::Esc | KeyCode::Char('?' | 'q') => {
                    self.show_help = false;
                }
                KeyCode::Down | KeyCode::Char('j') => self.help_scroll += 1,
                KeyCode::Up | KeyCode::Char('k') => {
                    self.help_scroll = self.help_scroll.saturating_sub(1);
                }
                KeyCode::PageDown => self.help_scroll += 10,
                KeyCode::PageUp => self.help_scroll = self.help_scroll.saturating_sub(10),
                KeyCode::Char('g') | KeyCode::Home => self.help_scroll = 0,
                // usize::MAX would overflow the += arms; the draw clamps this.
                KeyCode::Char('G') | KeyCode::End => self.help_scroll = usize::MAX / 2,
                // Swallow everything else while help is open.
                _ => {}
            }
            return Ok(false);
        }
        if self.panel.is_some() && self.handle_panel_key(code, mods)? {
            return Ok(false);
        }

        match code {
            KeyCode::Char('q' | 'Q') => return Ok(true),
            KeyCode::Char('?') => {
                self.show_help = true;
                self.help_scroll = 0;
            }
            KeyCode::Esc => {
                if self.panel.is_some() {
                    self.panel = None;
                } else if let Some(repo) = self.current_mut() {
                    // Clear success/fail state on Esc
                    if repo.state.has_result() {
                        repo.state = RepoOpState::Idle;
                    }
                }
            }
            KeyCode::Up | KeyCode::Char('k') => self.move_cursor(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_cursor(1),
            KeyCode::Char('g') | KeyCode::Home => {
                if self.worktree_mode {
                    self.wt_cursor = 0;
                } else {
                    self.cursor = 0;
                }
            }
            KeyCode::Char('G') | KeyCode::End => {
                if self.worktree_mode {
                    self.wt_cursor = self.wt_rows.len().saturating_sub(1);
                } else {
                    self.cursor = self.repos.len().saturating_sub(1);
                }
            }
            KeyCode::PageUp => self.move_cursor(-MAIN_PAGE_JUMP),
            KeyCode::PageDown => self.move_cursor(MAIN_PAGE_JUMP),
            KeyCode::Left => self.msg_scroll = self.msg_scroll.saturating_sub(4),
            KeyCode::Right => self.msg_scroll = self.msg_scroll.saturating_add(4),
            KeyCode::Char(' ') => self.toggle_queue(),
            KeyCode::Enter => self.run_current_mode(),
            KeyCode::Char('a') => self.queue_all(),
            KeyCode::Char('A') => self.clear_queue(),
            KeyCode::Char('m') => self.mode = self.mode.cycle(),
            KeyCode::Char('t') => self.toggle_sort(),
            KeyCode::Char('b') => self.open_panel(PanelKind::Branches).await?,
            KeyCode::Char('s') => self.open_panel(PanelKind::Status).await?,
            KeyCode::Char('v') => self.open_panel(PanelKind::Commits).await?,
            KeyCode::Char('f') => self.run_action_on_targets(Mode::Fetch),
            KeyCode::Char('p') => self.run_action_on_targets(Mode::Pull),
            KeyCode::Char('P') => self.run_action_on_targets(Mode::Push),
            KeyCode::Char('c') => {
                // Clear error/success state first; if clean, show commit prompt
                if let Some(repo) = self.current_mut()
                    && repo.state.has_result()
                {
                    repo.state = RepoOpState::Idle;
                    return Ok(false);
                }
                self.show_prompt(PromptKind::Commit);
            }
            KeyCode::Char('n') => {
                let kind = if self.worktree_mode {
                    PromptKind::WorktreeBranch
                } else {
                    PromptKind::Branch
                };
                self.show_prompt(kind);
            }
            KeyCode::Char('u') => {
                self.show_prompt_prefilled(PromptKind::SetUpstream, "origin".into());
            }
            KeyCode::Char('U') => {
                let paths = self.target_paths_where(|r| !r.snapshot.branch.no_upstream);
                if !paths.is_empty() {
                    let subject = self.confirm_subject(&paths);
                    self.enqueue_confirm(ConfirmPromptState {
                        title: "Reset to Upstream?",
                        subject,
                        warning: "Mixed keeps your changes; hard DISCARDS all local \
                                  changes and commits."
                            .into(),
                        action: ConfirmAction::ResetToUpstream { paths },
                    });
                }
            }
            KeyCode::Char('S') => self.show_prompt(PromptKind::Stash),
            KeyCode::Char('O') => {
                for path in self.target_paths_with_stash() {
                    self.spawn_local_op(path, LocalAction::StashPop);
                }
            }
            KeyCode::Char('D') => {
                let paths = self.target_paths_with_stash();
                if !paths.is_empty() {
                    let subject = self.confirm_subject(&paths);
                    self.enqueue_confirm(ConfirmPromptState {
                        title: "Drop Stash?",
                        subject,
                        warning: "Discards the newest stash entry — cannot be undone.".into(),
                        action: ConfirmAction::StashDrop { paths },
                    });
                }
            }
            KeyCode::Char('W') => {
                self.worktree_mode = !self.worktree_mode;
                if self.worktree_mode {
                    self.wt_rows.clear();
                    self.wt_cursor = 0;
                    self.wt_offset = 0;
                    self.spawn_load_worktrees();
                }
            }
            KeyCode::Char('d') if self.worktree_mode => {
                self.remove_selected_worktree().await?;
            }
            KeyCode::Char('L') if self.worktree_mode => {
                self.toggle_worktree_lock().await?;
            }
            KeyCode::Char('X') if self.worktree_mode => {
                if let Some(p) = self.current_path() {
                    let runner = self.runner.clone();
                    let result = runner.worktree_prune(&p).await;
                    self.apply_local_result(p, result).await?;
                }
            }
            KeyCode::Tab => self.open_lazygit()?,
            _ => {}
        }
        Ok(false)
    }

    async fn apply_local_result(&mut self, path: PathBuf, result: Result<String>) -> Result<()> {
        let snap = self.runner.status_snapshot(&path).await?;
        if let Some(repo) = self.repo_mut(&path) {
            repo.state = op_state_from_result(&result, "done");
            repo.snapshot = snap;
        }
        self.refresh_panel().await
    }

    fn show_prompt(&mut self, kind: PromptKind) {
        self.prompt = Some(PromptState {
            kind,
            input: String::new(),
            cursor: 0,
            error: None,
            description: String::new(),
            commit_field: CommitField::Subject,
        });
    }

    fn show_prompt_prefilled(&mut self, kind: PromptKind, value: String) {
        self.show_prompt(kind);
        if let Some(ref mut p) = self.prompt {
            p.cursor = value.len();
            p.input = value;
        }
    }

    async fn handle_prompt_key(&mut self, code: KeyCode, mods: KeyModifiers) -> Result<bool> {
        let Some(ref mut prompt) = self.prompt else {
            return Ok(false);
        };
        let is_commit = prompt.kind == PromptKind::Commit;
        // The multi-line commit description stays append-only; cursor editing
        // applies to the single-line input field.
        let in_desc = is_commit && prompt.commit_field == CommitField::Description;

        match code {
            KeyCode::Esc => {
                self.prompt = None;
            }
            KeyCode::Tab if is_commit => {
                prompt.commit_field = match prompt.commit_field {
                    CommitField::Subject => CommitField::Description,
                    CommitField::Description => CommitField::Subject,
                };
            }
            KeyCode::Left if !in_desc => {
                prompt.cursor = prev_char_boundary(&prompt.input, prompt.cursor);
            }
            KeyCode::Right if !in_desc => {
                prompt.cursor = next_char_boundary(&prompt.input, prompt.cursor);
            }
            KeyCode::Home if !in_desc => prompt.cursor = 0,
            KeyCode::End if !in_desc => prompt.cursor = prompt.input.len(),
            KeyCode::Delete if !in_desc => {
                let next = next_char_boundary(&prompt.input, prompt.cursor);
                if next > prompt.cursor {
                    prompt.input.replace_range(prompt.cursor..next, "");
                    prompt.error = None;
                }
            }
            KeyCode::Backspace => {
                if in_desc {
                    prompt.description.pop();
                } else if mods.intersects(KeyModifiers::ALT | KeyModifiers::CONTROL) {
                    let start = word_back_start(&prompt.input, prompt.cursor);
                    prompt.input.replace_range(start..prompt.cursor, "");
                    prompt.cursor = start;
                    prompt.error = None;
                } else {
                    let prev = prev_char_boundary(&prompt.input, prompt.cursor);
                    if prev < prompt.cursor {
                        prompt.input.replace_range(prev..prompt.cursor, "");
                        prompt.cursor = prev;
                        prompt.error = None;
                    }
                }
            }
            // Many terminals send Ctrl+Backspace as Ctrl+W or Ctrl+H.
            KeyCode::Char('w' | 'h') if mods.contains(KeyModifiers::CONTROL) && !in_desc => {
                let start = word_back_start(&prompt.input, prompt.cursor);
                prompt.input.replace_range(start..prompt.cursor, "");
                prompt.cursor = start;
                prompt.error = None;
            }
            KeyCode::Enter => {
                if in_desc {
                    prompt.description.push('\n');
                } else {
                    if matches!(prompt.kind, PromptKind::Branch | PromptKind::WorktreeBranch)
                        && let Err(msg) = validate_branch_name(prompt.input.trim())
                    {
                        prompt.error = Some(msg);
                        return Ok(false);
                    }
                    if let Some(prompt) = self.prompt.take() {
                        self.submit_prompt(prompt).await?;
                    }
                }
            }
            KeyCode::Char(ch) if !mods.contains(KeyModifiers::CONTROL) => {
                if in_desc {
                    prompt.description.push(ch);
                } else {
                    prompt.input.insert(prompt.cursor, ch);
                    prompt.cursor += ch.len_utf8();
                    prompt.error = None;
                }
            }
            _ => {}
        }
        Ok(false)
    }

    fn handle_paste(&mut self, text: &str) {
        if let Some(ref mut p) = self.prompt {
            if p.kind == PromptKind::Commit && p.commit_field == CommitField::Description {
                p.description.push_str(&text.replace('\r', ""));
            } else {
                // Single-line field: take the first line so an embedded
                // newline can't submit the prompt prematurely.
                let line = text.lines().next().unwrap_or("").replace('\r', "");
                p.input.insert_str(p.cursor, &line);
                p.cursor += line.len();
                p.error = None;
            }
        } else if let Some(ref mut a) = self.auth_prompt {
            let line = text.lines().next().unwrap_or("").replace('\r', "");
            a.input.push_str(&line);
        }
    }

    async fn handle_auth_key(&mut self, code: KeyCode) -> Result<bool> {
        match code {
            KeyCode::Esc => self.cancel_auth_prompt(),
            KeyCode::Tab => {
                if let Some(ref mut p) = self.auth_prompt {
                    match p.field {
                        CredentialField::Username => {
                            p.username = p.input.trim().to_string();
                            p.field = CredentialField::Password;
                            p.input = String::new();
                        }
                        CredentialField::Password => {
                            p.password = p.input.clone();
                            p.field = CredentialField::Username;
                            p.input = p.username.clone();
                        }
                    }
                }
            }
            KeyCode::Backspace => {
                if let Some(ref mut p) = self.auth_prompt {
                    p.input.pop();
                }
            }
            KeyCode::Enter => {
                if let Some(prompt) = self.auth_prompt.take() {
                    self.submit_auth_prompt(prompt)?;
                }
            }
            KeyCode::Char(ch) => {
                if let Some(ref mut p) = self.auth_prompt {
                    p.input.push(ch);
                }
            }
            _ => {}
        }
        Ok(false)
    }

    fn handle_confirm_key(&mut self, code: KeyCode) -> Result<bool> {
        match code {
            KeyCode::Esc | KeyCode::Char('n' | 'N') => {
                self.dismiss_confirm();
            }
            KeyCode::Char('y' | 'Y') | KeyCode::Enter => {
                self.execute_confirm(false);
            }
            KeyCode::Char('H') => {
                let is_reset = matches!(
                    self.confirm_prompt.as_ref().map(|p| &p.action),
                    Some(ConfirmAction::ResetToUpstream { .. })
                );
                if is_reset {
                    self.execute_confirm(true);
                }
            }
            _ => {}
        }
        Ok(false)
    }

    /// Branch name under the panel cursor, stripped of `* + ` markers.
    fn selected_branch_name(&self) -> Option<String> {
        self.panel
            .as_ref()
            .and_then(|p| p.selected_text())
            .and_then(parse_branch_name)
            .map(ToString::to_string)
    }

    /// Delete a remote branch (`origin/x` form): confirm dialog when fanning
    /// out over tagged repos, editable prompt for a single repo.
    fn remote_delete_flow(&mut self, name: String) {
        if self.has_multi_target() {
            let paths = self.target_paths();
            self.enqueue_confirm(ConfirmPromptState {
                title: "Delete Remote Branch?",
                subject: format!("'{name}' in {} repos", paths.len()),
                warning: "Deletes the branch on the remote — affects everyone.".into(),
                action: ConfirmAction::DeleteRemoteBranch { name, paths },
            });
        } else {
            self.show_prompt_prefilled(PromptKind::DeleteRemoteBranch, name);
        }
    }

    fn handle_panel_key(&mut self, code: KeyCode, mods: KeyModifiers) -> Result<bool> {
        let Some(panel) = &self.panel else {
            return Ok(false);
        };
        let kind = panel.kind;
        let is_navigable = panel.is_navigable();

        // Only quit (Ctrl+C) and close (Esc) fall through to the main handler;
        // every other key is consumed while a panel is open.
        if code == KeyCode::Esc {
            return Ok(false);
        }
        if code == KeyCode::Char('c') && mods.contains(KeyModifiers::CONTROL) {
            return Ok(false);
        }

        match code {
            KeyCode::Up | KeyCode::Char('k') => {
                if let Some(p) = &mut self.panel {
                    if is_navigable {
                        p.move_cursor(-1, 20);
                    } else {
                        p.scroll_text(-1, 20);
                    }
                }
                return Ok(true);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if let Some(p) = &mut self.panel {
                    if is_navigable {
                        p.move_cursor(1, 20);
                    } else {
                        p.scroll_text(1, 20);
                    }
                }
                return Ok(true);
            }
            KeyCode::PageUp => {
                if let Some(p) = &mut self.panel {
                    if is_navigable {
                        p.move_cursor(-10, 20);
                    } else {
                        p.scroll_text(-10, 20);
                    }
                }
                return Ok(true);
            }
            KeyCode::PageDown => {
                if let Some(p) = &mut self.panel {
                    if is_navigable {
                        p.move_cursor(10, 20);
                    } else {
                        p.scroll_text(10, 20);
                    }
                }
                return Ok(true);
            }
            KeyCode::Char('g') | KeyCode::Home => {
                if let Some(p) = &mut self.panel {
                    if is_navigable {
                        p.cursor_to(0, 20);
                    } else {
                        p.scroll = 0;
                    }
                }
                return Ok(true);
            }
            KeyCode::Char('G') | KeyCode::End => {
                if let Some(p) = &mut self.panel {
                    if is_navigable {
                        p.cursor_to(p.lines.len().saturating_sub(1), 20);
                    } else {
                        p.scroll_text(p.lines.len() as isize, 20);
                    }
                }
                return Ok(true);
            }
            KeyCode::Char('q') => {
                self.panel = None;
                return Ok(true);
            }
            KeyCode::Char('n') if kind == PanelKind::Branches => {
                self.panel = None;
                self.show_prompt(PromptKind::Branch);
                return Ok(true);
            }
            KeyCode::Char('c' | ' ') if kind == PanelKind::Branches => {
                let name = self.selected_branch_name();
                if let Some(name) = name {
                    self.panel = None;
                    // A `remotes/origin/x` row must go through the tracking
                    // checkout (short name + --track), not a literal checkout
                    // that would detach HEAD.
                    if let Some(short) = remote_short_ref(&name).map(String::from) {
                        if self.has_multi_target() {
                            for path in self.target_paths() {
                                self.spawn_local_op(
                                    path,
                                    LocalAction::CheckoutRemote(short.clone()),
                                );
                            }
                        } else {
                            self.show_prompt_prefilled(PromptKind::CheckoutRemoteBranch, short);
                        }
                    } else if self.has_multi_target() {
                        for path in self.target_paths() {
                            self.spawn_local_op(path, LocalAction::Checkout(name.clone()));
                        }
                    } else {
                        self.show_prompt_prefilled(PromptKind::CheckoutBranch, name);
                    }
                }
                return Ok(true);
            }
            KeyCode::Char('d') if kind == PanelKind::Branches => {
                let name = self.selected_branch_name();
                if let Some(name) = name {
                    self.panel = None;
                    if let Some(short) = remote_short_ref(&name).map(String::from) {
                        self.remote_delete_flow(short);
                    } else if self.has_multi_target() {
                        let paths = self.target_paths();
                        self.enqueue_confirm(ConfirmPromptState {
                            title: "Delete Branch?",
                            subject: format!("'{name}' in {} repos", paths.len()),
                            warning: "Unmerged commits may be kept only in reflogs.".into(),
                            action: ConfirmAction::DeleteBranch { name, paths },
                        });
                    } else {
                        self.show_prompt_prefilled(PromptKind::DeleteBranch, name);
                    }
                }
                return Ok(true);
            }
            KeyCode::Char('D') if kind == PanelKind::Branches => {
                let name = self.selected_branch_name();
                if let Some(name) = name {
                    self.panel = None;
                    // Remote branches have no force-delete variant; treat D like d.
                    if let Some(short) = remote_short_ref(&name).map(String::from) {
                        self.remote_delete_flow(short);
                    } else if self.has_multi_target() {
                        let paths = self.target_paths();
                        self.enqueue_confirm(ConfirmPromptState {
                            title: "Force Delete Branch?",
                            subject: format!("'{name}' in {} repos", paths.len()),
                            warning: "Unmerged commits on this branch will be lost.".into(),
                            action: ConfirmAction::ForceDeleteBranch { name, paths },
                        });
                    } else {
                        self.show_prompt_prefilled(PromptKind::ForceDeleteBranch, name);
                    }
                }
                return Ok(true);
            }
            // Swallow everything else so main-list hotkeys don't fire behind the panel.
            _ => {}
        }
        Ok(true)
    }

    fn open_lazygit(&mut self) -> Result<()> {
        let Some(path) = self.current_path() else {
            return Ok(());
        };
        disable_raw_mode()?;
        execute!(io::stdout(), DisableBracketedPaste, LeaveAlternateScreen)?;
        let status = Command::new("lazygit").arg("-p").arg(&path).status();
        execute!(io::stdout(), EnterAlternateScreen, EnableBracketedPaste)?;
        enable_raw_mode()?;
        if status.is_err()
            && let Some(repo) = self.current_mut()
        {
            repo.state = RepoOpState::Fail {
                message: "lazygit failed to launch (is it installed and on PATH?)".into(),
                kind: FailKind::Generic,
            };
        }
        self.spawn_refresh();
        self.needs_full_redraw = true;
        Ok(())
    }

    fn any_working(&self) -> bool {
        self.repos
            .iter()
            .any(|r| matches!(r.state, RepoOpState::Working))
    }

    fn queued_count(&self) -> usize {
        self.repos.iter().filter(|r| r.is_queued()).count()
    }
}

// ── Public entry point ────────────────────────────────────────────────────────

pub async fn run(runner: GitRunner, config: &AppConfig, repositories: Vec<PathBuf>) -> Result<()> {
    let mut terminal = setup_terminal()?;
    // Fetch is not a TUI batch mode (startup auto-fetch covers it).
    let mode = if config.mode == Mode::Fetch {
        Mode::Pull
    } else {
        config.mode
    };
    let mut app = App::new(runner, mode, config.auto_stash, repositories.len());
    app.start_loading(repositories);

    loop {
        app.drain_events();
        app.tick = app.tick.wrapping_add(1);

        if !app.loading
            && app.last_refresh.elapsed() >= Duration::from_secs(REFRESH_SECS)
            && !app.any_working()
            && app.auth_prompt.is_none()
            && app.confirm_prompt.is_none()
        {
            app.spawn_refresh();
        }

        if app.needs_full_redraw {
            app.needs_full_redraw = false;
            terminal.clear()?;
        }
        terminal.draw(|frame| draw(frame, &mut app))?;

        if !event::poll(Duration::from_millis(TICK_MS))? {
            continue;
        }

        match event::read()? {
            Event::Key(key)
                if key.kind == KeyEventKind::Press
                    && app.handle_key(key.code, key.modifiers).await? =>
            {
                break;
            }
            Event::Paste(text) => app.handle_paste(&text),
            Event::Resize(_, _) => {}
            _ => {}
        }
    }

    Ok(restore_terminal(terminal)?)
}

// ── Terminal setup ────────────────────────────────────────────────────────────

fn setup_terminal() -> io::Result<DefaultTerminal> {
    enable_raw_mode()?;
    execute!(io::stdout(), EnterAlternateScreen, EnableBracketedPaste)?;
    Ok(ratatui::init())
}

fn restore_terminal(mut terminal: DefaultTerminal) -> io::Result<()> {
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        DisableBracketedPaste,
        LeaveAlternateScreen
    )?;
    ratatui::restore();
    Ok(())
}

// ── Drawing ───────────────────────────────────────────────────────────────────

fn draw(frame: &mut Frame, app: &mut App) {
    let area = frame.area();

    if area.width < MIN_WIDTH || area.height < MIN_HEIGHT {
        draw_too_small(frame, area);
        return;
    }

    let layout = Layout::vertical([
        Constraint::Length(1),
        Constraint::Fill(1),
        Constraint::Length(1),
    ])
    .split(area);

    draw_title_bar(frame, app, layout[0]);
    draw_repo_table(frame, app, layout[1]);
    draw_status_bar(frame, app, layout[2]);

    if app.loading {
        draw_loading(frame, app, area);
        return;
    }

    if app.show_help {
        draw_help_popup(frame, area, &mut app.help_scroll);
        return;
    }
    {
        let repo_header = app
            .repos
            .get(app.cursor)
            .map(|r| format!("  {}  {}", r.snapshot.name, r.snapshot.branch.name))
            .unwrap_or_default();
        if let Some(panel) = &app.panel {
            draw_panel_popup(frame, area, panel, &repo_header);
            return;
        }
    }
    if let Some(p) = &app.confirm_prompt {
        draw_confirm_popup(frame, area, p);
        return;
    }
    if let Some(p) = &app.auth_prompt {
        draw_auth_popup(frame, area, p);
        return;
    }
    if let Some(p) = &app.prompt {
        draw_input_popup(frame, area, p, app.target_count());
    }
}

fn draw_too_small(frame: &mut Frame, area: Rect) {
    let msg = Paragraph::new(format!(
        "Terminal too small  minimum {MIN_WIDTH}×{MIN_HEIGHT}"
    ))
    .style(
        Style::default()
            .fg(Color::White)
            .bg(Color::Rgb(150, 20, 20)),
    )
    .centered();
    frame.render_widget(msg, area);
}

fn draw_loading(frame: &mut Frame, app: &App, area: Rect) {
    // Dark background
    frame.render_widget(
        Block::default().style(Style::default().bg(Color::Rgb(12, 12, 20))),
        area,
    );

    let total = app.load_total.max(1);
    let done = app.load_done;
    let ratio = done as f64 / total as f64;

    let box_w = 50u16.min(area.width.saturating_sub(4));
    let box_h = 10u16.min(area.height.saturating_sub(2));
    let box_area = centered_rect(box_w, box_h, area);

    frame.render_widget(Clear, box_area);
    frame.render_widget(
        Block::default()
            .style(Style::default().bg(C_POPUP_BG))
            .borders(Borders::ALL)
            .border_type(ratatui::widgets::BorderType::Rounded)
            .border_style(Style::default().fg(C_POPUP_BORDER)),
        box_area,
    );

    let inner = box_area.inner(Margin {
        horizontal: 2,
        vertical: 1,
    });
    if inner.height < 5 {
        return;
    }

    let spinner = SPINNER[(app.tick / 2) as usize % SPINNER.len()];
    let counter = format!("{done} / {total} repositories");

    let [title_area, _, spin_area, bar_area, count_area] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(inner);

    frame.render_widget(
        Paragraph::new("  gitbatch").style(
            Style::default()
                .fg(C_POPUP_TITLE)
                .add_modifier(Modifier::BOLD),
        ),
        title_area,
    );

    frame.render_widget(
        Paragraph::new(format!("  {spinner} Loading repositories…"))
            .style(Style::default().fg(C_HELP_FG)),
        spin_area,
    );

    let gauge = Gauge::default()
        .ratio(ratio)
        .label("")
        .gauge_style(Style::default().fg(C_GAUGE_FG).bg(C_GAUGE_BG));
    frame.render_widget(gauge, bar_area);

    frame.render_widget(
        Paragraph::new(format!("  {counter}")).style(Style::default().fg(C_DIM_FG)),
        count_area,
    );
}

fn draw_title_bar(frame: &mut Frame, app: &App, area: Rect) {
    let queued = app.queued_count();
    let working = app
        .repos
        .iter()
        .filter(|r| matches!(r.state, RepoOpState::Working))
        .count();
    let total_repos = app.repos.len();
    let sort_label = match app.sort_mode {
        SortMode::Name => "name",
        SortMode::Modified => "time",
    };

    let base = Style::default()
        .fg(C_TITLE_FG)
        .bg(C_TITLE_BG)
        .add_modifier(Modifier::BOLD);

    let left = format!(" gitbatch {}", crate::version::VERSION);
    let mut right_parts: Vec<Vec<Span>> = vec![vec![Span::raw(format!("repos:{total_repos}"))]];
    if queued > 0 {
        right_parts.push(vec![Span::raw(format!("selected:{queued}"))]);
    }
    if working > 0 {
        right_parts.push(vec![Span::raw(format!("working:{working}"))]);
    }
    right_parts.push(vec![
        Span::raw("sor"),
        Span::styled("t", base.add_modifier(Modifier::UNDERLINED)),
        Span::raw(format!(":{sort_label}")),
    ]);
    if app.worktree_mode {
        right_parts.push(vec![Span::raw("worktree")]);
    }
    right_parts.push(vec![Span::raw(" ?:help ")]);

    let total_w = area.width as usize;
    let left_w = left.chars().count();
    let right_w: usize = right_parts
        .iter()
        .flatten()
        .map(|s| s.content.chars().count())
        .sum::<usize>()
        + 2 * (right_parts.len() - 1);
    let gap = total_w.saturating_sub(left_w + right_w);

    let mut spans: Vec<Span> = vec![Span::raw(left), Span::raw(" ".repeat(gap))];
    for (i, part) in right_parts.into_iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw("  "));
        }
        spans.extend(part);
    }
    frame.render_widget(Paragraph::new(Line::from(spans)).style(base), area);
}

/// Shared per-row visuals — style, the cursor/icon/name/stash cell, and the
/// message text — used by both the repo table and the worktree table.
fn repo_row_visual(
    repo: &RepoView,
    display_name: &str,
    selected: bool,
    tick: u64,
    repo_col_w: u16,
) -> (Style, Cell<'static>, String) {
    let dirty = repo.snapshot.dirty;
    let no_upstream = repo.snapshot.branch.no_upstream;
    let pull_conflict = repo.snapshot.branch.behind > 0 && !repo.pull_safe;
    let conflict = repo.snapshot.has_conflicts || pull_conflict;
    let has_local = !conflict && (repo.snapshot.branch.ahead > 0 || dirty);
    let style = repo
        .state
        .row_style(conflict, has_local, no_upstream, selected);
    let fetch_spin = repo.fetching && matches!(repo.state, RepoOpState::Idle);
    let icon = if fetch_spin {
        SPINNER[(tick / 2) as usize % SPINNER.len()]
    } else {
        repo.state
            .status_icon(conflict, has_local, no_upstream, tick)
    };
    // Muted marker for a failed startup fetch — only when nothing more
    // important occupies the icon column.
    let fetch_failed = !repo.fetching
        && repo.fetch_error.is_some()
        && icon == " "
        && matches!(repo.state, RepoOpState::Idle);
    let cursor_sym = if selected { ICON_CURSOR } else { " " };
    let stash_badge = if repo.snapshot.stash_count > 0 {
        format!(" {{{}}}", repo.snapshot.stash_count)
    } else {
        String::new()
    };
    // 4 = cursor(1) + space(1) + icon(1) + space(1)
    let name_w = repo_col_w.saturating_sub(4 + stash_badge.chars().count() as u16) as usize;
    let name = truncate(display_name, name_w);
    let repo_cell: Cell<'static> =
        if fetch_spin || fetch_failed || (!stash_badge.is_empty() && !selected) {
            let mut spans: Vec<Span<'static>> = vec![Span::raw(format!("{cursor_sym} "))];
            if fetch_spin {
                spans.push(Span::styled(
                    icon.to_string(),
                    Style::default().fg(Color::White),
                ));
            } else if fetch_failed {
                spans.push(Span::styled("!", Style::default().fg(C_NET_FG)));
            } else {
                spans.push(Span::raw(icon.to_string()));
            }
            spans.push(Span::raw(format!(" {name}")));
            if !stash_badge.is_empty() && !selected {
                spans.push(Span::styled(stash_badge, Style::default().fg(C_STASH_FG)));
            } else if !stash_badge.is_empty() {
                spans.push(Span::raw(stash_badge));
            }
            Cell::from(Line::from(spans))
        } else {
            Cell::from(format!("{cursor_sym} {icon} {name}{stash_badge}"))
        };
    let msg_text = match &repo.state {
        RepoOpState::Fail { message: m, .. } => m.clone(),
        _ => repo.snapshot.commit_subject.clone(),
    };
    (style, repo_cell, msg_text)
}

fn draw_repo_table(frame: &mut Frame, app: &mut App, area: Rect) {
    let inner_w = area.width.saturating_sub(2) as usize;
    let viewport_h = area.height.saturating_sub(2) as usize;
    let in_wt = app.worktree_mode;

    // ── update scroll offset ──────────────────────────────────────────────
    let (cursor, total) = if in_wt {
        (app.wt_cursor, app.wt_rows.len())
    } else {
        (app.cursor, app.repos.len())
    };
    if total > 0 {
        if in_wt {
            if cursor < app.wt_offset {
                app.wt_offset = cursor;
            } else if viewport_h > 0 && cursor >= app.wt_offset + viewport_h {
                app.wt_offset = cursor + 1 - viewport_h;
            }
            app.wt_offset = app.wt_offset.min(total.saturating_sub(viewport_h));
        } else {
            if cursor < app.table_offset {
                app.table_offset = cursor;
            } else if viewport_h > 0 && cursor >= app.table_offset + viewport_h {
                app.table_offset = cursor + 1 - viewport_h;
            }
            app.table_offset = app.table_offset.min(total.saturating_sub(viewport_h));
        }
    }
    let visible_start = if in_wt {
        app.wt_offset
    } else {
        app.table_offset
    };
    let visible_end = (visible_start + viewport_h).min(total);

    // ── age column (normal mode, terminal ≥ 112 columns) ─────────────────
    let show_age = !in_wt && area.width >= 112;
    let age_col_w: u16 = if show_age {
        let max_len = app
            .repos
            .iter()
            .map(|r| {
                r.snapshot
                    .last_modified
                    .map(format_age)
                    .unwrap_or_default()
                    .chars()
                    .count()
            })
            .max()
            .unwrap_or(2)
            .clamp(2, 5) as u16;
        max_len + 1
    } else {
        0
    };

    // ── column widths ─────────────────────────────────────────────────────
    let max_name = if in_wt {
        app.wt_rows
            .iter()
            .map(|r| r.display_name.chars().count())
            .max()
            .unwrap_or(10)
    } else {
        app.repos
            .iter()
            .map(|r| r.snapshot.name.chars().count())
            .max()
            .unwrap_or(10)
    }
    .clamp(8, 36) as u16;

    let max_branch = if in_wt {
        app.wt_rows
            .iter()
            .map(|r| r.wt_label.chars().count())
            .max()
            .unwrap_or(6)
    } else {
        app.repos
            .iter()
            .map(|r| {
                let b = &r.snapshot.branch;
                let mut len = b.name.chars().count();
                if b.ahead > 0 {
                    len += 1 + 1 + b.ahead.to_string().len();
                }
                if b.behind > 0 {
                    len += 1 + 1 + b.behind.to_string().len();
                }
                len
            })
            .max()
            .unwrap_or(6)
    }
    .clamp(6, 28) as u16;

    let repo_col_w = (max_name + 4).min(inner_w as u16 / 3);
    let branch_col_w = (max_branch + 1).min(inner_w as u16 / 4);
    let msg_col_w = (inner_w as u16)
        .saturating_sub(repo_col_w + branch_col_w + age_col_w)
        .max(8) as usize;

    let mut widths: Vec<Constraint> = vec![
        Constraint::Length(repo_col_w),
        Constraint::Length(branch_col_w),
        Constraint::Fill(1),
    ];
    if show_age {
        widths.push(Constraint::Length(age_col_w));
    }

    // Capture scalars before row-building closures (avoids re-borrowing app)
    let msg_scroll = app.msg_scroll;
    let tick = app.tick;

    let rows: Vec<Row<'static>> = if in_wt {
        app.wt_rows[visible_start..visible_end]
            .iter()
            .enumerate()
            .map(|(vi, wt_row)| {
                let i = visible_start + vi;
                let selected = i == app.wt_cursor;
                let repo = &app.repos[wt_row.repo_idx];
                let (style, repo_cell, msg_text) =
                    repo_row_visual(repo, &wt_row.display_name, selected, tick, repo_col_w);
                let label = truncate(&wt_row.wt_label, branch_col_w as usize);
                let msg = truncate(&msg_text, msg_col_w);
                Row::new([repo_cell, Cell::from(label), Cell::from(msg)]).style(style)
            })
            .collect()
    } else {
        app.repos[visible_start..visible_end]
            .iter()
            .enumerate()
            .map(|(vi, repo)| {
                let i = visible_start + vi;
                let selected = i == app.cursor;
                let (style, repo_cell, msg_text) =
                    repo_row_visual(repo, &repo.snapshot.name, selected, tick, repo_col_w);

                let branch_cell: Cell<'static> =
                    Cell::from(truncate(&repo.branch_display(), branch_col_w as usize));

                // Message with horizontal scroll
                let scrolled: String = if msg_scroll > 0 {
                    msg_text.chars().skip(msg_scroll).collect()
                } else {
                    msg_text
                };
                let msg = truncate(&scrolled, msg_col_w);

                let mut cells: Vec<Cell<'static>> = vec![repo_cell, branch_cell, Cell::from(msg)];
                if show_age {
                    let age = repo
                        .snapshot
                        .last_modified
                        .map(format_age)
                        .unwrap_or_default();
                    cells.push(Cell::from(age).style(if selected {
                        Style::default()
                    } else {
                        Style::default().fg(C_AGE_FG)
                    }));
                }
                Row::new(cells).style(style)
            })
            .collect()
    };

    let dim = Style::default().fg(C_DIM_FG);
    let mut block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(C_BORDER));

    if visible_start > 0 {
        block = block.title_top(Line::from(Span::styled(" more above ", dim)).centered());
    }
    if visible_end < total {
        block = block.title_bottom(Line::from(Span::styled(" more below ", dim)).centered());
    }

    let table = Table::new(rows, widths)
        .block(block)
        .row_highlight_style(Style::default());
    frame.render_widget(table, area);
}

fn draw_status_bar(frame: &mut Frame, app: &App, area: Rect) {
    let (mode_bg, mode_fg, mode_sym) = match app.mode {
        Mode::Pull => (C_MODE_PULL_BG, C_MODE_LIGHT_FG, SYM_PULL),
        Mode::Merge => (C_MODE_MERGE_BG, C_MODE_LIGHT_FG, SYM_MERGE),
        Mode::Rebase => (C_MODE_REBASE_BG, C_MODE_LIGHT_FG, SYM_REBASE),
        Mode::Push => (C_MODE_PUSH_BG, C_MODE_DARK_FG, SYM_PUSH),
        Mode::Fetch => (C_MODE_FETCH_BG, C_MODE_LIGHT_FG, SYM_FETCH),
    };

    let mode_str = format!(" {mode_sym} {} ", app.mode.as_str().to_uppercase());
    let mode_w = mode_str.chars().count();

    let center = if let Some(repo) = app.repos.get(app.cursor) {
        match &repo.state {
            RepoOpState::Success(m) => format!(" ✓ {m}"),
            RepoOpState::Fail { message: m, .. } => format!(" ✗ {m}"),
            RepoOpState::Working => format!(" {} working…", repo.snapshot.name),
            _ => {
                let b = &repo.snapshot.branch;
                let mut s = format!(" {}  {}", repo.snapshot.name, b.name);
                if b.no_upstream {
                    s.push_str("  no upstream");
                } else {
                    if b.ahead > 0 {
                        let _ = write!(s, " {}{}", ICON_AHEAD, b.ahead);
                    }
                    if b.behind > 0 {
                        let _ = write!(s, " {}{}", ICON_BEHIND, b.behind);
                    }
                }
                let pull_conflict = b.behind > 0 && !repo.pull_safe;
                if repo.snapshot.has_conflicts {
                    s.push_str("  merge conflict");
                } else if pull_conflict && b.ahead > 0 {
                    s.push_str("  conflict");
                } else if pull_conflict {
                    s.push_str("  would conflict");
                } else if repo.snapshot.dirty {
                    s.push_str("  dirty");
                }
                if repo.snapshot.stash_count > 0 {
                    let _ = write!(s, "  {{{}}}", repo.snapshot.stash_count);
                }
                if repo.fetch_error.is_some() {
                    s.push_str("  ⚠ auto-fetch failed");
                }
                s
            }
        }
    } else {
        " no repositories".into()
    };

    let right: &str = if app.worktree_mode {
        "  n:new  d:rm  L:lock  X:prune  W:exit "
    } else if let Some(panel) = &app.panel {
        match panel.kind {
            PanelKind::Branches => "  j/k:nav  space:checkout  n:new  d/D:del  Esc:close ",
            _ => "  j/k:scroll  Esc:close ",
        }
    } else if let Some(repo) = app.repos.get(app.cursor) {
        match &repo.state {
            RepoOpState::Success(_) | RepoOpState::Fail { .. } => "  c/Esc:clear ",
            RepoOpState::Working => "  working… ",
            _ if repo.snapshot.dirty => "  c:commit  S:stash  TAB:lazygit ",
            _ if repo.snapshot.branch.behind > 0 => "  p:pull  f:fetch  TAB:lazygit ",
            _ if repo.snapshot.branch.ahead > 0 => "  P:push  TAB:lazygit ",
            _ => "  m:mode  TAB:lazygit ",
        }
    } else {
        "  m:mode "
    };
    let right_w = right.chars().count();
    let total_w = area.width as usize;
    let center_w = total_w.saturating_sub(mode_w + right_w);
    let center_trimmed = truncate(&center, center_w.saturating_sub(1));
    let pad = center_w.saturating_sub(center_trimmed.chars().count() + 1);

    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                &mode_str,
                Style::default()
                    .fg(mode_fg)
                    .bg(mode_bg)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(format!("{}{}", center_trimmed, " ".repeat(pad))),
            Span::styled(right, Style::default().fg(C_HELP_FG)),
        ])),
        area,
    );
}

// ── Popup helpers ─────────────────────────────────────────────────────────────

fn centered_rect(width: u16, height: u16, area: Rect) -> Rect {
    let x = area.x + area.width.saturating_sub(width) / 2;
    let y = area.y + area.height.saturating_sub(height) / 2;
    Rect::new(x, y, width.min(area.width), height.min(area.height))
}

fn popup_block(title: &str) -> Block<'_> {
    Block::default()
        .style(Style::default().bg(C_POPUP_BG))
        .borders(Borders::ALL)
        .border_type(ratatui::widgets::BorderType::Rounded)
        .border_style(Style::default().fg(C_POPUP_BORDER))
        .title(Span::styled(
            format!(" {title} "),
            Style::default()
                .fg(C_POPUP_TITLE)
                .add_modifier(Modifier::BOLD),
        ))
}

fn section_line(label: &str) -> Line<'static> {
    Line::from(Span::styled(
        label.to_string(),
        Style::default()
            .fg(C_SECTION_HDR)
            .add_modifier(Modifier::BOLD),
    ))
}

fn kv_line<'a>(key: &'a str, desc: &'a str) -> Line<'a> {
    Line::from(vec![
        Span::styled(
            format!("  {key:<12}"),
            Style::default().fg(C_KEY_FG).add_modifier(Modifier::BOLD),
        ),
        Span::styled(desc, Style::default().fg(Color::White)),
    ])
}

fn icon_kv_line(glyph: &'static str, desc: &'static str, color: Color) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            format!("  {glyph:<12}"),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ),
        Span::styled(desc, Style::default().fg(Color::White)),
    ])
}

fn draw_help_popup(frame: &mut Frame, area: Rect, scroll: &mut usize) {
    let popup_w = 78u16.min(area.width.saturating_sub(4));
    let popup_h = 35u16.min(area.height.saturating_sub(2));
    let popup_area = centered_rect(popup_w, popup_h, area);

    frame.render_widget(Clear, popup_area);
    frame.render_widget(popup_block("Help"), popup_area);

    let inner = popup_area.inner(Margin {
        horizontal: 2,
        vertical: 1,
    });
    if inner.width < 20 || inner.height < 3 {
        return;
    }

    const KEYS_H: u16 = 23;
    const ICONS_H: u16 = 10;

    let left_lines = vec![
        section_line("Navigation"),
        kv_line("j/k ↑↓", "move cursor"),
        kv_line("g/G", "top / bottom"),
        kv_line("PgUp/PgDn", "jump page"),
        kv_line("← / →", "scroll message"),
        Line::from(""),
        section_line("Git"),
        kv_line("f", "fetch"),
        kv_line("p", "pull"),
        kv_line("P", "push"),
        kv_line("m", "cycle mode"),
        Line::from(""),
        section_line("Local"),
        kv_line("c", "commit"),
        kv_line("n", "new branch"),
        kv_line("u", "set upstream"),
        kv_line("U", "reset to upstream"),
        kv_line("S / O / D", "stash/pop/drop"),
        Line::from(""),
        section_line("Worktrees (W toggles)"),
        kv_line("n / d", "new / remove"),
        kv_line("L / X", "lock / prune"),
    ];

    let right_lines = vec![
        section_line("Batch (tag to apply to all)"),
        kv_line("Space", "tag / untag repo"),
        kv_line("a / A", "tag all / clear all"),
        kv_line("Enter", "run mode on tagged"),
        kv_line("", "all actions apply to"),
        kv_line("", "tagged repos when set"),
        section_line("Panels"),
        kv_line("b", "branches"),
        kv_line("s", "status"),
        kv_line("v", "commits"),
        kv_line("q / Esc", "close panel"),
        Line::from(""),
        section_line("Other"),
        kv_line("t", "toggle sort"),
        kv_line("TAB", "lazygit"),
        kv_line("q / Ctrl+C", "quit"),
    ];

    // Status icons legend — full-width section below the keybindings.
    let icon_lines = vec![
        section_line("Status Icons"),
        icon_kv_line(
            ICON_CONFLICT,
            "conflict — active merge conflict, or pull would create conflicts",
            C_CONFLICT_FG,
        ),
        icon_kv_line(
            ICON_LOCAL,
            "local — uncommitted/unpushed changes, safe to pull",
            C_LOCAL_FG,
        ),
        icon_kv_line(
            ICON_AHEAD,
            "ahead — branch is ahead of its remote",
            C_AHEAD_FG,
        ),
        icon_kv_line(
            ICON_BEHIND,
            "behind — branch is behind its remote",
            C_BEHIND_FG,
        ),
        icon_kv_line(
            ICON_QUEUED,
            "queued — selected for batch operation",
            C_QUEUED_FG,
        ),
        icon_kv_line(ICON_SUCCESS, "ok — last operation succeeded", C_SUCCESS_FG),
        icon_kv_line(
            ICON_FAIL,
            "failed — last operation failed (gray: network error)",
            C_FAIL_FG,
        ),
        icon_kv_line(
            "?  /  !",
            "failed — credentials needed / push rejected (non-FF)",
            C_CREDS_FG,
        ),
        icon_kv_line("{N}", "stash — repo has N stashed changesets", C_STASH_FG),
    ];

    if inner.height >= KEYS_H + ICONS_H {
        // Tall terminal: two keybinding columns with the legend below.
        *scroll = 0;
        let [keys_area, icons_area] =
            Layout::vertical([Constraint::Length(KEYS_H), Constraint::Fill(1)]).areas(inner);
        let half = keys_area.width / 2;
        let [left_area, right_area] =
            Layout::horizontal([Constraint::Length(half), Constraint::Fill(1)]).areas(keys_area);
        frame.render_widget(Paragraph::new(left_lines), left_area);
        frame.render_widget(Paragraph::new(right_lines), right_area);
        frame.render_widget(Paragraph::new(icon_lines), icons_area);
        return;
    }

    // Short terminal: flatten everything into one scrollable column.
    let mut flat: Vec<Line> = left_lines;
    flat.push(Line::from(""));
    flat.extend(right_lines);
    flat.push(Line::from(""));
    flat.extend(icon_lines);

    let visible = inner.height as usize;
    *scroll = (*scroll).min(flat.len().saturating_sub(visible));
    let start = *scroll;
    let end = (start + visible).min(flat.len());

    let dim = Style::default().fg(C_DIM_FG);
    let mut lines: Vec<Line> = Vec::with_capacity(visible);
    if start > 0 {
        lines.push(Line::from(Span::styled("  ↑ more above (k)", dim)));
    }
    let body_start = start + usize::from(start > 0);
    let more_below = end < flat.len();
    let body_end = end - usize::from(more_below);
    lines.extend(flat[body_start..body_end].iter().cloned());
    if more_below {
        lines.push(Line::from(Span::styled("  ↓ more below (j)", dim)));
    }
    frame.render_widget(Paragraph::new(lines), inner);
}

fn draw_panel_popup(frame: &mut Frame, area: Rect, panel: &PanelState, repo_header: &str) {
    let popup_w = (area.width * 3 / 4)
        .clamp(50, 120)
        .min(area.width.saturating_sub(4));
    let popup_h = (area.height * 4 / 5)
        .clamp(12, 60)
        .min(area.height.saturating_sub(2));
    let popup_area = centered_rect(popup_w, popup_h, area);

    frame.render_widget(Clear, popup_area);
    frame.render_widget(popup_block(&panel.title), popup_area);

    let inner = popup_area.inner(Margin {
        horizontal: 1,
        vertical: 1,
    });

    frame.render_widget(
        Paragraph::new(Span::styled(
            repo_header,
            Style::default()
                .fg(C_BRANCH_CUR)
                .add_modifier(Modifier::BOLD),
        )),
        Rect { height: 1, ..inner },
    );

    // Hint line
    let hint = match panel.kind {
        PanelKind::Branches => Line::from(vec![
            key_span("j/k"),
            plain(" navigate  "),
            key_span("space/c"),
            plain(" checkout  "),
            key_span("n"),
            plain(" new  "),
            key_span("d"),
            plain(" delete  "),
            key_span("D"),
            plain(" force-del  "),
            key_span("Esc"),
            plain(" close"),
        ]),
        _ => Line::from(vec![
            key_span("j/k"),
            plain(" scroll  "),
            key_span("Esc"),
            plain(" close"),
        ]),
    };
    frame.render_widget(
        Paragraph::new(hint).style(Style::default().fg(C_HELP_FG)),
        Rect {
            y: inner.y + 1,
            height: 1,
            ..inner
        },
    );

    // Content area
    let body = Rect {
        y: inner.y + 3,
        height: inner.height.saturating_sub(3),
        ..inner
    };
    let visible_h = body.height as usize;
    let start = panel.scroll;
    let end = (start + visible_h).min(panel.lines.len());

    let mut lines: Vec<Line> = Vec::new();
    if start > 0 {
        lines.push(Line::from(Span::styled(
            "  ↑ more above",
            Style::default().fg(C_DIM_FG),
        )));
    }
    for (i, line) in panel.lines[start..end].iter().enumerate() {
        let idx = start + i;
        if panel.is_navigable() && idx == panel.cursor {
            lines.push(Line::from(Span::styled(
                format!("▶ {line}"),
                Style::default()
                    .fg(Color::White)
                    .bg(C_SEL_DEFAULT_BG)
                    .add_modifier(Modifier::BOLD),
            )));
        } else if line.trim_start().starts_with('*') {
            lines.push(Line::from(Span::styled(
                line.as_str(),
                Style::default()
                    .fg(C_BRANCH_CUR)
                    .add_modifier(Modifier::BOLD),
            )));
        } else {
            lines.push(Line::from(line.as_str()));
        }
    }
    if end < panel.lines.len() {
        lines.push(Line::from(Span::styled(
            "  ↓ more below",
            Style::default().fg(C_DIM_FG),
        )));
    }

    frame.render_widget(Paragraph::new(lines), body);
}

fn draw_confirm_popup(frame: &mut Frame, area: Rect, prompt: &ConfirmPromptState) {
    let popup_w = 60u16.min(area.width.saturating_sub(4));
    let popup_h = 8u16;
    let popup_area = centered_rect(popup_w, popup_h, area);

    frame.render_widget(Clear, popup_area);
    frame.render_widget(popup_block(prompt.title), popup_area);

    let inner = popup_area.inner(Margin {
        horizontal: 2,
        vertical: 1,
    });

    let lines = vec![
        Line::from(Span::styled(
            prompt.subject.as_str(),
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(Span::styled(
            prompt.warning.as_str(),
            Style::default().fg(C_FAIL_FG),
        )),
        Line::from(""),
        {
            let mut hint = vec![key_span("y / Enter")];
            if matches!(prompt.action, ConfirmAction::ResetToUpstream { .. }) {
                hint.push(plain("  mixed reset    "));
                hint.push(key_span("H"));
                hint.push(plain("  hard reset    "));
            } else {
                hint.push(plain("  confirm    "));
            }
            hint.push(key_span("n / Esc"));
            hint.push(plain("  cancel"));
            Line::from(hint)
        },
    ];
    frame.render_widget(Paragraph::new(lines), inner);
}

fn draw_auth_popup(frame: &mut Frame, area: Rect, prompt: &AuthPromptState) {
    let popup_w = 58u16.min(area.width.saturating_sub(4));
    let popup_h = 11u16;
    let popup_area = centered_rect(popup_w, popup_h, area);

    frame.render_widget(Clear, popup_area);
    frame.render_widget(popup_block("Credentials Required"), popup_area);

    let inner = popup_area.inner(Margin {
        horizontal: 2,
        vertical: 1,
    });
    let repo = prompt.repo_name.as_deref().unwrap_or("?");

    let active = Style::default()
        .fg(Color::White)
        .add_modifier(Modifier::BOLD);
    let inactive = Style::default().fg(C_HELP_FG);

    let user_style = if prompt.field == CredentialField::Username {
        active
    } else {
        inactive
    };
    let pass_style = if prompt.field == CredentialField::Password {
        active
    } else {
        inactive
    };

    let user_val = if prompt.field == CredentialField::Username {
        format!("{}_", prompt.input)
    } else {
        prompt.username.clone()
    };
    let pass_val = if prompt.field == CredentialField::Password {
        "•".repeat(prompt.input.chars().count()) + "_"
    } else {
        "•".repeat(prompt.password.chars().count())
    };

    let lines = vec![
        Line::from(vec![
            Span::styled("Repository: ", Style::default().fg(C_HELP_FG)),
            Span::styled(
                repo,
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(""),
        Line::from(vec![
            Span::styled("  Username  ", Style::default().fg(C_HELP_FG)),
            Span::styled(&user_val, user_style),
        ]),
        Line::from(vec![
            Span::styled("  Password  ", Style::default().fg(C_HELP_FG)),
            Span::styled(&pass_val, pass_style),
        ]),
        Line::from(""),
        Line::from(vec![
            key_span("Tab"),
            plain(" switch  "),
            key_span("Enter"),
            plain(" submit  "),
            key_span("Esc"),
            plain(" cancel"),
        ]),
    ];
    frame.render_widget(Paragraph::new(lines), inner);
}

fn draw_input_popup(frame: &mut Frame, area: Rect, prompt: &PromptState, target_count: usize) {
    let is_commit = prompt.kind == PromptKind::Commit;
    let popup_w = 64u16.min(area.width.saturating_sub(4));
    let popup_h = if is_commit { 14u16 } else { 7u16 };
    let popup_area = centered_rect(popup_w, popup_h, area);

    frame.render_widget(Clear, popup_area);
    let title = if target_count > 1 {
        format!("{} — {} repos", prompt.kind.title(), target_count)
    } else {
        prompt.kind.title().to_string()
    };
    frame.render_widget(popup_block(&title), popup_area);

    let inner = popup_area.inner(Margin {
        horizontal: 2,
        vertical: 1,
    });
    let active = Style::default()
        .fg(Color::White)
        .add_modifier(Modifier::BOLD);
    let inactive = Style::default().fg(C_HELP_FG);

    if is_commit {
        let subj_active = prompt.commit_field == CommitField::Subject;
        let subj_indicator = if subj_active { ">" } else { " " };
        let desc_indicator = if subj_active { " " } else { ">" };
        let subj_style = if subj_active { active } else { inactive };
        let desc_style = if subj_active { inactive } else { active };

        let subj_spans = if subj_active {
            cursor_spans(&prompt.input, prompt.cursor, subj_style)
        } else {
            vec![Span::styled(prompt.input.clone(), subj_style)]
        };

        // Show up to 4 visible description lines.
        let desc_text = if !subj_active {
            format!("{}_", prompt.description)
        } else {
            prompt.description.clone()
        };
        let desc_lines: Vec<&str> = if desc_text.is_empty() {
            vec![""]
        } else {
            let all: Vec<&str> = desc_text.lines().collect();
            let max_visible = 4;
            if all.len() > max_visible {
                all[all.len() - max_visible..].to_vec()
            } else {
                all
            }
        };

        let mut subj_line = vec![
            Span::styled(
                format!("{subj_indicator} "),
                Style::default().fg(C_SECTION_HDR),
            ),
            Span::styled("Summary:     ", Style::default().fg(C_SECTION_HDR)),
        ];
        subj_line.extend(subj_spans);
        let mut lines: Vec<Line> = vec![
            Line::from(subj_line),
            Line::from(""),
            Line::from(vec![
                Span::styled(
                    format!("{desc_indicator} "),
                    Style::default().fg(C_SECTION_HDR),
                ),
                Span::styled("Description:", Style::default().fg(C_SECTION_HDR)),
            ]),
        ];
        for dl in &desc_lines {
            lines.push(Line::from(Span::styled(format!("  {dl}"), desc_style)));
        }
        // Pad to keep the hint line at a stable position.
        for _ in desc_lines.len()..4 {
            lines.push(Line::from(""));
        }
        lines.push(Line::from(""));
        if subj_active {
            lines.push(Line::from(vec![
                key_span("Enter"),
                plain(" commit  "),
                key_span("Tab"),
                plain(" description  "),
                key_span("Esc"),
                plain(" cancel"),
            ]));
        } else {
            lines.push(Line::from(vec![
                key_span("Tab"),
                plain(" summary  "),
                key_span("Esc"),
                plain(" cancel"),
            ]));
        }
        frame.render_widget(Paragraph::new(lines), inner);
    } else {
        let mut input_line = vec![Span::styled("> ", active)];
        input_line.extend(cursor_spans(&prompt.input, prompt.cursor, active));
        let error_line = match prompt.error {
            Some(e) => Line::from(Span::styled(e, Style::default().fg(C_FAIL_FG))),
            None => Line::from(""),
        };
        let lines = vec![
            Line::from(Span::styled(
                prompt.kind.label(),
                Style::default().fg(C_SECTION_HDR),
            )),
            Line::from(Span::styled(
                "─".repeat(inner.width as usize),
                Style::default().fg(C_BORDER),
            )),
            Line::from(""),
            Line::from(input_line),
            error_line,
            Line::from(vec![
                key_span("Enter"),
                plain(" confirm  "),
                key_span("Esc"),
                plain(" cancel"),
            ]),
        ];
        frame.render_widget(Paragraph::new(lines), inner);
    }
}

/// Render `input` with a reversed-video cursor at byte offset `cursor`.
fn cursor_spans(input: &str, cursor: usize, style: Style) -> Vec<Span<'static>> {
    let cursor = cursor.min(input.len());
    let (before, after) = input.split_at(cursor);
    let mut spans = vec![Span::styled(before.to_string(), style)];
    match after.chars().next() {
        Some(c) => {
            spans.push(Span::styled(
                c.to_string(),
                style.add_modifier(Modifier::REVERSED),
            ));
            spans.push(Span::styled(after[c.len_utf8()..].to_string(), style));
        }
        None => spans.push(Span::styled(" ", style.add_modifier(Modifier::REVERSED))),
    }
    spans
}

// ── Style helpers ─────────────────────────────────────────────────────────────

fn key_span(s: &str) -> Span<'_> {
    Span::styled(
        s,
        Style::default().fg(C_KEY_FG).add_modifier(Modifier::BOLD),
    )
}

fn plain(s: &str) -> Span<'_> {
    Span::styled(s, Style::default().fg(C_HELP_FG))
}

fn format_age(t: SystemTime) -> String {
    let secs = SystemTime::now()
        .duration_since(t)
        .unwrap_or_default()
        .as_secs();
    match secs {
        s if s < 60 => format!("{s}s"),
        s if s < 3_600 => format!("{}m", s / 60),
        s if s < 86_400 => format!("{}h", s / 3_600),
        s if s < 7 * 86_400 => format!("{}d", s / 86_400),
        s if s < 30 * 86_400 => format!("{}w", s / (7 * 86_400)),
        s if s < 365 * 86_400 => format!("{}mo", s / (30 * 86_400)),
        s => format!("{}y", s / (365 * 86_400)),
    }
}

fn truncate(s: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    let count = s.chars().count();
    if count <= max {
        s.to_string()
    } else if max <= 1 {
        s.chars().take(max).collect()
    } else {
        let mut out: String = s.chars().take(max - 1).collect();
        out.push('…');
        out
    }
}

// ── Worktree helpers ──────────────────────────────────────────────────────────

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

fn default_worktree_path(repo_path: &Path, branch: &str) -> PathBuf {
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

fn display_action(action: &RemoteAction) -> &'static str {
    match action {
        RemoteAction::Fetch => "fetch",
        RemoteAction::PullFfOnly => "pull (ff-only)",
        RemoteAction::Merge => "merge",
        RemoteAction::Rebase => "rebase",
        RemoteAction::Push { force: true } => "force push",
        RemoteAction::Push { force: false } => "push",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filter_tracked_remotes_drops_only_tracked_remote_rows() {
        let sha = "a".repeat(40);
        let output = [
            // tracked, plain bracket
            format!("* main                {sha} [origin/main] tip"),
            // tracked with counts
            format!("  feature             {sha} [origin/feature: ahead 1, behind 2] wip"),
            // upstream gone — remote row doesn't exist anyway, must not panic
            format!("  stale               {sha} [origin/stale: gone] old"),
            // local without upstream
            format!("  local-only          {sha} no upstream here"),
            // symbolic ref line is kept
            "  remotes/origin/HEAD -> origin/main".to_string(),
            // tracked by the locals above → dropped
            format!("  remotes/origin/main {sha} tip"),
            format!("  remotes/origin/feature {sha} wip"),
            // remote-only branch → kept
            format!("  remotes/origin/other {sha} other work"),
        ]
        .join("\n");

        let filtered = filter_tracked_remotes(&output);
        assert!(filtered.contains("* main"));
        assert!(filtered.contains("  feature"));
        assert!(filtered.contains("  stale"));
        assert!(filtered.contains("local-only"));
        assert!(filtered.contains("remotes/origin/HEAD -> origin/main"));
        assert!(!filtered.contains("remotes/origin/main"));
        assert!(!filtered.contains("remotes/origin/feature"));
        assert!(filtered.contains("remotes/origin/other"));
    }

    #[test]
    fn validate_branch_name_accepts_normal_names() {
        for name in ["main", "feature/foo", "release-1.2", "hotfix_x", "a/b/c"] {
            assert!(validate_branch_name(name).is_ok(), "{name}");
        }
    }

    #[test]
    fn validate_branch_name_rejects_invalid_names() {
        for name in [
            "",
            "@",
            "-x",
            "a b",
            "a..b",
            "a.",
            "/a",
            "a/",
            "a//b",
            "a@{b",
            "a~b",
            "a^b",
            "a:b",
            "a?b",
            "a*b",
            "a[b",
            "a\\b",
            ".hidden",
            "a/.hidden",
            "a.lock",
            "a/b.lock",
            "a\tb",
        ] {
            assert!(validate_branch_name(name).is_err(), "{name}");
        }
    }

    #[test]
    fn word_back_start_deletes_segment_wise() {
        let s = "feat/my-branch";
        // From the end: deletes "branch".
        assert_eq!(word_back_start(s, s.len()), "feat/my-".len());
        // From "feat/my-": skips the '-' separator and deletes "my".
        assert_eq!(word_back_start(s, "feat/my-".len()), "feat/".len());
        // From "feat/": deletes everything back to the start.
        assert_eq!(word_back_start(s, "feat/".len()), 0);
        assert_eq!(word_back_start("", 0), 0);
    }

    fn git_err(output: &str) -> crate::error::AppError {
        crate::error::AppError::GitCommandFailed {
            command: "git test".into(),
            code: Some(1),
            output: output.into(),
        }
    }

    #[test]
    fn fail_kind_categorizes_git_errors() {
        assert_eq!(
            fail_kind(&git_err("fatal: Authentication failed for 'https://x'")),
            FailKind::Credentials
        );
        assert_eq!(
            fail_kind(&git_err("CONFLICT (content): Merge conflict in f.txt")),
            FailKind::Conflict
        );
        assert_eq!(
            fail_kind(&git_err(
                "fatal: unable to access: Failed to connect to host"
            )),
            FailKind::Network
        );
        assert_eq!(fail_kind(&git_err("some other failure")), FailKind::Generic);
    }

    #[test]
    fn op_state_from_result_builds_success_and_fail() {
        let ok: Result<String> = Ok("line one\nline two".into());
        match op_state_from_result(&ok, "done") {
            RepoOpState::Success(m) => assert_eq!(m, "line one | line two"),
            _ => panic!("expected success"),
        }
        let ok_empty: Result<String> = Ok("  ".into());
        match op_state_from_result(&ok_empty, "fallback msg") {
            RepoOpState::Success(m) => assert_eq!(m, "fallback msg"),
            _ => panic!("expected success"),
        }
        let err: Result<String> = Err(git_err("Could not resolve host: example.com"));
        match op_state_from_result(&err, "unused") {
            RepoOpState::Fail { kind, .. } => assert_eq!(kind, FailKind::Network),
            _ => panic!("expected fail"),
        }
    }

    #[tokio::test]
    async fn ctrl_c_quits_even_when_prompt_is_open() {
        let mut app = App::new(GitRunner::default(), Mode::Pull, false, 0);
        app.loading = false;
        app.prompt = Some(PromptState {
            kind: PromptKind::Branch,
            input: "feature/test".into(),
            cursor: "feature/test".len(),
            error: None,
            description: String::new(),
            commit_field: CommitField::Subject,
        });

        let quit = app
            .handle_key(KeyCode::Char('c'), KeyModifiers::CONTROL)
            .await
            .unwrap();
        assert!(quit);
    }

    #[test]
    fn remote_short_ref_strips_remotes_prefix() {
        assert_eq!(remote_short_ref("remotes/origin/main"), Some("origin/main"));
        assert_eq!(
            remote_short_ref("remotes/origin/feat/x"),
            Some("origin/feat/x")
        );
        assert_eq!(remote_short_ref("main"), None);
        assert_eq!(remote_short_ref("feature/remotes"), None);
    }

    #[test]
    fn char_boundary_helpers_handle_multibyte() {
        let s = "aöb";
        assert_eq!(next_char_boundary(s, 0), 1);
        assert_eq!(next_char_boundary(s, 1), 3); // 'ö' is two bytes
        assert_eq!(next_char_boundary(s, s.len()), s.len());
        assert_eq!(prev_char_boundary(s, 3), 1);
        assert_eq!(prev_char_boundary(s, 1), 0);
        assert_eq!(prev_char_boundary(s, 0), 0);
    }

    #[test]
    fn parse_branch_name_extracts_current_branch() {
        assert_eq!(
            parse_branch_name("* main  abc1234 commit msg"),
            Some("main")
        );
    }

    #[test]
    fn parse_branch_name_extracts_regular_branch() {
        assert_eq!(
            parse_branch_name("  feature/foo  abc1234 commit msg"),
            Some("feature/foo")
        );
    }

    #[test]
    fn parse_branch_name_extracts_remote_branch() {
        assert_eq!(
            parse_branch_name("  remotes/origin/main  abc1234 commit msg"),
            Some("remotes/origin/main")
        );
    }

    #[test]
    fn parse_branch_name_skips_symbolic_ref() {
        assert_eq!(
            parse_branch_name("  remotes/origin/HEAD -> origin/main"),
            None
        );
    }

    #[test]
    fn parse_branch_name_with_plus_marker() {
        // The `+` marker is used for worktree-checked-out branches.
        assert_eq!(parse_branch_name("+ develop  abc1234 msg"), Some("develop"));
    }

    #[test]
    fn parse_branch_names_collects_set() {
        let output = "\
* main       abc1234 first commit
  feature/a  def5678 second commit
  feature/b  ghi9012 third commit
  remotes/origin/HEAD -> origin/main
  remotes/origin/main abc1234 first commit";
        let names = parse_branch_names(output);
        assert!(names.contains("main"));
        assert!(names.contains("feature/a"));
        assert!(names.contains("feature/b"));
        assert!(names.contains("remotes/origin/main"));
        // Symbolic refs should be excluded.
        assert!(!names.iter().any(|n| n.contains("HEAD")));
        assert_eq!(names.len(), 4);
    }

    #[test]
    fn parse_branch_names_empty_input() {
        assert!(parse_branch_names("").is_empty());
    }
}
