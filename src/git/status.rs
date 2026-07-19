use std::{
    path::{Path, PathBuf},
    time::SystemTime,
};

use crate::{AppError, Result, git::GitRunner};

#[derive(Debug, Clone, Default)]
pub struct BranchStatus {
    pub name: String,
    pub detached: bool,
    pub ahead: u32,
    pub behind: u32,
    /// True when no upstream tracking branch is configured (and branch is not detached).
    pub no_upstream: bool,
    /// The upstream tracking ref name (e.g. `origin/main`), when configured.
    pub upstream: Option<String>,
}

#[derive(Debug, Clone)]
pub struct RepositorySnapshot {
    pub path: PathBuf,
    pub name: String,
    pub branch: BranchStatus,
    pub dirty: bool,
    /// True when the working tree has unresolved merge conflicts (unmerged paths).
    pub has_conflicts: bool,
    pub last_modified: Option<SystemTime>,
    /// First line of the HEAD commit message, empty on new repos.
    pub commit_subject: String,
    /// Number of stash entries.
    pub stash_count: usize,
}

impl RepositorySnapshot {
    pub async fn load(runner: &GitRunner, dir: &Path) -> Result<Self> {
        if !dir.join(".git").exists() {
            return Err(AppError::NotRepository(dir.to_path_buf()));
        }

        let (status_result, log_result) = tokio::join!(
            runner.run(
                dir,
                ["status", "--porcelain=v2", "--branch", "--show-stash"]
            ),
            runner.run(dir, ["log", "-1", "--pretty=format:%s"]),
        );
        let output = status_result?;
        let commit_subject = log_result
            .ok()
            .map(|o| o.stdout.trim().to_string())
            .unwrap_or_default();

        let mut branch = BranchStatus::default();
        let mut dirty = false;
        let mut has_conflicts = false;
        let mut saw_ab_line = false;
        let mut stash_count = 0usize;

        for line in output.stdout.lines() {
            if let Some(name) = line.strip_prefix("# branch.head ") {
                if name == "(detached)" {
                    branch.name = "detached".into();
                    branch.detached = true;
                } else {
                    branch.name = name.to_string();
                }
            } else if let Some(up) = line.strip_prefix("# branch.upstream ") {
                branch.upstream = Some(up.to_string());
            } else if let Some(ab) = line.strip_prefix("# branch.ab ") {
                saw_ab_line = true;
                let parts: Vec<_> = ab.split_whitespace().collect();
                if let Some(ahead) = parts.first().and_then(|value| value.strip_prefix('+')) {
                    branch.ahead = ahead.parse().unwrap_or_default();
                }
                if let Some(behind) = parts.get(1).and_then(|value| value.strip_prefix('-')) {
                    branch.behind = behind.parse().unwrap_or_default();
                }
            } else if let Some(n) = line.strip_prefix("# stash ") {
                stash_count = n.trim().parse().unwrap_or(0);
            } else if line.starts_with("u ") {
                // Unmerged entry: an active merge/rebase conflict that needs resolving.
                has_conflicts = true;
                dirty = true;
            } else if !line.starts_with('#') && !line.trim().is_empty() {
                // Non-header lines are file-status entries (ordinary, renamed,
                // untracked, ignored). Any such line means the working tree is dirty.
                dirty = true;
            }
        }

        // No `# branch.ab` line means the branch has no upstream tracking remote.
        // Detached HEAD is a special case that also lacks upstream by definition.
        branch.no_upstream = !saw_ab_line && !branch.detached;

        if branch.name.is_empty() {
            branch.name = "unknown".into();
        }

        let metadata = std::fs::metadata(dir)?;
        let name = dir.file_name().map_or_else(
            || dir.display().to_string(),
            |value| value.to_string_lossy().into_owned(),
        );

        Ok(Self {
            path: dir.to_path_buf(),
            name,
            branch,
            dirty,
            has_conflicts,
            last_modified: metadata.modified().ok(),
            commit_subject,
            stash_count,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, process::Command};

    use tempfile::TempDir;

    use super::*;

    // ── helpers ───────────────────────────────────────────────────────────────

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

    // ── state tests ───────────────────────────────────────────────────────────

    /// A freshly committed repo with no changes must be clean (not dirty).
    /// This was broken before: `# branch.oid` lines were misread as dirty files.
    #[tokio::test]
    async fn clean_repo_is_not_dirty() {
        let repo = init_repo();
        let snap = RepositorySnapshot::load(&GitRunner::default(), repo.path())
            .await
            .unwrap();
        assert!(!snap.dirty, "clean repo must not be dirty");
        assert_eq!(snap.branch.ahead, 0);
        assert_eq!(snap.branch.behind, 0);
    }

    /// A modified tracked file must mark the repo as dirty.
    #[tokio::test]
    async fn modified_tracked_file_is_dirty() {
        let repo = init_repo();
        fs::write(repo.path().join("tracked.txt"), "changed\n").unwrap();
        let snap = RepositorySnapshot::load(&GitRunner::default(), repo.path())
            .await
            .unwrap();
        assert!(snap.dirty, "repo with modified tracked file must be dirty");
    }

    /// An untracked file must mark the repo as dirty.
    #[tokio::test]
    async fn untracked_file_is_dirty() {
        let repo = init_repo();
        fs::write(repo.path().join("new_file.txt"), "new\n").unwrap();
        let snap = RepositorySnapshot::load(&GitRunner::default(), repo.path())
            .await
            .unwrap();
        assert!(snap.dirty, "repo with untracked file must be dirty");
    }

    /// A staged (indexed) change must mark the repo as dirty.
    #[tokio::test]
    async fn staged_change_is_dirty() {
        let repo = init_repo();
        fs::write(repo.path().join("tracked.txt"), "staged\n").unwrap();
        git(repo.path(), ["add", "."]);
        let snap = RepositorySnapshot::load(&GitRunner::default(), repo.path())
            .await
            .unwrap();
        assert!(snap.dirty, "repo with staged change must be dirty");
    }

    /// A branch without an upstream tracking remote must set no_upstream = true.
    #[tokio::test]
    async fn branch_without_upstream_sets_no_upstream() {
        let repo = init_repo();
        let snap = RepositorySnapshot::load(&GitRunner::default(), repo.path())
            .await
            .unwrap();
        assert!(
            snap.branch.no_upstream,
            "branch with no remote must report no_upstream"
        );
    }

    /// A branch with an upstream must not report no_upstream.
    /// We simulate this by creating a local remote (bare clone) and setting up tracking.
    #[tokio::test]
    async fn branch_with_upstream_clears_no_upstream() {
        let bare = tempfile::tempdir().unwrap();
        let clone = tempfile::tempdir().unwrap();

        // Create a source repo with one commit
        let src = init_repo();

        // Make a bare clone to act as the remote
        git(bare.path(), ["init", "--bare", "-b", "main"]);
        git(
            src.path(),
            ["remote", "add", "origin", bare.path().to_str().unwrap()],
        );
        git(src.path(), ["push", "-u", "origin", "main"]);

        // Clone from bare so the clone has tracking info
        git(clone.path(), ["init", "-b", "main"]);
        git(
            clone.path(),
            ["remote", "add", "origin", bare.path().to_str().unwrap()],
        );
        git(clone.path(), ["fetch", "origin"]);
        git(clone.path(), ["config", "branch.main.remote", "origin"]);
        git(
            clone.path(),
            ["config", "branch.main.merge", "refs/heads/main"],
        );
        git(clone.path(), ["reset", "--hard", "origin/main"]);

        let snap = RepositorySnapshot::load(&GitRunner::default(), clone.path())
            .await
            .unwrap();
        assert!(
            !snap.branch.no_upstream,
            "branch tracking a remote must not report no_upstream"
        );
        assert_eq!(snap.branch.ahead, 0);
        assert_eq!(snap.branch.behind, 0);
    }

    /// Ahead/behind counts must be parsed correctly.
    #[tokio::test]
    async fn ahead_behind_counts_are_parsed() {
        let bare = tempfile::tempdir().unwrap();
        let clone = tempfile::tempdir().unwrap();
        let src = init_repo();

        git(bare.path(), ["init", "--bare", "-b", "main"]);
        git(
            src.path(),
            ["remote", "add", "origin", bare.path().to_str().unwrap()],
        );
        git(src.path(), ["push", "-u", "origin", "main"]);

        git(clone.path(), ["init", "-b", "main"]);
        git(
            clone.path(),
            ["remote", "add", "origin", bare.path().to_str().unwrap()],
        );
        git(clone.path(), ["fetch", "origin"]);
        git(clone.path(), ["config", "branch.main.remote", "origin"]);
        git(
            clone.path(),
            ["config", "branch.main.merge", "refs/heads/main"],
        );
        git(clone.path(), ["reset", "--hard", "origin/main"]);
        git(clone.path(), ["config", "user.name", "gitbatch"]);
        git(
            clone.path(),
            ["config", "user.email", "gitbatch@example.invalid"],
        );

        // Add a local commit → ahead by 1
        fs::write(clone.path().join("local.txt"), "local\n").unwrap();
        git(clone.path(), ["add", "."]);
        git(clone.path(), ["commit", "-m", "local commit"]);

        let snap = RepositorySnapshot::load(&GitRunner::default(), clone.path())
            .await
            .unwrap();
        assert_eq!(snap.branch.ahead, 1, "should be 1 commit ahead");
        assert_eq!(snap.branch.behind, 0);
        assert!(!snap.branch.no_upstream);
        assert!(!snap.dirty);
    }

    #[tokio::test]
    async fn snapshot_reads_repo_name() {
        let repo = init_repo();
        let snap = RepositorySnapshot::load(&GitRunner::default(), repo.path())
            .await
            .unwrap();
        assert_eq!(
            snap.name,
            repo.path().file_name().unwrap().to_string_lossy()
        );
        assert!(!snap.branch.name.is_empty());
    }
}
