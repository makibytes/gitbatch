//! Pure text-parsing and text-editing helpers used by the TUI: branch-name
//! parsing from `git branch` output, and single-line prompt input editing
//! (char-boundary math, word-wise backspace, branch-name validation). None
//! of this touches `App` — it's plain string manipulation, split out so it
//! can be read and tested in isolation from the UI state machine.

use std::collections::HashSet;

// ── Branch-name parsing ──────────────────────────────────────────────────────

/// Extract the branch name from one line of `git branch --all -vv` output.
/// Strips leading markers (`*`, `+`, spaces) and returns the first
/// whitespace-delimited token (the ref name). Returns `None` for header /
/// arrow lines (e.g. `remotes/origin/HEAD -> origin/main`).
pub(super) fn parse_branch_name(line: &str) -> Option<&str> {
    let trimmed = line.trim_start_matches(['*', '+', ' ']);
    let name = trimmed.split_whitespace().next()?;
    // Skip symbolic refs like `remotes/origin/HEAD`
    if trimmed.contains(" -> ") {
        return None;
    }
    Some(name)
}

/// Collect the set of branch names from `git branch [-r] -vv` output.
pub(super) fn parse_branch_names(output: &str) -> HashSet<String> {
    output
        .lines()
        .filter_map(parse_branch_name)
        .map(ToString::to_string)
        .collect()
}

/// For a `git branch --all` row naming a remote-tracking branch
/// (`remotes/origin/x`), return the `origin/x` form used by the remote flows.
pub(super) fn remote_short_ref(name: &str) -> Option<&str> {
    name.strip_prefix("remotes/")
}

/// From one local-branch line of `git branch --all -vv --no-abbrev` output,
/// extract the upstream short ref (`origin/x`) out of the tracking bracket
/// that follows the SHA, e.g. `[origin/x]` or `[origin/x: ahead 1]`.
fn upstream_of_local_line(line: &str) -> Option<&str> {
    let name = parse_branch_name(line)?;
    if name.starts_with("remotes/") {
        return None;
    }
    // Tokens: name, 40-char SHA, then optionally `[upstream...`.
    let mut tokens = line.trim_start_matches(['*', '+', ' ']).split_whitespace();
    tokens.next()?; // name
    let sha = tokens.next()?;
    if sha.len() != 40 || !sha.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let bracket = tokens.next()?.strip_prefix('[')?;
    Some(bracket.trim_end_matches([']', ':']))
}

/// Drop `remotes/origin/x` rows from `git branch --all -vv` output when a
/// local branch in the same output already tracks `origin/x` — the tracking
/// info shown on the local line makes the remote row redundant.
pub(super) fn filter_tracked_remotes(output: &str) -> String {
    let tracked: HashSet<&str> = output.lines().filter_map(upstream_of_local_line).collect();
    output
        .lines()
        .filter(|line| {
            parse_branch_name(line)
                .and_then(remote_short_ref)
                .is_none_or(|short| !tracked.contains(short))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

// ── Prompt editing helpers ────────────────────────────────────────────────────

/// Byte offset of the char boundary before `idx` (0 if already at the start).
pub(super) fn prev_char_boundary(s: &str, idx: usize) -> usize {
    s[..idx]
        .chars()
        .next_back()
        .map_or(0, |c| idx - c.len_utf8())
}

/// Byte offset of the char boundary after `idx` (unchanged if at the end).
pub(super) fn next_char_boundary(s: &str, idx: usize) -> usize {
    s[idx..].chars().next().map_or(idx, |c| idx + c.len_utf8())
}

/// Start of the "word" preceding `cursor`: skips separators backwards, then
/// the run of non-separator chars — so `feat/my-branch` deletes one segment
/// at a time.
pub(super) fn word_back_start(s: &str, cursor: usize) -> usize {
    let is_sep = |c: char| matches!(c, '/' | '-' | '_' | '.' | ' ');
    let char_at = |idx: usize| s[idx..].chars().next();
    let mut idx = cursor;
    loop {
        let prev = prev_char_boundary(s, idx);
        if prev == idx || !char_at(prev).is_some_and(is_sep) {
            break;
        }
        idx = prev;
    }
    loop {
        let prev = prev_char_boundary(s, idx);
        if prev == idx || char_at(prev).is_some_and(is_sep) {
            break;
        }
        idx = prev;
    }
    idx
}

/// Approximate `git check-ref-format --branch`: catch obviously invalid names
/// before handing them to git.
pub(super) fn validate_branch_name(name: &str) -> std::result::Result<(), &'static str> {
    if name.is_empty() {
        return Err("branch name is empty");
    }
    if name == "@" {
        return Err("'@' is not a valid branch name");
    }
    if name.starts_with('-') {
        return Err("branch name must not start with '-'");
    }
    if name.starts_with('/') || name.ends_with('/') || name.contains("//") {
        return Err("invalid '/' placement in branch name");
    }
    if name.ends_with('.') || name.contains("..") {
        return Err("invalid '.' placement in branch name");
    }
    if name.contains("@{") {
        return Err("branch name must not contain '@{'");
    }
    if name
        .split('/')
        .any(|seg| seg.starts_with('.') || seg.ends_with(".lock"))
    {
        return Err("segment must not start with '.' or end with '.lock'");
    }
    if name.chars().any(|c| {
        c.is_whitespace() || c.is_control() || matches!(c, '~' | '^' | ':' | '?' | '*' | '[' | '\\')
    }) {
        return Err("branch name contains invalid characters");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filter_tracked_remotes_drops_only_tracked_remote_rows() {
        let sha = "a".repeat(40);
        let output = [
            // tracked, plain bracket
            format!("* main                {sha} [origin/main] tip"),
            // tracked with counts
            format!("  feature             {sha} [origin/feature: ahead 1, behind 2] wip"),
            // upstream gone — remote row doesn't exist anyway, must not panic
            format!("  stale               {sha} [origin/stale: gone] old"),
            // local without upstream
            format!("  local-only          {sha} no upstream here"),
            // symbolic ref line is kept
            "  remotes/origin/HEAD -> origin/main".to_string(),
            // tracked by the locals above → dropped
            format!("  remotes/origin/main {sha} tip"),
            format!("  remotes/origin/feature {sha} wip"),
            // remote-only branch → kept
            format!("  remotes/origin/other {sha} other work"),
        ]
        .join("\n");

        let filtered = filter_tracked_remotes(&output);
        assert!(filtered.contains("* main"));
        assert!(filtered.contains("  feature"));
        assert!(filtered.contains("  stale"));
        assert!(filtered.contains("local-only"));
        assert!(filtered.contains("remotes/origin/HEAD -> origin/main"));
        assert!(!filtered.contains("remotes/origin/main"));
        assert!(!filtered.contains("remotes/origin/feature"));
        assert!(filtered.contains("remotes/origin/other"));
    }

    #[test]
    fn validate_branch_name_accepts_normal_names() {
        for name in ["main", "feature/foo", "release-1.2", "hotfix_x", "a/b/c"] {
            assert!(validate_branch_name(name).is_ok(), "{name}");
        }
    }

    #[test]
    fn validate_branch_name_rejects_invalid_names() {
        for name in [
            "",
            "@",
            "-x",
            "a b",
            "a..b",
            "a.",
            "/a",
            "a/",
            "a//b",
            "a@{b",
            "a~b",
            "a^b",
            "a:b",
            "a?b",
            "a*b",
            "a[b",
            "a\\b",
            ".hidden",
            "a/.hidden",
            "a.lock",
            "a/b.lock",
            "a\tb",
        ] {
            assert!(validate_branch_name(name).is_err(), "{name}");
        }
    }

    #[test]
    fn word_back_start_deletes_segment_wise() {
        let s = "feat/my-branch";
        // From the end: deletes "branch".
        assert_eq!(word_back_start(s, s.len()), "feat/my-".len());
        // From "feat/my-": skips the '-' separator and deletes "my".
        assert_eq!(word_back_start(s, "feat/my-".len()), "feat/".len());
        // From "feat/": deletes everything back to the start.
        assert_eq!(word_back_start(s, "feat/".len()), 0);
        assert_eq!(word_back_start("", 0), 0);
    }

    #[test]
    fn remote_short_ref_strips_remotes_prefix() {
        assert_eq!(remote_short_ref("remotes/origin/main"), Some("origin/main"));
        assert_eq!(
            remote_short_ref("remotes/origin/feat/x"),
            Some("origin/feat/x")
        );
        assert_eq!(remote_short_ref("main"), None);
        assert_eq!(remote_short_ref("feature/remotes"), None);
    }

    #[test]
    fn char_boundary_helpers_handle_multibyte() {
        let s = "aöb";
        assert_eq!(next_char_boundary(s, 0), 1);
        assert_eq!(next_char_boundary(s, 1), 3); // 'ö' is two bytes
        assert_eq!(next_char_boundary(s, s.len()), s.len());
        assert_eq!(prev_char_boundary(s, 3), 1);
        assert_eq!(prev_char_boundary(s, 1), 0);
        assert_eq!(prev_char_boundary(s, 0), 0);
    }

    #[test]
    fn parse_branch_name_extracts_current_branch() {
        assert_eq!(
            parse_branch_name("* main  abc1234 commit msg"),
            Some("main")
        );
    }

    #[test]
    fn parse_branch_name_extracts_regular_branch() {
        assert_eq!(
            parse_branch_name("  feature/foo  abc1234 commit msg"),
            Some("feature/foo")
        );
    }

    #[test]
    fn parse_branch_name_extracts_remote_branch() {
        assert_eq!(
            parse_branch_name("  remotes/origin/main  abc1234 commit msg"),
            Some("remotes/origin/main")
        );
    }

    #[test]
    fn parse_branch_name_skips_symbolic_ref() {
        assert_eq!(
            parse_branch_name("  remotes/origin/HEAD -> origin/main"),
            None
        );
    }

    #[test]
    fn parse_branch_name_with_plus_marker() {
        // The `+` marker is used for worktree-checked-out branches.
        assert_eq!(parse_branch_name("+ develop  abc1234 msg"), Some("develop"));
    }

    #[test]
    fn parse_branch_names_collects_set() {
        let output = "\
* main       abc1234 first commit
  feature/a  def5678 second commit
  feature/b  ghi9012 third commit
  remotes/origin/HEAD -> origin/main
  remotes/origin/main abc1234 first commit";
        let names = parse_branch_names(output);
        assert!(names.contains("main"));
        assert!(names.contains("feature/a"));
        assert!(names.contains("feature/b"));
        assert!(names.contains("remotes/origin/main"));
        // Symbolic refs should be excluded.
        assert!(!names.iter().any(|n| n.contains("HEAD")));
        assert_eq!(names.len(), 4);
    }

    #[test]
    fn parse_branch_names_empty_input() {
        assert!(parse_branch_names("").is_empty());
    }
}
