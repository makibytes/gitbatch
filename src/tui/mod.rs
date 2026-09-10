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
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::{
    Result,
    config::AppConfig,
    git::{Credentials, GitRunner, RemoteAction, RepositorySnapshot, worker_limit},
    mode::Mode,
};

mod text;
use text::{
    filter_tracked_remotes, next_char_boundary, parse_branch_name, parse_branch_names,
    prev_char_boundary, remote_short_ref, validate_branch_name, word_back_start,
};

mod worktree;
use worktree::{
    WorktreeOp, WtDisplayRow, default_worktree_path, parse_worktree_listing, run_worktree_op,
};

mod draw;
use draw::{display_action, draw};

mod keys;

// ── Constants ─────────────────────────────────────────────────────────────────

const MIN_WIDTH: u16 = 50;
const MIN_HEIGHT: u16 = 8;
const TICK_MS: u64 = 80;
const REFRESH_SECS: u64 = 30;
const MAIN_PAGE_JUMP: isize = 10;

/// Minimum column budget the status bar's center status (repo/branch/dirty
/// info) must keep after the mode badge and the right-hand key hint. Below
/// this, `fit_hint` steps down to a shorter hint tier instead of letting the
/// center status get squeezed to nothing.
const STATUS_CENTER_FLOOR: usize = 20;

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
const C_NOTICE_FG: Color = Color::Rgb(179, 157, 219); // lavender — transient status-bar notice

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

/// Fetch the content for a `b`/`s`/`v` panel. With more than one path in
/// `targets` and `kind == Branches`, shows only the branches common to every
/// target (queried concurrently — a serial loop would stall the UI for the
/// sum of the git round-trips); otherwise shows `current`'s own content.
async fn load_panel_content(
    runner: &GitRunner,
    kind: PanelKind,
    current: &Path,
    targets: &[PathBuf],
) -> Result<PanelState> {
    if kind == PanelKind::Branches && targets.len() > 1 {
        let n = targets.len();
        let runner = runner.clone();
        let outputs = future::try_join_all(targets.iter().map(|p| {
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

        let mut names: Vec<&str> = common.iter().map(String::as_str).collect();
        names.sort_unstable();

        let title = format!("Branches ({n} repos)");
        let content = names.join("\n");
        return Ok(PanelState::new_navigable(kind, title, content));
    }

    let (title, content) = match kind {
        PanelKind::Branches => (
            "Branches".into(),
            filter_tracked_remotes(&runner.branch_list(current).await?),
        ),
        PanelKind::Commits => ("Commits".into(), runner.commit_log(current).await?),
        PanelKind::Status => ("Status".into(), runner.status_text(current).await?),
    };
    Ok(match kind {
        PanelKind::Branches => PanelState::new_navigable(kind, title, content),
        _ => PanelState::new_text(kind, title, content),
    })
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
    /// Result of a background worktree action (see `run_worktree_op`).
    WorktreeOpResult {
        /// The primary repo whose `RepoView` should receive the result —
        /// not necessarily the worktree acted on: a linked (non-primary)
        /// worktree has no `RepoView` of its own.
        repo_path: PathBuf,
        label: &'static str,
        result: crate::Result<String>,
    },
    /// Content fetched for a panel (`b`/`s`/`v`), or its refresh.
    PanelReady {
        kind: PanelKind,
        result: crate::Result<PanelState>,
    },
}

// ── Per-repo operation state ──────────────────────────────────────────────────

/// Where a `Queued` selection came from.
///
/// Distinguishing the two is what lets a mode change (`m`) clear the
/// startup auto-selection — which was computed for the *previous* mode and
/// must not silently carry over — without discarding a selection the user
/// built deliberately with `Space`/`a`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum QueueOrigin {
    /// Pre-selected by the startup fast-forward safety check.
    Auto,
    /// Selected by the user, via `Space` or `a` (queue-all).
    User,
}

#[derive(Default)]
enum RepoOpState {
    #[default]
    Idle,
    Queued(QueueOrigin),
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
            Self::Queued(_) => ICON_QUEUED,
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
                Self::Queued(_) => C_SEL_QUEUED_BG,
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
                Self::Queued(_) => C_QUEUED_FG,
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
        matches!(self.state, RepoOpState::Queued(_))
    }

    fn toggle_queue(&mut self) {
        self.state = match self.state {
            RepoOpState::Idle | RepoOpState::Success(_) | RepoOpState::Fail { .. } => {
                RepoOpState::Queued(QueueOrigin::User)
            }
            RepoOpState::Queued(_) => RepoOpState::Idle,
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
    /// A remote mode (`Enter`, `p`, `P`, `f`) run over more than one repo —
    /// gated behind a confirmation so a batch scope is never applied to a
    /// remote outward-facing action without the user seeing the count first.
    RunMode {
        mode: Mode,
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

/// Why `a` left one or more repositories out of the batch selection.
///
/// The fields deliberately describe user-visible eligibility rather than
/// implementation state, so the status bar can explain a partial selection.
#[derive(Debug, Default, PartialEq, Eq)]
struct SelectionOutcome {
    selected: usize,
    no_upstream: usize,
    no_updates: usize,
    /// Skipped due to an active, unresolved merge/rebase conflict. A merely
    /// *dirty* tree is not skipped for this reason — see `unsafe_pull`.
    conflicted: usize,
    /// Skipped because `git pull --ff-only` would fail: either the branches
    /// diverged, or (on a dirty tree) an incoming path collides with a local
    /// change. Pull-mode only; Merge/Rebase have no such restriction.
    unsafe_pull: usize,
}

impl SelectionOutcome {
    fn notice(&self) -> String {
        let mut skipped = Vec::new();
        if self.no_upstream > 0 {
            skipped.push(format!("{} no upstream", self.no_upstream));
        }
        if self.no_updates > 0 {
            skipped.push(format!("{} no updates", self.no_updates));
        }
        if self.conflicted > 0 {
            skipped.push(format!("{} conflicted", self.conflicted));
        }
        if self.unsafe_pull > 0 {
            skipped.push(format!("{} not FF-safe", self.unsafe_pull));
        }
        if skipped.is_empty() {
            format!("Selected {}", self.selected)
        } else {
            format!("Selected {}; skipped {}", self.selected, skipped.join(", "))
        }
    }
}

/// Run a remote action, optionally wrapped in an auto-stash (the tracked
/// changes of a dirty tree are stashed first and restored afterwards).
///
/// Uses `create_auto_stash`/`restore_auto_stash` rather than raw
/// `stash push`/`stash pop`: a no-op stash (nothing to save) produces no
/// guard, so a clean tree can never pop an unrelated, pre-existing stash
/// entry — and a changed stash stack at restore time (the user stashed
/// something else while the op was running) is reported instead of silently
/// popping the wrong entry.
async fn run_remote_with_autostash(
    runner: &GitRunner,
    path: &Path,
    action: RemoteAction,
    creds: Option<&Credentials>,
    stash: bool,
) -> Result<String> {
    let guard = if stash {
        runner.create_auto_stash(path).await?
    } else {
        None
    };
    let Some(guard) = guard else {
        // Either auto-stash was off, or the tree turned out to have nothing
        // to stash (stale `dirty` snapshot) — run the action directly.
        return match creds {
            Some(c) => {
                runner
                    .run_remote_action_with_credentials(path, action, c)
                    .await
            }
            None => runner.run_remote_action(path, action).await,
        };
    };
    let result = match creds {
        Some(c) => {
            runner
                .run_remote_action_with_credentials(path, action, c)
                .await
        }
        None => runner.run_remote_action(path, action).await,
    };
    match result {
        Ok(msg) => match runner.restore_auto_stash(path, guard).await {
            Ok(_) => Ok(format!("{msg}\nauto-stash restored")),
            Err(e) => Ok(format!("{msg}\nauto-stash kept ({e}) — resolve manually")),
        },
        Err(e) => {
            // Restore the user's tree; if this fails the "gitbatch auto-stash"
            // entry stays visible in the stash badge, and the failure is
            // folded into the returned error so it isn't silently dropped.
            match runner.restore_auto_stash(path, guard).await {
                Ok(_) => Err(e),
                Err(restore_err) => Err(crate::error::AppError::Config(format!(
                    "{e}\n(auto-stash also kept: {restore_err})"
                ))),
            }
        }
    }
}

/// Whether `repo` has an upstream and a commit count relevant to `mode`
/// (ahead for Push, behind for every pull-like mode). Used both to decide
/// what `queue_all` may select and, on a mode change, which existing
/// `User`-origin selections have become moot for the new mode.
///
/// Deliberately ignores dirty/conflict state — callers that care (currently
/// only `queue_all`) check that themselves, since how conservative to be
/// about it differs by call site.
fn mode_has_relevant_commits(repo: &RepoView, mode: Mode) -> bool {
    let b = &repo.snapshot.branch;
    if b.no_upstream || b.detached {
        return false;
    }
    match mode {
        Mode::Push => b.ahead > 0,
        Mode::Pull | Mode::Merge | Mode::Rebase | Mode::Fetch => b.behind > 0,
    }
}

/// Whether `mode` updates the remote (pushes refs there), as opposed to only
/// reading from it and writing locally. Fetch/Pull/Merge/Rebase all fetch
/// objects from the remote but only ever move local refs/the working tree —
/// a bad result is a local, cheaply-reverted mistake. Push is the only mode
/// that changes what's on the remote itself, which is what the multi-repo
/// confirmation in `App::run_action_on_targets` gates on.
const fn mode_writes_to_remote(mode: Mode) -> bool {
    matches!(mode, Mode::Push)
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
    /// Kind of panel content currently being fetched in the background, if
    /// any. Guards `PanelReady` against clobbering a newer view: if the user
    /// closed the panel or switched to a different kind before an in-flight
    /// fetch completes, the stale result is dropped instead of applied.
    pending_panel: Option<PanelKind>,
    /// Measured visible body height of the active panel. Updated during draw
    /// so keyboard navigation tracks the actual terminal size.
    panel_viewport: usize,
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
    /// Brief non-blocking feedback for selection and mode changes.
    notice: Option<(String, u64)>,
    /// Cached repo-table column widths, keyed by the area width and
    /// repo/worktree mode they were computed for. `None` forces a recompute.
    /// Set to `None` whenever repo or worktree data changes.
    col_widths: Option<ColWidths>,
}

/// Repo-table column widths, cached across frames (see `App::col_widths`).
#[derive(Clone, Copy)]
struct ColWidths {
    area_width: u16,
    in_wt: bool,
    repo_col_w: u16,
    branch_col_w: u16,
    age_col_w: u16,
    show_age: bool,
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
            pending_panel: None,
            panel_viewport: 1,
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
            notice: None,
            col_widths: None,
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
        let repo_path = self.current_repo_path()?;
        self.repo_mut(&repo_path)
    }

    /// The primary repo that owns the current selection: itself outside
    /// worktree mode, or the repo a selected worktree row belongs to. A
    /// linked (non-primary) worktree has no `RepoView` of its own, so this
    /// is not always the same as `current_path()`.
    fn current_repo_path(&self) -> Option<PathBuf> {
        if self.worktree_mode {
            self.wt_rows
                .get(self.wt_cursor)
                .map(|r| r.repo_path.clone())
        } else {
            self.current().map(|r| r.snapshot.path.clone())
        }
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

    fn set_notice(&mut self, message: impl Into<String>) {
        // At an 80ms tick, 38 ticks is just over three seconds.
        self.notice = Some((message.into(), self.tick.saturating_add(38)));
    }

    fn active_notice(&self) -> Option<&str> {
        self.notice
            .as_ref()
            .filter(|(_, until)| self.tick <= *until)
            .map(|(message, _)| message.as_str())
    }

    /// Clear an expired notice and report whether it did.
    ///
    /// `self.notice` otherwise stays `Some` (just past its `until`) forever
    /// once set — `active_notice()` alone would keep `is_animating()` seeing
    /// a *stale* one as gone but never actually own the redraw that erases
    /// it from the screen, since nothing else changed that tick. Clearing it
    /// here and reporting `true` for that one tick is what earns it that
    /// last frame.
    fn expire_notice(&mut self) -> bool {
        if self
            .notice
            .as_ref()
            .is_some_and(|(_, until)| self.tick > *until)
        {
            self.notice = None;
            true
        } else {
            false
        }
    }

    fn selection_scope_label(&self) -> String {
        match self.queued_count() {
            0 => "current repo".into(),
            1 => "1 selected".into(),
            count => format!("{count} selected"),
        }
    }

    /// Switch operation mode.
    ///
    /// Clears only the startup auto-selection (computed for the *previous*
    /// mode — carrying it into a new mode is exactly the defect this fixes:
    /// a pull preselection must never silently become a push batch). A
    /// selection the user built deliberately with `Space`/`a` survives; the
    /// notice instead reports how many of those no longer have anything
    /// relevant to do in the new mode, so the user can review before `Enter`.
    fn set_mode(&mut self, mode: Mode) {
        if self.mode == mode {
            return;
        }
        self.mode = mode;

        let mut cleared_auto = 0;
        for repo in &mut self.repos {
            if matches!(repo.state, RepoOpState::Queued(QueueOrigin::Auto)) {
                repo.state = RepoOpState::Idle;
                cleared_auto += 1;
            }
        }

        let user_selected: Vec<&RepoView> = self
            .repos
            .iter()
            .filter(|r| matches!(r.state, RepoOpState::Queued(QueueOrigin::User)))
            .collect();
        let ineligible = user_selected
            .iter()
            .filter(|r| !mode_has_relevant_commits(r, mode))
            .count();

        let mut notice = format!("Mode: {}", mode.as_str());
        if cleared_auto > 0 {
            let _ = write!(notice, " · cleared {cleared_auto} auto-selected");
        }
        if ineligible > 0 {
            let _ = write!(
                notice,
                " · {ineligible} of {} selected have nothing to {}",
                user_selected.len(),
                mode.as_str()
            );
        }
        self.set_notice(notice);
    }

    fn queue_all(&mut self) -> SelectionOutcome {
        let mode = self.mode;
        let mut outcome = SelectionOutcome::default();
        for repo in &mut self.repos {
            let b = &repo.snapshot.branch;
            // No upstream or detached HEAD: remote ops are impossible.
            if b.no_upstream || b.detached {
                outcome.no_upstream += 1;
                continue;
            }
            // A live merge/rebase conflict blocks any remote op outright. A
            // merely dirty (but conflict-free) tree is *not* skipped here —
            // `pull_safe` below is Phase 1.1's authoritative answer to
            // whether a dirty tree can still fast-forward.
            if repo.snapshot.has_conflicts {
                outcome.conflicted += 1;
                continue;
            }
            if !mode_has_relevant_commits(repo, mode) {
                outcome.no_updates += 1;
                continue;
            }
            // Fast-forward safety is a Pull-only requirement. Merge and Rebase
            // are the intentional ways to resolve diverged branches.
            if mode == Mode::Pull && !repo.pull_safe {
                outcome.unsafe_pull += 1;
                continue;
            }
            if matches!(
                repo.state,
                RepoOpState::Idle | RepoOpState::Success(_) | RepoOpState::Fail { .. }
            ) {
                repo.state = RepoOpState::Queued(QueueOrigin::User);
                outcome.selected += 1;
            }
        }
        outcome
    }

    /// `A`: clear the queue AND all finished results/markers in one stroke —
    /// the bulk counterpart to per-repo Esc.
    fn clear_queue(&mut self) {
        for repo in &mut self.repos {
            if matches!(repo.state, RepoOpState::Queued(_)) || repo.state.has_result() {
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

    /// Paths of selected (Queued) repos, or just the cursor repo when none
    /// are selected, filtered by an additional per-repo predicate.
    fn target_paths_where(&self, keep: impl Fn(&RepoView) -> bool) -> Vec<PathBuf> {
        // Single O(n) pass: the previous version collected every selected
        // path first and then, per path, rescanned all of `repos` to apply
        // `keep` — O(n²) when most repos are selected.
        let mut selected = Vec::new();
        let mut any_queued = false;
        for repo in &self.repos {
            if repo.is_queued() {
                any_queued = true;
                if keep(repo) {
                    selected.push(repo.snapshot.path.clone());
                }
            }
        }
        if any_queued {
            // Some repos are selected but none pass `keep`: that's a real
            // empty result, not "nothing selected" — falling back to the
            // cursor here would silently target a repo the user never chose.
            return selected;
        }
        match self.current() {
            Some(repo) if keep(repo) => vec![repo.snapshot.path.clone()],
            _ => Vec::new(),
        }
    }

    /// Paths of selected (Queued) repos, or just the cursor repo when none are selected.
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

    /// True when more than one repo is targeted (selected).
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
        // `order` preserves the repo table's current sort in the worktree
        // view (buffer_unordered below completes out of order); `repo_path`
        // is the stable identity each row is paired back to a repo with.
        let repos: Vec<_> = self
            .repos
            .iter()
            .enumerate()
            .map(|(order, r)| (order, r.snapshot.path.clone(), r.snapshot.name.clone()))
            .collect();
        let tx = self.event_tx.clone();
        let runner = self.runner.clone();
        let cap = worker_limit();
        tokio::spawn(async move {
            let mut results: Vec<(usize, PathBuf, String, String)> = stream::iter(repos)
                .map(|(order, repo_path, repo_name)| {
                    let runner = runner.clone();
                    async move {
                        runner
                            .worktree_list(&repo_path)
                            .await
                            .ok()
                            .map(|listing| (order, repo_path, repo_name, listing))
                    }
                })
                .buffer_unordered(cap)
                .filter_map(future::ready)
                .collect()
                .await;
            results.sort_by_key(|(order, ..)| *order);

            let mut wt_rows = Vec::new();
            for (_, repo_path, repo_name, listing) in results {
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
                        repo_path: repo_path.clone(),
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
                            repo_path: repo_path.clone(),
                        });
                    }
                }
            }
            let _ = tx.send(BgEvent::WorktreesReady(wt_rows));
        });
    }

    /// Returns whether any event was actually applied — the event-loop redraw
    /// gate's signal for "state changed since the last frame".
    fn drain_events(&mut self) -> bool {
        let mut any = false;
        while let Ok(event) = self.event_rx.try_recv() {
            self.apply_bg_event(event);
            any = true;
        }
        any
    }

    /// True while something on screen is mid-animation — a spinner or a
    /// counting-down notice — and therefore needs a redraw every tick even
    /// with no new event to react to.
    fn is_animating(&self) -> bool {
        self.loading
            || self.any_working()
            || self.repos.iter().any(|r| r.fetching)
            || self.active_notice().is_some()
    }

    fn apply_bg_event(&mut self, event: BgEvent) {
        // Every branch below mutates a repo's snapshot, its worktree rows, or
        // both — any of which can change the longest name/branch/age string
        // the repo table sizes its columns to. Invalidate unconditionally
        // rather than tracking each branch's effect individually; recompute
        // is cheap and only actually runs on the next draw.
        self.col_widths = None;
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
                            repo.state = RepoOpState::Queued(QueueOrigin::Auto);
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
            BgEvent::WorktreeOpResult {
                repo_path,
                label,
                result,
            } => {
                if let Some(repo) = self.repo_mut(&repo_path) {
                    repo.state = op_state_from_result(&result, label);
                }
                // Keeps both the repo table (dirty/ahead/behind, stash
                // count, …) and the worktree table itself in sync — a
                // remove/lock/add changes what `git worktree list` reports.
                self.spawn_refresh();
                if self.worktree_mode {
                    self.spawn_load_worktrees();
                }
            }
            BgEvent::PanelReady { kind, result } => {
                // Drop a result for a fetch the user has since moved past
                // (closed the panel, or opened a different kind) instead of
                // popping a stale view back open.
                if self.pending_panel != Some(kind) {
                    return;
                }
                self.pending_panel = None;
                match result {
                    Ok(state) => self.panel = Some(state),
                    Err(e) => {
                        self.panel = None;
                        self.set_notice(format!("Could not load panel: {e}"));
                    }
                }
            }
        }
    }

    // ── Operations ────────────────────────────────────────────────────────────

    fn run_current_mode(&mut self) {
        self.run_action_on_targets(self.mode);
    }

    /// Run a remote action (fetch/pull/merge/rebase/push) on all target
    /// repos — directly for a single target or for a mode that only reads
    /// from the remote, otherwise (mode writes to the remote, e.g. push,
    /// and more than one repo is targeted) behind a confirmation naming the
    /// mode and repo count.
    ///
    /// This is the single chokepoint for every outward-facing batch trigger
    /// (`Enter`, `p`, `P`, `f`), so a multi-repo push can never fire without
    /// the user seeing its scope first. Fetch/pull/merge/rebase only read
    /// from the remote and write locally — reverting a bad local result is
    /// cheap, so they fan out without asking.
    fn run_action_on_targets(&mut self, mode: Mode) {
        let paths = self.target_paths();
        if paths.is_empty() {
            return;
        }
        if paths.len() == 1 || !mode_writes_to_remote(mode) {
            let action = RemoteAction::from_mode(mode);
            for path in paths {
                self.spawn_op(path, action.clone());
            }
            return;
        }
        self.enqueue_confirm(ConfirmPromptState {
            title: confirm_title_for_mode(mode),
            subject: confirm_subject_for_mode(mode, paths.len()),
            warning: confirm_warning_for_mode(mode),
            action: ConfirmAction::RunMode { mode, paths },
        });
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

    // Every routine error source (a failed git command, a bad panel fetch,
    // …) is now caught well before it reaches here — see `handle_key` and
    // its callees. What can still surface is a genuine terminal-control
    // failure (crossterm/ratatui I/O). Even then, restore the terminal
    // before propagating: an early `?` here used to skip `restore_terminal`
    // entirely, leaving raw mode and the alternate screen engaged and the
    // user's shell looking broken until they blindly typed `reset`.
    let outcome = run_event_loop(&mut terminal, &mut app).await;
    restore_terminal(terminal)?;
    outcome
}

async fn run_event_loop(terminal: &mut DefaultTerminal, app: &mut App) -> Result<()> {
    // An idle gitbatch sitting on a couple hundred repos has no reason to
    // repaint 12.5 times a second: redraw only when a background event
    // landed, a key/paste/resize was handled, or something is mid-animation
    // (a spinner, a counting-down notice) and needs the next tick's frame.
    // Background events still drain and mutate state every loop iteration
    // either way — only the terminal repaint itself is what's gated.
    let mut needs_redraw = true;
    loop {
        let events_applied = app.drain_events();
        app.tick = app.tick.wrapping_add(1);

        if !app.loading
            && app.last_refresh.elapsed() >= Duration::from_secs(REFRESH_SECS)
            && !app.any_working()
            && app.auth_prompt.is_none()
            && app.confirm_prompt.is_none()
        {
            app.spawn_refresh();
        }

        let notice_expired = app.expire_notice();
        needs_redraw = needs_redraw || events_applied || notice_expired || app.is_animating();

        if needs_redraw {
            if app.needs_full_redraw {
                app.needs_full_redraw = false;
                terminal.clear()?;
            }
            terminal.draw(|frame| draw(frame, app))?;
            needs_redraw = false;
        }

        if !event::poll(Duration::from_millis(TICK_MS))? {
            continue;
        }

        match event::read()? {
            Event::Key(key)
                if key.kind == KeyEventKind::Press
                    && app.handle_key(key.code, key.modifiers).await? =>
            {
                return Ok(());
            }
            Event::Key(_) => needs_redraw = true,
            Event::Paste(text) => {
                app.handle_paste(&text);
                needs_redraw = true;
            }
            Event::Resize(_, _) => needs_redraw = true,
            _ => {}
        }
    }
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

// ── Multi-repo remote-op confirmation copy ────────────────────────────────

fn confirm_title_for_mode(mode: Mode) -> &'static str {
    match mode {
        Mode::Fetch => "Fetch?",
        Mode::Pull => "Pull?",
        Mode::Merge => "Merge?",
        Mode::Rebase => "Rebase?",
        Mode::Push => "Push?",
    }
}

fn confirm_subject_for_mode(mode: Mode, count: usize) -> String {
    match mode {
        Mode::Push => format!("Push to {count} remotes"),
        Mode::Fetch => format!("Fetch {count} repositories"),
        Mode::Pull => format!("Pull {count} repositories"),
        Mode::Merge => format!("Merge {count} repositories"),
        Mode::Rebase => format!("Rebase {count} repositories"),
    }
}

fn confirm_warning_for_mode(mode: Mode) -> String {
    match mode {
        Mode::Fetch => "Read-only, but reaches out to every remote below.".into(),
        Mode::Pull => "Fast-forwards every repo below from its remote.".into(),
        Mode::Merge => "Merges the upstream branch into every repo below.".into(),
        Mode::Rebase => "Rewrites local history in every repo below.".into(),
        Mode::Push => "Pushes local commits to every remote below.".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    fn test_repo_view(name: &str, dirty: bool) -> RepoView {
        RepoView::new(RepositorySnapshot {
            path: PathBuf::from(name),
            name: name.to_string(),
            branch: crate::git::BranchStatus::default(),
            dirty,
            has_conflicts: false,
            last_modified: None,
            commit_subject: String::new(),
            stash_count: 0,
        })
    }

    #[test]
    fn target_paths_where_is_empty_when_selected_repos_fail_keep() {
        let mut app = App::new(GitRunner::default(), Mode::Pull, false, 0);
        app.loading = false;

        let mut a = test_repo_view("a-clean-selected", false);
        a.state = RepoOpState::Queued(QueueOrigin::User);
        let mut b = test_repo_view("b-clean-selected", false);
        b.state = RepoOpState::Queued(QueueOrigin::User);
        // Not selected, but dirty and under the cursor — a fallback-to-cursor
        // bug would incorrectly pick this one up.
        let c = test_repo_view("c-dirty-unselected", true);
        app.repos = vec![a, b, c];
        app.cursor = 2;

        let dirty_targets = app.target_paths_where(|r| r.snapshot.dirty);

        assert!(
            dirty_targets.is_empty(),
            "two repos are selected (both clean); the dirty, unselected \
             cursor repo must not be silently substituted in: {dirty_targets:?}"
        );
    }

    #[test]
    fn target_paths_where_falls_back_to_cursor_when_nothing_selected() {
        let mut app = App::new(GitRunner::default(), Mode::Pull, false, 0);
        app.loading = false;
        app.repos = vec![test_repo_view("only-repo", true)];
        app.cursor = 0;

        assert_eq!(
            app.target_paths_where(|r| r.snapshot.dirty),
            vec![PathBuf::from("only-repo")]
        );
    }

    #[tokio::test]
    async fn multi_repo_pull_runs_without_confirmation() {
        let mut app = App::new(GitRunner::default(), Mode::Pull, false, 0);
        app.loading = false;
        let mut a = test_repo_view("a", false);
        a.state = RepoOpState::Queued(QueueOrigin::User);
        let mut b = test_repo_view("b", false);
        b.state = RepoOpState::Queued(QueueOrigin::User);
        app.repos = vec![a, b];

        app.run_action_on_targets(Mode::Pull);

        assert!(
            app.confirm_prompt.is_none(),
            "pull/fetch/merge/rebase only read from the remote and write \
             locally, so a multi-repo run must not require confirmation"
        );
    }

    #[tokio::test]
    async fn multi_repo_push_requires_confirmation() {
        let mut app = App::new(GitRunner::default(), Mode::Pull, false, 0);
        app.loading = false;
        let mut a = test_repo_view("a", false);
        a.state = RepoOpState::Queued(QueueOrigin::User);
        let mut b = test_repo_view("b", false);
        b.state = RepoOpState::Queued(QueueOrigin::User);
        app.repos = vec![a, b];

        app.run_action_on_targets(Mode::Push);

        assert!(
            app.confirm_prompt.is_some(),
            "push changes what's on the remote, so a multi-repo push must \
             still require confirmation"
        );
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
}
