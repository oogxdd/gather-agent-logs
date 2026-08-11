use std::{
    io::{self, Stdout},
    time::Duration,
};

use anyhow::Result;
use chrono::Utc;
use crossterm::{
    event::{self, Event, KeyCode, KeyEvent, KeyModifiers},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use nucleo_matcher::{
    Config, Matcher, Utf32Str,
    pattern::{CaseMatching, Normalization, Pattern},
};
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState, Wrap},
};

use crate::model::{Agent, Session};

pub fn pick(sessions: Vec<Session>, skipped: usize) -> Result<Option<Session>> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    if let Err(error) = execute!(stdout, EnterAlternateScreen) {
        let _ = disable_raw_mode();
        return Err(error.into());
    }
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = match Terminal::new(backend) {
        Ok(terminal) => terminal,
        Err(error) => {
            let _ = disable_raw_mode();
            let _ = execute!(io::stdout(), LeaveAlternateScreen);
            return Err(error.into());
        }
    };

    let result = run_picker(&mut terminal, sessions, skipped);

    let raw_mode_result = disable_raw_mode();
    let screen_result = execute!(terminal.backend_mut(), LeaveAlternateScreen);
    let cursor_result = terminal.show_cursor();
    raw_mode_result?;
    screen_result?;
    cursor_result?;

    result
}

fn run_picker(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    sessions: Vec<Session>,
    skipped: usize,
) -> Result<Option<Session>> {
    let mut app = App::new(sessions, skipped);

    loop {
        terminal.draw(|frame| render(frame, &mut app))?;
        if !event::poll(Duration::from_millis(250))? {
            continue;
        }
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return Ok(None);
        }

        match app.handle_key(key) {
            Action::Continue => {}
            Action::Quit => return Ok(None),
            Action::Resume => return Ok(app.selected().cloned()),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Mode {
    Browse,
    Search,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Focus {
    Sessions,
    Details,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Action {
    Continue,
    Quit,
    Resume,
}

struct App {
    sessions: Vec<Session>,
    filtered: Vec<usize>,
    searchable: Vec<String>,
    query: String,
    mode: Mode,
    focus: Focus,
    pending_g: bool,
    selected_position: Option<usize>,
    scroll_offset: usize,
    list_viewport_height: usize,
    details_scroll: u16,
    details_max_scroll: u16,
    skipped: usize,
}

impl App {
    fn new(sessions: Vec<Session>, skipped: usize) -> Self {
        let searchable = sessions.iter().map(Session::searchable_text).collect();
        let filtered = (0..sessions.len()).collect();
        let selected_position = (!sessions.is_empty()).then_some(0);
        Self {
            sessions,
            filtered,
            searchable,
            query: String::new(),
            mode: Mode::Browse,
            focus: Focus::Sessions,
            pending_g: false,
            selected_position,
            scroll_offset: 0,
            list_viewport_height: 10,
            details_scroll: 0,
            details_max_scroll: 0,
            skipped,
        }
    }

    fn selected(&self) -> Option<&Session> {
        let position = self.selected_position?;
        self.filtered
            .get(position)
            .and_then(|index| self.sessions.get(*index))
    }

    fn handle_key(&mut self, key: KeyEvent) -> Action {
        match self.mode {
            Mode::Search => self.handle_search_key(key),
            Mode::Browse => self.handle_browse_key(key),
        }
    }

    fn handle_search_key(&mut self, key: KeyEvent) -> Action {
        match key.code {
            KeyCode::Esc => {
                self.query.clear();
                self.mode = Mode::Browse;
                self.refilter();
            }
            KeyCode::Enter if !self.filtered.is_empty() => return Action::Resume,
            KeyCode::Tab | KeyCode::BackTab => {
                self.mode = Mode::Browse;
                self.focus = Focus::Sessions;
            }
            KeyCode::Backspace => {
                self.query.pop();
                self.refilter();
            }
            KeyCode::Char(character)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                self.query.push(character);
                self.refilter();
            }
            _ => {}
        }
        Action::Continue
    }

    fn handle_browse_key(&mut self, key: KeyEvent) -> Action {
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            let action = match key.code {
                KeyCode::Char('w') => {
                    self.toggle_focus();
                    Some(Action::Continue)
                }
                KeyCode::Char('d') => {
                    self.scroll_focused(self.half_page_step());
                    Some(Action::Continue)
                }
                KeyCode::Char('u') => {
                    self.scroll_focused(-self.half_page_step());
                    Some(Action::Continue)
                }
                KeyCode::Char('f') => {
                    self.scroll_focused(self.page_step());
                    Some(Action::Continue)
                }
                KeyCode::Char('b') => {
                    self.scroll_focused(-self.page_step());
                    Some(Action::Continue)
                }
                _ => None,
            };
            if let Some(action) = action {
                self.pending_g = false;
                return action;
            }
        }

        if key.code == KeyCode::Char('g') && key.modifiers.is_empty() {
            if self.pending_g {
                self.pending_g = false;
                self.jump_to_start();
            } else {
                self.pending_g = true;
            }
            return Action::Continue;
        }
        self.pending_g = false;

        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => Action::Quit,
            KeyCode::Enter if !self.filtered.is_empty() => Action::Resume,
            KeyCode::Char('/') => {
                self.mode = Mode::Search;
                Action::Continue
            }
            KeyCode::Char('c') if !self.query.is_empty() => {
                self.query.clear();
                self.refilter();
                Action::Continue
            }
            KeyCode::Tab | KeyCode::BackTab => {
                self.toggle_focus();
                Action::Continue
            }
            KeyCode::Left | KeyCode::Char('h') => {
                self.focus = Focus::Sessions;
                Action::Continue
            }
            KeyCode::Right | KeyCode::Char('l') => {
                self.focus = Focus::Details;
                Action::Continue
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.scroll_focused(-1);
                Action::Continue
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.scroll_focused(1);
                Action::Continue
            }
            KeyCode::PageUp => {
                self.scroll_focused(-self.page_step());
                Action::Continue
            }
            KeyCode::PageDown => {
                self.scroll_focused(self.page_step());
                Action::Continue
            }
            KeyCode::Home => {
                self.jump_to_start();
                Action::Continue
            }
            KeyCode::End | KeyCode::Char('G') => {
                self.jump_to_end();
                Action::Continue
            }
            KeyCode::Char('H') if self.focus == Focus::Sessions => {
                self.select_position(self.scroll_offset);
                Action::Continue
            }
            KeyCode::Char('M') if self.focus == Focus::Sessions => {
                self.select_position(
                    self.scroll_offset
                        .saturating_add(self.list_viewport_height / 2),
                );
                Action::Continue
            }
            KeyCode::Char('L') if self.focus == Focus::Sessions => {
                self.select_position(
                    self.scroll_offset
                        .saturating_add(self.list_viewport_height.saturating_sub(1)),
                );
                Action::Continue
            }
            _ => Action::Continue,
        }
    }

    fn toggle_focus(&mut self) {
        self.focus = match self.focus {
            Focus::Sessions => Focus::Details,
            Focus::Details => Focus::Sessions,
        };
    }

    fn page_step(&self) -> isize {
        match self.focus {
            Focus::Sessions => self.list_viewport_height.max(1) as isize,
            Focus::Details => 6,
        }
    }

    fn half_page_step(&self) -> isize {
        (self.page_step() / 2).max(1)
    }

    fn scroll_focused(&mut self, delta: isize) {
        match self.focus {
            Focus::Sessions => self.move_selection(delta),
            Focus::Details => {
                self.details_scroll = self
                    .details_scroll
                    .saturating_add_signed(delta.clamp(i16::MIN as isize, i16::MAX as isize) as i16)
                    .min(self.details_max_scroll);
            }
        }
    }

    fn jump_to_start(&mut self) {
        match self.focus {
            Focus::Sessions => self.select_position(0),
            Focus::Details => self.details_scroll = 0,
        }
    }

    fn jump_to_end(&mut self) {
        match self.focus {
            Focus::Sessions => self.select_position(self.filtered.len().saturating_sub(1)),
            Focus::Details => self.details_scroll = self.details_max_scroll,
        }
    }

    fn move_selection(&mut self, delta: isize) {
        if self.filtered.is_empty() {
            self.selected_position = None;
            return;
        }
        let current = self.selected_position.unwrap_or(0) as isize;
        let last = self.filtered.len().saturating_sub(1) as isize;
        self.select_position((current + delta).clamp(0, last) as usize);
    }

    fn select_position(&mut self, position: usize) {
        if self.filtered.is_empty() {
            self.selected_position = None;
            self.scroll_offset = 0;
            self.details_scroll = 0;
        } else {
            let position = position.min(self.filtered.len() - 1);
            if self.selected_position != Some(position) {
                self.details_scroll = 0;
            }
            self.selected_position = Some(position);
        }
    }

    fn ensure_selection_is_visible(&mut self, viewport_height: usize) {
        let Some(selected) = self.selected_position else {
            self.scroll_offset = 0;
            return;
        };
        let viewport_height = viewport_height.max(1);
        if selected < self.scroll_offset {
            self.scroll_offset = selected;
        } else if selected >= self.scroll_offset + viewport_height {
            self.scroll_offset = selected + 1 - viewport_height;
        }
        self.scroll_offset = self
            .scroll_offset
            .min(self.filtered.len().saturating_sub(viewport_height));
    }

    fn refilter(&mut self) {
        if self.query.trim().is_empty() {
            self.filtered = (0..self.sessions.len()).collect();
            self.select_position(0);
            return;
        }

        let pattern = Pattern::parse(&self.query, CaseMatching::Smart, Normalization::Smart);
        let mut matcher = Matcher::new(Config::DEFAULT);
        let mut buffer = Vec::new();
        let mut matches: Vec<(usize, u32)> = self
            .searchable
            .iter()
            .enumerate()
            .filter_map(|(index, text)| {
                let score = pattern.score(Utf32Str::new(text, &mut buffer), &mut matcher)?;
                Some((index, score))
            })
            .collect();
        matches.sort_by(|(left_index, left_score), (right_index, right_score)| {
            right_score
                .cmp(left_score)
                .then_with(|| {
                    self.sessions[*right_index]
                        .updated
                        .cmp(&self.sessions[*left_index].updated)
                })
                .then_with(|| left_index.cmp(right_index))
        });
        self.filtered = matches.into_iter().map(|(index, _)| index).collect();
        self.select_position(0);
    }
}

fn render(frame: &mut Frame<'_>, app: &mut App) {
    let page = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(4),
            Constraint::Length(1),
        ])
        .split(frame.area());

    render_search(frame, app, page[0]);

    if page[1].width >= 105 {
        let columns = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(68), Constraint::Percentage(32)])
            .split(page[1]);
        render_sessions(frame, app, columns[0]);
        render_details(frame, app, columns[1]);
    } else {
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(5), Constraint::Length(9)])
            .split(page[1]);
        render_sessions(frame, app, rows[0]);
        render_details(frame, app, rows[1]);
    }

    let help = if app.mode == Mode::Search {
        "type to filter  Enter resume  Tab keep filter  Esc clear"
    } else if app.focus == Focus::Details {
        "DETAILS  j/k scroll  gg/G ends  Ctrl+d/u page  Ctrl+w/h pane  Enter resume  q quit"
    } else {
        "SESSIONS  j/k move  gg/G ends  Ctrl+d/u page  Ctrl+w/l pane  / search  Enter resume  q quit"
    };
    frame.render_widget(
        Paragraph::new(help).style(Style::default().fg(Color::DarkGray)),
        page[2],
    );
}

fn render_search(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let count = format!(" {} of {} ", app.filtered.len(), app.sessions.len());
    let border = if app.mode == Mode::Search {
        Color::Cyan
    } else {
        Color::DarkGray
    };
    let query = if app.query.is_empty() && app.mode == Mode::Browse {
        Line::from(Span::styled(
            "Press / to search title, project, path, or session ID",
            Style::default().fg(Color::DarkGray),
        ))
    } else {
        Line::from(vec![
            Span::styled("/ ", Style::default().fg(Color::Cyan)),
            Span::raw(&app.query),
            Span::styled(
                if app.mode == Mode::Search { "▏" } else { "" },
                Style::default().fg(Color::Cyan),
            ),
        ])
    };

    frame.render_widget(
        Paragraph::new(query).block(
            Block::default()
                .title(" agent-resume ")
                .title_bottom(Line::from(count).right_aligned())
                .borders(Borders::ALL)
                .border_style(Style::default().fg(border)),
        ),
        area,
    );
}

fn render_sessions(frame: &mut Frame<'_>, app: &mut App, area: Rect) {
    let viewport_height = area.height.saturating_sub(3).max(1) as usize;
    app.list_viewport_height = viewport_height;
    app.ensure_selection_is_visible(viewport_height);
    let start = app.scroll_offset;
    let end = (start + viewport_height).min(app.filtered.len());
    let rows = app.filtered[start..end].iter().map(|index| {
        let session = &app.sessions[*index];
        let agent_style = match session.agent {
            Agent::Codex => Style::default().fg(Color::Cyan),
            Agent::Claude => Style::default().fg(Color::Magenta),
        };
        Row::new([
            Cell::from(session.agent.label()).style(agent_style),
            Cell::from(age(session)),
            Cell::from(session.project()),
            Cell::from(session.title.clone()),
        ])
    });
    let widths = [
        Constraint::Length(7),
        Constraint::Length(9),
        Constraint::Length(22),
        Constraint::Min(18),
    ];
    let table = Table::new(rows, widths)
        .header(
            Row::new(["Agent", "Updated", "Project", "First prompt"])
                .style(Style::default().fg(Color::DarkGray)),
        )
        .block(
            Block::default()
                .title(" Sessions ")
                .borders(Borders::ALL)
                .border_style(focus_style(app.focus == Focus::Sessions)),
        )
        .row_highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("› ");

    let mut table_state = TableState::default();
    table_state.select(
        app.selected_position
            .filter(|selected| *selected >= start && *selected < end)
            .map(|selected| selected - start),
    );
    frame.render_stateful_widget(table, area, &mut table_state);
}

fn render_details(frame: &mut Frame<'_>, app: &mut App, area: Rect) {
    let lines = if let Some(session) = app.selected() {
        vec![
            Line::from(vec![
                Span::styled("Agent   ", Style::default().fg(Color::DarkGray)),
                Span::raw(session.agent.label()),
            ]),
            Line::from(vec![
                Span::styled("Updated ", Style::default().fg(Color::DarkGray)),
                Span::raw(session.updated.format("%Y-%m-%d %H:%M UTC").to_string()),
            ]),
            Line::from(vec![
                Span::styled("Project ", Style::default().fg(Color::DarkGray)),
                Span::raw(session.cwd.display().to_string()),
            ]),
            Line::from(vec![
                Span::styled("ID      ", Style::default().fg(Color::DarkGray)),
                Span::raw(session.id.clone()),
            ]),
            Line::from(""),
            Line::from(session.title.clone()),
            Line::from(""),
            Line::from(vec![
                Span::styled("Log     ", Style::default().fg(Color::DarkGray)),
                Span::raw(session.path.display().to_string()),
            ]),
        ]
    } else {
        vec![Line::from("No sessions match the current search.")]
    };

    let title = if app.skipped == 0 {
        " Details ".to_owned()
    } else {
        format!(" Details · {} skipped ", app.skipped)
    };
    let content_width = area.width.saturating_sub(2).max(1) as usize;
    let visible_height = area.height.saturating_sub(2) as usize;
    let wrapped_height: usize = lines
        .iter()
        .map(|line| line.width().div_ceil(content_width).max(1))
        .sum();
    app.details_max_scroll = wrapped_height
        .saturating_sub(visible_height)
        .min(u16::MAX as usize) as u16;
    app.details_scroll = app.details_scroll.min(app.details_max_scroll);

    let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false }).block(
        Block::default()
            .title(title)
            .borders(Borders::ALL)
            .border_style(focus_style(app.focus == Focus::Details)),
    );
    frame.render_widget(paragraph.scroll((app.details_scroll, 0)), area);
}

fn focus_style(focused: bool) -> Style {
    if focused {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default().fg(Color::DarkGray)
    }
}

fn age(session: &Session) -> String {
    let duration = Utc::now().signed_duration_since(session.updated);
    if duration.num_seconds() < 60 {
        "now".to_owned()
    } else if duration.num_minutes() < 60 {
        format!("{}m ago", duration.num_minutes())
    } else if duration.num_hours() < 24 {
        format!("{}h ago", duration.num_hours())
    } else if duration.num_days() < 30 {
        format!("{}d ago", duration.num_days())
    } else {
        session.updated.format("%Y-%m-%d").to_string()
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use chrono::{TimeZone, Utc};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    use super::{Action, App, Focus, Mode};
    use crate::model::{Agent, Session};

    fn session(agent: Agent, title: &str, cwd: &str) -> Session {
        Session {
            agent,
            id: format!("{title}-id"),
            title: title.to_owned(),
            cwd: PathBuf::from(cwd),
            path: PathBuf::from("session.jsonl"),
            updated: Utc.timestamp_opt(1_700_000_000, 0).unwrap(),
        }
    }

    #[test]
    fn fuzzy_filter_matches_metadata_across_fields() {
        let sessions = vec![
            session(Agent::Codex, "Fix authentication", "/work/backend"),
            session(Agent::Claude, "Polish the header", "/work/frontend"),
        ];
        let mut app = App::new(sessions, 0);

        app.query = "frontend header".to_owned();
        app.refilter();

        assert_eq!(app.filtered, vec![1]);
    }

    #[test]
    fn scrolling_keeps_only_the_selected_window_in_view() {
        let sessions = (0..20)
            .map(|index| {
                session(
                    Agent::Codex,
                    &format!("Session {index}"),
                    &format!("/work/project-{index}"),
                )
            })
            .collect();
        let mut app = App::new(sessions, 0);

        app.select_position(12);
        app.ensure_selection_is_visible(5);
        assert_eq!(app.scroll_offset, 8);

        app.select_position(3);
        app.ensure_selection_is_visible(5);
        assert_eq!(app.scroll_offset, 3);
    }

    #[test]
    fn ctrl_w_switches_focus_and_details_use_vim_scrolling() {
        let mut app = App::new(vec![session(Agent::Codex, "Session", "/work/project")], 0);
        app.details_max_scroll = 20;

        let action = app.handle_key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL));
        assert_eq!(action, Action::Continue);
        assert_eq!(app.focus, Focus::Details);

        app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL));
        assert_eq!(app.details_scroll, 4);

        app.handle_key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL));
        assert_eq!(app.focus, Focus::Sessions);
    }

    #[test]
    fn double_g_and_uppercase_g_jump_to_session_ends() {
        let sessions = (0..20)
            .map(|index| {
                session(
                    Agent::Codex,
                    &format!("Session {index}"),
                    &format!("/work/project-{index}"),
                )
            })
            .collect();
        let mut app = App::new(sessions, 0);
        app.select_position(8);

        app.handle_key(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE));
        assert_eq!(app.selected_position, Some(8));
        app.handle_key(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE));
        assert_eq!(app.selected_position, Some(0));

        app.handle_key(KeyEvent::new(KeyCode::Char('G'), KeyModifiers::SHIFT));
        assert_eq!(app.selected_position, Some(19));
    }

    #[test]
    fn enter_resumes_directly_from_search() {
        let mut app = App::new(vec![session(Agent::Claude, "Session", "/work/project")], 0);
        app.mode = Mode::Search;

        let action = app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        assert_eq!(action, Action::Resume);
    }

    #[test]
    fn tab_keeps_search_results_for_vim_navigation() {
        let mut app = App::new(
            vec![
                session(Agent::Codex, "Backend", "/work/backend"),
                session(Agent::Claude, "Frontend", "/work/frontend"),
            ],
            0,
        );
        app.mode = Mode::Search;
        app.query = "frontend".to_owned();
        app.refilter();

        let action = app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));

        assert_eq!(action, Action::Continue);
        assert_eq!(app.mode, Mode::Browse);
        assert_eq!(app.filtered, vec![1]);
    }
}
