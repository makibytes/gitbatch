//! Rendering: turns `App` state into ratatui widgets. Every function here
//! takes `&App`/`&mut App` (never owns state), and the palette/icon/symbol
//! constants it draws with live in the parent module (`tui`) alongside
//! `App` itself — `use super::*` pulls in both, plus the crate- and
//! std-level imports `tui/mod.rs` already brought in.

use super::*;

// ── Drawing ───────────────────────────────────────────────────────────────────

pub(super) fn draw(frame: &mut Frame, app: &mut App) {
    let area = frame.area();

    if area.width < MIN_WIDTH || area.height < MIN_HEIGHT {
        draw_too_small(frame, area);
        return;
    }

    let layout = Layout::vertical([
        Constraint::Length(1),
        Constraint::Fill(1),
        Constraint::Length(1),
    ])
    .split(area);

    draw_title_bar(frame, app, layout[0]);
    draw_repo_table(frame, app, layout[1]);
    draw_status_bar(frame, app, layout[2]);

    if app.loading {
        draw_loading(frame, app, area);
        return;
    }

    if app.show_help {
        draw_help_popup(frame, area, &mut app.help_scroll);
        return;
    }
    {
        let repo_header = app
            .repos
            .get(app.cursor)
            .map(|r| format!("  {}  {}", r.snapshot.name, r.snapshot.branch.name))
            .unwrap_or_default();
        if app.panel.is_some() {
            draw_panel_popup(frame, area, app, &repo_header);
            return;
        }
    }
    if let Some(p) = &app.confirm_prompt {
        draw_confirm_popup(frame, area, p);
        return;
    }
    if let Some(p) = &app.auth_prompt {
        draw_auth_popup(frame, area, p);
        return;
    }
    if let Some(p) = &app.prompt {
        draw_input_popup(frame, area, p, app.target_count());
    }
}

fn draw_too_small(frame: &mut Frame, area: Rect) {
    let msg = Paragraph::new(format!(
        "Terminal too small  minimum {MIN_WIDTH}×{MIN_HEIGHT}"
    ))
    .style(
        Style::default()
            .fg(Color::White)
            .bg(Color::Rgb(150, 20, 20)),
    )
    .centered();
    frame.render_widget(msg, area);
}

fn draw_loading(frame: &mut Frame, app: &App, area: Rect) {
    // Dark background
    frame.render_widget(
        Block::default().style(Style::default().bg(Color::Rgb(12, 12, 20))),
        area,
    );

    let total = app.load_total.max(1);
    let done = app.load_done;
    let ratio = done as f64 / total as f64;

    let box_w = 50u16.min(area.width.saturating_sub(4));
    let box_h = 10u16.min(area.height.saturating_sub(2));
    let box_area = centered_rect(box_w, box_h, area);

    frame.render_widget(Clear, box_area);
    frame.render_widget(
        Block::default()
            .style(Style::default().bg(C_POPUP_BG))
            .borders(Borders::ALL)
            .border_type(ratatui::widgets::BorderType::Rounded)
            .border_style(Style::default().fg(C_POPUP_BORDER)),
        box_area,
    );

    let inner = box_area.inner(Margin {
        horizontal: 2,
        vertical: 1,
    });
    if inner.height < 5 {
        return;
    }

    let spinner = SPINNER[(app.tick / 2) as usize % SPINNER.len()];
    let counter = format!("{done} / {total} repositories");

    let [title_area, _, spin_area, bar_area, count_area] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(inner);

    frame.render_widget(
        Paragraph::new("  gitbatch").style(
            Style::default()
                .fg(C_POPUP_TITLE)
                .add_modifier(Modifier::BOLD),
        ),
        title_area,
    );

    frame.render_widget(
        Paragraph::new(format!("  {spinner} Loading repositories…"))
            .style(Style::default().fg(C_HELP_FG)),
        spin_area,
    );

    let gauge = Gauge::default()
        .ratio(ratio)
        .label("")
        .gauge_style(Style::default().fg(C_GAUGE_FG).bg(C_GAUGE_BG));
    frame.render_widget(gauge, bar_area);

    frame.render_widget(
        Paragraph::new(format!("  {counter}")).style(Style::default().fg(C_DIM_FG)),
        count_area,
    );
}

fn draw_title_bar(frame: &mut Frame, app: &App, area: Rect) {
    let queued = app.queued_count();
    let working = app
        .repos
        .iter()
        .filter(|r| matches!(r.state, RepoOpState::Working))
        .count();
    let total_repos = app.repos.len();
    let sort_label = match app.sort_mode {
        SortMode::Name => "name",
        SortMode::Modified => "time",
    };

    let base = Style::default()
        .fg(C_TITLE_FG)
        .bg(C_TITLE_BG)
        .add_modifier(Modifier::BOLD);

    let left = format!(" gitbatch {}", crate::version::VERSION);
    let mut right_parts: Vec<Vec<Span>> = vec![vec![Span::raw(format!("repos:{total_repos}"))]];
    if queued > 0 {
        right_parts.push(vec![Span::raw(format!("selected:{queued}"))]);
    }
    if working > 0 {
        right_parts.push(vec![Span::raw(format!("working:{working}"))]);
    }
    right_parts.push(vec![
        Span::raw("sor"),
        Span::styled("t", base.add_modifier(Modifier::UNDERLINED)),
        Span::raw(format!(":{sort_label}")),
    ]);
    if app.worktree_mode {
        right_parts.push(vec![Span::raw("worktree")]);
    }
    right_parts.push(vec![Span::raw(" ?:help ")]);

    let total_w = area.width as usize;
    let left_w = left.chars().count();
    let right_w: usize = right_parts
        .iter()
        .flatten()
        .map(|s| s.content.chars().count())
        .sum::<usize>()
        + 2 * (right_parts.len() - 1);
    let gap = total_w.saturating_sub(left_w + right_w);

    let mut spans: Vec<Span> = vec![Span::raw(left), Span::raw(" ".repeat(gap))];
    for (i, part) in right_parts.into_iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw("  "));
        }
        spans.extend(part);
    }
    frame.render_widget(Paragraph::new(Line::from(spans)).style(base), area);
}

/// Shared per-row visuals — style, the cursor/icon/name/stash cell, and the
/// message text — used by both the repo table and the worktree table.
fn repo_row_visual(
    repo: &RepoView,
    display_name: &str,
    selected: bool,
    tick: u64,
    repo_col_w: u16,
) -> (Style, Cell<'static>, String) {
    let dirty = repo.snapshot.dirty;
    let no_upstream = repo.snapshot.branch.no_upstream;
    let pull_conflict = repo.snapshot.branch.behind > 0 && !repo.pull_safe;
    let conflict = repo.snapshot.has_conflicts || pull_conflict;
    let has_local = !conflict && (repo.snapshot.branch.ahead > 0 || dirty);
    let style = repo
        .state
        .row_style(conflict, has_local, no_upstream, selected);
    let fetch_spin = repo.fetching && matches!(repo.state, RepoOpState::Idle);
    let icon = if fetch_spin {
        SPINNER[(tick / 2) as usize % SPINNER.len()]
    } else {
        repo.state
            .status_icon(conflict, has_local, no_upstream, tick)
    };
    // Muted marker for a failed startup fetch — only when nothing more
    // important occupies the icon column.
    let fetch_failed = !repo.fetching
        && repo.fetch_error.is_some()
        && icon == " "
        && matches!(repo.state, RepoOpState::Idle);
    let cursor_sym = if selected { ICON_CURSOR } else { " " };
    let stash_badge = if repo.snapshot.stash_count > 0 {
        format!(" {{{}}}", repo.snapshot.stash_count)
    } else {
        String::new()
    };
    // 4 = cursor(1) + space(1) + icon(1) + space(1)
    let name_w = repo_col_w.saturating_sub(4 + stash_badge.chars().count() as u16) as usize;
    let name = truncate(display_name, name_w);
    let repo_cell: Cell<'static> =
        if fetch_spin || fetch_failed || (!stash_badge.is_empty() && !selected) {
            let mut spans: Vec<Span<'static>> = vec![Span::raw(format!("{cursor_sym} "))];
            if fetch_spin {
                spans.push(Span::styled(
                    icon.to_string(),
                    Style::default().fg(Color::White),
                ));
            } else if fetch_failed {
                spans.push(Span::styled("!", Style::default().fg(C_NET_FG)));
            } else {
                spans.push(Span::raw(icon.to_string()));
            }
            spans.push(Span::raw(format!(" {name}")));
            if !stash_badge.is_empty() && !selected {
                spans.push(Span::styled(stash_badge, Style::default().fg(C_STASH_FG)));
            } else if !stash_badge.is_empty() {
                spans.push(Span::raw(stash_badge));
            }
            Cell::from(Line::from(spans))
        } else {
            Cell::from(format!("{cursor_sym} {icon} {name}{stash_badge}"))
        };
    let msg_text = match &repo.state {
        RepoOpState::Fail { message: m, .. } => m.clone(),
        _ => repo.snapshot.commit_subject.clone(),
    };
    (style, repo_cell, msg_text)
}

/// Recompute repo-table column widths from current repo/worktree data — an
/// O(n) scan that formats every branch's ahead/behind counts and every
/// repo's last-modified age. Called only when `App::col_widths` is
/// invalidated or the terminal width/mode changed (see `draw_repo_table`),
/// not on every frame.
fn compute_col_widths(app: &App, area_width: u16, inner_w: usize, in_wt: bool) -> ColWidths {
    // ── age column (normal mode, terminal ≥ 112 columns) ─────────────────
    let show_age = !in_wt && area_width >= 112;
    let age_col_w: u16 = if show_age {
        let now = SystemTime::now();
        let max_len = app
            .repos
            .iter()
            .map(|r| {
                r.snapshot
                    .last_modified
                    .map(|t| format_age(now, t))
                    .unwrap_or_default()
                    .chars()
                    .count()
            })
            .max()
            .unwrap_or(2)
            .clamp(2, 5) as u16;
        max_len + 1
    } else {
        0
    };

    // ── column widths ─────────────────────────────────────────────────────
    let max_name = if in_wt {
        app.wt_rows
            .iter()
            .map(|r| r.display_name.chars().count())
            .max()
            .unwrap_or(10)
    } else {
        app.repos
            .iter()
            .map(|r| r.snapshot.name.chars().count())
            .max()
            .unwrap_or(10)
    }
    .clamp(8, 36) as u16;

    // Digit count without allocating (unlike `n.to_string().len()`).
    let digits = |n: u32| n.checked_ilog10().unwrap_or(0) as usize + 1;

    let max_branch = if in_wt {
        app.wt_rows
            .iter()
            .map(|r| r.wt_label.chars().count())
            .max()
            .unwrap_or(6)
    } else {
        app.repos
            .iter()
            .map(|r| {
                let b = &r.snapshot.branch;
                let mut len = b.name.chars().count();
                if b.ahead > 0 {
                    len += 1 + 1 + digits(b.ahead);
                }
                if b.behind > 0 {
                    len += 1 + 1 + digits(b.behind);
                }
                len
            })
            .max()
            .unwrap_or(6)
    }
    .clamp(6, 28) as u16;

    let repo_col_w = (max_name + 4).min(inner_w as u16 / 3);
    let branch_col_w = (max_branch + 1).min(inner_w as u16 / 4);

    ColWidths {
        area_width,
        in_wt,
        repo_col_w,
        branch_col_w,
        age_col_w,
        show_age,
    }
}

fn draw_repo_table(frame: &mut Frame, app: &mut App, area: Rect) {
    let inner_w = area.width.saturating_sub(2) as usize;
    let in_wt = app.worktree_mode;

    // Reuse cached column widths unless the cache was invalidated by new
    // repo/worktree data (see `apply_bg_event`) or the width/mode changed —
    // recomputing is an O(n) scan and `draw` runs on every 80ms tick.
    if app
        .col_widths
        .is_none_or(|c| c.area_width != area.width || c.in_wt != in_wt)
    {
        app.col_widths = Some(compute_col_widths(app, area.width, inner_w, in_wt));
    }
    let ColWidths {
        repo_col_w,
        branch_col_w,
        age_col_w,
        show_age,
        ..
    } = app.col_widths.expect("populated above");

    // A header row costs one more line than the borders alone; skip it on
    // very short terminals so every row still goes to data.
    let show_header = area.height >= 10;
    let viewport_h = area.height.saturating_sub(if show_header { 3 } else { 2 }) as usize;

    // ── update scroll offset ──────────────────────────────────────────────
    let (cursor, total) = if in_wt {
        (app.wt_cursor, app.wt_rows.len())
    } else {
        (app.cursor, app.repos.len())
    };
    if total > 0 {
        if in_wt {
            if cursor < app.wt_offset {
                app.wt_offset = cursor;
            } else if viewport_h > 0 && cursor >= app.wt_offset + viewport_h {
                app.wt_offset = cursor + 1 - viewport_h;
            }
            app.wt_offset = app.wt_offset.min(total.saturating_sub(viewport_h));
        } else {
            if cursor < app.table_offset {
                app.table_offset = cursor;
            } else if viewport_h > 0 && cursor >= app.table_offset + viewport_h {
                app.table_offset = cursor + 1 - viewport_h;
            }
            app.table_offset = app.table_offset.min(total.saturating_sub(viewport_h));
        }
    }
    let visible_start = if in_wt {
        app.wt_offset
    } else {
        app.table_offset
    };
    let visible_end = (visible_start + viewport_h).min(total);

    let msg_col_w = (inner_w as u16)
        .saturating_sub(repo_col_w + branch_col_w + age_col_w)
        .max(8) as usize;

    let mut widths: Vec<Constraint> = vec![
        Constraint::Length(repo_col_w),
        Constraint::Length(branch_col_w),
        Constraint::Fill(1),
    ];
    if show_age {
        widths.push(Constraint::Length(age_col_w));
    }

    // Capture scalars before row-building closures (avoids re-borrowing app)
    let tick = app.tick;
    // One syscall for every visible row this frame, not one per row.
    let now = SystemTime::now();

    if !in_wt {
        // Clamp against the longest currently-visible message: holding →
        // used to scroll it clean off the edge with no way to tell how far
        // is too far, and no way back except holding ← for just as long.
        let max_msg_len = app.repos[visible_start..visible_end]
            .iter()
            .map(|r| match &r.state {
                RepoOpState::Fail { message: m, .. } => m.chars().count(),
                _ => r.snapshot.commit_subject.chars().count(),
            })
            .max()
            .unwrap_or(0);
        app.msg_scroll = app.msg_scroll.min(max_msg_len.saturating_sub(1));
    }
    let msg_scroll = app.msg_scroll;

    let rows: Vec<Row<'static>> = if in_wt {
        app.wt_rows[visible_start..visible_end]
            .iter()
            .enumerate()
            .map(|(vi, wt_row)| {
                let i = visible_start + vi;
                let selected = i == app.wt_cursor;
                // Looked up by path, not a captured index: `repos` can be
                // re-sorted (periodic refresh) after worktrees were loaded,
                // and an index would silently pair this row with whatever
                // repo now sits at that slot instead of its actual owner.
                let Some(repo) = app
                    .repos
                    .iter()
                    .find(|r| r.snapshot.path == wt_row.repo_path)
                else {
                    return Row::new([Cell::from(wt_row.display_name.clone())])
                        .style(Style::default().fg(C_DIM_FG));
                };
                let (style, repo_cell, msg_text) =
                    repo_row_visual(repo, &wt_row.display_name, selected, tick, repo_col_w);
                let label = truncate(&wt_row.wt_label, branch_col_w as usize);
                let msg = truncate(&msg_text, msg_col_w);
                Row::new([repo_cell, Cell::from(label), Cell::from(msg)]).style(style)
            })
            .collect()
    } else {
        app.repos[visible_start..visible_end]
            .iter()
            .enumerate()
            .map(|(vi, repo)| {
                let i = visible_start + vi;
                let selected = i == app.cursor;
                let (style, repo_cell, msg_text) =
                    repo_row_visual(repo, &repo.snapshot.name, selected, tick, repo_col_w);

                let branch_cell: Cell<'static> =
                    Cell::from(truncate(&repo.branch_display(), branch_col_w as usize));

                // Message with horizontal scroll
                let scrolled: String = if msg_scroll > 0 {
                    msg_text.chars().skip(msg_scroll).collect()
                } else {
                    msg_text
                };
                let msg = truncate(&scrolled, msg_col_w);

                let mut cells: Vec<Cell<'static>> = vec![repo_cell, branch_cell, Cell::from(msg)];
                if show_age {
                    let age = repo
                        .snapshot
                        .last_modified
                        .map(|t| format_age(now, t))
                        .unwrap_or_default();
                    cells.push(Cell::from(age).style(if selected {
                        Style::default()
                    } else {
                        Style::default().fg(C_AGE_FG)
                    }));
                }
                Row::new(cells).style(style)
            })
            .collect()
    };

    let dim = Style::default().fg(C_DIM_FG);
    let mut block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(C_BORDER));

    if visible_start > 0 {
        block = block.title_top(Line::from(Span::styled(" more above ", dim)).centered());
    }
    if visible_end < total {
        block = block.title_bottom(Line::from(Span::styled(" more below ", dim)).centered());
    }

    let mut table = Table::new(rows, widths)
        .block(block)
        .row_highlight_style(Style::default());
    if show_header {
        let mut header_cells = vec![
            // The repo cell is "<cursor> <icon> <name>" (see `repo_row_visual`) —
            // a 4-char prefix before the name starts. Indent the label to match.
            Cell::from("    repo"),
            Cell::from(if in_wt { "worktree" } else { "branch" }),
            Cell::from("message"),
        ];
        if show_age {
            header_cells.push(Cell::from("age"));
        }
        table = table.header(Row::new(header_cells).style(dim));
    }
    frame.render_widget(table, area);
}

/// Pick the widest of `tiers` (ordered full → short → minimal) whose width
/// still leaves the status bar's center status at least `STATUS_CENTER_FLOOR`
/// columns, given the total bar width and the mode badge's width. Falls back
/// to the last (shortest) tier if none fit — the caller then simply gets a
/// squeezed or empty center rather than a hint clipped mid-word.
///
/// Returns an owned `String` (rather than borrowing a tier) so a call site
/// can pass a widest tier built from a dynamic prefix — e.g. the selection
/// scope label — without fighting the borrow checker over a temporary that
/// doesn't outlive the match arm that built it.
fn fit_hint(tiers: &[&str], total_w: usize, mode_w: usize) -> String {
    for tier in tiers {
        let right_w = tier.chars().count();
        if total_w.saturating_sub(mode_w + right_w) >= STATUS_CENTER_FLOOR {
            return (*tier).to_string();
        }
    }
    tiers.last().copied().unwrap_or_default().to_string()
}

fn draw_status_bar(frame: &mut Frame, app: &App, area: Rect) {
    let (mode_bg, mode_fg, mode_sym) = match app.mode {
        Mode::Pull => (C_MODE_PULL_BG, C_MODE_LIGHT_FG, SYM_PULL),
        Mode::Merge => (C_MODE_MERGE_BG, C_MODE_LIGHT_FG, SYM_MERGE),
        Mode::Rebase => (C_MODE_REBASE_BG, C_MODE_LIGHT_FG, SYM_REBASE),
        Mode::Push => (C_MODE_PUSH_BG, C_MODE_DARK_FG, SYM_PUSH),
        Mode::Fetch => (C_MODE_FETCH_BG, C_MODE_LIGHT_FG, SYM_FETCH),
    };

    let mode_str = format!(" {mode_sym} {} ", app.mode.as_str().to_uppercase());
    let mode_w = mode_str.chars().count();

    // An active notice (mode change, queue-all result, …) takes over the
    // center slot for its ~3s lifetime — it's the only place those messages
    // are visible at all, so it must outrank the per-repo status line.
    let notice = app.active_notice();
    let center = if let Some(notice) = notice {
        format!(" {notice}")
    } else if let Some(repo) = app.repos.get(app.cursor) {
        match &repo.state {
            RepoOpState::Success(m) => format!(" ✓ {m}"),
            RepoOpState::Fail { message: m, .. } => format!(" ✗ {m}"),
            RepoOpState::Working => format!(" {} working…", repo.snapshot.name),
            _ => {
                let b = &repo.snapshot.branch;
                let mut s = format!(" {}  {}", repo.snapshot.name, b.name);
                if b.no_upstream {
                    s.push_str("  no upstream");
                } else {
                    if b.ahead > 0 {
                        let _ = write!(s, " {}{}", ICON_AHEAD, b.ahead);
                    }
                    if b.behind > 0 {
                        let _ = write!(s, " {}{}", ICON_BEHIND, b.behind);
                    }
                }
                let pull_conflict = b.behind > 0 && !repo.pull_safe;
                if repo.snapshot.has_conflicts {
                    s.push_str("  merge conflict");
                } else if pull_conflict && b.ahead > 0 {
                    s.push_str("  conflict");
                } else if pull_conflict {
                    s.push_str("  would conflict");
                } else if repo.snapshot.dirty {
                    s.push_str("  dirty");
                }
                if repo.snapshot.stash_count > 0 {
                    let _ = write!(s, "  {{{}}}", repo.snapshot.stash_count);
                }
                if repo.fetch_error.is_some() {
                    s.push_str("  ⚠ auto-fetch failed");
                }
                s
            }
        }
    } else {
        " no repositories".into()
    };

    let total_w = area.width as usize;
    let right: String = if app.worktree_mode {
        fit_hint(
            &[
                "  n:new  d:rm  L:lock  X:prune  W:exit ",
                "  n/d/L/X  W:exit ",
                "  ?:help ",
            ],
            total_w,
            mode_w,
        )
    } else if let Some(panel) = &app.panel {
        match panel.kind {
            PanelKind::Branches => fit_hint(
                &[
                    "  j/k:nav  space:checkout  n:new  d/D:del  Esc:close ",
                    "  j/k  space:co  d/D  Esc ",
                    "  ?:help ",
                ],
                total_w,
                mode_w,
            ),
            _ => fit_hint(
                &["  j/k:scroll  Esc:close ", "  j/k  Esc ", "  ?:help "],
                total_w,
                mode_w,
            ),
        }
    } else if let Some(repo) = app.repos.get(app.cursor) {
        match &repo.state {
            RepoOpState::Success(_) | RepoOpState::Fail { .. } => fit_hint(
                &["  c/Esc:clear ", "  c/Esc ", "  ?:help "],
                total_w,
                mode_w,
            ),
            RepoOpState::Working => fit_hint(
                &["  working… ", "  working… ", "  working… "],
                total_w,
                mode_w,
            ),
            // These three fan out over the selection (target_paths() falls
            // back to the current repo only when nothing is selected) — the
            // widest tier names the scope so `p:pull` reads as "current
            // repo" or "12 selected" before it runs, not after.
            _ if repo.snapshot.dirty => {
                let scoped = format!(
                    "  {} · c:commit  S:stash  TAB:lazygit ",
                    app.selection_scope_label()
                );
                fit_hint(
                    &[
                        &scoped,
                        "  c:commit  S:stash  TAB:lazygit ",
                        "  c  S  TAB ",
                        "  ?:help ",
                    ],
                    total_w,
                    mode_w,
                )
            }
            _ if repo.snapshot.branch.behind > 0 => {
                let scoped = format!(
                    "  {} · p:pull  f:fetch  TAB:lazygit ",
                    app.selection_scope_label()
                );
                fit_hint(
                    &[
                        &scoped,
                        "  p:pull  f:fetch  TAB:lazygit ",
                        "  p  f  TAB ",
                        "  ?:help ",
                    ],
                    total_w,
                    mode_w,
                )
            }
            _ if repo.snapshot.branch.ahead > 0 => {
                let scoped = format!("  {} · P:push  TAB:lazygit ", app.selection_scope_label());
                fit_hint(
                    &[&scoped, "  P:push  TAB:lazygit ", "  P  TAB ", "  ?:help "],
                    total_w,
                    mode_w,
                )
            }
            _ => fit_hint(
                &["  m:mode  TAB:lazygit ", "  m  TAB ", "  ?:help "],
                total_w,
                mode_w,
            ),
        }
    } else {
        fit_hint(&["  m:mode ", "  m:mode ", "  ?:help "], total_w, mode_w)
    };
    let right_w = right.chars().count();
    let center_w = total_w.saturating_sub(mode_w + right_w);
    let center_trimmed = truncate(&center, center_w.saturating_sub(1));
    let pad = center_w.saturating_sub(center_trimmed.chars().count() + 1);
    let center_style = if notice.is_some() {
        Style::default()
            .fg(C_NOTICE_FG)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    };

    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                &mode_str,
                Style::default()
                    .fg(mode_fg)
                    .bg(mode_bg)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("{}{}", center_trimmed, " ".repeat(pad)),
                center_style,
            ),
            Span::styled(right, Style::default().fg(C_HELP_FG)),
        ])),
        area,
    );
}

// ── Popup helpers ─────────────────────────────────────────────────────────────

fn centered_rect(width: u16, height: u16, area: Rect) -> Rect {
    let x = area.x + area.width.saturating_sub(width) / 2;
    let y = area.y + area.height.saturating_sub(height) / 2;
    Rect::new(x, y, width.min(area.width), height.min(area.height))
}

fn popup_block(title: &str) -> Block<'_> {
    Block::default()
        .style(Style::default().bg(C_POPUP_BG))
        .borders(Borders::ALL)
        .border_type(ratatui::widgets::BorderType::Rounded)
        .border_style(Style::default().fg(C_POPUP_BORDER))
        .title(Span::styled(
            format!(" {title} "),
            Style::default()
                .fg(C_POPUP_TITLE)
                .add_modifier(Modifier::BOLD),
        ))
}

fn section_line(label: &str) -> Line<'static> {
    Line::from(Span::styled(
        label.to_string(),
        Style::default()
            .fg(C_SECTION_HDR)
            .add_modifier(Modifier::BOLD),
    ))
}

fn kv_line<'a>(key: &'a str, desc: &'a str) -> Line<'a> {
    Line::from(vec![
        Span::styled(
            format!("  {key:<12}"),
            Style::default().fg(C_KEY_FG).add_modifier(Modifier::BOLD),
        ),
        Span::styled(desc, Style::default().fg(Color::White)),
    ])
}

fn icon_kv_line(glyph: &'static str, desc: &'static str, color: Color) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            format!("  {glyph:<12}"),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ),
        Span::styled(desc, Style::default().fg(Color::White)),
    ])
}

fn draw_help_popup(frame: &mut Frame, area: Rect, scroll: &mut usize) {
    let popup_w = 78u16.min(area.width.saturating_sub(4));
    let popup_h = 35u16.min(area.height.saturating_sub(2));
    let popup_area = centered_rect(popup_w, popup_h, area);

    frame.render_widget(Clear, popup_area);
    frame.render_widget(popup_block("Help"), popup_area);

    let inner = popup_area.inner(Margin {
        horizontal: 2,
        vertical: 1,
    });
    if inner.width < 20 || inner.height < 3 {
        return;
    }

    const KEYS_H: u16 = 23;
    const ICONS_H: u16 = 10;

    let left_lines = vec![
        section_line("Navigation"),
        kv_line("j/k ↑↓", "move cursor"),
        kv_line("g/G", "top / bottom"),
        kv_line("PgUp/PgDn", "jump page"),
        kv_line("← / →", "scroll message"),
        Line::from(""),
        section_line("Git"),
        kv_line("f", "fetch"),
        kv_line("p", "pull"),
        kv_line("P", "push"),
        kv_line("m", "cycle mode"),
        Line::from(""),
        section_line("Local"),
        kv_line("c", "commit"),
        kv_line("n", "new branch"),
        kv_line("u", "set upstream"),
        kv_line("U", "reset to upstream"),
        kv_line("S / O / D", "stash/pop/drop"),
        Line::from(""),
        section_line("Worktrees"),
        kv_line("W", "toggle worktree view"),
        kv_line("n / d", "new / remove"),
        kv_line("L / X", "lock / prune"),
    ];

    let right_lines = vec![
        section_line("Batch (select to apply to all)"),
        kv_line("Space", "select / deselect repo"),
        kv_line("a / A", "select all / clear all"),
        kv_line("Enter", "run mode on selection"),
        kv_line("", "(push to >1 repo confirms"),
        kv_line("", "first; other modes don't)"),
        kv_line("", "all actions apply to the"),
        kv_line("", "selection when non-empty"),
        section_line("Panels"),
        kv_line("b", "branches"),
        kv_line("s", "status"),
        kv_line("v", "commits"),
        kv_line("q / Esc", "close panel"),
        Line::from(""),
        section_line("Other"),
        kv_line("t", "toggle sort"),
        kv_line("TAB", "lazygit"),
        kv_line("q / Ctrl+C", "quit"),
    ];

    // Status icons legend — full-width section below the keybindings.
    let icon_lines = vec![
        section_line("Status Icons"),
        icon_kv_line(
            ICON_CONFLICT,
            "conflict — active merge conflict, or pull would create conflicts",
            C_CONFLICT_FG,
        ),
        icon_kv_line(
            ICON_LOCAL,
            "local — uncommitted/unpushed changes, safe to pull",
            C_LOCAL_FG,
        ),
        icon_kv_line(
            ICON_AHEAD,
            "ahead — branch is ahead of its remote",
            C_AHEAD_FG,
        ),
        icon_kv_line(
            ICON_BEHIND,
            "behind — branch is behind its remote",
            C_BEHIND_FG,
        ),
        icon_kv_line(
            ICON_QUEUED,
            "selected — included in the batch selection",
            C_QUEUED_FG,
        ),
        icon_kv_line(ICON_SUCCESS, "ok — last operation succeeded", C_SUCCESS_FG),
        icon_kv_line(
            ICON_FAIL,
            "failed — last operation failed (gray: network error)",
            C_FAIL_FG,
        ),
        icon_kv_line(
            "?  /  !",
            "failed — credentials needed / push rejected (non-FF)",
            C_CREDS_FG,
        ),
        icon_kv_line("{N}", "stash — repo has N stashed changesets", C_STASH_FG),
    ];

    if inner.height >= KEYS_H + ICONS_H {
        // Tall terminal: two keybinding columns with the legend below.
        *scroll = 0;
        let [keys_area, icons_area] =
            Layout::vertical([Constraint::Length(KEYS_H), Constraint::Fill(1)]).areas(inner);
        let half = keys_area.width / 2;
        let [left_area, right_area] =
            Layout::horizontal([Constraint::Length(half), Constraint::Fill(1)]).areas(keys_area);
        frame.render_widget(Paragraph::new(left_lines), left_area);
        frame.render_widget(Paragraph::new(right_lines), right_area);
        frame.render_widget(Paragraph::new(icon_lines), icons_area);
        return;
    }

    // Short terminal: flatten everything into one scrollable column.
    let mut flat: Vec<Line> = left_lines;
    flat.push(Line::from(""));
    flat.extend(right_lines);
    flat.push(Line::from(""));
    flat.extend(icon_lines);

    let visible = inner.height as usize;
    *scroll = (*scroll).min(flat.len().saturating_sub(visible));
    let start = *scroll;
    let end = (start + visible).min(flat.len());

    let dim = Style::default().fg(C_DIM_FG);
    let mut lines: Vec<Line> = Vec::with_capacity(visible);
    if start > 0 {
        lines.push(Line::from(Span::styled("  ↑ more above (k)", dim)));
    }
    let body_start = start + usize::from(start > 0);
    let more_below = end < flat.len();
    let body_end = end - usize::from(more_below);
    lines.extend(flat[body_start..body_end].iter().cloned());
    if more_below {
        lines.push(Line::from(Span::styled("  ↓ more below (j)", dim)));
    }
    frame.render_widget(Paragraph::new(lines), inner);
}

fn draw_panel_popup(frame: &mut Frame, area: Rect, app: &mut App, repo_header: &str) {
    let Some(panel) = app.panel.as_ref() else {
        return;
    };
    let popup_w = (area.width * 3 / 4)
        .clamp(50, 120)
        .min(area.width.saturating_sub(4));
    let popup_h = (area.height * 4 / 5)
        .clamp(12, 60)
        .min(area.height.saturating_sub(2));
    let popup_area = centered_rect(popup_w, popup_h, area);

    frame.render_widget(Clear, popup_area);
    frame.render_widget(popup_block(&panel.title), popup_area);

    let inner = popup_area.inner(Margin {
        horizontal: 1,
        vertical: 1,
    });

    frame.render_widget(
        Paragraph::new(Span::styled(
            repo_header,
            Style::default()
                .fg(C_BRANCH_CUR)
                .add_modifier(Modifier::BOLD),
        )),
        Rect { height: 1, ..inner },
    );

    // Hint line
    let hint = match panel.kind {
        PanelKind::Branches => Line::from(vec![
            key_span("j/k"),
            plain(" navigate  "),
            key_span("space/c"),
            plain(" checkout  "),
            key_span("n"),
            plain(" new  "),
            key_span("d"),
            plain(" delete  "),
            key_span("D"),
            plain(" force-del  "),
            key_span("Esc"),
            plain(" close"),
        ]),
        _ => Line::from(vec![
            key_span("j/k"),
            plain(" scroll  "),
            key_span("Esc"),
            plain(" close"),
        ]),
    };
    frame.render_widget(
        Paragraph::new(hint).style(Style::default().fg(C_HELP_FG)),
        Rect {
            y: inner.y + 1,
            height: 1,
            ..inner
        },
    );

    // Content area
    let body = Rect {
        y: inner.y + 3,
        height: inner.height.saturating_sub(3),
        ..inner
    };
    let visible_h = body.height as usize;
    // Read back by `handle_panel_key` so PageUp/PageDown/j/k track this
    // popup's actual measured height instead of a value guessed in advance.
    app.panel_viewport = visible_h;
    let panel = app.panel.as_ref().expect("checked above");
    let start = panel.scroll;
    let end = (start + visible_h).min(panel.lines.len());

    let mut lines: Vec<Line> = Vec::new();
    if start > 0 {
        lines.push(Line::from(Span::styled(
            "  ↑ more above",
            Style::default().fg(C_DIM_FG),
        )));
    }
    for (i, line) in panel.lines[start..end].iter().enumerate() {
        let idx = start + i;
        if panel.is_navigable() && idx == panel.cursor {
            lines.push(Line::from(Span::styled(
                format!("▶ {line}"),
                Style::default()
                    .fg(Color::White)
                    .bg(C_SEL_DEFAULT_BG)
                    .add_modifier(Modifier::BOLD),
            )));
        } else if line.trim_start().starts_with('*') {
            lines.push(Line::from(Span::styled(
                line.as_str(),
                Style::default()
                    .fg(C_BRANCH_CUR)
                    .add_modifier(Modifier::BOLD),
            )));
        } else {
            lines.push(Line::from(line.as_str()));
        }
    }
    if end < panel.lines.len() {
        lines.push(Line::from(Span::styled(
            "  ↓ more below",
            Style::default().fg(C_DIM_FG),
        )));
    }

    frame.render_widget(Paragraph::new(lines), body);
}

fn draw_confirm_popup(frame: &mut Frame, area: Rect, prompt: &ConfirmPromptState) {
    let popup_w = 60u16.min(area.width.saturating_sub(4));
    let popup_h = 8u16;
    let popup_area = centered_rect(popup_w, popup_h, area);

    frame.render_widget(Clear, popup_area);
    frame.render_widget(popup_block(prompt.title), popup_area);

    let inner = popup_area.inner(Margin {
        horizontal: 2,
        vertical: 1,
    });

    let lines = vec![
        Line::from(Span::styled(
            prompt.subject.as_str(),
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(Span::styled(
            prompt.warning.as_str(),
            Style::default().fg(C_FAIL_FG),
        )),
        Line::from(""),
        {
            let mut hint = vec![key_span("y / Enter")];
            if matches!(prompt.action, ConfirmAction::ResetToUpstream { .. }) {
                hint.push(plain("  mixed reset    "));
                hint.push(key_span("H"));
                hint.push(plain("  hard reset    "));
            } else {
                hint.push(plain("  confirm    "));
            }
            hint.push(key_span("n / Esc"));
            hint.push(plain("  cancel"));
            Line::from(hint)
        },
    ];
    frame.render_widget(Paragraph::new(lines), inner);
}

fn draw_auth_popup(frame: &mut Frame, area: Rect, prompt: &AuthPromptState) {
    let popup_w = 58u16.min(area.width.saturating_sub(4));
    let popup_h = 11u16;
    let popup_area = centered_rect(popup_w, popup_h, area);

    frame.render_widget(Clear, popup_area);
    frame.render_widget(popup_block("Credentials Required"), popup_area);

    let inner = popup_area.inner(Margin {
        horizontal: 2,
        vertical: 1,
    });
    let repo = prompt.repo_name.as_deref().unwrap_or("?");

    let active = Style::default()
        .fg(Color::White)
        .add_modifier(Modifier::BOLD);
    let inactive = Style::default().fg(C_HELP_FG);

    let user_style = if prompt.field == CredentialField::Username {
        active
    } else {
        inactive
    };
    let pass_style = if prompt.field == CredentialField::Password {
        active
    } else {
        inactive
    };

    let user_val = if prompt.field == CredentialField::Username {
        format!("{}_", prompt.input)
    } else {
        prompt.username.clone()
    };
    let pass_val = if prompt.field == CredentialField::Password {
        "•".repeat(prompt.input.chars().count()) + "_"
    } else {
        "•".repeat(prompt.password.chars().count())
    };

    let lines = vec![
        Line::from(vec![
            Span::styled("Repository: ", Style::default().fg(C_HELP_FG)),
            Span::styled(
                repo,
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(""),
        Line::from(vec![
            Span::styled("  Username  ", Style::default().fg(C_HELP_FG)),
            Span::styled(&user_val, user_style),
        ]),
        Line::from(vec![
            Span::styled("  Password  ", Style::default().fg(C_HELP_FG)),
            Span::styled(&pass_val, pass_style),
        ]),
        Line::from(""),
        Line::from(vec![
            key_span("Tab"),
            plain(" switch  "),
            key_span("Enter"),
            plain(" submit  "),
            key_span("Esc"),
            plain(" cancel"),
        ]),
    ];
    frame.render_widget(Paragraph::new(lines), inner);
}

fn draw_input_popup(frame: &mut Frame, area: Rect, prompt: &PromptState, target_count: usize) {
    let is_commit = prompt.kind == PromptKind::Commit;
    let popup_w = 64u16.min(area.width.saturating_sub(4));
    let popup_h = if is_commit { 14u16 } else { 7u16 };
    let popup_area = centered_rect(popup_w, popup_h, area);

    frame.render_widget(Clear, popup_area);
    let title = if target_count > 1 {
        format!("{} — {} repos", prompt.kind.title(), target_count)
    } else {
        prompt.kind.title().to_string()
    };
    frame.render_widget(popup_block(&title), popup_area);

    let inner = popup_area.inner(Margin {
        horizontal: 2,
        vertical: 1,
    });
    let active = Style::default()
        .fg(Color::White)
        .add_modifier(Modifier::BOLD);
    let inactive = Style::default().fg(C_HELP_FG);

    if is_commit {
        let subj_active = prompt.commit_field == CommitField::Subject;
        let subj_indicator = if subj_active { ">" } else { " " };
        let desc_indicator = if subj_active { " " } else { ">" };
        let subj_style = if subj_active { active } else { inactive };
        let desc_style = if subj_active { inactive } else { active };

        let subj_spans = if subj_active {
            cursor_spans(&prompt.input, prompt.cursor, subj_style)
        } else {
            vec![Span::styled(prompt.input.clone(), subj_style)]
        };

        // Show up to 4 visible description lines.
        let desc_text = if !subj_active {
            format!("{}_", prompt.description)
        } else {
            prompt.description.clone()
        };
        let desc_lines: Vec<&str> = if desc_text.is_empty() {
            vec![""]
        } else {
            let all: Vec<&str> = desc_text.lines().collect();
            let max_visible = 4;
            if all.len() > max_visible {
                all[all.len() - max_visible..].to_vec()
            } else {
                all
            }
        };

        let mut subj_line = vec![
            Span::styled(
                format!("{subj_indicator} "),
                Style::default().fg(C_SECTION_HDR),
            ),
            Span::styled("Summary:     ", Style::default().fg(C_SECTION_HDR)),
        ];
        subj_line.extend(subj_spans);
        let mut lines: Vec<Line> = vec![
            Line::from(subj_line),
            Line::from(""),
            Line::from(vec![
                Span::styled(
                    format!("{desc_indicator} "),
                    Style::default().fg(C_SECTION_HDR),
                ),
                Span::styled("Description:", Style::default().fg(C_SECTION_HDR)),
            ]),
        ];
        for dl in &desc_lines {
            lines.push(Line::from(Span::styled(format!("  {dl}"), desc_style)));
        }
        // Pad to keep the hint line at a stable position.
        for _ in desc_lines.len()..4 {
            lines.push(Line::from(""));
        }
        lines.push(Line::from(""));
        if subj_active {
            lines.push(Line::from(vec![
                key_span("Enter"),
                plain(" commit  "),
                key_span("Tab"),
                plain(" description  "),
                key_span("Esc"),
                plain(" cancel"),
            ]));
        } else {
            lines.push(Line::from(vec![
                key_span("Tab"),
                plain(" summary  "),
                key_span("Esc"),
                plain(" cancel"),
            ]));
        }
        frame.render_widget(Paragraph::new(lines), inner);
    } else {
        let mut input_line = vec![Span::styled("> ", active)];
        input_line.extend(cursor_spans(&prompt.input, prompt.cursor, active));
        let error_line = match prompt.error {
            Some(e) => Line::from(Span::styled(e, Style::default().fg(C_FAIL_FG))),
            None => Line::from(""),
        };
        let lines = vec![
            Line::from(Span::styled(
                prompt.kind.label(),
                Style::default().fg(C_SECTION_HDR),
            )),
            Line::from(Span::styled(
                "─".repeat(inner.width as usize),
                Style::default().fg(C_BORDER),
            )),
            Line::from(""),
            Line::from(input_line),
            error_line,
            Line::from(vec![
                key_span("Enter"),
                plain(" confirm  "),
                key_span("Esc"),
                plain(" cancel"),
            ]),
        ];
        frame.render_widget(Paragraph::new(lines), inner);
    }
}

/// Render `input` with a reversed-video cursor at byte offset `cursor`.
fn cursor_spans(input: &str, cursor: usize, style: Style) -> Vec<Span<'static>> {
    let cursor = cursor.min(input.len());
    let (before, after) = input.split_at(cursor);
    let mut spans = vec![Span::styled(before.to_string(), style)];
    match after.chars().next() {
        Some(c) => {
            spans.push(Span::styled(
                c.to_string(),
                style.add_modifier(Modifier::REVERSED),
            ));
            spans.push(Span::styled(after[c.len_utf8()..].to_string(), style));
        }
        None => spans.push(Span::styled(" ", style.add_modifier(Modifier::REVERSED))),
    }
    spans
}

// ── Style helpers ─────────────────────────────────────────────────────────────

fn key_span(s: &str) -> Span<'_> {
    Span::styled(
        s,
        Style::default().fg(C_KEY_FG).add_modifier(Modifier::BOLD),
    )
}

fn plain(s: &str) -> Span<'_> {
    Span::styled(s, Style::default().fg(C_HELP_FG))
}

/// `now` is threaded in rather than read here with `SystemTime::now()`: this
/// runs once per visible row on every draw, and a syscall per row per frame
/// is needless when the caller already has (or can cheaply take) a single
/// `now` shared across the whole table.
fn format_age(now: SystemTime, t: SystemTime) -> String {
    let secs = now.duration_since(t).unwrap_or_default().as_secs();
    match secs {
        s if s < 60 => format!("{s}s"),
        s if s < 3_600 => format!("{}m", s / 60),
        s if s < 86_400 => format!("{}h", s / 3_600),
        s if s < 7 * 86_400 => format!("{}d", s / 86_400),
        s if s < 30 * 86_400 => format!("{}w", s / (7 * 86_400)),
        s if s < 365 * 86_400 => format!("{}mo", s / (30 * 86_400)),
        s => format!("{}y", s / (365 * 86_400)),
    }
}

/// Truncate `s` to at most `max` terminal display cells (not chars), adding
/// an ellipsis when it doesn't fit. A char-count budget would let a repo or
/// branch name containing CJK or emoji (2 cells wide) overrun its column and
/// break the table's alignment; `unicode-width` is what ratatui itself uses
/// to size cells, so this measures the same way the terminal will render it.
fn truncate(s: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    if s.width() <= max {
        return s.to_string();
    }
    // No room for an ellipsis at a 1-cell budget; otherwise reserve one cell for it.
    let (budget, want_ellipsis) = if max == 1 {
        (1, false)
    } else {
        (max - 1, true)
    };
    let mut out = String::new();
    let mut used = 0;
    for c in s.chars() {
        let w = c.width().unwrap_or(0);
        if used + w > budget {
            break;
        }
        out.push(c);
        used += w;
    }
    if want_ellipsis {
        out.push('…');
    }
    out
}

pub(super) fn display_action(action: &RemoteAction) -> &'static str {
    match action {
        RemoteAction::Fetch => "fetch",
        RemoteAction::PullFfOnly => "pull (ff-only)",
        RemoteAction::Merge => "merge",
        RemoteAction::Rebase => "rebase",
        RemoteAction::Push { force: true } => "force push",
        RemoteAction::Push { force: false } => "push",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_leaves_short_ascii_untouched() {
        assert_eq!(truncate("main", 10), "main");
        assert_eq!(truncate("main", 4), "main");
    }

    #[test]
    fn truncate_adds_ellipsis_for_long_ascii() {
        assert_eq!(truncate("feature/very-long-name", 8), "feature…");
    }

    #[test]
    fn truncate_respects_double_width_cjk_cells() {
        // Each CJK char is 2 display cells wide; a char-count budget would
        // let this run to 8 cells (4 chars) and overrun an 8-cell column.
        let s = "仓库仓库仓库仓库"; // 8 chars, 16 cells
        let out = truncate(s, 8);
        assert!(
            out.width() <= 8,
            "{out:?} is {} cells wide, over the 8-cell budget",
            out.width()
        );
        assert!(out.ends_with('…'), "{out:?}");
    }

    #[test]
    fn truncate_zero_budget_is_empty() {
        assert_eq!(truncate("main", 0), "");
    }
}
