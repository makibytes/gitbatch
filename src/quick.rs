use std::path::PathBuf;

use futures::{stream, StreamExt};

use crate::{git::GitRunner, mode::Mode, AppError, Result};

pub async fn run(runner: &GitRunner, directories: Vec<PathBuf>, mode: Mode) -> Result<()> {
    let Some(mode) = mode.quick_mode() else {
        return Err(AppError::UnsupportedQuickMode(mode.to_string()));
    };

    let concurrency = std::thread::available_parallelism()
        .map(|value| value.get() * 4)
        .unwrap_or(4)
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
    for (directory, result) in results {
        match result {
            Ok(message) => {
                if message.trim().is_empty() {
                    println!("{}: successful", directory.display());
                } else {
                    println!("{}: {}", directory.display(), message.replace('\n', " | "));
                }
            }
            Err(error) => eprintln!(
                "could not perform {} on {}: {}",
                mode,
                directory.display(),
                error
            ),
        }
    }

    println!("{total} repositories finished in: {:?}", started.elapsed());
    Ok(())
}
