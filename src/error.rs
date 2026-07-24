use std::{io, path::PathBuf, time::Duration};

use thiserror::Error;

#[derive(Debug, Error)]
pub enum AppError {
    #[error("configuration error: {0}")]
    Config(String),
    #[error("io error: {0}")]
    Io(#[from] io::Error),
    #[error("yaml error: {0}")]
    Yaml(#[from] serde_yaml::Error),
    #[error("git command timed out after {timeout:?}: {command}")]
    CommandTimeout { command: String, timeout: Duration },
    #[error("git requested interactive credentials: {output}")]
    CredentialPromptDetected { output: String },
    #[error("git command failed ({command}, exit={code:?}): {output}")]
    GitCommandFailed {
        command: String,
        code: Option<i32>,
        output: String,
    },
    #[error("no git repositories found in the selected directories")]
    NoRepositoriesFound,
    #[error("path does not look like a git repository: {0}")]
    NotRepository(PathBuf),
    #[error("unsupported quick mode: {0}")]
    UnsupportedQuickMode(String),
    #[error("batch completed with failures: {failed}/{total}")]
    BatchFailures { failed: usize, total: usize },
}

pub type Result<T, E = AppError> = std::result::Result<T, E>;

impl AppError {
    pub fn requires_credentials(&self) -> bool {
        let lower = self.to_string().to_lowercase();
        matches!(self, Self::CredentialPromptDetected { .. })
            || lower.contains("authentication required")
            || lower.contains("authentication failed")
            || lower.contains("could not read username")
            || lower.contains("could not read password")
            || lower.contains("invalid username or password")
            || lower.contains("http basic: access denied")
            || lower.contains("fatal: authentication failed for")
            || lower.contains("permission denied")
            || lower.contains("401 unauthorized")
            || lower.contains("403 forbidden")
    }

    pub fn suggests_force_push(&self) -> bool {
        let lower = self.to_string().to_lowercase();
        lower.contains("non-fast-forward")
            || lower.contains("failed to push some refs")
            || lower.contains("updates were rejected because")
            || lower.contains("tip of your current branch is behind")
            || lower.contains("[rejected]")
            || lower.contains("fetch first")
    }

    pub fn is_merge_conflict(&self) -> bool {
        let lower = self.to_string().to_lowercase();
        lower.contains("conflict")
            || lower.contains("merge conflict")
            || lower.contains("automatic merge failed")
            || lower.contains("CONFLICT")
    }

    pub fn is_rebase_conflict(&self) -> bool {
        let lower = self.to_string().to_lowercase();
        (lower.contains("rebase") || lower.contains("apply")) && self.is_merge_conflict()
    }

    pub fn is_network_error(&self) -> bool {
        let lower = self.to_string().to_lowercase();
        lower.contains("connection")
            || lower.contains("network")
            || lower.contains("timeout")
            || lower.contains("unreachable")
            || lower.contains("could not resolve host")
            || lower.contains("failed to connect")
    }
}

#[cfg(test)]
mod tests {
    use super::AppError;

    #[test]
    fn detects_auth_related_git_errors() {
        let error = AppError::GitCommandFailed {
            command: "git fetch".into(),
            code: Some(128),
            output: "fatal: Authentication failed for 'https://example.invalid/repo.git/'".into(),
        };
        assert!(error.requires_credentials());
        assert!(!error.suggests_force_push());
    }

    #[test]
    fn detects_force_push_suggestion() {
        let error = AppError::GitCommandFailed {
            command: "git push".into(),
            code: Some(1),
            output: "! [rejected] main -> main (non-fast-forward)\nerror: failed to push some refs"
                .into(),
        };
        assert!(error.suggests_force_push());
        assert!(!error.requires_credentials());
    }
}
