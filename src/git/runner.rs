use std::{
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

use crate::{mode::Mode, AppError, Result};

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
        self.run_internal(dir, args, None).await
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
        self.run_internal(dir, args, Some(credentials)).await
    }

    async fn run_internal<I, S>(
        &self,
        dir: &Path,
        args: I,
        credentials: Option<&Credentials>,
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

        let askpass = credentials.and_then(|creds| AskPassScript::create(creds).ok());
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
            .env("LC_ALL", "C");
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
                    &["pull", "--ff-only", "--stat", "--progress"],
                    credentials,
                )
                .await
            }
            RemoteAction::Merge => {
                self.run_args(dir, &["merge", "@{upstream}"], credentials)
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
                        self.run_args(dir, &["push", "--force"], credentials).await
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
                        args.push("--force");
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
        self.run_simple(
            dir,
            ["branch", "--all", "--verbose", "--verbose", "--no-abbrev"],
        )
        .await
    }

    pub async fn commit_log(&self, dir: &Path) -> Result<String> {
        self.run_simple(
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
        .await
    }

    pub async fn status_text(&self, dir: &Path) -> Result<String> {
        self.run_simple(dir, ["status", "--short", "--branch"])
            .await
    }

    pub async fn create_branch(&self, dir: &Path, name: &str) -> Result<String> {
        self.run_simple(dir, ["checkout", "-b", name]).await
    }

    pub async fn checkout_reference(&self, dir: &Path, name: &str) -> Result<String> {
        self.run_simple(dir, ["checkout", name]).await
    }

    pub async fn checkout_remote_branch(&self, dir: &Path, name: &str) -> Result<String> {
        let short_name = name.rsplit('/').next().unwrap_or(name);
        match self.checkout_reference(dir, short_name).await {
            Ok(output) => Ok(output),
            Err(_) => self.run_simple(dir, ["checkout", "--track", name]).await,
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

    pub async fn stash_pop(&self, dir: &Path) -> Result<String> {
        self.run_simple(dir, ["stash", "pop"]).await
    }

    pub async fn stash_drop(&self, dir: &Path) -> Result<String> {
        self.run_simple(dir, ["stash", "drop"]).await
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
        let tracking_ref = format!("{}/{}", remote, branch);
        self.run_simple(dir, ["branch", "-u", &tracking_ref]).await
    }

    /// Returns true when a `git pull` (fast-forward) against `upstream` would
    /// succeed cleanly without conflicts or file-level overlaps with local edits.
    /// Mirrors Go's `fastForwardDryRunSucceeds`. Errors are treated as unsafe (false).
    pub async fn fast_forward_safe(&self, dir: &Path, upstream: &str, clean: bool) -> bool {
        if upstream.is_empty() {
            return false;
        }
        // If HEAD is a direct ancestor, it's a pure fast-forward — no 3-way merge.
        let pure_ff = self
            .run(dir, ["merge-base", "--is-ancestor", "HEAD", upstream])
            .await
            .is_ok();
        if !pure_ff {
            // Diverged branches: run a dry-run merge to check for conflicts.
            match self
                .run(dir, ["merge-tree", "--write-tree", "HEAD", upstream])
                .await
            {
                Ok(out) if !out.stdout.contains("CONFLICT") => {}
                _ => return false,
            }
        }
        if clean {
            return true;
        }
        // Dirty tree: check whether any locally modified file overlaps with incoming changes.
        let (diff_res, status_res) = tokio::join!(
            self.run(dir, ["diff", "--name-only", "HEAD", upstream]),
            self.run(dir, ["status", "--porcelain"]),
        );
        let (Ok(diff_out), Ok(status_out)) = (diff_res, status_res) else {
            return false;
        };
        let incoming: std::collections::HashSet<&str> = diff_out
            .stdout
            .lines()
            .filter(|l| !l.trim().is_empty())
            .collect();
        for line in status_out.stdout.lines() {
            if line.len() >= 4 {
                let file = line[3..].trim();
                if incoming.contains(file) {
                    return false;
                }
            }
        }
        true
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
}
