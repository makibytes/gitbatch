//! Key handling: `App::handle_key` and everything it dispatches to —
//! prompt/panel/confirm/auth submission and the popup-specific key
//! handlers. A second `impl App` block, split out from the state/spawn
//! methods in `tui/mod.rs` purely for file size; `use super::*` gives it
//! the same access to `App`'s fields that a method defined in `mod.rs`
//! itself would have.

use super::*;

impl App {
    // ── Panel handling ────────────────────────────────────────────────────────

    /// Fetch `kind`'s content in the background and apply it via
    /// `BgEvent::PanelReady` once it arrives — a git round-trip (or several,
    /// for a multi-repo Branches intersection) must never block the render
    /// loop, and a failure (e.g. a corrupt index) must surface as a notice
    /// rather than unwind out of the key handler and end the session.
    pub(super) fn open_panel(&mut self, kind: PanelKind) {
        let Some(current) = self.current_path() else {
            return;
        };
        let targets = if self.has_multi_target() {
            self.target_paths()
        } else {
            Vec::new()
        };
        self.pending_panel = Some(kind);
        let runner = self.runner.clone();
        let tx = self.event_tx.clone();
        tokio::spawn(async move {
            let result = load_panel_content(&runner, kind, &current, &targets).await;
            let _ = tx.send(BgEvent::PanelReady { kind, result });
        });
    }

    // ── Prompt submission ─────────────────────────────────────────────────────

    /// Everything a submitted prompt can do is now a background spawn (see
    /// `spawn_local_op`/`spawn_worktree_op`), so unlike its predecessor this
    /// can never fail or block the render loop.
    pub(super) fn submit_prompt(&mut self, prompt: PromptState) {
        let input = prompt.input.trim().to_string();
        if input.is_empty() && prompt.kind != PromptKind::Stash {
            return;
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
                return;
            }
            PromptKind::WorktreeBranch => {
                // Worktree operations stay single-repo (not meaningful to batch).
                let Some(path) = self.current_path() else {
                    return;
                };
                let branch = prompt.input.trim().to_string();
                let dest = default_worktree_path(&path, &branch);
                self.spawn_worktree_op(WorktreeOp::Add { dest, branch });
                return;
            }
        };

        if paths.is_empty() {
            return;
        }

        // Fan out as background tasks.
        for path in paths {
            self.spawn_local_op(path, action.clone());
        }
        self.panel = None;
    }

    // ── Auth prompt ───────────────────────────────────────────────────────────

    pub(super) fn enqueue_auth_prompt(&mut self, path: PathBuf, action: RemoteAction) {
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

    pub(super) fn advance_auth_prompt(&mut self) {
        self.auth_prompt = self.auth_prompt_queue.pop_front();
    }

    pub(super) fn submit_auth_prompt(&mut self, mut prompt: AuthPromptState) -> Result<()> {
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

    pub(super) fn cancel_auth_prompt(&mut self) {
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

    pub(super) fn enqueue_confirm(&mut self, prompt: ConfirmPromptState) {
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

    pub(super) fn dismiss_confirm(&mut self) {
        self.confirm_prompt = self.confirm_prompt_queue.pop_front();
    }

    pub(super) fn execute_confirm(&mut self, hard: bool) {
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
            ConfirmAction::RunMode { mode, paths } => {
                let action = RemoteAction::from_mode(mode);
                for path in paths {
                    self.spawn_op(path, action.clone());
                }
            }
        }
    }

    /// Subject line for a confirm dialog: the repo name, or the batch size.
    pub(super) fn confirm_subject(&self, paths: &[PathBuf]) -> String {
        if paths.len() == 1 {
            self.repo_name_for_path(&paths[0])
                .unwrap_or_else(|| "1 repo".into())
        } else {
            format!("{} repos", paths.len())
        }
    }

    // ── Worktree helpers ──────────────────────────────────────────────────────

    /// Run `op` against the currently selected worktree, entirely in the
    /// background (see `run_worktree_op`). The result lands on the primary
    /// repo's `RepoView` (`current_repo_path`, not necessarily the same path
    /// as the worktree acted on) via `BgEvent::WorktreeOpResult`.
    pub(super) fn spawn_worktree_op(&mut self, op: WorktreeOp) {
        let Some(selected_path) = self.current_path() else {
            return;
        };
        let Some(repo_path) = self.current_repo_path() else {
            return;
        };
        let label = op.label();
        if let Some(repo) = self.repo_mut(&repo_path) {
            repo.state = RepoOpState::Working;
        }
        let runner = self.runner.clone();
        let tx = self.event_tx.clone();
        tokio::spawn(async move {
            let result = run_worktree_op(&runner, &selected_path, op).await;
            let _ = tx.send(BgEvent::WorktreeOpResult {
                repo_path,
                label,
                result,
            });
        });
    }

    // ── Key handling ──────────────────────────────────────────────────────────

    pub(super) async fn handle_key(&mut self, code: KeyCode, mods: KeyModifiers) -> Result<bool> {
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
            KeyCode::Char('a') => {
                let outcome = self.queue_all();
                self.set_notice(outcome.notice());
            }
            KeyCode::Char('A') => self.clear_queue(),
            KeyCode::Char('m') => self.set_mode(self.mode.cycle()),
            KeyCode::Char('t') => self.toggle_sort(),
            KeyCode::Char('b') => self.open_panel(PanelKind::Branches),
            KeyCode::Char('s') => self.open_panel(PanelKind::Status),
            KeyCode::Char('v') => self.open_panel(PanelKind::Commits),
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
            KeyCode::Char('d') if self.worktree_mode => self.spawn_worktree_op(WorktreeOp::Remove),
            KeyCode::Char('L') if self.worktree_mode => {
                self.spawn_worktree_op(WorktreeOp::ToggleLock);
            }
            KeyCode::Char('X') if self.worktree_mode => self.spawn_worktree_op(WorktreeOp::Prune),
            KeyCode::Tab => self.open_lazygit()?,
            _ => {}
        }
        Ok(false)
    }

    pub(super) fn show_prompt(&mut self, kind: PromptKind) {
        self.prompt = Some(PromptState {
            kind,
            input: String::new(),
            cursor: 0,
            error: None,
            description: String::new(),
            commit_field: CommitField::Subject,
        });
    }

    pub(super) fn show_prompt_prefilled(&mut self, kind: PromptKind, value: String) {
        self.show_prompt(kind);
        if let Some(ref mut p) = self.prompt {
            p.cursor = value.len();
            p.input = value;
        }
    }

    pub(super) async fn handle_prompt_key(
        &mut self,
        code: KeyCode,
        mods: KeyModifiers,
    ) -> Result<bool> {
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
                        self.submit_prompt(prompt);
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

    pub(super) fn handle_paste(&mut self, text: &str) {
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

    pub(super) async fn handle_auth_key(&mut self, code: KeyCode) -> Result<bool> {
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

    pub(super) fn handle_confirm_key(&mut self, code: KeyCode) -> Result<bool> {
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
    pub(super) fn selected_branch_name(&self) -> Option<String> {
        self.panel
            .as_ref()
            .and_then(|p| p.selected_text())
            .and_then(parse_branch_name)
            .map(ToString::to_string)
    }

    /// Delete a remote branch (`origin/x` form): confirm dialog when fanning
    /// out over selected repos, editable prompt for a single repo.
    pub(super) fn remote_delete_flow(&mut self, name: String) {
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

    pub(super) fn handle_panel_key(&mut self, code: KeyCode, mods: KeyModifiers) -> Result<bool> {
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

        // Measured during the last draw (see `draw_panel_popup`); falls back
        // to 1 before the first frame so a scroll before that can't panic,
        // rather than the fixed `20` that over/under-shot on any terminal
        // where the popup's actual body height differed.
        let viewport = self.panel_viewport.max(1);

        match code {
            KeyCode::Up | KeyCode::Char('k') => {
                if let Some(p) = &mut self.panel {
                    if is_navigable {
                        p.move_cursor(-1, viewport);
                    } else {
                        p.scroll_text(-1, viewport);
                    }
                }
                return Ok(true);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if let Some(p) = &mut self.panel {
                    if is_navigable {
                        p.move_cursor(1, viewport);
                    } else {
                        p.scroll_text(1, viewport);
                    }
                }
                return Ok(true);
            }
            KeyCode::PageUp => {
                if let Some(p) = &mut self.panel {
                    if is_navigable {
                        p.move_cursor(-10, viewport);
                    } else {
                        p.scroll_text(-10, viewport);
                    }
                }
                return Ok(true);
            }
            KeyCode::PageDown => {
                if let Some(p) = &mut self.panel {
                    if is_navigable {
                        p.move_cursor(10, viewport);
                    } else {
                        p.scroll_text(10, viewport);
                    }
                }
                return Ok(true);
            }
            KeyCode::Char('g') | KeyCode::Home => {
                if let Some(p) = &mut self.panel {
                    if is_navigable {
                        p.cursor_to(0, viewport);
                    } else {
                        p.scroll = 0;
                    }
                }
                return Ok(true);
            }
            KeyCode::Char('G') | KeyCode::End => {
                if let Some(p) = &mut self.panel {
                    if is_navigable {
                        p.cursor_to(p.lines.len().saturating_sub(1), viewport);
                    } else {
                        p.scroll_text(p.lines.len() as isize, viewport);
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

    pub(super) fn open_lazygit(&mut self) -> Result<()> {
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
}
