use std::collections::VecDeque;
use std::io::{IsTerminal, Stdout, stdout};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use base64::Engine;
use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use crossterm::{cursor::Hide, cursor::Show};
use futures::StreamExt;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::symbols::border;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, Paragraph, Wrap};

use crate::client::DaemonClient;
use crate::model::{
    AgentActionState, AgentSession, AgentSessionDetail, AgentSessionState, AgentTurnState,
    Execution, ExecutionEvent, ExecutionEventPayload, ExecutionOutcome, ExecutionState,
    OutputStream, now_ms,
};
use crate::session::{open_html, write_replay_html};
use crate::{Error, Result};

const POLL_INTERVAL: Duration = Duration::from_millis(250);
const RETRY_INTERVAL: Duration = Duration::from_secs(1);
const OUTPUT_PAGE_BYTES: usize = 256 * 1024;
const OUTPUT_CAP_BYTES: usize = 1024 * 1024;
const MIN_WIDTH: u16 = 60;
const MIN_HEIGHT: u16 = 22;
const WIDE_WIDTH: u16 = 100;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Focus {
    Executions,
    Output,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Confirmation {
    Cancel(String),
}

#[derive(Debug, Clone)]
struct OutputChunk {
    stream: OutputStream,
    text: String,
    bytes: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Command {
    None,
    Quit,
    ReloadOutput,
    Cancel,
    OpenReplay,
    ExportReplay,
}

#[derive(Debug)]
struct WatchState {
    detail: AgentSessionDetail,
    selected_id: Option<String>,
    focus: Focus,
    follow: bool,
    output_scroll: usize,
    output_cursor: u64,
    output: VecDeque<OutputChunk>,
    output_bytes: usize,
    confirmation: Option<Confirmation>,
    notice: Option<String>,
    error: Option<String>,
}

impl WatchState {
    fn new(detail: AgentSessionDetail) -> Self {
        let selected_id = detail.executions.last().map(|item| item.id.clone());
        Self {
            detail,
            selected_id,
            focus: Focus::Executions,
            follow: true,
            output_scroll: 0,
            output_cursor: 0,
            output: VecDeque::new(),
            output_bytes: 0,
            confirmation: None,
            notice: None,
            error: None,
        }
    }

    fn selected_index(&self) -> Option<usize> {
        self.selected_id.as_ref().and_then(|selected| {
            self.detail
                .executions
                .iter()
                .position(|execution| execution.id == *selected)
        })
    }

    fn selected(&self) -> Option<&Execution> {
        self.selected_index()
            .and_then(|index| self.detail.executions.get(index))
    }

    fn refresh(&mut self, detail: AgentSessionDetail) -> bool {
        let previous = self.selected_id.clone();
        self.detail = detail;
        if self
            .selected_id
            .as_ref()
            .is_none_or(|id| !self.detail.executions.iter().any(|item| item.id == *id))
        {
            self.selected_id = self.detail.executions.last().map(|item| item.id.clone());
        }
        previous != self.selected_id
    }

    fn select_relative(&mut self, amount: isize) -> bool {
        if self.detail.executions.is_empty() {
            return false;
        }
        let current = self.selected_index().unwrap_or(0) as isize;
        let last = self.detail.executions.len().saturating_sub(1) as isize;
        let next = (current + amount).clamp(0, last) as usize;
        let id = self.detail.executions[next].id.clone();
        if self.selected_id.as_deref() == Some(&id) {
            return false;
        }
        self.selected_id = Some(id);
        self.reset_output();
        true
    }

    fn reset_output(&mut self) {
        self.output.clear();
        self.output_bytes = 0;
        self.output_cursor = 0;
        self.output_scroll = 0;
        self.follow = true;
    }

    fn ingest(&mut self, events: &[ExecutionEvent], next_seq: u64) -> Result<()> {
        for event in events {
            match &event.payload {
                ExecutionEventPayload::Output {
                    stream,
                    data_base64,
                } => {
                    let data = base64::engine::general_purpose::STANDARD
                        .decode(data_base64)
                        .map_err(|error| {
                            Error::Protocol(format!("invalid output encoding: {error}"))
                        })?;
                    let bytes = data.len();
                    self.output.push_back(OutputChunk {
                        stream: *stream,
                        text: sanitize_output(&String::from_utf8_lossy(&data)),
                        bytes,
                    });
                    self.output_bytes = self.output_bytes.saturating_add(bytes);
                    while self.output_bytes > OUTPUT_CAP_BYTES {
                        let Some(removed) = self.output.pop_front() else {
                            break;
                        };
                        self.output_bytes = self.output_bytes.saturating_sub(removed.bytes);
                    }
                }
                ExecutionEventPayload::CaptureTruncated { limit_bytes } => {
                    self.notice = Some(format!("capture truncated at {limit_bytes} bytes"));
                }
                ExecutionEventPayload::Started { .. } | ExecutionEventPayload::Finished { .. } => {}
            }
        }
        self.output_cursor = next_seq;
        Ok(())
    }

    fn handle_key(&mut self, key: KeyEvent) -> Command {
        if key.kind != KeyEventKind::Press {
            return Command::None;
        }
        if self.confirmation.is_some() {
            return match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                    self.confirmation = None;
                    Command::Cancel
                }
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                    self.confirmation = None;
                    Command::None
                }
                _ => Command::None,
            };
        }
        match key.code {
            KeyCode::Char('q') => Command::Quit,
            KeyCode::Tab => {
                self.focus = match self.focus {
                    Focus::Executions => Focus::Output,
                    Focus::Output => Focus::Executions,
                };
                Command::None
            }
            KeyCode::Up if self.focus == Focus::Executions => {
                if self.select_relative(-1) {
                    Command::ReloadOutput
                } else {
                    Command::None
                }
            }
            KeyCode::Down if self.focus == Focus::Executions => {
                if self.select_relative(1) {
                    Command::ReloadOutput
                } else {
                    Command::None
                }
            }
            KeyCode::Up if self.focus == Focus::Output => {
                self.follow = false;
                self.output_scroll = self.output_scroll.saturating_sub(1);
                Command::None
            }
            KeyCode::Down if self.focus == Focus::Output => {
                self.follow = false;
                self.output_scroll = self.output_scroll.saturating_add(1);
                Command::None
            }
            KeyCode::Home if self.focus == Focus::Output => {
                self.follow = false;
                self.output_scroll = 0;
                Command::None
            }
            KeyCode::End if self.focus == Focus::Output => {
                self.follow = true;
                Command::None
            }
            KeyCode::Char('f') => {
                self.follow = !self.follow;
                Command::None
            }
            KeyCode::Char('c') => {
                let Some(execution) = self.selected() else {
                    return Command::None;
                };
                if !matches!(
                    execution.state,
                    ExecutionState::Queued | ExecutionState::Running
                ) {
                    self.notice = Some("only queued or running executions can be cancelled".into());
                    return Command::None;
                }
                self.confirmation = Some(Confirmation::Cancel(execution.id.clone()));
                Command::None
            }
            KeyCode::Char('o') => Command::OpenReplay,
            KeyCode::Char('e') => Command::ExportReplay,
            _ => Command::None,
        }
    }

    fn output_lines(&self) -> Vec<Line<'static>> {
        if self.output.is_empty() {
            return vec![Line::styled(
                "No captured output",
                Style::default().fg(Color::DarkGray),
            )];
        }
        let mut lines = Vec::new();
        for chunk in &self.output {
            let style = match chunk.stream {
                OutputStream::Stdout => Style::default().fg(Color::Gray),
                OutputStream::Stderr => Style::default().fg(Color::LightRed),
            };
            for line in chunk.text.split('\n') {
                lines.push(Line::styled(line.to_owned(), style));
            }
        }
        lines
    }
}

pub fn ensure_interactive() -> Result<()> {
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        return Err(Error::InvalidRequest(
            "`loom watch` requires an interactive terminal".into(),
        ));
    }
    Ok(())
}

pub fn active_session_id(sessions: &[AgentSession]) -> Option<String> {
    sessions
        .iter()
        .find(|session| session.state == AgentSessionState::Recording)
        .map(|session| session.id.clone())
}

pub async fn run(client: &DaemonClient, detail: AgentSessionDetail) -> Result<()> {
    ensure_interactive()?;
    let mut session = TerminalSession::enter()?;
    let result = run_loop(client, &mut session.terminal, detail).await;
    session.terminal.show_cursor()?;
    result
}

async fn run_loop(
    client: &DaemonClient,
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    detail: AgentSessionDetail,
) -> Result<()> {
    let session_id = detail.session.id.clone();
    let mut state = WatchState::new(detail);
    let mut events = EventStream::new();
    let mut poll = tokio::time::interval(POLL_INTERVAL);
    poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut retry_at = Instant::now();
    load_output(client, &mut state).await;

    loop {
        terminal.draw(|frame| render(frame, &mut state))?;
        tokio::select! {
            event = events.next() => {
                match event {
                    Some(Ok(Event::Key(key))) => {
                        let command = state.handle_key(key);
                        if handle_command(client, &mut state, command).await? {
                            return Ok(());
                        }
                    }
                    Some(Ok(Event::Resize(_, _))) => {}
                    Some(Ok(_)) => {}
                    Some(Err(error)) => {
                        state.error = Some(format!("terminal input: {error}"));
                    }
                    None => return Ok(()),
                }
            }
            _ = poll.tick() => {
                if Instant::now() < retry_at {
                    continue;
                }
                match client.get_agent_session(session_id.clone()).await {
                    Ok(detail) => {
                        if state.refresh(detail) {
                            state.reset_output();
                        }
                        state.error = None;
                        load_output(client, &mut state).await;
                        if state.error.is_some() {
                            retry_at = Instant::now() + RETRY_INTERVAL;
                        }
                    }
                    Err(error) => {
                        state.error = Some(format!("daemon read failed; retrying: {error}"));
                        retry_at = Instant::now() + RETRY_INTERVAL;
                    }
                }
            }
        }
    }
}

async fn load_output(client: &DaemonClient, state: &mut WatchState) {
    let Some(execution_id) = state.selected_id.clone() else {
        return;
    };
    loop {
        let before = state.output_cursor;
        match client
            .read_output(execution_id.clone(), before, OUTPUT_PAGE_BYTES)
            .await
        {
            Ok(response) => {
                let has_more = response.has_more;
                if let Err(error) = state.ingest(&response.events, response.next_seq) {
                    state.error = Some(error.to_string());
                    return;
                }
                if !has_more || state.output_cursor <= before {
                    return;
                }
            }
            Err(error) => {
                state.error = Some(format!("output read failed; retrying: {error}"));
                return;
            }
        }
    }
}

async fn handle_command(
    client: &DaemonClient,
    state: &mut WatchState,
    command: Command,
) -> Result<bool> {
    match command {
        Command::None => {}
        Command::Quit => return Ok(true),
        Command::ReloadOutput => load_output(client, state).await,
        Command::Cancel => {
            let Some(execution_id) = state.selected_id.clone() else {
                return Ok(false);
            };
            match client.cancel(execution_id).await {
                Ok(_) => state.notice = Some("cancel requested".into()),
                Err(error) => state.error = Some(format!("cancel failed: {error}")),
            }
        }
        Command::OpenReplay => {
            if state.detail.session.state == AgentSessionState::Recording {
                state.notice = Some("replay is available after recording finishes".into());
            } else {
                let cast = Path::new(&state.detail.session.cast_path);
                let html = Path::new(&state.detail.session.html_path);
                match write_replay_html(&state.detail, cast, html, &[])
                    .and_then(|()| open_html(html))
                {
                    Ok(()) => state.notice = Some(format!("opened {}", html.display())),
                    Err(error) => state.error = Some(format!("open failed: {error}")),
                }
            }
        }
        Command::ExportReplay => {
            if state.detail.session.state == AgentSessionState::Recording {
                state.notice = Some("export is available after recording finishes".into());
            } else {
                let output = export_path(&state.detail.session.id)?;
                if output.exists() {
                    state.error = Some(format!("refusing to overwrite {}", output.display()));
                } else {
                    let cast = Path::new(&state.detail.session.cast_path);
                    match write_replay_html(&state.detail, cast, &output, &[]) {
                        Ok(()) => {
                            state.notice = Some(format!(
                                "exported {} (review for sensitive data)",
                                output.display()
                            ));
                        }
                        Err(error) => state.error = Some(format!("export failed: {error}")),
                    }
                }
            }
        }
    }
    Ok(false)
}

fn export_path(session_id: &str) -> Result<PathBuf> {
    Ok(std::env::current_dir()?.join(format!("loomterm-session-{session_id}.html")))
}

fn sanitize_output(value: &str) -> String {
    value
        .chars()
        .map(|character| match character {
            '\n' => '\n',
            '\t' => ' ',
            character if character.is_control() => '?',
            character => character,
        })
        .collect()
}

struct TerminalSession {
    terminal: Terminal<CrosstermBackend<Stdout>>,
}

impl TerminalSession {
    fn enter() -> Result<Self> {
        enable_raw_mode()?;
        let mut output = stdout();
        if let Err(error) = execute!(output, EnterAlternateScreen, Hide) {
            let _ = disable_raw_mode();
            return Err(error.into());
        }
        match Terminal::new(CrosstermBackend::new(output)) {
            Ok(terminal) => Ok(Self { terminal }),
            Err(error) => {
                let _ = disable_raw_mode();
                let mut output = stdout();
                let _ = execute!(output, LeaveAlternateScreen, Show);
                Err(error.into())
            }
        }
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(self.terminal.backend_mut(), LeaveAlternateScreen, Show);
    }
}

fn render(frame: &mut ratatui::Frame<'_>, state: &mut WatchState) {
    let area = frame.area();
    if area.width < MIN_WIDTH || area.height < MIN_HEIGHT {
        frame.render_widget(
            Paragraph::new(format!(
                "Terminal too small\nminimum: {MIN_WIDTH}x{MIN_HEIGHT}\ncurrent: {}x{}",
                area.width, area.height
            ))
            .alignment(Alignment::Center)
            .block(plain_block("Loomterm Live Observer")),
            area,
        );
        return;
    }

    let sections = Layout::vertical([
        Constraint::Length(3),
        Constraint::Length(7),
        Constraint::Min(10),
        Constraint::Length(1),
    ])
    .split(area);
    render_header(frame, state, sections[0]);
    render_agent_request(frame, state, sections[1]);
    if area.width >= WIDE_WIDTH {
        let columns = Layout::horizontal([Constraint::Percentage(38), Constraint::Percentage(62)])
            .split(sections[2]);
        render_executions(frame, state, columns[0]);
        render_output(frame, state, columns[1]);
    } else {
        let rows = Layout::vertical([Constraint::Percentage(45), Constraint::Percentage(55)])
            .split(sections[2]);
        render_executions(frame, state, rows[0]);
        render_output(frame, state, rows[1]);
    }
    render_notice(frame, state, sections[3]);
    if let Some(Confirmation::Cancel(execution_id)) = &state.confirmation {
        let area = centered_rect(54, 7, area);
        frame.render_widget(Clear, area);
        frame.render_widget(
            Paragraph::new(vec![
                Line::from("Cancel this execution?"),
                Line::styled(short_id(execution_id), Style::default().fg(Color::Yellow)),
                Line::from("y/Enter confirm   n/Esc keep running"),
            ])
            .alignment(Alignment::Center)
            .block(plain_block("Confirm").border_style(Style::default().fg(Color::Yellow))),
            area,
        );
    }
}

fn render_header(frame: &mut ratatui::Frame<'_>, state: &WatchState, area: Rect) {
    let session = &state.detail.session;
    let name = session.name.as_deref().unwrap_or(&session.command_display);
    let duration = session.duration_ms.unwrap_or_else(|| {
        u64::try_from(now_ms().saturating_sub(session.created_at_ms)).unwrap_or_default()
    });
    let status_style = session_status_style(&session.state);
    let line = Line::from(vec![
        Span::styled(
            name.to_owned(),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(format!("{:>11}", session.state.as_str()), status_style),
        Span::raw(format!(
            "  {}  {}  {} executions",
            session.agent_kind,
            format_duration(duration),
            state.detail.executions.len()
        )),
        Span::raw(format!("  {} requests", state.detail.turns.len())),
    ]);
    frame.render_widget(
        Paragraph::new(line).block(plain_block("Loomterm Live Observer")),
        area,
    );
}

fn render_agent_request(frame: &mut ratatui::Frame<'_>, state: &WatchState, area: Rect) {
    let Some(turn) = state.detail.turns.last() else {
        frame.render_widget(
            Paragraph::new("Waiting for a Codex or Claude Code prompt...")
                .style(Style::default().fg(Color::DarkGray))
                .block(plain_block("Agent request")),
            area,
        );
        return;
    };
    let actions = state
        .detail
        .actions
        .iter()
        .filter(|action| action.turn_id == turn.id)
        .collect::<Vec<_>>();
    let turn_style = match turn.state {
        AgentTurnState::Active => Style::default().fg(Color::Cyan),
        AgentTurnState::Completed => Style::default().fg(Color::Green),
        AgentTurnState::Failed | AgentTurnState::Interrupted => {
            Style::default().fg(Color::LightRed)
        }
    };
    let action_summary = if actions.is_empty() {
        "No tool actions yet".to_owned()
    } else {
        let recent = actions
            .iter()
            .rev()
            .take(4)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .map(|action| {
                let status = match action.state {
                    AgentActionState::Running => "run",
                    AgentActionState::Completed => "ok",
                    AgentActionState::Failed => "fail",
                };
                format!("[{status}] {}", compact_tool_name(&action.tool_name))
            })
            .collect::<Vec<_>>()
            .join("  ");
        if actions.len() > 4 {
            format!("+{} earlier  {recent}", actions.len() - 4)
        } else {
            recent
        }
    };
    let prompt = if turn.prompt.is_empty() {
        "Prompt unavailable (the session started before hooks were active)".to_owned()
    } else {
        turn.prompt.split_whitespace().collect::<Vec<_>>().join(" ")
    };
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(vec![
                Span::styled(
                    format!("{}  {}", turn.provider, turn.state.as_str()),
                    turn_style.add_modifier(Modifier::BOLD),
                ),
                Span::raw(format!("  {} actions", actions.len())),
            ]),
            Line::styled(prompt, Style::default().fg(Color::White)),
            Line::styled(action_summary, Style::default().fg(Color::DarkGray)),
        ])
        .wrap(Wrap { trim: true })
        .block(plain_block("Agent request")),
        area,
    );
}

fn compact_tool_name(name: &str) -> &str {
    name.strip_prefix("mcp__loomterm__").unwrap_or(name)
}

fn render_executions(frame: &mut ratatui::Frame<'_>, state: &WatchState, area: Rect) {
    let items = state
        .detail
        .executions
        .iter()
        .map(|execution| {
            let (label, style) = execution_status(execution);
            let handoff =
                execution.initiator.session_id.as_deref() != Some(state.detail.session.id.as_str());
            let marker = if state.selected_id.as_deref() == Some(&execution.id) {
                ">"
            } else {
                " "
            };
            ListItem::new(Line::from(vec![
                Span::styled(format!("{marker} {label:<9}"), style),
                Span::raw(" "),
                Span::styled(
                    short_id(&execution.id),
                    Style::default().fg(Color::DarkGray),
                ),
                Span::raw(" "),
                Span::styled(
                    if handoff { "handoff " } else { "" },
                    Style::default().fg(Color::Yellow),
                ),
                Span::raw(execution.command_display.clone()),
            ]))
        })
        .collect::<Vec<_>>();
    let border_style = focus_style(state.focus == Focus::Executions);
    frame.render_widget(
        List::new(items).block(plain_block("Executions").border_style(border_style)),
        area,
    );
}

fn render_output(frame: &mut ratatui::Frame<'_>, state: &mut WatchState, area: Rect) {
    let inner = Layout::vertical([Constraint::Length(5), Constraint::Min(1)]).split(area);
    let detail = state.selected().map_or_else(
        || vec![Line::from("No execution selected")],
        |execution| {
            let relation = if execution.initiator.session_id.as_deref()
                == Some(state.detail.session.id.as_str())
            {
                "current session"
            } else {
                "handoff"
            };
            vec![
                Line::from(vec![Span::styled(
                    execution.command_display.clone(),
                    Style::default().add_modifier(Modifier::BOLD),
                )]),
                Line::from(format!("cwd  {}", execution.cwd)),
                Line::from(format!("source  {relation}")),
                Line::from(format!(
                    "{}  {}",
                    format_outcome(execution.outcome.as_ref()),
                    format_duration(execution.duration_ms.unwrap_or_else(|| {
                        u64::try_from(now_ms().saturating_sub(
                            execution.started_at_ms.unwrap_or(execution.created_at_ms),
                        ))
                        .unwrap_or_default()
                    }))
                )),
            ]
        },
    );
    frame.render_widget(
        Paragraph::new(detail).block(plain_block("Selected")),
        inner[0],
    );
    let lines = state.output_lines();
    let viewport_height = usize::from(inner[1].height.saturating_sub(2));
    let max_scroll = lines.len().saturating_sub(viewport_height);
    if state.follow {
        state.output_scroll = max_scroll;
    } else {
        state.output_scroll = state.output_scroll.min(max_scroll);
    }
    let scroll = u16::try_from(state.output_scroll).unwrap_or(u16::MAX);
    let title = if state.follow {
        "Output [follow]"
    } else {
        "Output"
    };
    frame.render_widget(
        Paragraph::new(lines)
            .scroll((scroll, 0))
            .block(plain_block(title).border_style(focus_style(state.focus == Focus::Output))),
        inner[1],
    );
}

fn render_notice(frame: &mut ratatui::Frame<'_>, state: &WatchState, area: Rect) {
    let (message, style) = if let Some(error) = &state.error {
        (error.as_str(), Style::default().fg(Color::LightRed))
    } else if let Some(notice) = &state.notice {
        (notice.as_str(), Style::default().fg(Color::Yellow))
    } else {
        (
            "Tab focus  arrows navigate  f follow  c cancel  o open  e export  q quit",
            Style::default().fg(Color::DarkGray),
        )
    };
    frame.render_widget(Paragraph::new(message).style(style), area);
}

fn execution_status(execution: &Execution) -> (&'static str, Style) {
    match (&execution.state, &execution.outcome) {
        (ExecutionState::Queued, _) => ("queued", Style::default().fg(Color::Yellow)),
        (ExecutionState::Running, _) => ("running", Style::default().fg(Color::Cyan)),
        (ExecutionState::Cancelled, _) | (_, Some(ExecutionOutcome::Cancelled { .. })) => {
            ("cancelled", Style::default().fg(Color::Magenta))
        }
        (ExecutionState::Interrupted, _) | (_, Some(ExecutionOutcome::Interrupted { .. })) => {
            ("failed", Style::default().fg(Color::LightRed))
        }
        (_, Some(ExecutionOutcome::Exited { code: 0 })) => {
            ("passed", Style::default().fg(Color::Green))
        }
        (ExecutionState::Finished, _) => ("failed", Style::default().fg(Color::LightRed)),
    }
}

fn session_status_style(state: &AgentSessionState) -> Style {
    match state {
        AgentSessionState::Recording => Style::default().fg(Color::Cyan),
        AgentSessionState::Finished => Style::default().fg(Color::Green),
        AgentSessionState::Interrupted => Style::default().fg(Color::LightRed),
    }
}

fn format_outcome(outcome: Option<&ExecutionOutcome>) -> String {
    match outcome {
        Some(ExecutionOutcome::Exited { code }) => format!("exit {code}"),
        Some(ExecutionOutcome::Signaled { signal }) => format!("signal {signal}"),
        Some(ExecutionOutcome::SpawnError { message }) => format!("spawn error: {message}"),
        Some(ExecutionOutcome::Cancelled { signal }) => signal
            .map(|value| format!("cancelled (signal {value})"))
            .unwrap_or_else(|| "cancelled".into()),
        Some(ExecutionOutcome::Interrupted { reason }) => format!("interrupted: {reason}"),
        None => "in progress".into(),
    }
}

fn format_duration(duration_ms: u64) -> String {
    let seconds = duration_ms / 1_000;
    if seconds < 60 {
        format!("{seconds}s")
    } else {
        format!("{}m {:02}s", seconds / 60, seconds % 60)
    }
}

fn short_id(id: &str) -> &str {
    id.get(..12).unwrap_or(id)
}

fn focus_style(active: bool) -> Style {
    if active {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default().fg(Color::DarkGray)
    }
}

fn plain_block(title: &str) -> Block<'_> {
    Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_set(border::PLAIN)
}

fn centered_rect(width: u16, height: u16, area: Rect) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{AgentSession, CommandSpec, Initiator};
    use ratatui::backend::TestBackend;

    fn execution(id: &str, state: ExecutionState) -> Execution {
        Execution {
            id: id.into(),
            workspace_id: "workspace".into(),
            state,
            command: CommandSpec::Argv {
                program: "cargo".into(),
                args: vec!["test".into()],
            },
            command_display: "cargo test".into(),
            cwd: "/tmp/project".into(),
            env_keys: Vec::new(),
            initiator: Initiator {
                kind: "mcp".into(),
                name: Some("codex".into()),
                session_id: Some("session".into()),
            },
            created_at_ms: 1,
            started_at_ms: Some(2),
            ended_at_ms: None,
            duration_ms: None,
            pid: Some(10),
            pgid: Some(10),
            outcome: None,
            captured_bytes: 0,
            output_truncated: false,
            last_seq: 0,
        }
    }

    fn detail(executions: Vec<Execution>) -> AgentSessionDetail {
        AgentSessionDetail {
            session: AgentSession {
                id: "01900000-0000-7000-8000-000000000000".into(),
                workspace_id: "workspace".into(),
                state: AgentSessionState::Recording,
                agent_kind: "codex".into(),
                name: Some("Live observer test".into()),
                command: CommandSpec::Argv {
                    program: "codex".into(),
                    args: vec!["exec".into()],
                },
                command_display: "codex exec".into(),
                cwd: "/tmp/project".into(),
                created_at_ms: now_ms(),
                ended_at_ms: None,
                duration_ms: None,
                recorder_pid: 123,
                outcome: None,
                captured_bytes: 0,
                output_truncated: false,
                initial_cols: 120,
                initial_rows: 30,
                cast_path: "/tmp/session.cast".into(),
                html_path: "/tmp/session.html".into(),
            },
            executions,
            turns: Vec::new(),
            actions: Vec::new(),
        }
    }

    fn buffer_text(terminal: &Terminal<TestBackend>) -> String {
        let area = terminal.backend().buffer().area;
        (area.y..area.y + area.height)
            .map(|y| {
                (area.x..area.x + area.width)
                    .map(|x| terminal.backend().buffer()[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn refresh_preserves_selection_and_appends_executions() {
        let first = execution("first", ExecutionState::Finished);
        let second = execution("second", ExecutionState::Running);
        let mut state = WatchState::new(detail(vec![first.clone(), second.clone()]));
        state.selected_id = Some(first.id.clone());
        let third = execution("third", ExecutionState::Queued);

        assert!(!state.refresh(detail(vec![first, second, third])));
        assert_eq!(state.selected_id.as_deref(), Some("first"));
    }

    #[test]
    fn active_selection_uses_newest_recording_session() {
        let newest = detail(Vec::new()).session;
        let mut finished = newest.clone();
        finished.id = "finished".into();
        finished.state = AgentSessionState::Finished;
        let mut older = newest.clone();
        older.id = "older".into();
        assert_eq!(
            active_session_id(&[finished, newest.clone(), older]),
            Some(newest.id)
        );
    }

    #[test]
    fn output_is_merged_and_capped() {
        let mut state = WatchState::new(detail(vec![execution("one", ExecutionState::Running)]));
        for seq in 1..=6 {
            let data = vec![b'x'; 200 * 1024];
            state
                .ingest(
                    &[ExecutionEvent {
                        execution_id: "one".into(),
                        seq,
                        timestamp_ms: 1,
                        payload: ExecutionEventPayload::Output {
                            stream: if seq % 2 == 0 {
                                OutputStream::Stderr
                            } else {
                                OutputStream::Stdout
                            },
                            data_base64: base64::engine::general_purpose::STANDARD.encode(data),
                        },
                    }],
                    seq,
                )
                .unwrap();
        }
        assert!(state.output_bytes <= OUTPUT_CAP_BYTES);
        assert_eq!(state.output_cursor, 6);
        assert_eq!(state.output.len(), 5);
    }

    #[test]
    fn output_control_sequences_are_not_rendered() {
        assert_eq!(sanitize_output("ok\u{1b}[31m\tbad\u{7}"), "ok?[31m bad?");
    }

    #[test]
    fn input_changes_focus_selection_follow_and_confirmation() {
        let mut state = WatchState::new(detail(vec![
            execution("one", ExecutionState::Finished),
            execution("two", ExecutionState::Running),
        ]));
        assert_eq!(state.selected_id.as_deref(), Some("two"));
        assert_eq!(
            state.handle_key(KeyEvent::from(KeyCode::Up)),
            Command::ReloadOutput
        );
        assert_eq!(state.selected_id.as_deref(), Some("one"));
        state.handle_key(KeyEvent::from(KeyCode::Tab));
        state.handle_key(KeyEvent::from(KeyCode::Up));
        assert!(!state.follow);
        state.focus = Focus::Executions;
        state.selected_id = Some("two".into());
        assert_eq!(
            state.handle_key(KeyEvent::from(KeyCode::Char('c'))),
            Command::None
        );
        assert!(state.confirmation.is_some());
        assert_eq!(
            state.handle_key(KeyEvent::from(KeyCode::Char('y'))),
            Command::Cancel
        );
    }

    #[test]
    fn renders_wide_and_narrow_layouts() {
        for (width, height) in [(120, 30), (70, 30)] {
            let backend = TestBackend::new(width, height);
            let mut terminal = Terminal::new(backend).unwrap();
            let mut state = WatchState::new(detail(vec![execution(
                "01900000-aaaa-bbbb-cccc-dddddddddddd",
                ExecutionState::Running,
            )]));
            terminal.draw(|frame| render(frame, &mut state)).unwrap();
            let text = buffer_text(&terminal);
            assert!(text.contains("Loomterm Live Observer"));
            assert!(text.contains("Executions"));
            assert!(text.contains("Output [follow]"));
            assert!(text.contains("cargo test"));
        }
    }

    #[test]
    fn renders_cross_session_execution_as_handoff() {
        let mut linked = execution("linked", ExecutionState::Running);
        linked.initiator.session_id = Some("other-session".into());
        let backend = TestBackend::new(120, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut state = WatchState::new(detail(vec![linked]));

        terminal.draw(|frame| render(frame, &mut state)).unwrap();

        let text = buffer_text(&terminal);
        assert!(text.contains("handoff cargo test"));
        assert!(text.contains("source  handoff"));
    }

    #[test]
    fn renders_terminal_too_small_message() {
        let backend = TestBackend::new(40, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut state = WatchState::new(detail(Vec::new()));
        terminal.draw(|frame| render(frame, &mut state)).unwrap();
        assert!(buffer_text(&terminal).contains("Terminal too small"));
    }

    #[test]
    fn classifies_success_and_failure() {
        let mut passed = execution("passed", ExecutionState::Finished);
        passed.outcome = Some(ExecutionOutcome::Exited { code: 0 });
        let mut failed = execution("failed", ExecutionState::Finished);
        failed.outcome = Some(ExecutionOutcome::Exited { code: 2 });
        assert_eq!(execution_status(&passed).0, "passed");
        assert_eq!(execution_status(&failed).0, "failed");
    }
}
