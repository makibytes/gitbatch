mod runner;
mod status;

pub use runner::{AutoStashGuard, Credentials, GitOutput, GitRunner, RemoteAction};
pub use status::{BranchStatus, RepositorySnapshot};

/// Concurrency cap for background git operations: `available_parallelism * 4`,
/// min 4. Matches the Go reference's `runtime.GOMAXPROCS(0) * 4` semaphore.
/// Shared by the TUI and quick (`-q`) modes rather than each picking its own.
pub fn worker_limit() -> usize {
    std::thread::available_parallelism()
        .map_or(4, |n| n.get().saturating_mul(4))
        .max(4)
}
