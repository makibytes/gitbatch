use std::path::PathBuf;

use clap::Parser;

use crate::mode::Mode;

#[derive(Debug, Clone, Parser)]
#[command(
    name = "gitbatch",
    version = crate::version::VERSION,
    about = "Manage many Git repositories at once"
)]
pub struct Cli {
    /// Directory(s) to scan for Git repositories.
    #[arg(short = 'd', long = "directory", value_name = "PATH")]
    pub directories: Vec<PathBuf>,

    /// Operation mode: fetch, pull, merge, rebase, push.
    #[arg(short = 'm', long = "mode")]
    pub mode: Option<Mode>,

    /// Find directories recursively. 0 means immediate children.
    #[arg(short = 'r', long = "recursive-depth")]
    pub recursive_depth: Option<usize>,

    /// Run without the TUI and execute the selected operation in batch.
    #[arg(short = 'q', long = "quick")]
    pub quick: bool,

    /// Trace application events.
    #[arg(short = 't', long = "trace")]
    pub trace: bool,
}

impl Cli {
    pub fn parse() -> Self {
        <Self as Parser>::parse()
    }
}
