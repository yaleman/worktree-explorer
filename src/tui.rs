use std::{
    io::{self},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};
use crossterm::{
    cursor::{Hide, MoveTo, Show},
    event::{self, Event, KeyCode, KeyEvent, KeyModifiers},
    execute,
    terminal::{Clear, ClearType, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear as ClearWidget, List, ListItem, ListState, Paragraph, Wrap},
};

use crate::git::{
    BranchDeleteResult, BranchDeletionAssessment, BranchInfo, BranchStatus, ChangeCounts,
    DeleteResult, DeletionAssessment, LogEntry, RepositoryManager, StatusArea, UpstreamState,
    WorktreeInfo, WorktreeKey, WorktreeStatus,
};

pub fn run(repositories: Vec<RepositoryManager>, recursive: bool, branches: bool) -> Result<()> {
    let _guard = TerminalGuard::enter()?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))
        .context("failed to initialize the terminal")?;
    let mut app = App::new(repositories, recursive, branches)?;

    loop {
        terminal
            .draw(|frame| app.render(frame))
            .context("failed to draw the interface")?;
        if app.process_pending_deletion() || app.process_pending_branch_deletion() {
            continue;
        }
        if let Event::Key(key) = event::read().context("failed to read terminal input")?
            && app.handle_key(key)?
        {
            break;
        }
    }
    Ok(())
}

struct TerminalGuard;

impl TerminalGuard {
    fn enter() -> Result<Self> {
        enable_raw_mode().context("failed to enable terminal raw mode")?;
        let guard = Self;
        execute!(io::stdout(), Clear(ClearType::All), MoveTo(0, 0), Hide)
            .context("failed to prepare the terminal")?;
        Ok(guard)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), Show, Clear(ClearType::All), MoveTo(0, 0));
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ConfirmationPhase {
    Initial,
    Force,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DeletionMode {
    Normal,
    Force,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ExploreMode {
    Worktrees,
    Branches,
}

impl DeletionMode {
    fn force(self) -> bool {
        matches!(self, Self::Force)
    }
}

#[derive(Debug)]
enum View {
    Worktrees,
    Log {
        worktree: WorktreeInfo,
        entries: Vec<LogEntry>,
        offset: usize,
    },
    Status {
        worktree: WorktreeInfo,
        status: WorktreeStatus,
        offset: usize,
    },
    ConfirmDelete {
        repository_index: usize,
        worktree: WorktreeInfo,
        assessment: DeletionAssessment,
        phase: ConfirmationPhase,
        offset: usize,
    },
    Deleting {
        repository_index: usize,
        worktree: WorktreeInfo,
        mode: DeletionMode,
    },
    BranchLog {
        branch: BranchInfo,
        entries: Vec<LogEntry>,
        offset: usize,
    },
    BranchStatus {
        status: BranchStatus,
        offset: usize,
    },
    ConfirmBranchDelete {
        repository_index: usize,
        branch: BranchInfo,
        assessment: BranchDeletionAssessment,
        phase: ConfirmationPhase,
    },
    DeletingBranch {
        repository_index: usize,
        branch: BranchInfo,
        mode: DeletionMode,
    },
}

struct RepositoryGroup {
    repository: RepositoryManager,
    worktrees: Vec<WorktreeInfo>,
    branches: Vec<BranchInfo>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum TreeIdentity {
    Repository(std::path::PathBuf),
    Worktree(std::path::PathBuf, WorktreeKey),
    Branch(std::path::PathBuf, String),
}

struct App {
    repositories: Vec<RepositoryGroup>,
    selected: usize,
    view: View,
    message: Option<String>,
    recursive: bool,
    hide_without_linked: bool,
    page_size: usize,
    mode: ExploreMode,
}

impl App {
    fn new(
        repositories: Vec<RepositoryManager>,
        recursive: bool,
        branch_mode: bool,
    ) -> Result<Self> {
        let repositories = repositories
            .into_iter()
            .map(|repository| {
                let worktrees = repository.list_worktrees()?;
                let branches = if branch_mode {
                    repository.list_branches()?
                } else {
                    Vec::new()
                };
                Ok(RepositoryGroup {
                    repository,
                    worktrees,
                    branches,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let selected = repositories.first().is_some_and(|group| {
            if branch_mode {
                !group.branches.is_empty()
            } else {
                !group.worktrees.is_empty()
            }
        }) as usize;
        Ok(Self {
            repositories,
            selected,
            view: View::Worktrees,
            message: None,
            recursive,
            hide_without_linked: false,
            page_size: 1,
            mode: if branch_mode {
                ExploreMode::Branches
            } else {
                ExploreMode::Worktrees
            },
        })
    }

    fn render(&mut self, frame: &mut Frame<'_>) {
        let area = frame.area();
        self.page_size = usize::from(area.height.saturating_sub(4)).max(1);
        if self.mode == ExploreMode::Branches {
            self.render_branches(frame, area);
            return;
        }
        match &self.view {
            View::Worktrees => self.render_worktrees(frame, area),
            View::Log {
                worktree,
                entries,
                offset,
            } => self.render_log(frame, area, worktree, entries, *offset),
            View::Status {
                worktree,
                status,
                offset,
            } => self.render_status(frame, area, worktree, status, *offset),
            View::ConfirmDelete {
                repository_index: _,
                worktree,
                assessment,
                phase,
                offset,
            } => {
                self.render_worktrees(frame, area);
                self.render_confirmation(frame, area, worktree, assessment, *phase, *offset);
            }
            View::Deleting {
                repository_index: _,
                worktree,
                mode: _,
            } => {
                self.render_worktrees(frame, area);
                self.render_deleting(frame, area, worktree);
            }
            _ => {}
        }
    }

    fn render_branches(&self, frame: &mut Frame<'_>, area: Rect) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(3), Constraint::Length(2)])
            .split(area);
        let mut items = Vec::new();
        for group in self
            .repositories
            .iter()
            .filter(|group| self.group_is_visible(group))
        {
            items.push(ListItem::new(Line::from(vec![
                Span::styled("▾ ", Style::default().fg(Color::Magenta)),
                Span::styled(
                    group.repository.root().display().to_string(),
                    Style::default().add_modifier(Modifier::BOLD),
                ),
            ])));
            let last_index = group.branches.len().saturating_sub(1);
            for (branch_index, branch) in group.branches.iter().enumerate() {
                let row_index = items.len();
                let checkout = if branch.checked_out_paths.is_empty() {
                    String::new()
                } else {
                    format!(
                        " [checked out: {}]",
                        branch
                            .checked_out_paths
                            .iter()
                            .map(|path| path.display().to_string())
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                };
                let connector = if branch_index == last_index {
                    "  └─ "
                } else {
                    "  ├─ "
                };
                let metadata_style = Style::default().fg(if row_index == self.selected {
                    Color::White
                } else {
                    Color::DarkGray
                });
                items.push(ListItem::new(Line::from(vec![
                    Span::styled(connector, Style::default().fg(Color::DarkGray)),
                    Span::styled(format!("{:<8} ", branch.head), metadata_style),
                    Span::styled(
                        format!("{:<10} ", relative_age(branch.committed_at)),
                        metadata_style,
                    ),
                    Span::styled(
                        format!("{}{}", branch.name, checkout),
                        Style::default().fg(Color::Cyan),
                    ),
                ])));
            }
        }
        let list = List::new(items)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" Repositories and local branches "),
            )
            .highlight_style(
                Style::default()
                    .bg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol("› ");
        let selected = (self.row_count() > 0).then_some(self.selected.min(self.row_count() - 1));
        let mut state = ListState::default().with_selected(selected);
        frame.render_stateful_widget(list, chunks[0], &mut state);
        let filter_help = if self.recursive {
            if self.hide_without_linked {
                "   h show all"
            } else {
                "   h hide empty"
            }
        } else {
            ""
        };
        let help = self.message.as_deref().map_or_else(
            || format!("↑/↓ or j/k select   PgUp/PgDn page   l logs   s status   d delete   r refresh{filter_help}   q quit"),
            |message| format!("Error: {message}"),
        );
        let style = if self.message.is_some() {
            Style::default().fg(Color::Red)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        frame.render_widget(
            Paragraph::new(help).style(style).wrap(Wrap { trim: true }),
            chunks[1],
        );
        match &self.view {
            View::BranchLog {
                branch,
                entries,
                offset,
            } => {
                self.render_branch_log(frame, area, branch, entries, *offset);
            }
            View::BranchStatus { status, offset } => {
                self.render_branch_status(frame, area, status, *offset);
            }
            View::ConfirmBranchDelete {
                branch,
                assessment,
                phase,
                ..
            } => {
                self.render_branch_confirmation(frame, area, branch, assessment, *phase);
            }
            View::DeletingBranch { branch, .. } => self.render_branch_deleting(frame, area, branch),
            _ => {}
        }
    }

    fn render_branch_log(
        &self,
        frame: &mut Frame<'_>,
        area: Rect,
        branch: &BranchInfo,
        entries: &[LogEntry],
        offset: usize,
    ) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(3), Constraint::Length(1)])
            .split(area);
        let lines = if entries.is_empty() {
            vec![Line::from("No commits reachable from this branch")]
        } else {
            entries
                .iter()
                .map(|entry| {
                    Line::from(vec![
                        Span::styled(format!("{} ", entry.id), Style::default().fg(Color::Yellow)),
                        Span::raw(format!("{}  ", entry.subject)),
                        Span::styled(
                            format!("{} · {}", entry.author, relative_age(entry.committed_at)),
                            Style::default().fg(Color::DarkGray),
                        ),
                    ])
                })
                .collect()
        };
        let paragraph = Paragraph::new(lines)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(format!(" Log — {} ", branch.name)),
            )
            .scroll((offset.min(u16::MAX as usize) as u16, 0));
        frame.render_widget(paragraph, chunks[0]);
        render_detail_help(frame, chunks[1]);
    }

    fn render_branch_status(
        &self,
        frame: &mut Frame<'_>,
        area: Rect,
        status: &BranchStatus,
        offset: usize,
    ) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(3), Constraint::Length(1)])
            .split(area);
        let mut lines = vec![Line::from(Span::styled(
            format!("Branch {}", status.branch),
            Style::default().fg(Color::Cyan),
        ))];
        match status.upstream_state {
            UpstreamState::NotConfigured => lines.push(Line::from("No configured upstream")),
            UpstreamState::Missing => lines.push(Line::from(format!(
                "Upstream {} is missing",
                status.upstream.as_deref().unwrap_or("(unknown)")
            ))),
            UpstreamState::Present => {
                if status.ahead > 0 {
                    lines.push(Line::from(format!(
                        "Ahead of {} by {} commit(s)",
                        status.upstream.as_deref().unwrap_or("upstream"),
                        status.ahead
                    )));
                }
                if status.behind > 0 {
                    lines.push(Line::from(format!(
                        "Behind {} by {} commit(s)",
                        status.upstream.as_deref().unwrap_or("upstream"),
                        status.behind
                    )));
                }
                if status.ahead == 0 && status.behind == 0 {
                    lines.push(Line::from("Up to date with upstream"));
                }
            }
        }
        if status.checked_out_paths.is_empty() {
            lines.push(Line::from("Not checked out"));
        } else {
            for path in &status.checked_out_paths {
                lines.push(Line::from(format!("Checked out at {}", path.display())));
            }
            append_change_count(&mut lines, status.changes);
        }
        let paragraph = Paragraph::new(lines)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" Branch status "),
            )
            .scroll((offset.min(u16::MAX as usize) as u16, 0));
        frame.render_widget(paragraph, chunks[0]);
        render_detail_help(frame, chunks[1]);
    }

    fn render_branch_confirmation(
        &self,
        frame: &mut Frame<'_>,
        area: Rect,
        branch: &BranchInfo,
        assessment: &BranchDeletionAssessment,
        phase: ConfirmationPhase,
    ) {
        let popup = centered_rect(76, 8, area);
        frame.render_widget(ClearWidget, popup);
        let mut lines = vec![Line::from(branch.name.clone())];
        if assessment.unmerged {
            lines.push(Line::from(Span::styled(
                "This branch is not merged into the repository HEAD.",
                Style::default().fg(Color::Yellow),
            )));
        }
        match phase {
            ConfirmationPhase::Initial => lines.push(Line::from("Delete this local branch? y/N")),
            ConfirmationPhase::Force => lines.push(Line::from(Span::styled(
                "This permanently discards the unmerged branch.",
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            ))),
        }
        if phase == ConfirmationPhase::Force {
            lines.push(Line::from(
                "Press uppercase D to force deletion; any other key cancels.",
            ));
        }
        frame.render_widget(
            Paragraph::new(lines).alignment(Alignment::Center).block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" Confirm branch deletion "),
            ),
            popup,
        );
    }

    fn render_branch_deleting(&self, frame: &mut Frame<'_>, area: Rect, branch: &BranchInfo) {
        let popup = centered_rect(76, 5, area);
        frame.render_widget(ClearWidget, popup);
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(branch.name.clone()),
                Line::from(Span::styled(
                    "Deleting...",
                    Style::default().add_modifier(Modifier::BOLD),
                )),
            ])
            .alignment(Alignment::Center)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" Delete branch "),
            ),
            popup,
        );
    }

    fn render_worktrees(&self, frame: &mut Frame<'_>, area: Rect) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(3), Constraint::Length(2)])
            .split(area);
        let mut items = Vec::new();
        for group in self
            .repositories
            .iter()
            .filter(|group| self.group_is_visible(group))
        {
            items.push(ListItem::new(Line::from(vec![
                Span::styled("▾ ", Style::default().fg(Color::Magenta)),
                Span::styled(
                    group.repository.root().display().to_string(),
                    Style::default().add_modifier(Modifier::BOLD),
                ),
            ])));
            let last_index = group.worktrees.len().saturating_sub(1);
            for (worktree_index, worktree) in group.worktrees.iter().enumerate() {
                let row_index = items.len();
                let mut markers = Vec::new();
                if matches!(worktree.key, WorktreeKey::Main) {
                    markers.push("main");
                }
                if worktree.locked {
                    markers.push("locked");
                }
                if !worktree.available {
                    markers.push("missing");
                }
                let marker = if markers.is_empty() {
                    String::new()
                } else {
                    format!(" [{}]", markers.join(", "))
                };
                let head = worktree.head.as_deref().unwrap_or("--------");
                let age = worktree
                    .committed_at
                    .map(relative_age)
                    .unwrap_or_else(|| "—".to_owned());
                let metadata_style = Style::default().fg(if row_index == self.selected {
                    Color::White
                } else {
                    Color::DarkGray
                });
                let connector = if worktree_index == last_index {
                    "  └─ "
                } else {
                    "  ├─ "
                };
                items.push(ListItem::new(Line::from(vec![
                    Span::styled(connector, Style::default().fg(Color::DarkGray)),
                    Span::styled(format!("{head:<8} "), metadata_style),
                    Span::styled(format!("{age:<10} "), metadata_style),
                    Span::styled(
                        format!("{:<20} ", worktree.reference_label()),
                        Style::default().fg(Color::Cyan),
                    ),
                    Span::raw(worktree.path.display().to_string()),
                    Span::styled(marker, Style::default().fg(Color::Yellow)),
                ])));
            }
        }
        let list = List::new(items)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" Repositories and worktrees "),
            )
            .highlight_style(
                Style::default()
                    .bg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol("› ");
        let selected = if self.row_count() == 0 {
            None
        } else {
            Some(self.selected.min(self.row_count() - 1))
        };
        let mut state = ListState::default().with_selected(selected);
        frame.render_stateful_widget(list, chunks[0], &mut state);

        let help = self.message.as_deref().map_or_else(
            || {
                let filter_help = if self.recursive {
                    if self.hide_without_linked {
                        "   h show all"
                    } else {
                        "   h hide without linked"
                    }
                } else {
                    ""
                };
                format!(
                    "↑/↓ or j/k select   PgUp/PgDn page   l logs   s status   d delete   r refresh{filter_help}   q quit"
                )
            },
            |message| format!("Error: {message}"),
        );
        let style = if self.message.is_some() {
            Style::default().fg(Color::Red)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        frame.render_widget(
            Paragraph::new(help).style(style).wrap(Wrap { trim: true }),
            chunks[1],
        );
    }

    fn render_log(
        &self,
        frame: &mut Frame<'_>,
        area: Rect,
        worktree: &WorktreeInfo,
        entries: &[LogEntry],
        offset: usize,
    ) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(3), Constraint::Length(1)])
            .split(area);
        let lines = if entries.is_empty() {
            vec![Line::from("No commits reachable from HEAD")]
        } else {
            entries
                .iter()
                .map(|entry| {
                    Line::from(vec![
                        Span::styled(format!("{} ", entry.id), Style::default().fg(Color::Yellow)),
                        Span::raw(format!("{}  ", entry.subject)),
                        Span::styled(
                            format!("{} · {}", entry.author, relative_age(entry.committed_at)),
                            Style::default().fg(Color::DarkGray),
                        ),
                    ])
                })
                .collect()
        };
        let title = format!(" Log — {} ", worktree.path.display());
        let paragraph = Paragraph::new(lines)
            .block(Block::default().borders(Borders::ALL).title(title))
            .scroll((offset.min(u16::MAX as usize) as u16, 0));
        frame.render_widget(paragraph, chunks[0]);
        render_detail_help(frame, chunks[1]);
    }

    fn render_status(
        &self,
        frame: &mut Frame<'_>,
        area: Rect,
        worktree: &WorktreeInfo,
        status: &WorktreeStatus,
        offset: usize,
    ) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(3), Constraint::Length(1)])
            .split(area);
        let mut lines = vec![Line::from(vec![
            Span::styled("On ", Style::default().fg(Color::DarkGray)),
            Span::styled(&status.reference, Style::default().fg(Color::Cyan)),
        ])];
        if status.entries.is_empty() {
            lines.push(Line::from("Working tree clean"));
        } else {
            lines.extend(status.entries.iter().map(|entry| {
                let area = match entry.area {
                    StatusArea::Index => "staged",
                    StatusArea::Worktree => "worktree",
                };
                Line::from(vec![
                    Span::styled(format!("{area:<9}"), Style::default().fg(Color::DarkGray)),
                    Span::styled(
                        format!("{:<14}", entry.kind.label()),
                        Style::default().fg(Color::Yellow),
                    ),
                    Span::raw(&entry.path),
                ])
            }));
        }
        let title = format!(" Status — {} ", worktree.path.display());
        let paragraph = Paragraph::new(lines)
            .block(Block::default().borders(Borders::ALL).title(title))
            .scroll((offset.min(u16::MAX as usize) as u16, 0));
        frame.render_widget(paragraph, chunks[0]);
        render_detail_help(frame, chunks[1]);
    }

    fn render_confirmation(
        &self,
        frame: &mut Frame<'_>,
        area: Rect,
        worktree: &WorktreeInfo,
        assessment: &DeletionAssessment,
        phase: ConfirmationPhase,
        offset: usize,
    ) {
        let desired_height = assessment
            .changes
            .len()
            .saturating_add(8)
            .min(u16::MAX as usize) as u16;
        let popup = centered_rect(76, desired_height.min(area.height.saturating_sub(2)), area);
        frame.render_widget(ClearWidget, popup);
        let mut lines = vec![Line::from(worktree.path.display().to_string())];
        if assessment.locked {
            lines.push(Line::from(Span::styled(
                "This worktree is locked.",
                Style::default().fg(Color::Yellow),
            )));
        }
        if assessment.missing {
            lines.push(Line::from(
                "The checkout is missing; only stale metadata will be removed.",
            ));
        }
        if !assessment.changes.is_empty() {
            lines.push(Line::from(Span::styled(
                format!("Changes ({})", assessment.changes.len()),
                Style::default().add_modifier(Modifier::BOLD),
            )));
            let reserved_lines = lines.len().saturating_add(3);
            let visible_changes = usize::from(popup.height.saturating_sub(2))
                .saturating_sub(reserved_lines)
                .max(1);
            lines.extend(
                assessment
                    .changes
                    .iter()
                    .skip(offset)
                    .take(visible_changes)
                    .map(|entry| {
                        let area = match entry.area {
                            StatusArea::Index => "staged",
                            StatusArea::Worktree => "worktree",
                        };
                        Line::from(vec![
                            Span::styled(
                                format!("{area:<9}"),
                                Style::default().fg(Color::DarkGray),
                            ),
                            Span::styled(
                                format!("{:<14}", entry.kind.label()),
                                Style::default().fg(Color::Yellow),
                            ),
                            Span::raw(&entry.path),
                        ])
                    }),
            );
            if assessment.changes.len() > visible_changes {
                lines.push(Line::from(Span::styled(
                    format!(
                        "Showing {}–{} of {} · ↑/↓ or j/k scroll",
                        offset.saturating_add(1),
                        offset
                            .saturating_add(visible_changes)
                            .min(assessment.changes.len()),
                        assessment.changes.len()
                    ),
                    Style::default().fg(Color::DarkGray),
                )));
            }
        }
        match phase {
            ConfirmationPhase::Initial => {
                lines.push(Line::from("Delete this worktree? y/N"));
            }
            ConfirmationPhase::Force => {
                lines.push(Line::from(Span::styled(
                    "This permanently discards local worktree data.",
                    Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                )));
                lines.push(Line::from(
                    "Press uppercase D to force deletion; any other key cancels.",
                ));
            }
        }
        let paragraph = Paragraph::new(lines)
            .alignment(Alignment::Center)
            .wrap(Wrap { trim: true })
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" Confirm deletion "),
            );
        frame.render_widget(paragraph, popup);
    }

    fn render_deleting(&self, frame: &mut Frame<'_>, area: Rect, worktree: &WorktreeInfo) {
        let popup = centered_rect(76, 5, area);
        frame.render_widget(ClearWidget, popup);
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(worktree.path.display().to_string()),
                Line::from(Span::styled(
                    "Deleting...",
                    Style::default().add_modifier(Modifier::BOLD),
                )),
            ])
            .alignment(Alignment::Center)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" Delete worktree "),
            ),
            popup,
        );
    }

    fn handle_key(&mut self, key: KeyEvent) -> Result<bool> {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return Ok(true);
        }
        if self.mode == ExploreMode::Branches {
            return self.handle_branch_key(key);
        }
        let page_size = self.page_size;
        match &mut self.view {
            View::Worktrees => self.handle_worktree_key(key),
            View::Log {
                entries, offset, ..
            } => {
                handle_scroll_key(key, offset, entries.len(), page_size);
                if matches!(key.code, KeyCode::Esc | KeyCode::Char('q')) {
                    self.view = View::Worktrees;
                }
                Ok(false)
            }
            View::Status { status, offset, .. } => {
                handle_scroll_key(
                    key,
                    offset,
                    status.entries.len().saturating_add(1),
                    page_size,
                );
                if matches!(key.code, KeyCode::Esc | KeyCode::Char('q')) {
                    self.view = View::Worktrees;
                }
                Ok(false)
            }
            View::ConfirmDelete {
                assessment,
                phase,
                offset,
                ..
            } => {
                if matches!(
                    key.code,
                    KeyCode::Up
                        | KeyCode::Down
                        | KeyCode::Char('j')
                        | KeyCode::Char('k')
                        | KeyCode::PageUp
                        | KeyCode::PageDown
                ) {
                    handle_scroll_key(key, offset, assessment.changes.len(), page_size);
                    return Ok(false);
                }
                match phase {
                    ConfirmationPhase::Initial => self.handle_initial_confirmation(key),
                    ConfirmationPhase::Force => self.handle_force_confirmation(key),
                }
            }
            View::Deleting { .. } => Ok(false),
            _ => Ok(false),
        }
    }

    fn handle_branch_key(&mut self, key: KeyEvent) -> Result<bool> {
        let page_size = self.page_size;
        match &mut self.view {
            View::Worktrees => {
                self.message = None;
                match key.code {
                    KeyCode::Char('q') => return Ok(true),
                    KeyCode::Up | KeyCode::Char('k') => {
                        self.selected = self.selected.saturating_sub(1)
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        self.selected = self
                            .selected
                            .saturating_add(1)
                            .min(self.row_count().saturating_sub(1))
                    }
                    KeyCode::PageUp => self.selected = self.selected.saturating_sub(page_size),
                    KeyCode::PageDown => {
                        self.selected = self
                            .selected
                            .saturating_add(page_size)
                            .min(self.row_count().saturating_sub(1))
                    }
                    KeyCode::Char('h') if self.recursive => self.toggle_hide_without_linked(),
                    KeyCode::Char('r') => self.refresh(),
                    KeyCode::Char('l') => {
                        if let Some((repository_index, branch)) = self.selected_branch() {
                            match self.repositories[repository_index]
                                .repository
                                .log_branch(&branch.name)
                            {
                                Ok(entries) => {
                                    self.view = View::BranchLog {
                                        branch,
                                        entries,
                                        offset: 0,
                                    }
                                }
                                Err(error) => self.message = Some(format!("{error:#}")),
                            }
                        } else {
                            self.message = Some("select a branch, not a repository".to_owned());
                        }
                    }
                    KeyCode::Char('s') => {
                        if let Some((repository_index, branch)) = self.selected_branch() {
                            match self.repositories[repository_index]
                                .repository
                                .branch_status(&branch.name)
                            {
                                Ok(status) => self.view = View::BranchStatus { status, offset: 0 },
                                Err(error) => self.message = Some(format!("{error:#}")),
                            }
                        } else {
                            self.message = Some("select a branch, not a repository".to_owned());
                        }
                    }
                    KeyCode::Char('d') => {
                        if let Some((repository_index, branch)) = self.selected_branch() {
                            match self.repositories[repository_index]
                                .repository
                                .assess_branch_deletion(&branch.name)
                            {
                                Ok(assessment) if assessment.checked_out_paths.is_empty() => {
                                    self.view = View::ConfirmBranchDelete {
                                        repository_index,
                                        branch,
                                        assessment,
                                        phase: ConfirmationPhase::Initial,
                                    }
                                }
                                Ok(assessment) => {
                                    self.message = Some(format!(
                                        "branch is checked out in {}",
                                        assessment
                                            .checked_out_paths
                                            .iter()
                                            .map(|path| path.display().to_string())
                                            .collect::<Vec<_>>()
                                            .join(", ")
                                    ))
                                }
                                Err(error) => self.message = Some(format!("{error:#}")),
                            }
                        } else {
                            self.message = Some("select a branch, not a repository".to_owned());
                        }
                    }
                    _ => {}
                }
                Ok(false)
            }
            View::BranchLog {
                entries, offset, ..
            } => {
                handle_scroll_key(key, offset, entries.len(), page_size);
                if matches!(key.code, KeyCode::Esc | KeyCode::Char('q')) {
                    self.view = View::Worktrees;
                }
                Ok(false)
            }
            View::BranchStatus { status, offset } => {
                handle_scroll_key(key, offset, status_line_count(status), page_size);
                if matches!(key.code, KeyCode::Esc | KeyCode::Char('q')) {
                    self.view = View::Worktrees;
                }
                Ok(false)
            }
            View::ConfirmBranchDelete { phase, .. } => {
                match phase {
                    ConfirmationPhase::Initial if key.code == KeyCode::Char('y') => {
                        let (repository_index, branch) = match &self.view {
                            View::ConfirmBranchDelete {
                                repository_index,
                                branch,
                                ..
                            } => (*repository_index, branch.clone()),
                            _ => return Err(anyhow::anyhow!("invalid state")),
                        };
                        self.view = View::DeletingBranch {
                            repository_index,
                            branch,
                            mode: DeletionMode::Normal,
                        };
                    }
                    ConfirmationPhase::Force if key.code == KeyCode::Char('D') => {
                        let (repository_index, branch) = match &self.view {
                            View::ConfirmBranchDelete {
                                repository_index,
                                branch,
                                ..
                            } => (*repository_index, branch.clone()),
                            _ => return Err(anyhow::anyhow!("invalid state")),
                        };
                        self.view = View::DeletingBranch {
                            repository_index,
                            branch,
                            mode: DeletionMode::Force,
                        };
                    }
                    _ => self.view = View::Worktrees,
                }
                Ok(false)
            }
            View::DeletingBranch { .. } => Ok(false),
            _ => Ok(false),
        }
    }

    fn handle_worktree_key(&mut self, key: KeyEvent) -> Result<bool> {
        self.message = None;
        match key.code {
            KeyCode::Char('q') => return Ok(true),
            KeyCode::Up | KeyCode::Char('k') => {
                self.selected = self.selected.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if self.selected + 1 < self.row_count() {
                    self.selected += 1;
                }
            }
            KeyCode::PageUp => {
                self.selected = self.selected.saturating_sub(self.page_size);
            }
            KeyCode::PageDown => {
                self.selected = self
                    .selected
                    .saturating_add(self.page_size)
                    .min(self.row_count().saturating_sub(1));
            }
            KeyCode::Char('h') if self.recursive => self.toggle_hide_without_linked(),
            KeyCode::Char('r') => self.refresh(),
            KeyCode::Char('l') => {
                if let Some((repository_index, worktree)) = self.selected_worktree() {
                    match self.repositories[repository_index]
                        .repository
                        .log(&worktree.key)
                    {
                        Ok(entries) => {
                            self.view = View::Log {
                                worktree,
                                entries,
                                offset: 0,
                            };
                        }
                        Err(error) => self.message = Some(format!("{error:#}")),
                    }
                } else {
                    self.message = Some("select a worktree, not a repository".to_owned());
                }
            }
            KeyCode::Char('s') => {
                if let Some((repository_index, worktree)) = self.selected_worktree() {
                    match self.repositories[repository_index]
                        .repository
                        .status(&worktree.key)
                    {
                        Ok(status) => {
                            self.view = View::Status {
                                worktree,
                                status,
                                offset: 0,
                            };
                        }
                        Err(error) => self.message = Some(format!("{error:#}")),
                    }
                } else {
                    self.message = Some("select a worktree, not a repository".to_owned());
                }
            }
            KeyCode::Char('d') => {
                if let Some((repository_index, worktree)) = self.selected_worktree() {
                    match self.repositories[repository_index]
                        .repository
                        .assess_deletion(&worktree.key)
                    {
                        Ok(assessment) => {
                            self.view = View::ConfirmDelete {
                                repository_index,
                                worktree,
                                assessment,
                                phase: ConfirmationPhase::Initial,
                                offset: 0,
                            };
                        }
                        Err(error) => self.message = Some(format!("{error:#}")),
                    }
                } else {
                    self.message = Some("select a worktree, not a repository".to_owned());
                }
            }
            _ => {}
        }
        Ok(false)
    }

    fn handle_initial_confirmation(&mut self, key: KeyEvent) -> Result<bool> {
        if key.code != KeyCode::Char('y') {
            self.view = View::Worktrees;
            return Ok(false);
        }
        let (repository_index, worktree) = match &self.view {
            View::ConfirmDelete {
                repository_index,
                worktree,
                ..
            } => (*repository_index, worktree.clone()),
            _ => return Ok(false),
        };
        self.view = View::Deleting {
            repository_index,
            worktree,
            mode: DeletionMode::Normal,
        };
        Ok(false)
    }

    fn handle_force_confirmation(&mut self, key: KeyEvent) -> Result<bool> {
        if key.code != KeyCode::Char('D') {
            self.view = View::Worktrees;
            return Ok(false);
        }
        let (repository_index, worktree) = match &self.view {
            View::ConfirmDelete {
                repository_index,
                worktree,
                ..
            } => (*repository_index, worktree.clone()),
            _ => return Ok(false),
        };
        self.view = View::Deleting {
            repository_index,
            worktree,
            mode: DeletionMode::Force,
        };
        Ok(false)
    }

    fn process_pending_deletion(&mut self) -> bool {
        let (repository_index, worktree, mode) = match &self.view {
            View::Deleting {
                repository_index,
                worktree,
                mode,
            } => (*repository_index, worktree.clone(), *mode),
            _ => return false,
        };
        match self.repositories[repository_index]
            .repository
            .delete_worktree(&worktree.key, mode.force())
        {
            Ok(DeleteResult::Deleted) => {
                self.message = None;
                self.view = View::Worktrees;
                self.refresh();
            }
            Ok(DeleteResult::NeedsForce(assessment)) => match mode {
                DeletionMode::Normal => {
                    self.view = View::ConfirmDelete {
                        repository_index,
                        worktree,
                        assessment,
                        phase: ConfirmationPhase::Force,
                        offset: 0,
                    };
                }
                DeletionMode::Force => {
                    self.message =
                        Some("worktree state changed; deletion was not performed".to_owned());
                    self.view = View::Worktrees;
                }
            },
            Err(error) => {
                self.message = Some(format!("{error:#}"));
                self.view = View::Worktrees;
                self.refresh_preserving_message();
            }
        }
        true
    }

    fn process_pending_branch_deletion(&mut self) -> bool {
        let (repository_index, branch, mode) = match &self.view {
            View::DeletingBranch {
                repository_index,
                branch,
                mode,
            } => (*repository_index, branch.clone(), *mode),
            _ => return false,
        };
        match self.repositories[repository_index]
            .repository
            .delete_branch(&branch.name, mode.force())
        {
            Ok(BranchDeleteResult::Deleted) => {
                self.message = None;
                self.view = View::Worktrees;
                self.refresh();
            }
            Ok(BranchDeleteResult::NeedsForce(assessment)) => match mode {
                DeletionMode::Normal => {
                    self.view = View::ConfirmBranchDelete {
                        repository_index,
                        branch,
                        assessment,
                        phase: ConfirmationPhase::Force,
                    }
                }
                DeletionMode::Force => {
                    self.message =
                        Some("branch state changed; deletion was not performed".to_owned());
                    self.view = View::Worktrees;
                }
            },
            Err(error) => {
                self.message = Some(format!("{error:#}"));
                self.view = View::Worktrees;
                self.refresh_preserving_message();
            }
        }
        true
    }

    fn row_count(&self) -> usize {
        self.repositories
            .iter()
            .filter(|group| self.group_is_visible(group))
            .map(|group| {
                let count = match self.mode {
                    ExploreMode::Worktrees => group.worktrees.len(),
                    ExploreMode::Branches => group.branches.len(),
                };
                count.saturating_add(1)
            })
            .sum()
    }

    fn selected_branch(&self) -> Option<(usize, BranchInfo)> {
        let mut row = 0;
        for (repository_index, group) in self.repositories.iter().enumerate() {
            if !self.group_is_visible(group) {
                continue;
            }
            if self.selected == row {
                return None;
            }
            row += 1;
            if self.selected < row + group.branches.len() {
                return Some((
                    repository_index,
                    group.branches[self.selected - row].clone(),
                ));
            }
            row += group.branches.len();
        }
        None
    }

    fn selected_worktree(&self) -> Option<(usize, WorktreeInfo)> {
        let mut row = 0;
        for (repository_index, group) in self.repositories.iter().enumerate() {
            if !self.group_is_visible(group) {
                continue;
            }
            if self.selected == row {
                return None;
            }
            row += 1;
            if self.selected < row + group.worktrees.len() {
                return Some((
                    repository_index,
                    group.worktrees[self.selected - row].clone(),
                ));
            }
            row += group.worktrees.len();
        }
        None
    }

    fn selected_identity(&self) -> Option<TreeIdentity> {
        if self.mode == ExploreMode::Branches {
            let mut row = 0;
            for group in self
                .repositories
                .iter()
                .filter(|group| self.group_is_visible(group))
            {
                if self.selected == row {
                    return Some(TreeIdentity::Repository(
                        group.repository.root().to_path_buf(),
                    ));
                }
                row += 1;
                if self.selected < row + group.branches.len() {
                    return Some(TreeIdentity::Branch(
                        group.repository.root().to_path_buf(),
                        group.branches[self.selected - row].name.clone(),
                    ));
                }
                row += group.branches.len();
            }
            return None;
        }
        let mut row = 0;
        for group in self
            .repositories
            .iter()
            .filter(|group| self.group_is_visible(group))
        {
            if self.selected == row {
                return Some(TreeIdentity::Repository(
                    group.repository.root().to_path_buf(),
                ));
            }
            row += 1;
            if self.selected < row + group.worktrees.len() {
                return Some(TreeIdentity::Worktree(
                    group.repository.root().to_path_buf(),
                    group.worktrees[self.selected - row].key.clone(),
                ));
            }
            row += group.worktrees.len();
        }
        None
    }

    fn row_for_identity(&self, identity: &TreeIdentity) -> Option<usize> {
        if self.mode == ExploreMode::Branches {
            let mut row = 0;
            for group in self
                .repositories
                .iter()
                .filter(|group| self.group_is_visible(group))
            {
                let root = group.repository.root();
                if identity == &TreeIdentity::Repository(root.to_path_buf()) {
                    return Some(row);
                }
                row += 1;
                for branch in &group.branches {
                    if identity == &TreeIdentity::Branch(root.to_path_buf(), branch.name.clone()) {
                        return Some(row);
                    }
                    row += 1;
                }
            }
            return None;
        }
        let mut row = 0;
        for group in self
            .repositories
            .iter()
            .filter(|group| self.group_is_visible(group))
        {
            let root = group.repository.root();
            if identity == &TreeIdentity::Repository(root.to_path_buf()) {
                return Some(row);
            }
            row += 1;
            for worktree in &group.worktrees {
                if identity == &TreeIdentity::Worktree(root.to_path_buf(), worktree.key.clone()) {
                    return Some(row);
                }
                row += 1;
            }
        }
        None
    }

    fn refresh(&mut self) {
        self.message = None;
        self.refresh_preserving_message();
    }

    fn refresh_preserving_message(&mut self) {
        let selected_identity = self.selected_identity();
        for group in &mut self.repositories {
            match group.repository.list_worktrees() {
                Ok(worktrees) => group.worktrees = worktrees,
                Err(error) => {
                    self.message = Some(format!("{error:#}"));
                    return;
                }
            }
            if self.mode == ExploreMode::Branches {
                match group.repository.list_branches() {
                    Ok(branches) => group.branches = branches,
                    Err(error) => {
                        self.message = Some(format!("{error:#}"));
                        return;
                    }
                }
            }
        }
        self.selected = selected_identity
            .as_ref()
            .and_then(|identity| self.row_for_identity(identity))
            .unwrap_or_else(|| self.selected.min(self.row_count().saturating_sub(1)));
    }

    fn group_is_visible(&self, group: &RepositoryGroup) -> bool {
        if !self.recursive || !self.hide_without_linked {
            return true;
        }
        match self.mode {
            ExploreMode::Worktrees => group
                .worktrees
                .iter()
                .any(|worktree| matches!(worktree.key, WorktreeKey::Linked(_))),
            ExploreMode::Branches => !group.branches.is_empty(),
        }
    }

    fn toggle_hide_without_linked(&mut self) {
        let selected_identity = self.selected_identity();
        self.hide_without_linked = !self.hide_without_linked;
        self.selected = selected_identity
            .as_ref()
            .and_then(|identity| self.row_for_identity(identity))
            .unwrap_or_else(|| usize::from(self.row_count() > 1));
    }
}

fn handle_scroll_key(key: KeyEvent, offset: &mut usize, len: usize, page_size: usize) {
    match key.code {
        KeyCode::Up | KeyCode::Char('k') => *offset = offset.saturating_sub(1),
        KeyCode::Down | KeyCode::Char('j') => {
            *offset = offset.saturating_add(1).min(len.saturating_sub(1));
        }
        KeyCode::PageUp => *offset = offset.saturating_sub(page_size),
        KeyCode::PageDown => {
            *offset = offset.saturating_add(page_size).min(len.saturating_sub(1));
        }
        KeyCode::Home => *offset = 0,
        KeyCode::End => *offset = len.saturating_sub(1),
        _ => {}
    }
}

fn relative_age(timestamp: i64) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_secs() as i64;
    let seconds = now.saturating_sub(timestamp).max(0) as u64;
    match seconds {
        0..=59 => format!("{seconds}s ago"),
        60..=3_599 => format!("{}m ago", seconds / 60),
        3_600..=86_399 => format!("{}h ago", seconds / 3_600),
        86_400..=2_592_000 => format!("{}d ago", seconds / 86_400),
        2_592_001..=31_536_000 => format!("{}mo ago", seconds / 2_592_000),
        _ => format!("{}y ago", seconds / 31_536_000),
    }
}

fn centered_rect(percent_x: u16, height: u16, area: Rect) -> Rect {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Fill(1),
            Constraint::Length(height.min(area.height)),
            Constraint::Fill(1),
        ])
        .split(area);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(vertical[1])[1]
}

fn render_detail_help(frame: &mut Frame<'_>, area: Rect) {
    frame.render_widget(
        Paragraph::new("↑/↓ or j/k scroll   PgUp/PgDn page   Esc/q back")
            .alignment(Alignment::Center)
            .style(Style::default().fg(Color::DarkGray)),
        area,
    );
}

fn append_change_count(lines: &mut Vec<Line<'static>>, counts: ChangeCounts) {
    if counts.staged > 0 {
        lines.push(Line::from(format!("{} staged file(s)", counts.staged)));
    }
    if counts.unstaged > 0 {
        lines.push(Line::from(format!("{} unstaged file(s)", counts.unstaged)));
    }
    if counts.untracked > 0 {
        lines.push(Line::from(format!(
            "{} untracked file(s)",
            counts.untracked
        )));
    }
    if counts.deleted > 0 {
        lines.push(Line::from(format!("{} deleted file(s)", counts.deleted)));
    }
    if counts.conflicted > 0 {
        lines.push(Line::from(format!(
            "{} conflicted file(s)",
            counts.conflicted
        )));
    }
    if counts == ChangeCounts::default() {
        lines.push(Line::from("Working tree clean"));
    }
}

fn status_line_count(status: &BranchStatus) -> usize {
    let mut count = 2;
    if status.ahead > 0 {
        count += 1;
    }
    if status.behind > 0 {
        count += 1;
    }
    if status.checked_out_paths.is_empty() {
        count += 1;
    } else {
        count += status.checked_out_paths.len();
    }
    count
        + usize::from(status.changes.staged > 0)
        + usize::from(status.changes.unstaged > 0)
        + usize::from(status.changes.untracked > 0)
        + usize::from(status.changes.deleted > 0)
        + usize::from(status.changes.conflicted > 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use tempfile::TempDir;

    fn repository_manager() -> (TempDir, RepositoryManager) {
        let directory = tempfile::tempdir().expect("temporary directory should be created");
        gix::init(directory.path()).expect("repository should initialize");
        let repository =
            RepositoryManager::discover(directory.path()).expect("repository should be discovered");
        (directory, repository)
    }

    #[test]
    fn scroll_stays_within_content() {
        let mut offset = 0;
        handle_scroll_key(
            KeyEvent::new(KeyCode::Down, KeyModifiers::NONE),
            &mut offset,
            2,
            10,
        );
        assert_eq!(offset, 1);
        handle_scroll_key(
            KeyEvent::new(KeyCode::Down, KeyModifiers::NONE),
            &mut offset,
            2,
            10,
        );
        assert_eq!(offset, 1);
        handle_scroll_key(
            KeyEvent::new(KeyCode::Up, KeyModifiers::NONE),
            &mut offset,
            2,
            10,
        );
        assert_eq!(offset, 0);
    }

    #[test]
    fn page_scroll_moves_by_the_visible_page_size() {
        let mut offset = 2;
        handle_scroll_key(
            KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE),
            &mut offset,
            20,
            5,
        );
        assert_eq!(offset, 7);
        handle_scroll_key(
            KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE),
            &mut offset,
            20,
            5,
        );
        assert_eq!(offset, 2);
    }

    #[test]
    fn age_format_uses_largest_relevant_unit() {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock must be after the Unix epoch")
            .as_secs() as i64;
        assert_eq!(relative_age(now - 90), "1m ago");
        assert_eq!(relative_age(now - 7_200), "2h ago");
    }

    #[test]
    fn repository_and_worktree_are_separate_tree_rows() {
        let (_directory, repository) = repository_manager();
        let app = App::new(vec![repository], false, false).expect("application should initialize");

        assert_eq!(app.row_count(), 2);
        assert_eq!(app.selected, 1);
        assert!(app.selected_worktree().is_some());
        assert!(matches!(
            app.selected_identity(),
            Some(TreeIdentity::Worktree(_, WorktreeKey::Main))
        ));
    }

    #[test]
    fn recursive_filter_keeps_only_repositories_with_linked_worktrees() {
        let (_first_directory, first_repository) = repository_manager();
        let (_second_directory, second_repository) = repository_manager();
        let mut app = App::new(vec![first_repository, second_repository], true, false)
            .expect("application should initialize");
        app.repositories[1].worktrees.push(WorktreeInfo {
            key: WorktreeKey::Linked("feature".to_owned()),
            path: std::path::PathBuf::from("feature"),
            head: None,
            committed_at: None,
            branch: Some("feature".to_owned()),
            locked: false,
            available: true,
        });

        assert_eq!(app.row_count(), 5);
        app.toggle_hide_without_linked();

        assert!(app.hide_without_linked);
        assert_eq!(app.row_count(), 3);
        assert_eq!(app.selected, 1);
        assert!(matches!(
            app.selected_identity(),
            Some(TreeIdentity::Worktree(_, WorktreeKey::Main))
        ));
    }

    #[test]
    fn confirmed_deletion_renders_progress_before_processing() {
        let (_directory, repository) = repository_manager();
        let mut app =
            App::new(vec![repository], false, false).expect("application should initialize");
        let worktree = app.repositories[0].worktrees[0].clone();
        app.view = View::ConfirmDelete {
            repository_index: 0,
            worktree: worktree.clone(),
            assessment: DeletionAssessment {
                changes: Vec::new(),
                locked: false,
                missing: false,
            },
            phase: ConfirmationPhase::Initial,
            offset: 0,
        };

        app.handle_initial_confirmation(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE))
            .expect("confirmation should be handled");
        assert!(matches!(
            app.view,
            View::Deleting {
                mode: DeletionMode::Normal,
                ..
            }
        ));

        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).expect("test terminal should initialize");
        terminal
            .draw(|frame| app.render(frame))
            .expect("deletion progress should render");
        let contents = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();

        assert!(contents.contains("Deleting..."));
        assert!(!contents.contains("Delete this worktree?"));
    }

    #[test]
    fn force_confirmation_enters_force_deletion_progress() {
        let (_directory, repository) = repository_manager();
        let mut app =
            App::new(vec![repository], false, false).expect("application should initialize");
        let worktree = app.repositories[0].worktrees[0].clone();
        app.view = View::ConfirmDelete {
            repository_index: 0,
            worktree,
            assessment: DeletionAssessment {
                changes: Vec::new(),
                locked: true,
                missing: false,
            },
            phase: ConfirmationPhase::Force,
            offset: 0,
        };

        app.handle_force_confirmation(KeyEvent::new(KeyCode::Char('D'), KeyModifiers::SHIFT))
            .expect("force confirmation should be handled");

        assert!(matches!(
            app.view,
            View::Deleting {
                mode: DeletionMode::Force,
                ..
            }
        ));
    }
}
