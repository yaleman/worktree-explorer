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
    DeleteResult, DeletionAssessment, LogEntry, RepositoryManager, StatusArea, WorktreeInfo,
    WorktreeKey, WorktreeStatus,
};

pub fn run(repository: RepositoryManager) -> Result<()> {
    let _guard = TerminalGuard::enter()?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))
        .context("failed to initialize the terminal")?;
    let mut app = App::new(repository)?;

    loop {
        terminal
            .draw(|frame| app.render(frame))
            .context("failed to draw the interface")?;
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
        worktree: WorktreeInfo,
        assessment: DeletionAssessment,
        phase: ConfirmationPhase,
        offset: usize,
    },
}

struct App {
    repository: RepositoryManager,
    worktrees: Vec<WorktreeInfo>,
    selected: usize,
    view: View,
    message: Option<String>,
}

impl App {
    fn new(repository: RepositoryManager) -> Result<Self> {
        let worktrees = repository.list_worktrees()?;
        Ok(Self {
            repository,
            worktrees,
            selected: 0,
            view: View::Worktrees,
            message: None,
        })
    }

    fn render(&self, frame: &mut Frame<'_>) {
        let area = frame.area();
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
                worktree,
                assessment,
                phase,
                offset,
            } => {
                self.render_worktrees(frame, area);
                self.render_confirmation(frame, area, worktree, assessment, *phase, *offset);
            }
        }
    }

    fn render_worktrees(&self, frame: &mut Frame<'_>, area: Rect) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(3), Constraint::Length(2)])
            .split(area);
        let items = self.worktrees.iter().enumerate().map(|(index, worktree)| {
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
            let metadata_style = Style::default().fg(if index == self.selected {
                Color::White
            } else {
                Color::DarkGray
            });
            ListItem::new(Line::from(vec![
                Span::styled(format!("{head:<8} "), metadata_style),
                Span::styled(format!("{age:<10} "), metadata_style),
                Span::styled(
                    format!("{:<20} ", worktree.reference_label()),
                    Style::default().fg(Color::Cyan),
                ),
                Span::raw(worktree.path.display().to_string()),
                Span::styled(marker, Style::default().fg(Color::Yellow)),
            ]))
        });
        let list = List::new(items)
            .block(Block::default().borders(Borders::ALL).title(" Worktrees "))
            .highlight_style(
                Style::default()
                    .bg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol("› ");
        let selected = if self.worktrees.is_empty() {
            None
        } else {
            Some(self.selected.min(self.worktrees.len() - 1))
        };
        let mut state = ListState::default().with_selected(selected);
        frame.render_stateful_widget(list, chunks[0], &mut state);

        let help = self.message.as_deref().map_or_else(
            || "↑/↓ or j/k select   l logs   s status   d delete   r refresh   q quit".to_owned(),
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

    fn handle_key(&mut self, key: KeyEvent) -> Result<bool> {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return Ok(true);
        }
        match &mut self.view {
            View::Worktrees => self.handle_worktree_key(key),
            View::Log {
                entries, offset, ..
            } => {
                handle_scroll_key(key, offset, entries.len());
                if matches!(key.code, KeyCode::Esc | KeyCode::Char('q')) {
                    self.view = View::Worktrees;
                }
                Ok(false)
            }
            View::Status { status, offset, .. } => {
                handle_scroll_key(key, offset, status.entries.len().saturating_add(1));
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
                    KeyCode::Up | KeyCode::Down | KeyCode::Char('j') | KeyCode::Char('k')
                ) {
                    handle_scroll_key(key, offset, assessment.changes.len());
                    return Ok(false);
                }
                match phase {
                    ConfirmationPhase::Initial => self.handle_initial_confirmation(key),
                    ConfirmationPhase::Force => self.handle_force_confirmation(key),
                }
            }
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
                if self.selected + 1 < self.worktrees.len() {
                    self.selected += 1;
                }
            }
            KeyCode::Char('r') => self.refresh(),
            KeyCode::Char('l') => {
                if let Some(worktree) = self.selected_worktree().cloned() {
                    match self.repository.log(&worktree.key) {
                        Ok(entries) => {
                            self.view = View::Log {
                                worktree,
                                entries,
                                offset: 0,
                            };
                        }
                        Err(error) => self.message = Some(format!("{error:#}")),
                    }
                }
            }
            KeyCode::Char('s') => {
                if let Some(worktree) = self.selected_worktree().cloned() {
                    match self.repository.status(&worktree.key) {
                        Ok(status) => {
                            self.view = View::Status {
                                worktree,
                                status,
                                offset: 0,
                            };
                        }
                        Err(error) => self.message = Some(format!("{error:#}")),
                    }
                }
            }
            KeyCode::Char('d') => {
                if let Some(worktree) = self.selected_worktree().cloned() {
                    match self.repository.assess_deletion(&worktree.key) {
                        Ok(assessment) => {
                            self.view = View::ConfirmDelete {
                                worktree,
                                assessment,
                                phase: ConfirmationPhase::Initial,
                                offset: 0,
                            };
                        }
                        Err(error) => self.message = Some(format!("{error:#}")),
                    }
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
        let worktree = match &self.view {
            View::ConfirmDelete { worktree, .. } => worktree.clone(),
            _ => return Ok(false),
        };
        match self.repository.delete_worktree(&worktree.key, false) {
            Ok(DeleteResult::Deleted) => {
                self.message = None;
                self.view = View::Worktrees;
                self.refresh();
            }
            Ok(DeleteResult::NeedsForce(assessment)) => {
                self.view = View::ConfirmDelete {
                    worktree,
                    assessment,
                    phase: ConfirmationPhase::Force,
                    offset: 0,
                };
            }
            Err(error) => {
                self.message = Some(format!("{error:#}"));
                self.view = View::Worktrees;
                self.refresh_preserving_message();
            }
        }
        Ok(false)
    }

    fn handle_force_confirmation(&mut self, key: KeyEvent) -> Result<bool> {
        if key.code != KeyCode::Char('D') {
            self.view = View::Worktrees;
            return Ok(false);
        }
        let worktree = match &self.view {
            View::ConfirmDelete { worktree, .. } => worktree.clone(),
            _ => return Ok(false),
        };
        match self.repository.delete_worktree(&worktree.key, true) {
            Ok(DeleteResult::Deleted) => {
                self.message = None;
                self.view = View::Worktrees;
                self.refresh();
            }
            Ok(DeleteResult::NeedsForce(_)) => {
                self.message =
                    Some("worktree state changed; deletion was not performed".to_owned());
                self.view = View::Worktrees;
            }
            Err(error) => {
                self.message = Some(format!("{error:#}"));
                self.view = View::Worktrees;
                self.refresh_preserving_message();
            }
        }
        Ok(false)
    }

    fn selected_worktree(&self) -> Option<&WorktreeInfo> {
        self.worktrees.get(self.selected)
    }

    fn refresh(&mut self) {
        self.message = None;
        self.refresh_preserving_message();
    }

    fn refresh_preserving_message(&mut self) {
        let selected_key = self
            .selected_worktree()
            .map(|worktree| worktree.key.clone());
        match self.repository.list_worktrees() {
            Ok(worktrees) => {
                self.worktrees = worktrees;
                self.selected = selected_key
                    .and_then(|key| {
                        self.worktrees
                            .iter()
                            .position(|worktree| worktree.key == key)
                    })
                    .unwrap_or_else(|| self.selected.min(self.worktrees.len().saturating_sub(1)));
            }
            Err(error) => self.message = Some(format!("{error:#}")),
        }
    }
}

fn handle_scroll_key(key: KeyEvent, offset: &mut usize, len: usize) {
    match key.code {
        KeyCode::Up | KeyCode::Char('k') => *offset = offset.saturating_sub(1),
        KeyCode::Down | KeyCode::Char('j') => {
            *offset = offset.saturating_add(1).min(len.saturating_sub(1));
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
        Paragraph::new("↑/↓ or j/k scroll   Esc/q back")
            .alignment(Alignment::Center)
            .style(Style::default().fg(Color::DarkGray)),
        area,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scroll_stays_within_content() {
        let mut offset = 0;
        handle_scroll_key(
            KeyEvent::new(KeyCode::Down, KeyModifiers::NONE),
            &mut offset,
            2,
        );
        assert_eq!(offset, 1);
        handle_scroll_key(
            KeyEvent::new(KeyCode::Down, KeyModifiers::NONE),
            &mut offset,
            2,
        );
        assert_eq!(offset, 1);
        handle_scroll_key(
            KeyEvent::new(KeyCode::Up, KeyModifiers::NONE),
            &mut offset,
            2,
        );
        assert_eq!(offset, 0);
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
}
