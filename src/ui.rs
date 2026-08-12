use std::{
    collections::HashMap,
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

use crate::{
    model::{Agent, Origin, Session, SortMode, sort_sessions},
    transcript::{Role, read_transcript},
};

/// Everything the picker needs that lives outside it: a session's records when
/// the reader is opened, and a search across collected conversations.
pub trait Backend {
    /// Sessions from another machine come from the database; local ones come
    /// from disk.
    fn transcript(&mut self, session: &Session) -> Result<Vec<u8>>;

    /// Finds sessions by what was said in them, not just by their metadata.
    fn search_messages(&mut self, query: &str) -> Result<Vec<SearchMatch>>;
}

/// One collected message that matched a content search.
#[derive(Clone, Debug)]
pub struct SearchMatch {
    pub host: String,
    pub agent: String,
    pub session_id: String,
    pub snippet: String,
}

pub fn pick(
    sessions: Vec<Session>,
    skipped: usize,
    sort_mode: SortMode,
    backend: &mut dyn Backend,
) -> Result<Option<Session>> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    if let Err(error) = execute!(stdout, EnterAlternateScreen) {
        let _ = disable_raw_mode();
        return Err(error.into());
    }
    let mut terminal = match Terminal::new(CrosstermBackend::new(stdout)) {
        Ok(terminal) => terminal,
        Err(error) => {
            let _ = disable_raw_mode();
            let _ = execute!(io::stdout(), LeaveAlternateScreen);
            return Err(error.into());
        }
    };

    let result = run_picker(&mut terminal, sessions, skipped, sort_mode, backend);

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
    sort_mode: SortMode,
    backend: &mut dyn Backend,
) -> Result<Option<Session>> {
    let mut app = App::new(sessions, skipped, sort_mode);

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
            Action::View => {
                if let Some(session) = app.selected().cloned() {
                    app.open_reader(&session, backend);
                }
            }
            Action::SearchContent => app.run_content_search(backend),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Mode {
    Browse,
    /// Fuzzy filter over what is already loaded.
    Search,
    /// Full-text query against the collected conversations.
    ContentSearch,
    Reader,
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
    View,
    SearchContent,
}

/// The result of searching inside conversations: which sessions matched, and
/// the line that matched in each.
#[derive(Debug, Default)]
struct ContentFilter {
    query: String,
    snippets: HashMap<(String, String, String), String>,
    /// Matches in sessions that are not in the loaded list.
    elsewhere: usize,
    error: Option<String>,
}

/// A transcript opened for reading inside the picker.
struct Reader {
    title: String,
    lines: Vec<Line<'static>>,
    scroll: u16,
    max_scroll: u16,
}

struct App {
    sessions: Vec<Session>,
    filtered: Vec<usize>,
    searchable: Vec<String>,
    query: String,
    mode: Mode,
    focus: Focus,
    sort_mode: SortMode,
    /// Set when sessions come from more than one machine.
    show_host: bool,
    pending_g: bool,
    selected_position: Option<usize>,
    scroll_offset: usize,
    list_viewport_height: usize,
    details_scroll: u16,
    details_max_scroll: u16,
    reader: Option<Reader>,
    content: Option<ContentFilter>,
    skipped: usize,
}

impl App {
    fn new(sessions: Vec<Session>, skipped: usize, sort_mode: SortMode) -> Self {
        let searchable = sessions.iter().map(Session::searchable_text).collect();
        let filtered = (0..sessions.len()).collect();
        let selected_position = (!sessions.is_empty()).then_some(0);
        let show_host = sessions
            .iter()
            .any(|session| session.origin == Origin::Remote)
            || sessions
                .iter()
                .map(|session| session.host.as_str())
                .collect::<std::collections::HashSet<_>>()
                .len()
                > 1;
        Self {
            sessions,
            filtered,
            searchable,
            query: String::new(),
            mode: Mode::Browse,
            focus: Focus::Sessions,
            sort_mode,
            show_host,
            pending_g: false,
            selected_position,
            scroll_offset: 0,
            list_viewport_height: 10,
            details_scroll: 0,
            details_max_scroll: 0,
            reader: None,
            content: None,
            skipped,
        }
    }

    fn selected(&self) -> Option<&Session> {
        let position = self.selected_position?;
        self.filtered
            .get(position)
            .and_then(|index| self.sessions.get(*index))
    }

    fn open_reader(&mut self, session: &Session, backend: &mut dyn Backend) {
        let lines = match backend.transcript(session) {
            Ok(jsonl) => transcript_lines(&session.agent, &jsonl),
            Err(error) => vec![Line::from(Span::styled(
                format!("{error:#}"),
                Style::default().fg(Color::Red),
            ))],
        };
        self.reader = Some(Reader {
            title: format!(
                "{} · {} · {}",
                session.host,
                session.agent.label(),
                session.title
            ),
            lines,
            scroll: 0,
            max_scroll: 0,
        });
        self.mode = Mode::Reader;
    }

    /// Restricts the list to sessions that said something matching the query.
    /// Matches in sessions outside the loaded list are counted, not hidden.
    fn run_content_search(&mut self, backend: &mut dyn Backend) {
        let Some(content) = self.content.as_mut() else {
            return;
        };
        let query = content.query.clone();
        if query.trim().is_empty() {
            self.content = None;
            self.refilter();
            return;
        }

        let known: std::collections::HashSet<_> = self
            .sessions
            .iter()
            .map(|session| session.dedup_key())
            .collect();
        match backend.search_messages(&query) {
            Ok(matches) => {
                content.error = None;
                content.snippets.clear();
                content.elsewhere = 0;
                for found in matches {
                    let key = (found.host, found.agent, found.session_id);
                    if !known.contains(&key) {
                        content.elsewhere += 1;
                        continue;
                    }
                    content.snippets.entry(key).or_insert(found.snippet);
                }
            }
            Err(error) => {
                content.error = Some(format!("{error:#}"));
                content.snippets.clear();
            }
        }
        self.mode = Mode::Browse;
        self.focus = Focus::Sessions;
        self.refilter();
    }

    fn snippet_for_selected(&self) -> Option<&str> {
        let content = self.content.as_ref()?;
        let session = self.selected()?;
        content
            .snippets
            .get(&session.dedup_key())
            .map(String::as_str)
    }

    fn handle_key(&mut self, key: KeyEvent) -> Action {
        match self.mode {
            Mode::Search => self.handle_search_key(key),
            Mode::ContentSearch => self.handle_content_search_key(key),
            Mode::Browse => self.handle_browse_key(key),
            Mode::Reader => self.handle_reader_key(key),
        }
    }

    fn handle_content_search_key(&mut self, key: KeyEvent) -> Action {
        match key.code {
            KeyCode::Esc => {
                self.content = None;
                self.mode = Mode::Browse;
                self.refilter();
            }
            KeyCode::Enter => return Action::SearchContent,
            KeyCode::Backspace => {
                if let Some(content) = self.content.as_mut() {
                    content.query.pop();
                }
            }
            KeyCode::Char(character)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                if let Some(content) = self.content.as_mut() {
                    content.query.push(character);
                }
            }
            _ => {}
        }
        Action::Continue
    }

    fn handle_search_key(&mut self, key: KeyEvent) -> Action {
        match key.code {
            KeyCode::Esc => {
                self.query.clear();
                self.mode = Mode::Browse;
                self.refilter();
            }
            KeyCode::Enter if !self.filtered.is_empty() => return self.open_selected(),
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

    fn handle_reader_key(&mut self, key: KeyEvent) -> Action {
        let Some(reader) = self.reader.as_mut() else {
            self.mode = Mode::Browse;
            return Action::Continue;
        };

        if key.code == KeyCode::Char('g') && key.modifiers.is_empty() {
            if self.pending_g {
                self.pending_g = false;
                reader.scroll = 0;
            } else {
                self.pending_g = true;
            }
            return Action::Continue;
        }
        self.pending_g = false;

        let page = 20_i32;
        let step = match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                self.mode = Mode::Browse;
                self.reader = None;
                return Action::Continue;
            }
            KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => page / 2,
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => -page / 2,
            KeyCode::Char('f') if key.modifiers.contains(KeyModifiers::CONTROL) => page,
            KeyCode::Char('b') if key.modifiers.contains(KeyModifiers::CONTROL) => -page,
            KeyCode::Down | KeyCode::Char('j') => 1,
            KeyCode::Up | KeyCode::Char('k') => -1,
            KeyCode::PageDown => page,
            KeyCode::PageUp => -page,
            KeyCode::Home => {
                reader.scroll = 0;
                return Action::Continue;
            }
            KeyCode::End | KeyCode::Char('G') => {
                reader.scroll = reader.max_scroll;
                return Action::Continue;
            }
            _ => return Action::Continue,
        };

        reader.scroll = reader
            .scroll
            .saturating_add_signed(step.clamp(i16::MIN as i32, i16::MAX as i32) as i16)
            .min(reader.max_scroll);
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
            KeyCode::Enter if !self.filtered.is_empty() => self.open_selected(),
            KeyCode::Char('v') if !self.filtered.is_empty() => Action::View,
            KeyCode::Char('/') => {
                self.mode = Mode::Search;
                Action::Continue
            }
            KeyCode::Char('f') => {
                self.content = Some(ContentFilter {
                    query: self
                        .content
                        .as_ref()
                        .map(|content| content.query.clone())
                        .unwrap_or_default(),
                    ..ContentFilter::default()
                });
                self.mode = Mode::ContentSearch;
                Action::Continue
            }
            KeyCode::Char('c') if !self.query.is_empty() || self.content.is_some() => {
                self.query.clear();
                self.content = None;
                self.refilter();
                Action::Continue
            }
            KeyCode::Char('s') => {
                self.toggle_sort();
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

    /// Enter resumes what can be resumed, and reads what cannot: a session
    /// recorded on another machine has no local process to return to.
    fn open_selected(&self) -> Action {
        match self.selected() {
            Some(session) if session.origin.is_local() => Action::Resume,
            Some(_) => Action::View,
            None => Action::Continue,
        }
    }

    fn toggle_focus(&mut self) {
        self.focus = match self.focus {
            Focus::Sessions => Focus::Details,
            Focus::Details => Focus::Sessions,
        };
    }

    fn toggle_sort(&mut self) {
        let selected_id = self.selected().map(|session| session.id.clone());
        self.sort_mode = self.sort_mode.toggle();
        sort_sessions(&mut self.sessions, self.sort_mode);
        self.searchable = self.sessions.iter().map(Session::searchable_text).collect();
        self.refilter();

        if let Some(selected_id) = selected_id
            && let Some(position) = self.filtered.iter().position(|index| {
                self.sessions
                    .get(*index)
                    .is_some_and(|session| session.id == selected_id)
            })
        {
            self.select_position(position);
        }
        self.scroll_offset = 0;
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
        let content = self
            .content
            .as_ref()
            .filter(|content| content.error.is_none());
        let matches_content = |session: &Session| match content {
            Some(content) => content.snippets.contains_key(&session.dedup_key()),
            None => true,
        };

        if self.query.trim().is_empty() {
            self.filtered = (0..self.sessions.len())
                .filter(|index| matches_content(&self.sessions[*index]))
                .collect();
            self.select_position(0);
            return;
        }

        let pattern = Pattern::parse(&self.query, CaseMatching::Smart, Normalization::Smart);
        let mut matcher = Matcher::new(Config::DEFAULT);
        let mut buffer = Vec::new();
        self.filtered = self
            .searchable
            .iter()
            .enumerate()
            .filter(|(index, _)| matches_content(&self.sessions[*index]))
            .filter_map(|(index, text)| {
                pattern
                    .score(Utf32Str::new(text, &mut buffer), &mut matcher)
                    .map(|_| index)
            })
            .collect();
        self.select_position(0);
    }
}

/// Formats a stored transcript for reading, keeping the reasoning and tool
/// traffic visually subordinate to the conversation.
fn transcript_lines(agent: &Agent, jsonl: &[u8]) -> Vec<Line<'static>> {
    let entries = read_transcript(agent, jsonl);
    if entries.is_empty() {
        return vec![Line::from("No readable messages in this transcript.")];
    }

    let mut lines = Vec::new();
    for entry in entries {
        let (label, color) = match entry.role {
            Role::User => ("user", Color::Green),
            Role::Assistant => ("assistant", Color::Cyan),
            Role::Reasoning => ("thinking", Color::DarkGray),
            Role::Tool => ("tool", Color::Yellow),
        };
        let time = entry
            .timestamp
            .map(|timestamp| timestamp.format("%Y-%m-%d %H:%M:%S").to_string())
            .unwrap_or_default();
        lines.push(Line::from(vec![
            Span::styled(
                format!("{label} "),
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            ),
            Span::styled(time, Style::default().fg(Color::DarkGray)),
        ]));
        let body_style = if entry.role == Role::User {
            Style::default()
        } else {
            Style::default().fg(color)
        };
        for line in entry.text.lines() {
            lines.push(Line::from(Span::styled(line.to_owned(), body_style)));
        }
        lines.push(Line::from(""));
    }
    lines
}

fn render(frame: &mut Frame<'_>, app: &mut App) {
    if app.mode == Mode::Reader {
        render_reader(frame, app);
        return;
    }

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

    let help = match app.mode {
        Mode::Search => "type to filter  Enter open  Tab keep filter  Esc clear",
        Mode::ContentSearch => "type words said in a session  Enter search  Esc cancel",
        _ if app.focus == Focus::Details => {
            "DETAILS  j/k scroll  gg/G ends  Ctrl+w/h pane  s sort  v read  q quit"
        }
        _ => {
            "SESSIONS  j/k move  Ctrl+w/l pane  s sort  / filter  f find in text  v read  Enter open"
        }
    };
    frame.render_widget(
        Paragraph::new(help).style(Style::default().fg(Color::DarkGray)),
        page[2],
    );
}

fn render_reader(frame: &mut Frame<'_>, app: &mut App) {
    let page = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(3), Constraint::Length(1)])
        .split(frame.area());

    let Some(reader) = app.reader.as_mut() else {
        return;
    };
    let content_width = page[0].width.saturating_sub(2).max(1) as usize;
    let visible_height = page[0].height.saturating_sub(2) as usize;
    let wrapped_height: usize = reader
        .lines
        .iter()
        .map(|line| line.width().div_ceil(content_width).max(1))
        .sum();
    reader.max_scroll = wrapped_height
        .saturating_sub(visible_height)
        .min(u16::MAX as usize) as u16;
    reader.scroll = reader.scroll.min(reader.max_scroll);

    let position = if reader.max_scroll == 0 {
        " all ".to_owned()
    } else {
        format!(
            " {}% ",
            100 * u32::from(reader.scroll) / u32::from(reader.max_scroll)
        )
    };
    let paragraph = Paragraph::new(reader.lines.clone())
        .wrap(Wrap { trim: false })
        .block(
            Block::default()
                .title(format!(" {} ", truncate(&reader.title, content_width)))
                .title_bottom(Line::from(position).right_aligned())
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Cyan)),
        );
    frame.render_widget(paragraph.scroll((reader.scroll, 0)), page[0]);
    frame.render_widget(
        Paragraph::new("TRANSCRIPT  j/k scroll  Ctrl+d/u page  gg/G ends  q back")
            .style(Style::default().fg(Color::DarkGray)),
        page[1],
    );
}

fn render_search(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let count = format!(
        " {} of {} · sort: {} ",
        app.filtered.len(),
        app.sessions.len(),
        app.sort_mode.label()
    );
    let border = if matches!(app.mode, Mode::Search | Mode::ContentSearch) {
        Color::Cyan
    } else {
        Color::DarkGray
    };
    let query = if app.mode == Mode::ContentSearch || app.content.is_some() {
        content_search_line(app)
    } else if app.query.is_empty() && app.mode == Mode::Browse {
        Line::from(Span::styled(
            "Press / to filter, f to search inside conversations",
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

fn content_search_line(app: &App) -> Line<'static> {
    let Some(content) = app.content.as_ref() else {
        return Line::from("");
    };
    let mut spans = vec![
        Span::styled("find ", Style::default().fg(Color::Yellow)),
        Span::raw(content.query.clone()),
    ];
    if app.mode == Mode::ContentSearch {
        spans.push(Span::styled("▏", Style::default().fg(Color::Yellow)));
        spans.push(Span::styled(
            "  Enter to search conversations",
            Style::default().fg(Color::DarkGray),
        ));
        return Line::from(spans);
    }
    if let Some(error) = &content.error {
        spans.push(Span::styled(
            format!("  {error}"),
            Style::default().fg(Color::Red),
        ));
    } else {
        let elsewhere = if content.elsewhere > 0 {
            format!(", {} outside this list", content.elsewhere)
        } else {
            String::new()
        };
        spans.push(Span::styled(
            format!(
                "  {} session(s) matched{elsewhere}  ·  c to clear",
                content.snippets.len()
            ),
            Style::default().fg(Color::DarkGray),
        ));
    }
    Line::from(spans)
}

fn render_sessions(frame: &mut Frame<'_>, app: &mut App, area: Rect) {
    let viewport_height = area.height.saturating_sub(3).max(1) as usize;
    app.list_viewport_height = viewport_height;
    app.ensure_selection_is_visible(viewport_height);
    let start = app.scroll_offset;
    let end = (start + viewport_height).min(app.filtered.len());
    let show_host = app.show_host;
    let rows = app.filtered[start..end].iter().map(|index| {
        let session = &app.sessions[*index];
        let agent_style = match session.agent {
            Agent::Codex => Style::default().fg(Color::Cyan),
            Agent::Claude => Style::default().fg(Color::Magenta),
            Agent::Other(_) => Style::default().fg(Color::Blue),
        };
        let host_style = if session.origin.is_local() {
            Style::default().fg(Color::Green)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        let mut cells = Vec::with_capacity(5);
        if show_host {
            cells.push(Cell::from(session.host.clone()).style(host_style));
        }
        cells.extend([
            Cell::from(session.agent.label().to_owned()).style(agent_style),
            Cell::from(age(app.sort_mode.timestamp(session))),
            Cell::from(session.project()),
            Cell::from(session.title.clone()),
        ]);
        Row::new(cells)
    });

    let mut widths = Vec::with_capacity(5);
    let mut headers = Vec::with_capacity(5);
    if show_host {
        widths.push(Constraint::Length(14));
        headers.push("Machine");
    }
    widths.extend([
        Constraint::Length(7),
        Constraint::Length(9),
        Constraint::Length(20),
        Constraint::Min(18),
    ]);
    headers.extend([
        "Agent",
        match app.sort_mode {
            SortMode::Created => "Created",
            SortMode::Updated => "Updated",
        },
        "Project",
        "First prompt",
    ]);

    let table = Table::new(rows, widths)
        .header(Row::new(headers).style(Style::default().fg(Color::DarkGray)))
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
    let snippet = app.snippet_for_selected().map(str::to_owned);
    let mut lines = if let Some(session) = app.selected() {
        let origin = if session.origin.is_local() {
            "local log".to_owned()
        } else {
            format!("collected from {}", session.host)
        };
        vec![
            Line::from(vec![
                Span::styled("Machine ", Style::default().fg(Color::DarkGray)),
                Span::raw(session.host.clone()),
            ]),
            Line::from(vec![
                Span::styled("Agent   ", Style::default().fg(Color::DarkGray)),
                Span::raw(session.agent.label().to_owned()),
            ]),
            Line::from(vec![
                Span::styled("Source  ", Style::default().fg(Color::DarkGray)),
                Span::raw(origin),
            ]),
            Line::from(vec![
                Span::styled("Created ", Style::default().fg(Color::DarkGray)),
                Span::raw(session.created.format("%Y-%m-%d %H:%M UTC").to_string()),
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

    if let Some(snippet) = snippet {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "Match",
            Style::default().fg(Color::Yellow),
        )));
        lines.push(Line::from(snippet));
    }

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

fn truncate(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        return text.to_owned();
    }
    let mut result: String = text.chars().take(width.saturating_sub(1)).collect();
    result.push('…');
    result
}

fn age(timestamp: chrono::DateTime<Utc>) -> String {
    let duration = Utc::now().signed_duration_since(timestamp);
    if duration.num_seconds() < 60 {
        "now".to_owned()
    } else if duration.num_minutes() < 60 {
        format!("{}m ago", duration.num_minutes())
    } else if duration.num_hours() < 24 {
        format!("{}h ago", duration.num_hours())
    } else if duration.num_days() < 30 {
        format!("{}d ago", duration.num_days())
    } else {
        timestamp.format("%Y-%m-%d").to_string()
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use anyhow::Result;
    use chrono::{TimeZone, Utc};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    use super::{Action, App, Backend, Focus, Mode, SearchMatch};
    use crate::model::{Agent, Origin, Session, SortMode};

    #[derive(Default)]
    struct FakeBackend {
        jsonl: &'static str,
        matches: Vec<SearchMatch>,
        failure: Option<&'static str>,
    }

    impl Backend for FakeBackend {
        fn transcript(&mut self, _session: &Session) -> Result<Vec<u8>> {
            Ok(self.jsonl.as_bytes().to_vec())
        }

        fn search_messages(&mut self, _query: &str) -> Result<Vec<SearchMatch>> {
            match self.failure {
                Some(error) => Err(anyhow::anyhow!(error)),
                None => Ok(self.matches.clone()),
            }
        }
    }

    fn session(agent: Agent, title: &str, cwd: &str) -> Session {
        Session {
            agent,
            host: "laptop".to_owned(),
            origin: Origin::Local,
            id: format!("{title}-id"),
            title: title.to_owned(),
            cwd: PathBuf::from(cwd),
            path: PathBuf::from("session.jsonl"),
            created: Utc.timestamp_opt(1_600_000_000, 0).unwrap(),
            updated: Utc.timestamp_opt(1_700_000_000, 0).unwrap(),
            remote_key: None,
            bytes: 0,
            messages: 0,
        }
    }

    fn collected(agent: Agent, title: &str, host: &str) -> Session {
        let mut session = session(agent, title, "/work/remote");
        session.origin = Origin::Remote;
        session.host = host.to_owned();
        session.remote_key = Some(1);
        session
    }

    #[test]
    fn fuzzy_filter_matches_metadata_across_fields() {
        let sessions = vec![
            session(Agent::Codex, "Fix authentication", "/work/backend"),
            session(Agent::Claude, "Polish the header", "/work/frontend"),
        ];
        let mut app = App::new(sessions, 0, SortMode::Updated);

        app.query = "frontend header".to_owned();
        app.refilter();

        assert_eq!(app.filtered, vec![1]);
    }

    #[test]
    fn fuzzy_filter_matches_the_machine_name() {
        let sessions = vec![
            session(Agent::Codex, "Local work", "/work/backend"),
            collected(Agent::Claude, "Sandbox work", "sandbox-7"),
        ];
        let mut app = App::new(sessions, 0, SortMode::Updated);

        app.query = "sandbox".to_owned();
        app.refilter();

        assert_eq!(app.filtered, vec![1]);
        assert!(
            app.show_host,
            "the machine column appears once hosts differ"
        );
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
        let mut app = App::new(sessions, 0, SortMode::Updated);

        app.select_position(12);
        app.ensure_selection_is_visible(5);
        assert_eq!(app.scroll_offset, 8);

        app.select_position(3);
        app.ensure_selection_is_visible(5);
        assert_eq!(app.scroll_offset, 3);
    }

    #[test]
    fn ctrl_w_switches_focus_and_details_use_vim_scrolling() {
        let mut app = App::new(
            vec![session(Agent::Codex, "Session", "/work/project")],
            0,
            SortMode::Updated,
        );
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
        let mut app = App::new(sessions, 0, SortMode::Updated);
        app.select_position(8);

        app.handle_key(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE));
        assert_eq!(app.selected_position, Some(8));
        app.handle_key(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE));
        assert_eq!(app.selected_position, Some(0));

        app.handle_key(KeyEvent::new(KeyCode::Char('G'), KeyModifiers::SHIFT));
        assert_eq!(app.selected_position, Some(19));
    }

    #[test]
    fn enter_resumes_local_sessions_and_reads_collected_ones() {
        let mut app = App::new(
            vec![session(Agent::Claude, "Session", "/work/project")],
            0,
            SortMode::Updated,
        );
        app.mode = Mode::Search;
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Action::Resume
        );

        let mut app = App::new(
            vec![collected(Agent::Claude, "Session", "sandbox-7")],
            0,
            SortMode::Updated,
        );
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Action::View,
            "a session from another machine has nothing to resume into"
        );
    }

    #[test]
    fn the_reader_opens_scrolls_and_closes() {
        let mut app = App::new(
            vec![collected(Agent::Claude, "Session", "sandbox-7")],
            0,
            SortMode::Updated,
        );
        let selected = app.selected().cloned().unwrap();
        let mut backend = FakeBackend {
            jsonl: r#"{"type":"user","timestamp":"2026-02-01T10:01:00Z","message":{"role":"user","content":"hello"}}"#,
            ..FakeBackend::default()
        };

        app.open_reader(&selected, &mut backend);
        assert_eq!(app.mode, Mode::Reader);
        let reader = app.reader.as_mut().unwrap();
        reader.max_scroll = 10;
        assert!(
            reader
                .lines
                .iter()
                .any(|line| line.spans.iter().any(|span| span.content.contains("hello"))),
            "the transcript body is rendered"
        );

        app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
        assert_eq!(app.reader.as_ref().unwrap().scroll, 1);

        app.handle_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE));
        assert_eq!(app.mode, Mode::Browse);
        assert!(app.reader.is_none());
    }

    #[test]
    fn content_search_keeps_only_sessions_that_said_it() {
        let mut app = App::new(
            vec![
                session(Agent::Codex, "Local work", "/work/backend"),
                collected(Agent::Claude, "Sandbox work", "sandbox-7"),
            ],
            0,
            SortMode::Updated,
        );
        let wanted = app.sessions[1].clone();
        let mut backend = FakeBackend {
            matches: vec![
                SearchMatch {
                    host: wanted.host.clone(),
                    agent: wanted.agent.key().to_owned(),
                    session_id: wanted.id.clone(),
                    snippet: "the «auth race» again".to_owned(),
                },
                SearchMatch {
                    host: "a-machine-not-loaded".to_owned(),
                    agent: "codex".to_owned(),
                    session_id: "unknown".to_owned(),
                    snippet: "elsewhere".to_owned(),
                },
            ],
            ..FakeBackend::default()
        };

        app.handle_key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::NONE));
        for character in "auth race".chars() {
            app.handle_key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE));
        }
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Action::SearchContent
        );
        app.run_content_search(&mut backend);

        assert_eq!(app.mode, Mode::Browse);
        assert_eq!(app.filtered, vec![1]);
        assert_eq!(app.snippet_for_selected(), Some("the «auth race» again"));
        assert_eq!(
            app.content.as_ref().unwrap().elsewhere,
            1,
            "matches outside the loaded list are counted, not silently dropped"
        );

        app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE));
        assert!(app.content.is_none());
        assert_eq!(app.filtered, vec![0, 1]);
    }

    #[test]
    fn a_failed_content_search_reports_instead_of_hiding_everything() {
        let mut app = App::new(
            vec![session(Agent::Codex, "Local work", "/work/backend")],
            0,
            SortMode::Updated,
        );
        let mut backend = FakeBackend {
            failure: Some("no database configured"),
            ..FakeBackend::default()
        };

        app.handle_key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
        app.run_content_search(&mut backend);

        assert_eq!(app.filtered, vec![0], "the list stays usable");
        assert!(
            app.content
                .as_ref()
                .unwrap()
                .error
                .as_ref()
                .unwrap()
                .contains("no database")
        );
    }

    #[test]
    fn tab_keeps_search_results_for_vim_navigation() {
        let mut app = App::new(
            vec![
                session(Agent::Codex, "Backend", "/work/backend"),
                session(Agent::Claude, "Frontend", "/work/frontend"),
            ],
            0,
            SortMode::Updated,
        );
        app.mode = Mode::Search;
        app.query = "frontend".to_owned();
        app.refilter();

        let action = app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));

        assert_eq!(action, Action::Continue);
        assert_eq!(app.mode, Mode::Browse);
        assert_eq!(app.filtered, vec![1]);
    }

    #[test]
    fn s_toggles_sort_mode_and_preserves_selected_session() {
        let mut older_created = session(Agent::Codex, "Older created", "/work/one");
        older_created.created = Utc.timestamp_opt(100, 0).unwrap();
        older_created.updated = Utc.timestamp_opt(400, 0).unwrap();
        let mut newer_created = session(Agent::Claude, "Newer created", "/work/two");
        newer_created.created = Utc.timestamp_opt(300, 0).unwrap();
        newer_created.updated = Utc.timestamp_opt(350, 0).unwrap();
        let mut app = App::new(vec![older_created, newer_created], 0, SortMode::Updated);
        let selected_id = app.selected().unwrap().id.clone();

        app.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE));

        assert_eq!(app.sort_mode, SortMode::Created);
        assert_eq!(app.sessions[0].title, "Newer created");
        assert_eq!(app.selected().unwrap().id, selected_id);
    }
}
