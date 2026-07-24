use std::path::PathBuf;

use futures::{StreamExt, stream};

use crate::{AppError, Result, git::GitRunner, mode::Mode};

pub async fn run(runner: &GitRunner, directories: Vec<PathBuf>, mode: Mode) -> Result<()> {
    let Some(mode) = mode.quick_mode() else {
        return Err(AppError::UnsupportedQuickMode(mode.to_string()));
    };

    let concurrency = std::thread::available_parallelism()
        .map_or(4, |value| value.get() * 4)
        .max(4);

    let started = std::time::Instant::now();
    let results = stream::iter(directories.into_iter().map(|directory| {
        let runner = runner.clone();
        async move {
            let result = runner.run_mode(&directory, mode).await;
            (directory, result)
        }
    }))
    .buffer_unordered(concurrency)
    .collect::<Vec<_>>()
    .await;

    let total = results.len();
    let mut failed = 0usize;
    for (directory, result) in results {
        match result {
            Ok(message) => {
                if message.trim().is_empty() {
                    println!("{}: successful", directory.display());
                } else {
                    println!("{}: {}", directory.display(), message.replace('\n', " | "));
                }
            }
            Err(error) => {
                failed += 1;
                eprintln!(
                    "could not perform {} on {}: {}",
                    mode,
                    directory.display(),
                    error
                );
            }
        }
    }

    println!("{total} repositories finished in: {:?}", started.elapsed());
    if failed > 0 {
        return Err(AppError::BatchFailures { failed, total });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn returns_batch_failure_when_any_repo_fails() {
        let temp = tempfile::tempdir().unwrap();
        let result = run(
            &GitRunner::default(),
            vec![temp.path().to_path_buf()],
            Mode::Pull,
        )
        .await;

        match result {
            Err(AppError::BatchFailures { failed, total }) => {
                assert_eq!(failed, 1);
                assert_eq!(total, 1);
            }
            other => panic!("expected batch failure, got {other:?}"),
        }
    }
}
