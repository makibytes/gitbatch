[![MIT License](https://img.shields.io/badge/license-MIT-brightgreen.svg)](/LICENSE) [![CI](https://github.com/makibytes/gitbatch/actions/workflows/ci.yml/badge.svg)](https://github.com/makibytes/gitbatch/actions/workflows/ci.yml)

## gitbatch

Managing multiple git repositories is easier than ever. I (*was*) often end up working on many directories and manually pulling updates etc. To make this routine faster, I created a simple tool to handle this job. Although the focus is batch jobs, you can still do de facto micro management of your git repositories (e.g *add/reset, stash, commit etc.*). And for the more complex stuff, you can always open lazygit from within gitbatch.

![gitbatch demo](.github/assets/gitbatch-demo.gif)

## Installation

Download the latest release artifact from the [GitHub Releases page](https://github.com/makibytes/gitbatch/releases/latest), then extract and place the binary on your `PATH`.

Release artifacts include binaries for `linux` (amd64/arm64, fully static), `darwin` (amd64/arm64), and `windows` (amd64).

Example (macOS/Linux):
```bash
# 1) Download the archive for your OS/architecture from the latest release page
# 2) Extract it
tar -xzf gitbatch_<version>_<os>_<arch>.tar.gz

# 3) Move binary to PATH
chmod +x gitbatch
sudo mv gitbatch /usr/local/bin/gitbatch
```

Windows:
1. Download the `windows` release artifact from [Releases](https://github.com/makibytes/gitbatch/releases/latest).
2. Extract `gitbatch.exe`.
3. Add its directory to your `PATH`.

From source (for developers and advanced users):
1. install a recent version of Rust 
2. run `cargo install --path .`

gitbatch requires a `git` binary on `PATH` — version 2.30 or newer (for `--force-if-includes`, used by force-push). The optional `Tab` handoff needs [lazygit](https://github.com/jesseduffield/lazygit) installed.

## Use

Run `gitbatch` from the parent directory of your git repositories. The TUI starts in **pull** mode and fetches all repositories automatically.

```bash
gitbatch                          # scan current directory
gitbatch -d ~/src                 # scan a specific directory
gitbatch -d ~/src -r 2            # scan recursively (depth 2)
gitbatch -q                       # quick mode: batch pull without TUI
gitbatch -q -m merge              # quick mode: batch merge
gitbatch -m push                  # start TUI in push mode
gitbatch --trace                  # append git command traces to gitbatch.log
gitbatch --help                   # show all options
```

Quick mode exits with a non-zero status if at least one repository operation fails.

### Key bindings

| Key | Action |
| --- | --- |
| `j` / `k`, arrows | Move cursor |
| `PgUp` / `PgDn` | Jump by 10 rows |
| `g` / `G` | Jump to top / bottom |
| `←` / `→` | Scroll message column |
| `Space` | Select / deselect repository |
| `Enter` | Run on the selection, or on the current repo if nothing is selected |
| `a` / `A` | Select all / clear selection **and** all result markers |
| `m` | Cycle mode: `Pull → Merge → Rebase → Push → Pull` |
| `f` | Fetch current repository |
| `p` | Pull current repository (fast-forward) |
| `P` | Push current repository |
| `b` | Branches panel (local and remote branches) |
| `s` | Status panel |
| `v` | Commit log panel |
| `c` | Commit prompt |
| `n` | New branch prompt (also inside the branches panel), or new worktree branch in worktree mode |
| `u` | Set upstream tracking branch (e.g. `origin` or `origin/main`) |
| `U` | Reset to upstream — confirm with `y` (mixed) or `H` (hard) |
| `S` / `O` / `D` | Stash push / pop / drop (drop asks for confirmation) |
| `W` | Toggle worktree mode |
| `d` / `L` / `X` | Remove worktree / lock-unlock worktree / prune worktrees (in worktree mode) |
| `t` | Toggle sorting by name / last commit time |
| `Tab` | Open lazygit for the selected repository |
| `?` | Toggle help |
| `q` / `Ctrl+C` | Quit (`q` closes an open panel first) |

Inside the branches panel: `Space`/`c` checkout, `n` new branch, `d` delete, `D` force-delete. The panel lists local branches followed by remote branches; remote branches that are already tracked by a local branch are hidden (the tracking info on the local line covers them). Checking out a remote branch creates a local tracking branch; `d` on a remote branch deletes it on the remote. When several repos are selected, panels show the branches common to all of them and every action fans out over the whole selection — destructive ones after a confirmation dialog.

### Mode cycle

The `m` key cycles through git operations:

1. **Pull (FF)** — `git pull --ff-only` — merge only if it's a fast-forward (safe default; fails visibly if branches diverged)
2. **Merge** — `git merge @{upstream}` — create a merge commit from upstream
3. **Rebase** — `git pull --rebase` — rebase local commits on upstream (linear history)
4. **Push** — `git push` — push local commits to remote (with a confirmation dialog for `--force` if rejected). If the branch has no upstream yet, gitbatch pushes with `-u` so the remote branch is created and tracking is set — create a branch locally and simply push to publish it.

Fetch is not part of the cycle: gitbatch fetches all repositories automatically at startup. Use `f` for an on-demand fetch of the current/selected repos, or `-q -m fetch` for a headless fetch.

### Worktree mode

Press `W` to switch the overview into **worktree mode**. Repositories that share a common Git directory are grouped into a single worktree family so you can inspect the main worktree and linked worktrees together.

Available worktree actions:

- `n` — create a new linked worktree by entering a branch name and path
- `d` — remove the selected linked worktree
- `L` — lock or unlock the selected linked worktree
- `X` — prune stale worktree metadata

When you type a branch name in the worktree prompt, gitbatch prefills the path with a sibling directory named `<repo>.<branch-sanitized>`, for example `myproject.feature-auth`.

The status panel (`s`) also reflects the selected worktree.

### Configuration

Configuration is stored at `$XDG_CONFIG_HOME/gitbatch/config.yml` (macOS: `~/Library/Application Support/gitbatch/config.yml`).

```yaml
# Directories to scan (used when no -d flag is given)
paths:
  - ~/projects
  - ~/work/repos

mode: pull          # default mode: pull | merge | rebase | push (fetch is quick-mode-only)
recursion: 1        # directory scan depth
quick: false        # start in quick mode by default
trace: false        # append git command traces to gitbatch.log
auto_stash: false   # stash a dirty tree before pull/merge/rebase, restore after
```

## Error handling

Failures show per-repo with a categorized icon and color, plus the git message in the row:

- **🔐 Authentication**: `?` icon (purple) — a credentials prompt opens and the operation is retried via `GIT_ASKPASS`
- **⚠️ Conflicts**: amber — merge/rebase conflicts detected, use lazygit (`Tab`) to resolve
- **🔄 Non-FF push**: `!` icon — a force-push confirmation dialog opens
- **📡 Network**: gray — connection/timeout/unreachable-host failures
- everything else: red with the git output inline

### Conflict resolution

When `git pull` or `git merge` results in conflicts:

1. Conflicts are highlighted with the ⚠ icon and amber color
2. The status panel (`s`) shows the working-tree state
3. Press `Tab` to open lazygit for detailed conflict resolution
4. After resolving in lazygit, return and press `Enter` to retry

### Force-push safety

When a push is rejected as non-fast-forward, a force-push confirmation dialog opens automatically. Confirm with `y` / `Enter`, or cancel with `n` / `Esc`. Use this **only when intentional** — force-push overwrites remote history. The retry uses `--force-with-lease --force-if-includes`, so it still refuses if the remote moved again after your last fetch — even gitbatch's own background fetch — rather than overwriting an unseen commit.

### Batch confirmation

`P` (push) over more than one selected repo opens a confirmation naming the repo count first (e.g. "Push to 12 remotes") — a multi-repo push, the one operation that changes what's on the remote, never fires without you seeing its scope. `Enter`, `p`, and `f` fan out immediately regardless of selection size: fetch/pull/merge/rebase only read from the remote and write locally, so a bad result is a local, cheaply-reverted mistake, not something that touches the remote.

### Reset operations

Press `U` to reset the current (or all selected) repositories to upstream:

- `y` / `Enter` — `git reset --mixed @{upstream}` — keep changes, unstage commits
- `H` — `git reset --hard @{upstream}` — discard all local changes and commits

Repos without an upstream are skipped automatically.

### Auto-stash

With `auto_stash: true` in the config, pull/merge/rebase on a dirty repository automatically runs `git stash push --include-untracked` first and `git stash pop` afterwards:

- on success the result message ends with `auto-stash restored`
- if the pop conflicts, or if the stash stack changed underneath the operation (e.g. you stashed something else manually while it was running), the stash entry (`gitbatch auto-stash`) is kept rather than risk popping the wrong one — the message says so, resolve manually; the stash badge `{N}` marks the repo
- if the operation itself fails, the stash is restored the same way
- a clean tree never creates a stash entry in the first place, so a stale "dirty" reading can never pop a stash you made yourself
- untracked files are stashed too, so pull/rebase/merge can proceed on trees with new files

Note: `a` (select-all) has its own, more conservative safety gating — a repo with incoming changes that overlap the dirty tree still isn't auto-selected; auto-stash only comes into play once you run the operation.

### Cherry-pick & tag management

Not built into gitbatch — press `Tab` to handle cherry-picks and tags interactively in lazygit.

## For git power users

### Recommended workflows

**Daily sync (safe default)**

1. Start `gitbatch -d ~/projects -r 2`
2. gitbatch fetches every repo automatically at startup; repos that can fast-forward cleanly are pre-selected (`●`) for you
3. Mode is "Pull (FF)" by default — press `Enter` and it pulls every selected repo immediately
4. Verify results: green `✓`, amber `⚠` for conflicts, red `✗` for FF failures
5. Press `m` → "Merge" or "Rebase" for diverged branches, reselect with `Space`/`a` and retry

**Feature branch cleanup**: press `W` for worktree mode, `X` to prune dead worktrees, `L` to lock/unlock, then commit and `P` to push.

**Staged release**: select only your staging repos with `Space`, switch to "Push" mode, `Enter`, verify results are clean — then repeat for the production repos. Test with a subset first; the best batch operation is one you can undo.

**Batch branch management**: select repos with `Space`, press `b` — the panel shows the branches **common** to all selected repos. `n` creates and checks out the same branch everywhere; later `d` on that branch deletes it everywhere (destructive variants always ask for confirmation first).

### Error recovery

- **Red ✗** (e.g. `no tracking information`): press `u` to set an upstream directly (accepts `origin` or `origin/branch`, works across all selected repos), or `Tab` for lazygit diagnosis
- **Gray ✗** (network): check connectivity, then `f` to retry the fetch
- **Amber ⚠** (conflicts): `Tab` → resolve in lazygit → `git merge --continue` / `git rebase --continue` → back in gitbatch press `Enter` to retry
- **`?`** (auth): the credentials prompt opens automatically and retries; if it fails again, check SSH keys or token permissions
- **`!`** (force-push): review whether you really intend to rewrite history before confirming
- **Bad merge**: press `U` — `y` for a mixed reset (keeps your changes), `H` for a hard reset (discards them) — or `Tab` for surgical history editing in lazygit

### Visual indicators

| Icon | Meaning |
|------|---------|
| ` ` (space) | Idle |
| `●` | Selected — will run on Enter |
| `⠋` | Working |
| `✓` | Success |
| `✗` | Failed (gray = network problem) |
| `⚠` | Merge/rebase conflict |
| `?` | Credentials needed |
| `!` | Force-push requires confirmation |
| `!` (muted) | Startup auto-fetch failed; `f` retries, `A` clears |

| Branch indicator | Meaning |
|------------------|---------|
| `↖ +3` | 3 commits ahead (to push) |
| `↘ +2` | 2 commits behind (to pull) |
| `↖+1 ↘+1` | Diverged |
| `~` / muted gray | No upstream set |

### Pro tips

- `t` toggles sorting between name and last commit time — use commit-time sort to find stale repos
- `←` / `→` scroll the message column to read long error messages
- `a` then a glance at the `●` count is a quick sanity check before a mass operation; `A` clears the selection and all result markers in one stroke
- `v` opens the commit log panel — check what you're about to push before switching to push mode
- Stash workflow: `S` stash → pull → `O` pop — or set `auto_stash: true` and let gitbatch do exactly this around every pull/merge/rebase
- Select first, review, then press `Enter` — don't run blindly, and verify before force-pushes, hard resets, and batch deletes

### Debugging

Run with `--trace` (or `trace: true` in the config) and gitbatch appends every git invocation to `gitbatch.log` in the current directory:

```
[1718123456] cwd=/home/user/projects/repo1 code=0 cmd=git fetch --prune output=From github.com:user/repo1
```

## Credits

- [ratatui](https://github.com/ratatui/ratatui) and [crossterm](https://github.com/crossterm-rs/crossterm) for the terminal user interface
- [clap](https://github.com/clap-rs/clap) for command-line flags & options
- [tokio](https://github.com/tokio-rs/tokio) for async runtime
- [serde](https://github.com/serde-rs/serde) for configuration management
- [lazygit](https://github.com/jesseduffield/lazygit) for everything gitbatch hands off
- the original Go [gitbatch](https://github.com/makibytes/gitbatch-legacy) by Thorsten Hirsch and Ibrahim Serdar Acikgoz, which this project reimplements

## License

[MIT](/LICENSE) — Copyright (c) 2026 [Maki Bytes UG](https://github.com/makibytes)
