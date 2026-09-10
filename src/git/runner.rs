use std::{
    collections::HashSet,
    ffi::OsStr,
    fs,
    fs::OpenOptions,
    io::Write,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

use regex::Regex;
use tokio::{process::Command, time::timeout};

use crate::{AppError, Result, mode::Mode};

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(90);

#[derive(Debug, Clone)]
pub struct GitRunner {
    timeout: Duration,
    trace_log: Option<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct GitOutput {
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

impl GitOutput {
    pub fn combined(&self) -> String {
        let stdout = self.stdout.trim();
        let stderr = self.stderr.trim();
        match (stdout.is_empty(), stderr.is_empty()) {
            (false, false) => format!("{stdout}\n{stderr}"),
            (false, true) => stdout.to_string(),
            (true, false) => stderr.to_string(),
            (true, true) => String::new(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Credentials {
    pub username: String,
    pub password: String,
}

/// Identifies the stash entry created for one automatic operation.
///
/// The object ID is intentionally private: callers can only give the guard
/// back to `restore_auto_stash`, which prevents an automatic restore from
/// popping a stash entry that was already present (or was created later).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutoStashGuard {
    oid: String,
}

impl Default for GitRunner {
    fn default() -> Self {
        Self {
            timeout: DEFAULT_TIMEOUT,
            trace_log: None,
        }
    }
}

impl GitRunner {
    pub fn new(timeout: Duration) -> Self {
        Self {
            timeout,
            trace_log: None,
        }
    }

    pub fn with_trace(timeout: Duration, trace_log: Option<PathBuf>) -> Self {
        Self { timeout, trace_log }
    }

    pub async fn run<I, S>(&self, dir: &Path, args: I) -> Result<GitOutput>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.run_internal(dir, args, None, true).await
    }

    pub async fn run_with_credentials<I, S>(
        &self,
        dir: &Path,
        args: I,
        credentials: &Credentials,
    ) -> Result<GitOutput>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.run_internal(dir, args, Some(credentials), true).await
    }

    /// Like `run`, but disables git's optional-lock taking
    /// (`GIT_OPTIONAL_LOCKS=0`) for a call that only reads repository state.
    /// Plain `git status`/`log`/`branch` otherwise take (and refresh) the
    /// index lock, which contends with a user's editor and with gitbatch's
    /// own concurrent read-only workers running across dozens of repos at
    /// once. Never use this for a command that writes.
    pub(crate) async fn run_readonly<I, S>(&self, dir: &Path, args: I) -> Result<GitOutput>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.run_internal(dir, args, None, false).await
    }

    async fn run_internal<I, S>(
        &self,
        dir: &Path,
        args: I,
        credentials: Option<&Credentials>,
        take_optional_locks: bool,
    ) -> Result<GitOutput>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let args_vec: Vec<_> = args
            .into_iter()
            .map(|item| item.as_ref().to_owned())
            .collect();
        let command_display = format!(
            "git {}",
            args_vec
                .iter()
                .map(|arg| arg.to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join(" ")
        );

        let askpass = match credentials {
            Some(creds) => Some(AskPassScript::create(creds).map_err(|error| {
                AppError::Config(format!("failed to create askpass helper: {error}"))
            })?),
            None => None,
        };
        let mut command = Command::new("git");
        command
            .args(&args_vec)
            .current_dir(dir)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .env("GIT_TERMINAL_PROMPT", "0")
            .env(
                "GIT_SSH_COMMAND",
                "ssh -o BatchMode=yes -o ConnectTimeout=5 -o ConnectionAttempts=1",
            )
            .env("GIT_HTTP_LOW_SPEED_LIMIT", "1")
            .env("GIT_HTTP_LOW_SPEED_TIME", "10")
            .env("LANG", "C")
            .env("LC_ALL", "C")
            // stdin is already null: any git subcommand that would open an
            // editor (merge, rebase -i, commit --amend, …) must fail cleanly
            // instead of hanging or reading garbage from a closed stdin.
            .env("GIT_EDITOR", "true")
            .env("GIT_SEQUENCE_EDITOR", "true");
        if !take_optional_locks {
            command.env("GIT_OPTIONAL_LOCKS", "0");
        }
        if let Some(askpass) = askpass.as_ref() {
            command
                .env("GIT_ASKPASS", &askpass.path)
                .env("SSH_ASKPASS", &askpass.path)
                .env("SSH_ASKPASS_REQUIRE", "force")
                .env("DISPLAY", "gitbatch:0")
                .env("GITBATCH_USERNAME", &askpass.username)
                .env("GITBATCH_PASSWORD", &askpass.password);
        }

        let output = timeout(self.timeout, command.output())
            .await
            .map_err(|_| AppError::CommandTimeout {
                command: command_display.clone(),
                timeout: self.timeout,
            })??;

        let result = GitOutput {
            code: output.status.code(),
            stdout: String::from_utf8_lossy(&output.stdout).trim().to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        };

        let combined = result.combined();
        self.trace_command(dir, &command_display, result.code, &combined);
        if output.status.success() {
            return Ok(result);
        }

        if looks_like_credential_prompt(&combined) {
            return Err(AppError::CredentialPromptDetected { output: combined });
        }

        Err(AppError::GitCommandFailed {
            command: command_display,
            code: result.code,
            output: combined,
        })
    }

    pub async fn status_snapshot(&self, dir: &Path) -> Result<crate::git::RepositorySnapshot> {
        crate::git::RepositorySnapshot::load(self, dir).await
    }

    pub async fn run_remote_action(&self, dir: &Path, action: RemoteAction) -> Result<String> {
        self.run_remote_action_with_optional_credentials(dir, action, None)
            .await
    }

    pub async fn run_remote_action_with_credentials(
        &self,
        dir: &Path,
        action: RemoteAction,
        credentials: &Credentials,
    ) -> Result<String> {
        self.run_remote_action_with_optional_credentials(dir, action, Some(credentials))
            .await
    }

    async fn run_remote_action_with_optional_credentials(
        &self,
        dir: &Path,
        action: RemoteAction,
        credentials: Option<&Credentials>,
    ) -> Result<String> {
        match action {
            RemoteAction::Fetch => self.run_args(dir, &["fetch", "--prune"], credentials).await,
            RemoteAction::PullFfOnly => {
                self.run_args(
                    dir,
                    // Override `pull.rebase` explicitly: this operation is
                    // advertised as fast-forward-only and must not silently
                    // become a rebase because of a repository or global config.
                    &["pull", "--ff-only", "--no-rebase", "--stat", "--progress"],
                    credentials,
                )
                .await
            }
            RemoteAction::Merge => {
                self.run_args(dir, &["fetch", "--prune"], credentials)
                    .await?;
                // GIT_EDITOR=true already prevents a hang if an editor would
                // open, but --no-edit also skips generating the merge
                // commit message in the first place.
                self.run_args(dir, &["merge", "--no-edit", "@{upstream}"], credentials)
                    .await
            }
            RemoteAction::Rebase => {
                self.run_args(
                    dir,
                    &["pull", "--rebase", "--stat", "--progress"],
                    credentials,
                )
                .await
            }
            RemoteAction::Push { force } => {
                // A branch without an upstream can't plain-push; push with -u
                // so the remote branch is created and tracking is set.
                let has_upstream = self
                    .run(dir, ["rev-parse", "--abbrev-ref", "@{upstream}"])
                    .await
                    .is_ok();
                if has_upstream {
                    if force {
                        // `--force-with-lease` alone compares against the
                        // locally cached remote-tracking ref, which
                        // gitbatch's own startup and periodic background
                        // fetches keep advancing — so a lease alone can miss
                        // a remote change that arrived between the fetch and
                        // this push. `--force-if-includes` additionally
                        // requires the remote's current tip to be an
                        // ancestor of what our tracking ref last saw,
                        // closing that gap.
                        self.run_args(
                            dir,
                            &["push", "--force-with-lease", "--force-if-includes"],
                            credentials,
                        )
                        .await
                    } else {
                        self.run_args(dir, &["push"], credentials).await
                    }
                } else {
                    let remotes = self.run_simple(dir, ["remote"]).await.unwrap_or_default();
                    let remote = remotes
                        .lines()
                        .map(str::trim)
                        .filter(|l| !l.is_empty())
                        .find(|&l| l == "origin")
                        .or_else(|| remotes.lines().map(str::trim).find(|l| !l.is_empty()))
                        .unwrap_or("origin")
                        .to_string();
                    let mut args = vec!["push", "-u", remote.as_str(), "HEAD"];
                    if force {
                        args.push("--force-with-lease");
                        args.push("--force-if-includes");
                    }
                    self.run_args(dir, &args, credentials).await
                }
            }
        }
    }

    async fn run_args(
        &self,
        dir: &Path,
        args: &[&str],
        credentials: Option<&Credentials>,
    ) -> Result<String> {
        let output = match credentials {
            Some(credentials) => self.run_with_credentials(dir, args, credentials).await?,
            None => self.run(dir, args).await?,
        };
        Ok(output.combined())
    }

    pub async fn run_mode(&self, dir: &Path, mode: Mode) -> Result<String> {
        self.run_remote_action(dir, RemoteAction::from_mode(mode))
            .await
    }

    pub async fn run_simple<const N: usize>(&self, dir: &Path, args: [&str; N]) -> Result<String> {
        let output = self.run(dir, args).await?;
        Ok(output.combined())
    }

    pub async fn branch_list(&self, dir: &Path) -> Result<String> {
        let output = self
            .run_readonly(
                dir,
                ["branch", "--all", "--verbose", "--verbose", "--no-abbrev"],
            )
            .await?;
        Ok(output.combined())
    }

    pub async fn commit_log(&self, dir: &Path) -> Result<String> {
        let output = self
            .run_readonly(
                dir,
                [
                    "log",
                    "--decorate",
                    "--graph",
                    "--oneline",
                    "--max-count",
                    "20",
                ],
            )
            .await?;
        Ok(output.combined())
    }

    pub async fn status_text(&self, dir: &Path) -> Result<String> {
        let output = self
            .run_readonly(dir, ["status", "--short", "--branch"])
            .await?;
        Ok(output.combined())
    }

    pub async fn create_branch(&self, dir: &Path, name: &str) -> Result<String> {
        self.run_simple(dir, ["checkout", "-b", name]).await
    }

    pub async fn checkout_reference(&self, dir: &Path, name: &str) -> Result<String> {
        self.run_simple(dir, ["checkout", name]).await
    }

    pub async fn checkout_remote_branch(&self, dir: &Path, name: &str) -> Result<String> {
        let Some((_, local_branch)) = name.split_once('/') else {
            return Err(AppError::Config(format!(
                "remote branch must be in remote/branch form: {name}"
            )));
        };

        // Do not shorten nested refs (`origin/feature/foo` -> `foo`): that
        // could select an unrelated local branch. If the intended local
        // branch already exists, check it out by its complete ref name;
        // otherwise let Git create the matching tracking branch.
        if self
            .run(
                dir,
                [
                    "show-ref",
                    "--verify",
                    "--quiet",
                    &format!("refs/heads/{local_branch}"),
                ],
            )
            .await
            .is_ok()
        {
            self.checkout_reference(dir, local_branch).await
        } else {
            self.run_simple(dir, ["checkout", "--track", name]).await
        }
    }

    pub async fn delete_local_branch(&self, dir: &Path, name: &str) -> Result<String> {
        self.run_simple(dir, ["branch", "-d", name]).await
    }

    pub async fn force_delete_local_branch(&self, dir: &Path, name: &str) -> Result<String> {
        self.run_simple(dir, ["branch", "-D", name]).await
    }

    pub async fn delete_remote_branch(&self, dir: &Path, name: &str) -> Result<String> {
        let mut parts = name.splitn(2, '/');
        let remote = parts.next().unwrap_or("origin");
        let branch = parts.next().unwrap_or(name);
        self.run_simple(dir, ["push", remote, "--delete", branch])
            .await
    }

    pub async fn commit_all(&self, dir: &Path, message: &str) -> Result<String> {
        self.run(dir, ["add", "-A"]).await?;
        self.run_simple(dir, ["commit", "-m", message]).await
    }

    pub async fn stash_push(&self, dir: &Path, message: Option<&str>) -> Result<String> {
        match message {
            Some(message) if !message.trim().is_empty() => {
                self.run_simple(dir, ["stash", "push", "-m", message]).await
            }
            _ => self.run_simple(dir, ["stash", "push"]).await,
        }
    }

    pub async fn stash_push_include_untracked(
        &self,
        dir: &Path,
        message: Option<&str>,
    ) -> Result<String> {
        match message {
            Some(message) if !message.trim().is_empty() => {
                self.run_simple(dir, ["stash", "push", "--include-untracked", "-m", message])
                    .await
            }
            _ => {
                self.run_simple(dir, ["stash", "push", "--include-untracked"])
                    .await
            }
        }
    }

    /// Stash a tree for an automatic operation, returning a guard only when
    /// this invocation actually created a new top-of-stack stash entry.
    ///
    /// `git stash push` succeeds even if there is nothing to save. Comparing
    /// `refs/stash` before and after avoids later popping a user's existing
    /// stash in that no-op case.
    pub async fn create_auto_stash(&self, dir: &Path) -> Result<Option<AutoStashGuard>> {
        let before = self.stash_oid(dir).await?;
        self.stash_push_include_untracked(dir, Some("gitbatch auto-stash"))
            .await?;
        let after = self.stash_oid(dir).await?;

        Ok((after != before)
            .then_some(after)
            .flatten()
            .map(|oid| AutoStashGuard { oid }))
    }

    /// Restore precisely the entry produced by `create_auto_stash`.
    ///
    /// A changed top-of-stack means a user or another process intervened. In
    /// that case leave every stash untouched and return an error rather than
    /// risking restoration of the wrong entry.
    pub async fn restore_auto_stash(&self, dir: &Path, guard: AutoStashGuard) -> Result<String> {
        if self.stash_oid(dir).await?.as_deref() != Some(&guard.oid) {
            return Err(AppError::AutoStashNotRestored {
                reason: "the stash stack changed since the auto-stash was created".into(),
            });
        }
        self.stash_pop(dir).await
    }

    pub async fn stash_pop(&self, dir: &Path) -> Result<String> {
        self.run_simple(dir, ["stash", "pop"]).await
    }

    pub async fn stash_drop(&self, dir: &Path) -> Result<String> {
        self.run_simple(dir, ["stash", "drop"]).await
    }

    async fn stash_oid(&self, dir: &Path) -> Result<Option<String>> {
        match self
            .run(dir, ["rev-parse", "-q", "--verify", "refs/stash"])
            .await
        {
            Ok(output) => Ok(Some(output.stdout)),
            Err(AppError::GitCommandFailed { code: Some(1), .. }) => Ok(None),
            Err(error) => Err(error),
        }
    }

    pub async fn worktree_list(&self, dir: &Path) -> Result<String> {
        self.run_simple(dir, ["worktree", "list", "--porcelain"])
            .await
    }

    pub async fn worktree_add(
        &self,
        dir: &Path,
        path: &Path,
        branch: &str,
        create_new: bool,
    ) -> Result<String> {
        let path_string = path_to_string(path);
        if create_new {
            self.run_simple(dir, ["worktree", "add", "-b", branch, &path_string])
                .await
        } else {
            self.run_simple(dir, ["worktree", "add", &path_string, branch])
                .await
        }
    }

    pub async fn worktree_remove(&self, dir: &Path, path: &Path) -> Result<String> {
        let path_string = path_to_string(path);
        self.run_simple(dir, ["worktree", "remove", &path_string])
            .await
    }

    pub async fn worktree_lock(&self, dir: &Path, path: &Path) -> Result<String> {
        let path_string = path_to_string(path);
        self.run_simple(dir, ["worktree", "lock", &path_string])
            .await
    }

    pub async fn worktree_unlock(&self, dir: &Path, path: &Path) -> Result<String> {
        let path_string = path_to_string(path);
        self.run_simple(dir, ["worktree", "unlock", &path_string])
            .await
    }

    pub async fn worktree_prune(&self, dir: &Path) -> Result<String> {
        self.run_simple(dir, ["worktree", "prune"]).await
    }

    /// Reset branch to upstream (safe: --mixed, doesn't discard changes)
    pub async fn reset_to_upstream(&self, dir: &Path) -> Result<String> {
        self.run_simple(dir, ["reset", "--mixed", "@{upstream}"])
            .await
    }

    /// Hard reset to upstream (discards all local changes and commits)
    pub async fn hard_reset_to_upstream(&self, dir: &Path) -> Result<String> {
        self.run_simple(dir, ["reset", "--hard", "@{upstream}"])
            .await
    }

    /// Set upstream branch for tracking
    pub async fn set_upstream(&self, dir: &Path, remote: &str, branch: &str) -> Result<String> {
        let tracking_ref = format!("{remote}/{branch}");
        self.run_simple(dir, ["branch", "-u", &tracking_ref]).await
    }

    /// Returns true when a fast-forward-only pull against `upstream` can run
    /// without conflicting with the current worktree.
    ///
    /// `HEAD` must be an ancestor of `upstream` — that's exactly the
    /// condition under which `git pull --ff-only` succeeds; a clean 3-way
    /// merge is still not a fast-forward and must not be auto-queued here.
    /// On a clean tree that ancestry check alone is sufficient. On a dirty
    /// tree, `--ff-only` still succeeds unless an incoming path collides
    /// with a path Git considers locally changed (modified, staged, or
    /// untracked) — Git refuses to overwrite any of those, fast-forward or
    /// not. Errors are treated as unsafe.
    pub async fn fast_forward_safe(&self, dir: &Path, upstream: &str, clean: bool) -> bool {
        if upstream.is_empty() {
            return false;
        }
        let pure_ff = self
            .run_readonly(dir, ["merge-base", "--is-ancestor", "HEAD", upstream])
            .await
            .is_ok();
        if !pure_ff {
            return false;
        }
        if clean {
            return true;
        }

        // Dirty tree: a fast-forward still fails if an incoming path
        // overlaps a locally modified, staged, or untracked path. Query all
        // four NUL-delimited path lists concurrently — a serial loop would
        // stall the safety check for the sum of four git round-trips.
        let (unstaged, staged, untracked, incoming) = tokio::join!(
            self.run_readonly(dir, ["diff", "-z", "--name-only"]),
            self.run_readonly(dir, ["diff", "-z", "--cached", "--name-only"]),
            self.run_readonly(dir, ["ls-files", "-z", "--others", "--exclude-standard"]),
            self.run_readonly(dir, ["diff", "-z", "--name-only", "HEAD", upstream]),
        );
        let (Ok(unstaged), Ok(staged), Ok(untracked), Ok(incoming)) =
            (unstaged, staged, untracked, incoming)
        else {
            return false;
        };

        let incoming_paths = parse_nul_list(&incoming.stdout);
        let mut local_paths = parse_nul_list(&unstaged.stdout);
        local_paths.extend(parse_nul_list(&staged.stdout));
        local_paths.extend(parse_nul_list(&untracked.stdout));

        local_paths.is_disjoint(&incoming_paths)
    }

    fn trace_command(&self, dir: &Path, command: &str, code: Option<i32>, output: &str) {
        let Some(path) = &self.trace_log else {
            return;
        };
        let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) else {
            return;
        };
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|value| value.as_secs())
            .unwrap_or_default();
        let _ = writeln!(
            file,
            "[{timestamp}] cwd={} code={code:?} cmd={command} output={}",
            dir.display(),
            output.replace('\n', " | ")
        );
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteAction {
    Fetch,
    /// Pull with fast-forward only (fails if branches diverge)
    PullFfOnly,
    Merge,
    /// Pull with rebase
    Rebase,
    Push {
        force: bool,
    },
}

impl RemoteAction {
    pub fn from_mode(mode: Mode) -> Self {
        match mode {
            Mode::Fetch => Self::Fetch,
            Mode::Pull => Self::PullFfOnly,
            Mode::Merge => Self::Merge,
            Mode::Rebase => Self::Rebase,
            Mode::Push => Self::Push { force: false },
        }
    }

    pub fn is_push(&self) -> bool {
        matches!(self, Self::Push { .. })
    }
}

struct AskPassScript {
    path: PathBuf,
    username: String,
    password: String,
}

impl AskPassScript {
    fn create(credentials: &Credentials) -> std::io::Result<Self> {
        let path = std::env::temp_dir().join(format!(
            "gitbatch-askpass-{}-{}.sh",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|value| value.as_nanos())
                .unwrap_or_default()
        ));
        fs::write(
            &path,
            "#!/bin/sh\ncase \"$1\" in\n  *sername*|*USERNAME*) printf '%s' \"$GITBATCH_USERNAME\" ;;\n  *) printf '%s' \"$GITBATCH_PASSWORD\" ;;\nesac\n",
        )?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o700))?;
        }
        Ok(Self {
            path,
            username: credentials.username.clone(),
            password: credentials.password.clone(),
        })
    }
}

impl Drop for AskPassScript {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn looks_like_credential_prompt(output: &str) -> bool {
    static PROMPTS: std::sync::LazyLock<Vec<Regex>> = std::sync::LazyLock::new(|| {
        vec![
            Regex::new(r"Password:").unwrap(),
            Regex::new(r".+['’]s password:").unwrap(),
            Regex::new(r"Password\s*for\s*'.+':").unwrap(),
            Regex::new(r"Username\s*for\s*'.+':").unwrap(),
            Regex::new(r"Enter\s*passphrase\s*for\s*key\s*'.+':").unwrap(),
            Regex::new(r"Enter\s*PIN\s*for\s*.+\s*key\s*.+:").unwrap(),
            Regex::new(r".*2FA Token.*").unwrap(),
        ]
    });

    PROMPTS.iter().any(|pattern| pattern.is_match(output))
}

fn path_to_string(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

/// Split NUL-delimited `git … -z` output into a path set. NUL-delimiting
/// (rather than the newline-delimited default) is what keeps this correct
/// for paths containing newlines and other unusual bytes.
fn parse_nul_list(output: &str) -> HashSet<&str> {
    output.split('\0').filter(|p| !p.is_empty()).collect()
}

#[cfg(test)]
mod tests {
    use std::{fs, process::Command};

    use tempfile::TempDir;

    use super::*;

    #[tokio::test]
    async fn create_branch_creates_new_local_branch() {
        let repo = init_repo();
        let runner = GitRunner::default();

        runner
            .create_branch(repo.path(), "feature/test")
            .await
            .unwrap();
        let branches = runner.branch_list(repo.path()).await.unwrap();

        assert!(branches.contains("feature/test"));
    }

    #[tokio::test]
    async fn stash_commands_round_trip() {
        let repo = init_repo();
        let runner = GitRunner::default();
        fs::write(repo.path().join("tracked.txt"), "changed\n").unwrap();

        runner
            .stash_push(repo.path(), Some("savepoint"))
            .await
            .unwrap();
        let after_stash = runner.status_text(repo.path()).await.unwrap();
        assert!(!after_stash.contains("tracked.txt"));

        runner.stash_pop(repo.path()).await.unwrap();
        let after_pop = runner.status_text(repo.path()).await.unwrap();
        assert!(after_pop.contains("tracked.txt"));
    }

    #[tokio::test]
    async fn stash_include_untracked_round_trip() {
        let repo = init_repo();
        let runner = GitRunner::default();
        fs::write(repo.path().join("new.txt"), "new\n").unwrap();

        runner
            .stash_push_include_untracked(repo.path(), Some("savepoint"))
            .await
            .unwrap();
        let after_stash = runner.status_text(repo.path()).await.unwrap();
        assert!(!after_stash.contains("new.txt"));

        runner.stash_pop(repo.path()).await.unwrap();
        let after_pop = runner.status_text(repo.path()).await.unwrap();
        assert!(after_pop.contains("new.txt"));
    }

    #[tokio::test]
    async fn auto_stash_returns_no_guard_without_new_changes() {
        let repo = init_repo();
        let runner = GitRunner::default();
        fs::write(repo.path().join("tracked.txt"), "saved first\n").unwrap();
        runner
            .stash_push(repo.path(), Some("user savepoint"))
            .await
            .unwrap();
        let existing = runner.stash_oid(repo.path()).await.unwrap();

        let guard = runner.create_auto_stash(repo.path()).await.unwrap();

        assert_eq!(guard, None, "a clean tree must not produce an auto-stash");
        assert_eq!(runner.stash_oid(repo.path()).await.unwrap(), existing);
        assert!(
            !runner
                .status_text(repo.path())
                .await
                .unwrap()
                .contains("tracked.txt"),
            "a clean tree must have nothing left to report besides the branch header"
        );
    }

    #[tokio::test]
    async fn auto_stash_guard_restores_its_own_dirty_tree() {
        let repo = init_repo();
        let runner = GitRunner::default();
        fs::write(repo.path().join("tracked.txt"), "stash and restore\n").unwrap();

        let guard = runner
            .create_auto_stash(repo.path())
            .await
            .unwrap()
            .expect("dirty tree must create an auto-stash");
        assert!(
            !runner
                .status_text(repo.path())
                .await
                .unwrap()
                .contains("tracked.txt")
        );

        runner.restore_auto_stash(repo.path(), guard).await.unwrap();
        assert!(
            runner
                .status_text(repo.path())
                .await
                .unwrap()
                .contains("tracked.txt")
        );
    }

    #[tokio::test]
    async fn auto_stash_restore_refuses_when_stack_changed_underneath() {
        let repo = init_repo();
        let runner = GitRunner::default();
        fs::write(repo.path().join("tracked.txt"), "auto-stashed change\n").unwrap();

        let guard = runner
            .create_auto_stash(repo.path())
            .await
            .unwrap()
            .expect("dirty tree must create an auto-stash");

        // Something else pushes a new stash entry on top before the restore
        // runs (e.g. the user manually stashed while the op was in flight).
        fs::write(repo.path().join("other.txt"), "unrelated\n").unwrap();
        runner
            .stash_push_include_untracked(repo.path(), Some("someone else's stash"))
            .await
            .unwrap();

        let result = runner.restore_auto_stash(repo.path(), guard).await;

        assert!(
            result.is_err(),
            "a changed stash stack must not be popped automatically"
        );
        let listing = runner
            .run_simple(repo.path(), ["stash", "list"])
            .await
            .unwrap();
        assert!(
            listing.contains("someone else's stash"),
            "the intervening stash must remain untouched: {listing}"
        );
    }

    #[tokio::test]
    async fn worktree_add_and_remove_round_trip() {
        let repo = init_repo();
        let runner = GitRunner::default();
        let worktree_path = repo.path().parent().unwrap().join("repo.feature-test");

        runner
            .worktree_add(repo.path(), &worktree_path, "feature/test", true)
            .await
            .unwrap();
        let listing = runner.worktree_list(repo.path()).await.unwrap();
        assert!(listing.contains(worktree_path.to_string_lossy().as_ref()));

        runner
            .worktree_remove(repo.path(), &worktree_path)
            .await
            .unwrap();
        let listing = runner.worktree_list(repo.path()).await.unwrap();
        assert!(!listing.contains(worktree_path.to_string_lossy().as_ref()));
    }

    /// A repo cloned from a local bare origin, with `main` pushed and tracking.
    fn init_repo_with_origin() -> (TempDir, std::path::PathBuf) {
        let root = tempfile::tempdir().unwrap();
        let origin = root.path().join("origin.git");
        fs::create_dir(&origin).unwrap();
        git(&origin, ["init", "--bare", "-b", "main"]);
        let clone = root.path().join("clone");
        git(root.path(), ["clone", "origin.git", "clone"]);
        git(&clone, ["config", "user.name", "gitbatch"]);
        git(&clone, ["config", "user.email", "gitbatch@example.invalid"]);
        fs::write(clone.join("tracked.txt"), "hello\n").unwrap();
        git(&clone, ["add", "."]);
        git(&clone, ["commit", "-m", "initial"]);
        git(&clone, ["branch", "-M", "main"]);
        git(&clone, ["push", "-u", "origin", "main"]);
        (root, clone)
    }

    #[tokio::test]
    async fn delete_remote_branch_accepts_remote_slash_name() {
        let (_root, clone) = init_repo_with_origin();
        let runner = GitRunner::default();
        git(&clone, ["branch", "zap"]);
        git(&clone, ["push", "origin", "zap"]);

        runner
            .delete_remote_branch(&clone, "origin/zap")
            .await
            .unwrap();
        git(&clone, ["fetch", "--prune"]);
        let branches = runner.branch_list(&clone).await.unwrap();
        assert!(!branches.contains("origin/zap"));
    }

    #[tokio::test]
    async fn push_without_upstream_creates_remote_branch_with_tracking() {
        let (_root, clone) = init_repo_with_origin();
        let runner = GitRunner::default();
        git(&clone, ["checkout", "-b", "feature"]);

        runner
            .run_remote_action(&clone, RemoteAction::Push { force: false })
            .await
            .unwrap();

        let upstream = runner
            .run_simple(&clone, ["rev-parse", "--abbrev-ref", "@{upstream}"])
            .await
            .unwrap();
        assert!(upstream.contains("origin/feature"), "{upstream}");
        let branches = runner.branch_list(&clone).await.unwrap();
        assert!(branches.contains("remotes/origin/feature"));
    }

    #[tokio::test]
    async fn checkout_remote_branch_preserves_nested_name_despite_short_name_collision() {
        let (_root, clone) = init_repo_with_origin();
        let runner = GitRunner::default();

        git(&clone, ["checkout", "-b", "feature/foo"]);
        git(&clone, ["push", "-u", "origin", "feature/foo"]);
        git(&clone, ["checkout", "main"]);
        git(&clone, ["branch", "foo"]);

        runner
            .checkout_remote_branch(&clone, "origin/feature/foo")
            .await
            .unwrap();

        assert_eq!(
            runner
                .run_simple(&clone, ["branch", "--show-current"])
                .await
                .unwrap(),
            "feature/foo"
        );
        assert_eq!(
            runner
                .run_simple(&clone, ["rev-parse", "--abbrev-ref", "@{upstream}"])
                .await
                .unwrap(),
            "origin/feature/foo"
        );
    }

    #[tokio::test]
    async fn force_push_refuses_when_tracking_ref_is_stale() {
        let (root, clone) = init_repo_with_origin();
        let runner = GitRunner::default();
        let other = root.path().join("other");
        git(root.path(), ["clone", "origin.git", "other"]);
        git(&other, ["config", "user.name", "gitbatch"]);
        git(&other, ["config", "user.email", "gitbatch@example.invalid"]);
        fs::write(other.join("remote.txt"), "remote update\n").unwrap();
        git(&other, ["add", "."]);
        git(&other, ["commit", "-m", "remote update"]);
        git(&other, ["push", "origin", "main"]);
        let remote_head = git_output(&other, ["rev-parse", "HEAD"]);

        let result = runner
            .run_remote_action(&clone, RemoteAction::Push { force: true })
            .await;

        assert!(result.is_err(), "a stale lease must reject the force push");
        assert_eq!(
            git_output(&other, ["rev-parse", "origin/main"]),
            remote_head,
            "the newer remote commit must not be overwritten"
        );
    }

    /// A plain `--force-with-lease` compares against the locally cached
    /// remote-tracking ref — which gitbatch's own background fetch keeps
    /// refreshing. This reproduces exactly that: `clone` fetches (as
    /// gitbatch's periodic refresh would) an `other` commit it never merges,
    /// then diverges locally. A lease alone would now see a tracking ref
    /// that matches the remote and let the force push through, silently
    /// discarding `other`'s commit. `--force-if-includes` must catch it.
    #[tokio::test]
    async fn force_push_refuses_when_own_background_fetch_masks_unmerged_remote_commit() {
        let (root, clone) = init_repo_with_origin();
        let runner = GitRunner::default();
        let other = root.path().join("other");
        git(root.path(), ["clone", "origin.git", "other"]);
        git(&other, ["config", "user.name", "gitbatch"]);
        git(&other, ["config", "user.email", "gitbatch@example.invalid"]);
        fs::write(other.join("remote.txt"), "remote update\n").unwrap();
        git(&other, ["add", "."]);
        git(&other, ["commit", "-m", "remote update"]);
        git(&other, ["push", "origin", "main"]);
        let remote_head = git_output(&other, ["rev-parse", "HEAD"]);

        // The background fetch: refreshes `clone`'s origin/main tracking ref
        // to `remote_head` without merging it into `clone`'s local main.
        git(&clone, ["fetch", "origin"]);
        assert_eq!(
            git_output(&clone, ["rev-parse", "origin/main"]),
            remote_head
        );

        // `clone` now rewrites its own local history, unaware of the fetch.
        fs::write(clone.join("tracked.txt"), "local rewrite\n").unwrap();
        git(
            &clone,
            ["commit", "-am", "local rewrite unaware of remote_head"],
        );

        let result = runner
            .run_remote_action(&clone, RemoteAction::Push { force: true })
            .await;

        assert!(
            result.is_err(),
            "force-if-includes must reject a force push that would discard \
             a remote commit our local branch never incorporated, even \
             though our tracking ref was refreshed by a prior fetch"
        );
        assert_eq!(
            git_output(&other, ["rev-parse", "origin/main"]),
            remote_head,
            "the un-integrated remote commit must not be overwritten"
        );
    }

    #[tokio::test]
    async fn fast_forward_safe_accepts_clean_behind_branch_and_rejects_divergence() {
        let (root, clone) = init_repo_with_origin();
        let runner = GitRunner::default();
        let other = root.path().join("other");
        git(root.path(), ["clone", "origin.git", "other"]);
        git(&other, ["config", "user.name", "gitbatch"]);
        git(&other, ["config", "user.email", "gitbatch@example.invalid"]);
        fs::write(other.join("remote.txt"), "remote update\n").unwrap();
        git(&other, ["add", "."]);
        git(&other, ["commit", "-m", "remote update"]);
        git(&other, ["push", "origin", "main"]);
        git(&clone, ["fetch", "origin"]);

        assert!(runner.fast_forward_safe(&clone, "origin/main", true).await);

        fs::write(clone.join("local.txt"), "local update\n").unwrap();
        git(&clone, ["add", "."]);
        git(&clone, ["commit", "-m", "local update"]);
        assert!(
            !runner.fast_forward_safe(&clone, "origin/main", true).await,
            "a clean mergeable divergence still cannot be pulled with --ff-only"
        );
    }

    #[tokio::test]
    async fn fast_forward_safe_accepts_dirty_tree_with_non_overlapping_changes() {
        let (root, clone) = init_repo_with_origin();
        let runner = GitRunner::default();
        let other = root.path().join("other");
        git(root.path(), ["clone", "origin.git", "other"]);
        git(&other, ["config", "user.name", "gitbatch"]);
        git(&other, ["config", "user.email", "gitbatch@example.invalid"]);
        fs::write(other.join("remote.txt"), "remote update\n").unwrap();
        git(&other, ["add", "."]);
        git(&other, ["commit", "-m", "remote update"]);
        git(&other, ["push", "origin", "main"]);
        git(&clone, ["fetch", "origin"]);

        // Dirty, but the local edit touches a different file than the
        // incoming commit — `git pull --ff-only` doesn't care.
        fs::write(clone.join("local.txt"), "local edit\n").unwrap();

        assert!(
            runner.fast_forward_safe(&clone, "origin/main", false).await,
            "a non-overlapping local edit must not block a fast-forward"
        );
    }

    #[tokio::test]
    async fn fast_forward_safe_rejects_dirty_tree_with_overlapping_changes() {
        let (root, clone) = init_repo_with_origin();
        let runner = GitRunner::default();
        let other = root.path().join("other");
        git(root.path(), ["clone", "origin.git", "other"]);
        git(&other, ["config", "user.name", "gitbatch"]);
        git(&other, ["config", "user.email", "gitbatch@example.invalid"]);
        fs::write(other.join("tracked.txt"), "remote change\n").unwrap();
        git(&other, ["commit", "-am", "remote edits tracked.txt"]);
        git(&other, ["push", "origin", "main"]);
        git(&clone, ["fetch", "origin"]);

        // Same file the incoming commit touches — a real `--ff-only` pull
        // would refuse to clobber this local edit.
        fs::write(clone.join("tracked.txt"), "local edit\n").unwrap();

        assert!(
            !runner.fast_forward_safe(&clone, "origin/main", false).await,
            "an overlapping local edit must block the fast-forward"
        );
    }

    #[tokio::test]
    async fn fast_forward_safe_rejects_when_untracked_file_collides_with_incoming() {
        let (root, clone) = init_repo_with_origin();
        let runner = GitRunner::default();
        let other = root.path().join("other");
        git(root.path(), ["clone", "origin.git", "other"]);
        git(&other, ["config", "user.name", "gitbatch"]);
        git(&other, ["config", "user.email", "gitbatch@example.invalid"]);
        fs::write(other.join("new_file.txt"), "incoming\n").unwrap();
        git(&other, ["add", "."]);
        git(&other, ["commit", "-m", "add new_file.txt"]);
        git(&other, ["push", "origin", "main"]);
        git(&clone, ["fetch", "origin"]);

        // Untracked, same name as a file the incoming commit creates.
        fs::write(clone.join("new_file.txt"), "local, untracked\n").unwrap();

        assert!(
            !runner.fast_forward_safe(&clone, "origin/main", false).await,
            "an untracked path that incoming would create must block the fast-forward"
        );
    }

    #[tokio::test]
    async fn set_upstream_and_reset_to_upstream_round_trip() {
        let (_root, clone) = init_repo_with_origin();
        let runner = GitRunner::default();

        // Break the tracking relationship, then restore it via set_upstream.
        git(&clone, ["branch", "--unset-upstream"]);
        runner.set_upstream(&clone, "origin", "main").await.unwrap();

        // A local commit puts us ahead; a mixed reset unwinds it but keeps the file.
        fs::write(clone.join("tracked.txt"), "changed\n").unwrap();
        git(&clone, ["commit", "-am", "local work"]);
        runner.reset_to_upstream(&clone).await.unwrap();
        let status = runner.status_text(&clone).await.unwrap();
        assert!(
            status.contains("tracked.txt"),
            "changes kept in working tree"
        );

        runner.hard_reset_to_upstream(&clone).await.unwrap();
        let status = runner.status_text(&clone).await.unwrap();
        assert!(
            !status.contains("tracked.txt"),
            "hard reset discards changes"
        );
    }

    fn init_repo() -> TempDir {
        let repo = tempfile::tempdir().unwrap();
        git(repo.path(), ["init", "-b", "main"]);
        git(repo.path(), ["config", "user.name", "gitbatch"]);
        git(
            repo.path(),
            ["config", "user.email", "gitbatch@example.invalid"],
        );
        fs::write(repo.path().join("tracked.txt"), "hello\n").unwrap();
        git(repo.path(), ["add", "."]);
        git(repo.path(), ["commit", "-m", "initial"]);
        repo
    }

    fn git<const N: usize>(dir: &Path, args: [&str; N]) {
        let status = Command::new("git")
            .args(args)
            .current_dir(dir)
            .status()
            .unwrap();
        assert!(status.success(), "git command failed");
    }

    fn git_output<const N: usize>(dir: &Path, args: [&str; N]) -> String {
        let output = Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(output.status.success(), "git command failed");
        String::from_utf8(output.stdout).unwrap().trim().to_string()
    }
}
