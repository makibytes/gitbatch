pub mod app;
pub mod cli;
pub mod config;
pub mod discovery;
pub mod error;
pub mod git;
pub mod mode;
pub mod quick;
pub mod tui;
pub mod version;

pub use error::{AppError, Result};
