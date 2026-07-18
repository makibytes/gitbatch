use std::{
    fs,
    path::{Path, PathBuf},
};

use crate::{AppError, Result};

pub fn discover_repositories(roots: &[PathBuf], depth: usize) -> Result<Vec<PathBuf>> {
    if roots.is_empty() {
        return Err(AppError::NoRepositoriesFound);
    }

    let mut search = roots.to_vec();
    let mut repositories = Vec::new();
    let search_depth = if depth == 0 { 1 } else { depth };

    for _ in 0..search_depth {
        let (next_search, found) = walk_once(&search)?;
        search = next_search;
        repositories.extend(found);
    }

    if repositories.is_empty() {
        for root in roots {
            if is_git_repository(root) {
                repositories.push(canonical_or_original(root.clone()));
            }
        }
    }

    repositories.sort();
    repositories.dedup();

    if repositories.is_empty() {
        return Err(AppError::NoRepositoriesFound);
    }

    Ok(repositories)
}

fn walk_once(search: &[PathBuf]) -> Result<(Vec<PathBuf>, Vec<PathBuf>)> {
    let mut next_search = Vec::new();
    let mut repositories = Vec::new();

    for directory in search {
        let Ok(entries) = fs::read_dir(directory) else {
            continue;
        };

        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(_) => continue,
            };
            let path = entry.path();
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if !file_type.is_dir() {
                continue;
            }
            if entry.file_name() == ".git" {
                continue;
            }

            if is_git_repository(&path) {
                repositories.push(canonical_or_original(path));
            } else {
                next_search.push(canonical_or_original(path));
            }
        }
    }

    Ok((next_search, repositories))
}

pub fn is_git_repository(path: &Path) -> bool {
    path.join(".git").exists()
}

fn canonical_or_original(path: PathBuf) -> PathBuf {
    path.canonicalize().unwrap_or(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn depth_zero_behaves_like_immediate_children() {
        let root = tempfile::tempdir().unwrap();
        let repo = root.path().join("repo-a");
        fs::create_dir_all(repo.join(".git")).unwrap();

        let discovered = discover_repositories(&[root.path().to_path_buf()], 0).unwrap();
        assert_eq!(discovered, vec![repo.canonicalize().unwrap()]);
    }

    #[test]
    fn falls_back_to_root_when_no_child_repo_exists() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir_all(root.path().join(".git")).unwrap();

        let discovered = discover_repositories(&[root.path().to_path_buf()], 1).unwrap();
        assert_eq!(discovered, vec![root.path().canonicalize().unwrap()]);
    }
}
