use std::{
    fs,
    path::{Path, PathBuf},
};

use directories::BaseDirs;
use serde::{Deserialize, Serialize};

use crate::{Result, cli::Cli, error::AppError, mode::Mode};

const APP_NAME: &str = "gitbatch";
const CONFIG_FILE_NAME: &str = "config.yml";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FileConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mode: Option<Mode>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub paths: Option<Vec<PathBuf>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quick: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recursion: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trace: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auto_stash: Option<bool>,
}

#[derive(Debug, Clone)]
pub struct AppConfig {
    pub directories: Vec<PathBuf>,
    pub depth: usize,
    pub quick: bool,
    pub mode: Mode,
    pub trace: bool,
    /// Stash a dirty tree before pull/merge/rebase and restore it afterwards.
    pub auto_stash: bool,
    pub config_path: PathBuf,
}

impl AppConfig {
    pub fn load(cli: Cli) -> Result<Self> {
        let config_path = config_file_path()?;
        let file_config = load_or_create_config(&config_path)?;
        let current_dir = std::env::current_dir()?;

        let directories = if cli.directories.is_empty() {
            file_config
                .paths
                .clone()
                .filter(|paths| !paths.is_empty())
                .unwrap_or_else(|| vec![current_dir.clone()])
        } else {
            cli.directories.clone()
        };

        let depth = cli.recursive_depth.or(file_config.recursion).unwrap_or(1);
        let quick = cli.quick || file_config.quick.unwrap_or(false);
        let mode = cli.mode.or(file_config.mode).unwrap_or_default();
        let trace = cli.trace || file_config.trace.unwrap_or(false);
        let auto_stash = file_config.auto_stash.unwrap_or(false);

        let mut normalized_directories = Vec::with_capacity(directories.len());
        for directory in directories {
            if directory.exists() {
                normalized_directories.push(normalize_path(&directory));
            }
        }

        if normalized_directories.is_empty() {
            return Err(AppError::Config(
                "no valid directories remain after configuration merge".into(),
            ));
        }

        Ok(Self {
            directories: normalized_directories,
            depth,
            quick,
            mode,
            trace,
            auto_stash,
            config_path,
        })
    }
}

pub fn config_dir_path() -> Result<PathBuf> {
    let base = BaseDirs::new()
        .map(|dirs| dirs.config_dir().to_path_buf())
        .ok_or_else(|| AppError::Config("could not determine user config directory".into()))?;
    Ok(base.join(APP_NAME))
}

pub fn config_file_path() -> Result<PathBuf> {
    Ok(config_dir_path()?.join(CONFIG_FILE_NAME))
}

fn load_or_create_config(path: &Path) -> Result<FileConfig> {
    if !path.exists() {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let defaults = FileConfig {
            mode: Some(Mode::Pull),
            paths: None,
            quick: Some(false),
            recursion: Some(1),
            trace: Some(false),
            auto_stash: Some(false),
        };
        fs::write(path, serde_yaml::to_string(&defaults)?)?;
        return Ok(defaults);
    }

    let raw = fs::read_to_string(path)?;
    if raw.trim().is_empty() {
        return Ok(FileConfig::default());
    }

    let mut config: FileConfig = serde_yaml::from_str(&raw)?;

    // Fetch is no longer a batch mode (startup auto-fetch covers it) —
    // migrate old config files to the new default.
    if config.mode == Some(Mode::Fetch) {
        config.mode = Some(Mode::Pull);
        fs::write(path, serde_yaml::to_string(&config)?)?;
    }

    Ok(config)
}

fn normalize_path(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mode::Mode;

    #[test]
    fn default_config_path_ends_with_gitbatch_config() {
        let path = config_file_path().unwrap();
        assert!(path.ends_with("gitbatch/config.yml"));
    }

    #[test]
    fn load_migrates_fetch_mode_to_pull() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.yml");
        fs::write(&path, "mode: fetch\nrecursion: 2\nauto_stash: true\n").unwrap();

        let config = load_or_create_config(&path).unwrap();
        assert_eq!(config.mode, Some(Mode::Pull));
        assert_eq!(config.recursion, Some(2));
        assert_eq!(config.auto_stash, Some(true));

        let rewritten = fs::read_to_string(&path).unwrap();
        assert!(rewritten.contains("mode: pull"));
        assert!(rewritten.contains("auto_stash: true"));
        assert!(
            !rewritten.contains("null"),
            "None fields must not serialize"
        );
    }

    #[test]
    fn mode_serializes_as_yaml_string() {
        let config = FileConfig {
            mode: Some(Mode::Pull),
            paths: None,
            quick: Some(false),
            recursion: Some(1),
            trace: Some(false),
            auto_stash: None,
        };

        let yaml = serde_yaml::to_string(&config).unwrap();
        assert!(yaml.contains("mode: pull"));
    }
}
