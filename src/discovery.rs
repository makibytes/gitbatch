use std::{
    fs,
    io::{BufRead, BufReader, Read},
    path::{Path, PathBuf},
};

use crate::{AppError, Result};

pub fn discover_repositories(roots: &[PathBuf], depth: usize) -> Result<Vec<PathBuf>> {
    if roots.is_empty() {
        return Err(AppError::NoRepositoriesFound);
    }

    let mut repositories = Vec::new();
    for root in roots {
        if is_git_repository(root) {
            repositories.push(canonical_or_original(root.clone()));
        }
    }

    let mut search = roots.to_vec();
    let search_depth = if depth == 0 { 1 } else { depth };

    for _ in 0..search_depth {
        let (next_search, found) = walk_once(&search);
        search = next_search;
        repositories.extend(found);
    }

    repositories.sort();
    repositories.dedup();

    if repositories.is_empty() {
        return Err(AppError::NoRepositoriesFound);
    }

    Ok(repositories)
}

fn walk_once(search: &[PathBuf]) -> (Vec<PathBuf>, Vec<PathBuf>) {
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

    (next_search, repositories)
}

pub fn is_git_repository(path: &Path) -> bool {
    let marker = path.join(".git");
    let Ok(metadata) = fs::metadata(&marker) else {
        return false;
    };

    if metadata.is_dir() {
        return true;
    }
    if !metadata.is_file() {
        return false;
    }

    let Ok(file) = fs::File::open(marker) else {
        return false;
    };
    // A `.git` file is normally a one-line `gitdir: …` pointer (tens of
    // bytes); bound the read so a stray large file with that name can't
    // pull megabytes into memory just to check for a prefix.
    let mut first_line = String::new();
    BufReader::new(file.take(4096))
        .read_line(&mut first_line)
        .is_ok_and(|_| first_line.starts_with("gitdir:"))
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

    #[test]
    fn includes_explicit_root_alongside_child_repositories() {
        let root = tempfile::tempdir().unwrap();
        let child = root.path().join("child");
        fs::create_dir_all(root.path().join(".git")).unwrap();
        fs::create_dir_all(child.join(".git")).unwrap();

        let discovered = discover_repositories(&[root.path().to_path_buf()], 1).unwrap();
        assert_eq!(
            discovered,
            vec![
                root.path().canonicalize().unwrap(),
                child.canonicalize().unwrap(),
            ]
        );
    }

    #[test]
    fn recognizes_linked_worktree_gitdir_marker_file() {
        let root = tempfile::tempdir().unwrap();
        let worktree = root.path().join("linked-worktree");
        fs::create_dir_all(&worktree).unwrap();
        fs::write(
            worktree.join(".git"),
            "gitdir: /example/repo/.git/worktrees/linked-worktree\n",
        )
        .unwrap();

        assert!(is_git_repository(&worktree));
        assert_eq!(
            discover_repositories(&[root.path().to_path_buf()], 1).unwrap(),
            vec![worktree.canonicalize().unwrap()]
        );
    }

    #[test]
    fn rejects_malformed_git_marker_file() {
        let repo = tempfile::tempdir().unwrap();
        fs::write(repo.path().join(".git"), "this is not a gitdir marker\n").unwrap();

        assert!(!is_git_repository(repo.path()));
    }
}
