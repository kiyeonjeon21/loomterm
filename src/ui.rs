use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use base64::Engine;
use crossterm::event::{
    Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent,
    MouseEventKind,
};
use futures::StreamExt;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::symbols::border;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, Paragraph, Wrap};

use crate::client::{DaemonClient, ExecutionSubscription};
use crate::model::{
    AgentSession, AgentSessionDetail, AgentSessionState, Execution, ExecutionEvent,
    ExecutionEventPayload, ExecutionOutcome, ExecutionState, OutputStream, Workspace, now_ms,
};
use crate::session::{open_html, write_replay_html};
use crate::terminal::TerminalSession;
use crate::{Error, Result};

const POLL_INTERVAL: Duration = Duration::from_millis(500);
const RETRY_INTERVAL: Duration = Duration::from_secs(1);
const OUTPUT_PAGE_BYTES: usize = 256 * 1024;
const OUTPUT_CAP_BYTES: usize = 1024 * 1024;
const MIN_WIDTH: u16 = 60;
const MIN_HEIGHT: u16 = 20;
const MEDIUM_WIDTH: u16 = 100;
const WIDE_WIDTH: u16 = 140;
const LIST_LIMIT: u32 = 1000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentProvider {
    Codex,
    Claude,
}

impl AgentProvider {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Claude => "claude",
        }
    }

    fn toggled(self) -> Self {
        match self {
            Self::Codex => Self::Claude,
            Self::Claude => Self::Codex,
        }
    }
}

#[derive(Debug, Clone)]
pub struct UiOptions {
    pub workspace: Workspace,
    pub selected_session_id: Option<String>,
    pub notice: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UiAction {
    Quit,
    Launch {
        provider: AgentProvider,
        prompt: Option<String>,
    },
    Handoff {
        provider: AgentProvider,
        source_session_id: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Focus {
    Sessions,
    Executions,
    Output,
}

impl Focus {
    fn next(self) -> Self {
        match self {
            Self::Sessions => Self::Executions,
            Self::Executions => Self::Output,
            Self::Output => Self::Sessions,
        }
    }

    fn previous(self) -> Self {
        match self {
            Self::Sessions => Self::Output,
            Self::Executions => Self::Sessions,
            Self::Output => Self::Executions,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AgentModalField {
    Provider,
    Prompt,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum AgentModalPurpose {
    Launch,
    Handoff {
        source_session_id: String,
        source_provider: String,
        active_count: usize,
        command_preview: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PaletteAction {
    NewAgent,
    Handoff,
    Cancel,
    OpenReplay,
    ExportReplay,
    ToggleFollow,
    Refresh,
    Help,
    Quit,
}

impl PaletteAction {
    const ALL: [Self; 9] = [
        Self::NewAgent,
        Self::Handoff,
        Self::Cancel,
        Self::OpenReplay,
        Self::ExportReplay,
        Self::ToggleFollow,
        Self::Refresh,
        Self::Help,
        Self::Quit,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::NewAgent => "New agent",
            Self::Handoff => "Handoff active work",
            Self::Cancel => "Cancel execution",
            Self::OpenReplay => "Open session replay",
            Self::ExportReplay => "Export session replay",
            Self::ToggleFollow => "Toggle output follow",
            Self::Refresh => "Refresh now",
            Self::Help => "Keyboard help",
            Self::Quit => "Quit Loomterm",
        }
    }

    fn shortcut(self) -> &'static str {
        match self {
            Self::NewAgent => "n",
            Self::Handoff => "h",
            Self::Cancel => "c",
            Self::OpenReplay => "o",
            Self::ExportReplay => "e",
            Self::ToggleFollow => "f",
            Self::Refresh => "r",
            Self::Help => "?",
            Self::Quit => "q",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Modal {
    Help,
    ConfirmCancel(String),
    Palette {
        selected: usize,
    },
    Agent {
        purpose: AgentModalPurpose,
        provider: AgentProvider,
        field: AgentModalField,
        prompt: String,
    },
}

#[derive(Debug, Clone)]
struct OutputChunk {
    stream: OutputStream,
    text: String,
    bytes: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum HitTarget {
    AllWork,
    Session(String),
    Execution(String),
    Pane(Focus),
}

#[derive(Debug, Clone)]
struct HitRegion {
    area: Rect,
    target: HitTarget,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Command {
    None,
    Quit,
    Refresh,
    ReloadOutput,
    Cancel(String),
    OpenReplay(String),
    ExportReplay(String),
    PrepareHandoff(String),
    Launch(AgentProvider, Option<String>),
    Handoff(AgentProvider, String),
}

#[derive(Debug)]
struct OperatorState {
    workspace: Workspace,
    sessions: Vec<AgentSession>,
    workspace_executions: Vec<Execution>,
    detail: Option<AgentSessionDetail>,
    selected_session_id: Option<String>,
    selected_execution_id: Option<String>,
    session_offset: usize,
    execution_offset: usize,
    focus: Focus,
    query: String,
    editing_search: bool,
    follow: bool,
    output_scroll: usize,
    output_cursor: u64,
    output: VecDeque<OutputChunk>,
    output_bytes: usize,
    modal: Option<Modal>,
    notice: Option<String>,
    error: Option<String>,
    hit_regions: Vec<HitRegion>,
}

impl OperatorState {
    fn new(options: UiOptions) -> Self {
        Self {
            workspace: options.workspace,
            sessions: Vec::new(),
            workspace_executions: Vec::new(),
            detail: None,
            selected_session_id: options.selected_session_id,
            selected_execution_id: None,
            session_offset: 0,
            execution_offset: 0,
            focus: Focus::Sessions,
            query: String::new(),
            editing_search: false,
            follow: true,
            output_scroll: 0,
            output_cursor: 0,
            output: VecDeque::new(),
            output_bytes: 0,
            modal: None,
            notice: options.notice,
            error: None,
            hit_regions: Vec::new(),
        }
    }

    fn apply_snapshot(
        &mut self,
        mut sessions: Vec<AgentSession>,
        mut executions: Vec<Execution>,
        detail: Option<AgentSessionDetail>,
    ) -> bool {
        let previous_execution = self.selected_execution_id.clone();
        sessions.sort_by_key(|session| std::cmp::Reverse(session.created_at_ms));
        executions.sort_by_key(|execution| std::cmp::Reverse(execution.created_at_ms));
        self.sessions = sessions;
        self.workspace_executions = executions;
        if self
            .selected_session_id
            .as_ref()
            .is_some_and(|id| !self.sessions.iter().any(|session| session.id == *id))
        {
            self.selected_session_id = None;
        }
        self.detail = detail.filter(|detail| {
            self.selected_session_id.as_deref() == Some(detail.session.id.as_str())
        });
        self.ensure_execution_selection();
        previous_execution != self.selected_execution_id
    }

    fn scoped_executions(&self) -> &[Execution] {
        self.detail
            .as_ref()
            .map_or(self.workspace_executions.as_slice(), |detail| {
                detail.executions.as_slice()
            })
    }

    fn visible_session_ids(&self) -> Vec<Option<String>> {
        let mut items = vec![None];
        items.extend(
            self.sessions
                .iter()
                .filter(|session| self.matches_session(session))
                .map(|session| Some(session.id.clone())),
        );
        items
    }

    fn visible_execution_ids(&self) -> Vec<String> {
        let mut executions = self
            .scoped_executions()
            .iter()
            .filter(|execution| self.matches_execution(execution))
            .collect::<Vec<_>>();
        executions.sort_by_key(|execution| std::cmp::Reverse(execution.created_at_ms));
        executions
            .into_iter()
            .map(|execution| execution.id.clone())
            .collect()
    }

    fn matches_session(&self, session: &AgentSession) -> bool {
        if self.query.is_empty() {
            return true;
        }
        let query = self.query.to_ascii_lowercase();
        [
            session.id.as_str(),
            session.agent_kind.as_str(),
            session.state.as_str(),
            session.name.as_deref().unwrap_or_default(),
            session.command_display.as_str(),
        ]
        .iter()
        .any(|value| value.to_ascii_lowercase().contains(&query))
    }

    fn matches_execution(&self, execution: &Execution) -> bool {
        if self.query.is_empty() {
            return true;
        }
        let query = self.query.to_ascii_lowercase();
        let outcome = format_outcome(execution.outcome.as_ref());
        [
            execution.id.as_str(),
            execution.command_display.as_str(),
            execution.cwd.as_str(),
            execution.state.as_str(),
            execution.initiator.kind.as_str(),
            execution.initiator.name.as_deref().unwrap_or_default(),
            outcome.as_str(),
        ]
        .iter()
        .any(|value| value.to_ascii_lowercase().contains(&query))
    }

    fn ensure_execution_selection(&mut self) -> bool {
        let previous = self.selected_execution_id.clone();
        let visible = self.visible_execution_ids();
        if self
            .selected_execution_id
            .as_ref()
            .is_none_or(|selected| !visible.iter().any(|id| id == selected))
        {
            self.selected_execution_id = visible.first().cloned();
        }
        let changed = previous != self.selected_execution_id;
        if changed {
            self.reset_output();
        }
        changed
    }

    fn selected_execution(&self) -> Option<&Execution> {
        let selected = self.selected_execution_id.as_deref()?;
        self.scoped_executions()
            .iter()
            .find(|execution| execution.id == selected)
    }

    fn selected_session(&self) -> Option<&AgentSession> {
        let selected = self.selected_session_id.as_deref()?;
        self.sessions.iter().find(|session| session.id == selected)
    }

    fn selected_source_session_id(&self) -> Option<String> {
        self.selected_session_id.clone().or_else(|| {
            self.selected_execution()
                .and_then(|execution| execution.initiator.session_id.clone())
        })
    }

    fn select_session_relative(&mut self, amount: isize) -> bool {
        let visible = self.visible_session_ids();
        if visible.is_empty() {
            return false;
        }
        let current = visible
            .iter()
            .position(|id| *id == self.selected_session_id)
            .unwrap_or(0) as isize;
        let last = visible.len().saturating_sub(1) as isize;
        let next = (current + amount).clamp(0, last) as usize;
        if visible[next] == self.selected_session_id {
            return false;
        }
        self.selected_session_id = visible[next].clone();
        self.selected_execution_id = None;
        self.execution_offset = 0;
        self.detail = None;
        self.reset_output();
        true
    }

    fn select_execution_relative(&mut self, amount: isize) -> bool {
        let visible = self.visible_execution_ids();
        if visible.is_empty() {
            return false;
        }
        let current = self
            .selected_execution_id
            .as_ref()
            .and_then(|selected| visible.iter().position(|id| id == selected))
            .unwrap_or(0) as isize;
        let last = visible.len().saturating_sub(1) as isize;
        let next = (current + amount).clamp(0, last) as usize;
        if self.selected_execution_id.as_deref() == Some(visible[next].as_str()) {
            return false;
        }
        self.selected_execution_id = Some(visible[next].clone());
        self.reset_output();
        true
    }

    fn select_session(&mut self, id: Option<String>) -> bool {
        if self.selected_session_id == id {
            return false;
        }
        self.selected_session_id = id;
        self.selected_execution_id = None;
        self.execution_offset = 0;
        self.detail = None;
        self.reset_output();
        true
    }

    fn select_execution(&mut self, id: String) -> bool {
        if self.selected_execution_id.as_deref() == Some(&id) {
            return false;
        }
        self.selected_execution_id = Some(id);
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

    fn output_lines(&self) -> Vec<Line<'static>> {
        if self.output.is_empty() {
            return vec![Line::styled(
                "No captured output",
                Style::default().fg(Color::DarkGray),
            )];
        }
        self.output
            .iter()
            .flat_map(|chunk| {
                let style = match chunk.stream {
                    OutputStream::Stdout => Style::default().fg(Color::Gray),
                    OutputStream::Stderr => Style::default().fg(Color::LightRed),
                };
                chunk
                    .text
                    .split('\n')
                    .map(move |line| Line::styled(line.to_owned(), style))
            })
            .collect()
    }

    fn handle_key(&mut self, key: KeyEvent) -> Command {
        if key.kind != KeyEventKind::Press {
            return Command::None;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char('c') | KeyCode::Char('C'))
        {
            return Command::Quit;
        }
        if self.editing_search {
            return self.handle_search_key(key);
        }
        if self.modal.is_some() {
            return self.handle_modal_key(key);
        }
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char('p') | KeyCode::Char('P'))
        {
            self.modal = Some(Modal::Palette { selected: 0 });
            return Command::None;
        }
        match key.code {
            KeyCode::Char('q') => Command::Quit,
            KeyCode::Char('/') => {
                self.editing_search = true;
                self.selected_session_id = None;
                self.detail = None;
                self.selected_execution_id = None;
                self.session_offset = 0;
                self.execution_offset = 0;
                self.ensure_execution_selection();
                Command::ReloadOutput
            }
            KeyCode::Char(':') => {
                self.modal = Some(Modal::Palette { selected: 0 });
                Command::None
            }
            KeyCode::Char('?') => {
                self.modal = Some(Modal::Help);
                Command::None
            }
            KeyCode::Tab => {
                self.focus = self.focus.next();
                Command::None
            }
            KeyCode::BackTab => {
                self.focus = self.focus.previous();
                Command::None
            }
            KeyCode::Up | KeyCode::Char('k') => self.navigate(-1),
            KeyCode::Down | KeyCode::Char('j') => self.navigate(1),
            KeyCode::Home if self.focus == Focus::Output => {
                self.follow = false;
                self.output_scroll = 0;
                Command::None
            }
            KeyCode::End if self.focus == Focus::Output => {
                self.follow = true;
                Command::None
            }
            KeyCode::Enter => {
                self.focus = match self.focus {
                    Focus::Sessions => Focus::Executions,
                    Focus::Executions => Focus::Output,
                    Focus::Output => Focus::Output,
                };
                Command::None
            }
            KeyCode::Char('n') => self.invoke(PaletteAction::NewAgent),
            KeyCode::Char('h') => self.invoke(PaletteAction::Handoff),
            KeyCode::Char('c') => self.invoke(PaletteAction::Cancel),
            KeyCode::Char('o') => self.invoke(PaletteAction::OpenReplay),
            KeyCode::Char('e') => self.invoke(PaletteAction::ExportReplay),
            KeyCode::Char('f') => self.invoke(PaletteAction::ToggleFollow),
            KeyCode::Char('r') => Command::Refresh,
            KeyCode::Esc if !self.query.is_empty() => {
                self.query.clear();
                self.ensure_execution_selection();
                Command::ReloadOutput
            }
            _ => Command::None,
        }
    }

    fn handle_search_key(&mut self, key: KeyEvent) -> Command {
        match key.code {
            KeyCode::Esc | KeyCode::Enter => {
                self.editing_search = false;
                self.ensure_execution_selection();
                Command::ReloadOutput
            }
            KeyCode::Backspace => {
                self.query.pop();
                self.ensure_execution_selection();
                Command::ReloadOutput
            }
            KeyCode::Char(character)
                if !key.modifiers.contains(KeyModifiers::CONTROL)
                    && !key.modifiers.contains(KeyModifiers::ALT) =>
            {
                self.query.push(character);
                self.ensure_execution_selection();
                Command::ReloadOutput
            }
            _ => Command::None,
        }
    }

    fn handle_modal_key(&mut self, key: KeyEvent) -> Command {
        let Some(modal) = self.modal.take() else {
            return Command::None;
        };
        match modal {
            Modal::Help => {
                if !matches!(key.code, KeyCode::Esc | KeyCode::Char('?')) {
                    self.modal = Some(Modal::Help);
                }
                Command::None
            }
            Modal::ConfirmCancel(execution_id) => match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                    Command::Cancel(execution_id)
                }
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => Command::None,
                _ => {
                    self.modal = Some(Modal::ConfirmCancel(execution_id));
                    Command::None
                }
            },
            Modal::Palette { mut selected } => match key.code {
                KeyCode::Esc => Command::None,
                KeyCode::Up | KeyCode::Char('k') => {
                    selected = selected.saturating_sub(1);
                    self.modal = Some(Modal::Palette { selected });
                    Command::None
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    selected = (selected + 1).min(PaletteAction::ALL.len() - 1);
                    self.modal = Some(Modal::Palette { selected });
                    Command::None
                }
                KeyCode::Enter => self.invoke(PaletteAction::ALL[selected]),
                _ => {
                    self.modal = Some(Modal::Palette { selected });
                    Command::None
                }
            },
            Modal::Agent {
                purpose,
                mut provider,
                mut field,
                mut prompt,
            } => match key.code {
                KeyCode::Esc => Command::None,
                KeyCode::Left | KeyCode::Right if field == AgentModalField::Provider => {
                    provider = provider.toggled();
                    self.modal = Some(Modal::Agent {
                        purpose,
                        provider,
                        field,
                        prompt,
                    });
                    Command::None
                }
                KeyCode::Tab if purpose == AgentModalPurpose::Launch => {
                    field = match field {
                        AgentModalField::Provider => AgentModalField::Prompt,
                        AgentModalField::Prompt => AgentModalField::Provider,
                    };
                    self.modal = Some(Modal::Agent {
                        purpose,
                        provider,
                        field,
                        prompt,
                    });
                    Command::None
                }
                KeyCode::Backspace if field == AgentModalField::Prompt => {
                    prompt.pop();
                    self.modal = Some(Modal::Agent {
                        purpose,
                        provider,
                        field,
                        prompt,
                    });
                    Command::None
                }
                KeyCode::Char(character)
                    if field == AgentModalField::Prompt
                        && !key.modifiers.contains(KeyModifiers::CONTROL)
                        && !key.modifiers.contains(KeyModifiers::ALT) =>
                {
                    prompt.push(character);
                    self.modal = Some(Modal::Agent {
                        purpose,
                        provider,
                        field,
                        prompt,
                    });
                    Command::None
                }
                KeyCode::Enter => match purpose {
                    AgentModalPurpose::Launch => {
                        let prompt = (!prompt.trim().is_empty()).then(|| prompt.trim().to_owned());
                        Command::Launch(provider, prompt)
                    }
                    AgentModalPurpose::Handoff {
                        source_session_id, ..
                    } => Command::Handoff(provider, source_session_id),
                },
                _ => {
                    self.modal = Some(Modal::Agent {
                        purpose,
                        provider,
                        field,
                        prompt,
                    });
                    Command::None
                }
            },
        }
    }

    fn navigate(&mut self, amount: isize) -> Command {
        match self.focus {
            Focus::Sessions => {
                if self.select_session_relative(amount) {
                    Command::Refresh
                } else {
                    Command::None
                }
            }
            Focus::Executions => {
                if self.select_execution_relative(amount) {
                    Command::ReloadOutput
                } else {
                    Command::None
                }
            }
            Focus::Output => {
                self.follow = false;
                if amount < 0 {
                    self.output_scroll = self.output_scroll.saturating_sub(amount.unsigned_abs());
                } else {
                    self.output_scroll = self.output_scroll.saturating_add(amount as usize);
                }
                Command::None
            }
        }
    }

    fn invoke(&mut self, action: PaletteAction) -> Command {
        match action {
            PaletteAction::NewAgent => {
                self.modal = Some(Modal::Agent {
                    purpose: AgentModalPurpose::Launch,
                    provider: AgentProvider::Codex,
                    field: AgentModalField::Provider,
                    prompt: String::new(),
                });
                Command::None
            }
            PaletteAction::Handoff => match self.selected_source_session_id() {
                Some(session_id) => Command::PrepareHandoff(session_id),
                None => {
                    self.notice = Some("select a recorded session before handoff".into());
                    Command::None
                }
            },
            PaletteAction::Cancel => match self.selected_execution() {
                Some(execution)
                    if matches!(
                        execution.state,
                        ExecutionState::Queued | ExecutionState::Running
                    ) =>
                {
                    self.modal = Some(Modal::ConfirmCancel(execution.id.clone()));
                    Command::None
                }
                Some(_) => {
                    self.notice = Some("only queued or running executions can be cancelled".into());
                    Command::None
                }
                None => {
                    self.notice = Some("select an execution first".into());
                    Command::None
                }
            },
            PaletteAction::OpenReplay | PaletteAction::ExportReplay => {
                let Some(session_id) = self.selected_source_session_id() else {
                    self.notice = Some("select a recorded session first".into());
                    return Command::None;
                };
                if action == PaletteAction::OpenReplay {
                    Command::OpenReplay(session_id)
                } else {
                    Command::ExportReplay(session_id)
                }
            }
            PaletteAction::ToggleFollow => {
                self.follow = !self.follow;
                Command::None
            }
            PaletteAction::Refresh => Command::Refresh,
            PaletteAction::Help => {
                self.modal = Some(Modal::Help);
                Command::None
            }
            PaletteAction::Quit => Command::Quit,
        }
    }

    fn handle_mouse(&mut self, event: MouseEvent) -> Command {
        let pane = self
            .hit_regions
            .iter()
            .find_map(|region| match region.target {
                HitTarget::Pane(focus) if contains(region.area, event.column, event.row) => {
                    Some(focus)
                }
                _ => None,
            });
        let target = self
            .hit_regions
            .iter()
            .rev()
            .find(|region| contains(region.area, event.column, event.row))
            .map(|region| region.target.clone());
        match event.kind {
            MouseEventKind::Down(MouseButton::Left) => match target {
                Some(HitTarget::AllWork) => {
                    self.focus = Focus::Sessions;
                    if self.select_session(None) {
                        Command::Refresh
                    } else {
                        Command::None
                    }
                }
                Some(HitTarget::Session(id)) => {
                    self.focus = Focus::Sessions;
                    if self.select_session(Some(id)) {
                        Command::Refresh
                    } else {
                        Command::None
                    }
                }
                Some(HitTarget::Execution(id)) => {
                    self.focus = Focus::Executions;
                    if self.select_execution(id) {
                        Command::ReloadOutput
                    } else {
                        Command::None
                    }
                }
                Some(HitTarget::Pane(focus)) => {
                    self.focus = focus;
                    Command::None
                }
                None => Command::None,
            },
            MouseEventKind::ScrollUp => {
                if let Some(focus) = pane {
                    self.focus = focus;
                }
                self.navigate(-1)
            }
            MouseEventKind::ScrollDown => {
                if let Some(focus) = pane {
                    self.focus = focus;
                }
                self.navigate(1)
            }
            _ => Command::None,
        }
    }
}

pub async fn run(client: &DaemonClient, options: UiOptions) -> Result<UiAction> {
    crate::terminal::ensure_interactive("loom ui")?;
    let mut state = OperatorState::new(options);
    refresh(client, &mut state).await?;
    load_output(client, &mut state).await;
    let mut subscription = subscribe_output(client, &mut state).await;
    let mut terminal_session = TerminalSession::enter(true)?;
    let mut events = EventStream::new();
    let mut poll = tokio::time::interval(POLL_INTERVAL);
    poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut retry_at = Instant::now();

    loop {
        terminal_session
            .terminal
            .draw(|frame| render(frame, &mut state))?;
        tokio::select! {
            input = events.next() => {
                let command = match input {
                    Some(Ok(Event::Key(key))) => state.handle_key(key),
                    Some(Ok(Event::Mouse(mouse))) => state.handle_mouse(mouse),
                    Some(Ok(Event::Resize(_, _))) | Some(Ok(_)) => Command::None,
                    Some(Err(error)) => {
                        state.error = Some(format!("terminal input: {error}"));
                        Command::None
                    }
                    None => Command::Quit,
                };
                if let Some(action) = handle_command(client, &mut state, command).await? {
                    return Ok(action);
                }
                sync_subscription(client, &mut state, &mut subscription).await;
            }
            subscription_event = next_subscription_event(&mut subscription) => {
                match subscription_event {
                    Ok(Some(event)) => {
                        let seq = event.seq;
                        if let Err(error) = state.ingest(&[event], seq) {
                            state.error = Some(error.to_string());
                        }
                    }
                    Ok(None) => {
                        subscription = None;
                        retry_at = Instant::now() + RETRY_INTERVAL;
                    }
                    Err(error) => {
                        state.error = Some(format!("output stream disconnected; retrying: {error}"));
                        subscription = None;
                        retry_at = Instant::now() + RETRY_INTERVAL;
                    }
                }
            }
            _ = poll.tick() => {
                match refresh(client, &mut state).await {
                    Ok(selection_changed) => {
                        state.error = None;
                        if selection_changed {
                            state.reset_output();
                            subscription = None;
                        }
                        if subscription.is_none() && Instant::now() >= retry_at {
                            load_output(client, &mut state).await;
                            subscription = subscribe_output(client, &mut state).await;
                        }
                    }
                    Err(error) => {
                        state.error = Some(format!("daemon unavailable; retrying: {error}"));
                        retry_at = Instant::now() + RETRY_INTERVAL;
                    }
                }
            }
        }
    }
}

async fn refresh(client: &DaemonClient, state: &mut OperatorState) -> Result<bool> {
    let workspace_id = state.workspace.id.clone();
    let (sessions, executions) = tokio::try_join!(
        client.list_agent_sessions(Some(workspace_id.clone()), LIST_LIMIT),
        client.list(Some(workspace_id), LIST_LIMIT),
    )?;
    if state
        .selected_session_id
        .as_ref()
        .is_some_and(|selected| !sessions.iter().any(|session| session.id == *selected))
    {
        state.selected_session_id = None;
    }
    let detail = match state.selected_session_id.clone() {
        Some(session_id) => Some(client.get_agent_session(session_id).await?),
        None => None,
    };
    Ok(state.apply_snapshot(sessions, executions, detail))
}

async fn load_output(client: &DaemonClient, state: &mut OperatorState) {
    let Some(execution_id) = state.selected_execution_id.clone() else {
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

async fn subscribe_output(
    client: &DaemonClient,
    state: &mut OperatorState,
) -> Option<ExecutionSubscription> {
    let execution_id = state.selected_execution_id.clone()?;
    match client.subscribe(execution_id, state.output_cursor).await {
        Ok(subscription) => Some(subscription),
        Err(error) => {
            state.error = Some(format!("output stream unavailable; retrying: {error}"));
            None
        }
    }
}

async fn sync_subscription(
    client: &DaemonClient,
    state: &mut OperatorState,
    subscription: &mut Option<ExecutionSubscription>,
) {
    let expected = state.selected_execution_id.as_deref();
    let current = subscription
        .as_ref()
        .map(|subscription| subscription.execution.id.as_str());
    if expected == current {
        return;
    }
    *subscription = None;
    load_output(client, state).await;
    *subscription = subscribe_output(client, state).await;
}

async fn next_subscription_event(
    subscription: &mut Option<ExecutionSubscription>,
) -> Result<Option<ExecutionEvent>> {
    match subscription {
        Some(subscription) => subscription.next_event().await,
        None => std::future::pending().await,
    }
}

async fn handle_command(
    client: &DaemonClient,
    state: &mut OperatorState,
    command: Command,
) -> Result<Option<UiAction>> {
    match command {
        Command::None => {}
        Command::Quit => return Ok(Some(UiAction::Quit)),
        Command::Refresh => {
            if refresh(client, state).await? {
                state.reset_output();
            }
        }
        Command::ReloadOutput => {}
        Command::Cancel(execution_id) => match client.cancel(execution_id).await {
            Ok(_) => state.notice = Some("cancel requested".into()),
            Err(error) => state.error = Some(format!("cancel failed: {error}")),
        },
        Command::OpenReplay(session_id) => {
            let detail = client.get_agent_session(session_id).await?;
            if detail.session.state == AgentSessionState::Recording {
                state.notice = Some("replay is available after recording finishes".into());
            } else {
                let cast = Path::new(&detail.session.cast_path);
                let html = Path::new(&detail.session.html_path);
                match write_replay_html(&detail, cast, html, &[]).and_then(|()| open_html(html)) {
                    Ok(()) => state.notice = Some(format!("opened {}", html.display())),
                    Err(error) => state.error = Some(format!("open failed: {error}")),
                }
            }
        }
        Command::ExportReplay(session_id) => {
            let detail = client.get_agent_session(session_id).await?;
            if detail.session.state == AgentSessionState::Recording {
                state.notice = Some("export is available after recording finishes".into());
            } else {
                let output = export_path(&detail.session.id)?;
                if output.exists() {
                    state.error = Some(format!("refusing to overwrite {}", output.display()));
                } else {
                    let cast = Path::new(&detail.session.cast_path);
                    match write_replay_html(&detail, cast, &output, &[]) {
                        Ok(()) => {
                            state.notice = Some(format!(
                                "exported {} (review for sensitive data)",
                                output.display()
                            ))
                        }
                        Err(error) => state.error = Some(format!("export failed: {error}")),
                    }
                }
            }
        }
        Command::PrepareHandoff(session_id) => {
            let detail = client.get_agent_session(session_id.clone()).await?;
            if detail.session.state == AgentSessionState::Recording {
                state.notice = Some("exit the source agent before handoff".into());
            } else if !detail
                .executions
                .iter()
                .any(|execution| !execution.state.is_terminal())
            {
                state.notice = Some("this session has no active durable work to hand off".into());
            } else {
                let provider = if detail.session.agent_kind == "codex" {
                    AgentProvider::Claude
                } else {
                    AgentProvider::Codex
                };
                let active = detail
                    .executions
                    .iter()
                    .filter(|execution| !execution.state.is_terminal())
                    .collect::<Vec<_>>();
                state.modal = Some(Modal::Agent {
                    purpose: AgentModalPurpose::Handoff {
                        source_session_id: session_id,
                        source_provider: detail.session.agent_kind.clone(),
                        active_count: active.len(),
                        command_preview: active
                            .first()
                            .map(|execution| truncate(&execution.command_display, 56))
                            .unwrap_or_default(),
                    },
                    provider,
                    field: AgentModalField::Provider,
                    prompt: String::new(),
                });
            }
        }
        Command::Launch(provider, prompt) => {
            return Ok(Some(UiAction::Launch { provider, prompt }));
        }
        Command::Handoff(provider, source_session_id) => {
            return Ok(Some(UiAction::Handoff {
                provider,
                source_session_id,
            }));
        }
    }
    Ok(None)
}

fn render(frame: &mut ratatui::Frame<'_>, state: &mut OperatorState) {
    state.hit_regions.clear();
    let area = frame.area();
    if area.width < MIN_WIDTH || area.height < MIN_HEIGHT {
        frame.render_widget(
            Paragraph::new(format!(
                "Terminal too small\nminimum: {MIN_WIDTH}x{MIN_HEIGHT}\ncurrent: {}x{}",
                area.width, area.height
            ))
            .alignment(Alignment::Center)
            .block(plain_block("Loomterm")),
            area,
        );
        return;
    }

    let sections = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(10),
        Constraint::Length(1),
    ])
    .split(area);
    render_header(frame, state, sections[0]);
    if area.width >= WIDE_WIDTH {
        let columns = Layout::horizontal([
            Constraint::Percentage(25),
            Constraint::Percentage(34),
            Constraint::Percentage(41),
        ])
        .split(sections[1]);
        render_sessions(frame, state, columns[0]);
        render_executions(frame, state, columns[1]);
        render_output(frame, state, columns[2]);
    } else if area.width >= MEDIUM_WIDTH {
        let columns = Layout::horizontal([Constraint::Percentage(34), Constraint::Percentage(66)])
            .split(sections[1]);
        render_sessions(frame, state, columns[0]);
        if state.focus == Focus::Output {
            render_output(frame, state, columns[1]);
        } else {
            render_executions(frame, state, columns[1]);
        }
    } else {
        match state.focus {
            Focus::Sessions => render_sessions(frame, state, sections[1]),
            Focus::Executions => render_executions(frame, state, sections[1]),
            Focus::Output => render_output(frame, state, sections[1]),
        }
    }
    render_footer(frame, state, sections[2]);
    render_modal(frame, state, area);
}

fn render_header(frame: &mut ratatui::Frame<'_>, state: &OperatorState, area: Rect) {
    let running = state
        .workspace_executions
        .iter()
        .filter(|execution| execution.state == ExecutionState::Running)
        .count();
    let recording = state
        .sessions
        .iter()
        .filter(|session| session.state == AgentSessionState::Recording)
        .count();
    let failed = state
        .workspace_executions
        .iter()
        .filter(|execution| execution_failed(execution))
        .count();
    let line = Line::from(vec![
        Span::styled(
            "LOOMTERM",
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(format!("  {}  ", state.workspace.name)),
        Span::styled(
            format!("{recording} recording"),
            Style::default().fg(Color::Cyan),
        ),
        Span::raw("  "),
        Span::styled(
            format!("{running} running"),
            Style::default().fg(Color::Yellow),
        ),
        Span::raw("  "),
        Span::styled(
            format!("{failed} failed"),
            if failed == 0 {
                Style::default().fg(Color::DarkGray)
            } else {
                Style::default().fg(Color::LightRed)
            },
        ),
    ]);
    let daemon = if state.error.is_some() {
        "Operator UI | daemon retrying"
    } else {
        "Operator UI | daemon connected"
    };
    frame.render_widget(Paragraph::new(line).block(plain_block(daemon)), area);
}

fn render_sessions(frame: &mut ratatui::Frame<'_>, state: &mut OperatorState, area: Rect) {
    state.hit_regions.push(HitRegion {
        area,
        target: HitTarget::Pane(Focus::Sessions),
    });
    let ids = state.visible_session_ids();
    let capacity = usize::from(area.height.saturating_sub(2)).max(1);
    let selected_index = ids
        .iter()
        .position(|id| *id == state.selected_session_id)
        .unwrap_or(0);
    state.session_offset =
        selection_offset(state.session_offset, selected_index, capacity, ids.len());
    let displayed = ids
        .into_iter()
        .skip(state.session_offset)
        .take(capacity)
        .collect::<Vec<_>>();
    let mut items = Vec::with_capacity(displayed.len());
    for (row, id) in displayed.into_iter().enumerate() {
        let selected = id == state.selected_session_id;
        let marker = if selected { ">" } else { " " };
        let line = match &id {
            None => Line::from(vec![
                Span::styled(
                    format!("{marker} ALL WORK"),
                    Style::default().add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!("  {}", state.workspace_executions.len()),
                    Style::default().fg(Color::DarkGray),
                ),
            ]),
            Some(id) => {
                let session = state
                    .sessions
                    .iter()
                    .find(|session| session.id == *id)
                    .expect("visible session must exist");
                let name = session.name.as_deref().unwrap_or(&session.command_display);
                Line::from(vec![
                    Span::styled(
                        format!(
                            "{marker} {:<6} {:<4}",
                            provider_label(&session.agent_kind),
                            session_label(&session.state)
                        ),
                        session_style(&session.state),
                    ),
                    Span::raw(format!(" {}", truncate(name, 24))),
                    Span::styled(
                        format!("  {}", format_age(session.created_at_ms)),
                        Style::default().fg(Color::DarkGray),
                    ),
                ])
            }
        };
        items.push(ListItem::new(line));
        state.hit_regions.push(HitRegion {
            area: Rect::new(
                area.x + 1,
                area.y + 1 + row as u16,
                area.width.saturating_sub(2),
                1,
            ),
            target: id.map_or(HitTarget::AllWork, HitTarget::Session),
        });
    }
    frame.render_widget(
        List::new(items).block(
            plain_block("Sessions").border_style(focus_style(state.focus == Focus::Sessions)),
        ),
        area,
    );
}

fn render_executions(frame: &mut ratatui::Frame<'_>, state: &mut OperatorState, area: Rect) {
    state.hit_regions.push(HitRegion {
        area,
        target: HitTarget::Pane(Focus::Executions),
    });
    let ids = state.visible_execution_ids();
    let capacity = usize::from(area.height.saturating_sub(2)).max(1);
    let selected_index = state
        .selected_execution_id
        .as_ref()
        .and_then(|selected| ids.iter().position(|id| id == selected))
        .unwrap_or(0);
    state.execution_offset =
        selection_offset(state.execution_offset, selected_index, capacity, ids.len());
    let displayed = ids
        .into_iter()
        .skip(state.execution_offset)
        .take(capacity)
        .collect::<Vec<_>>();
    let mut items = Vec::with_capacity(displayed.len().max(1));
    if displayed.is_empty() {
        items.push(ListItem::new(Line::styled(
            "No executions match this view",
            Style::default().fg(Color::DarkGray),
        )));
    }
    for (row, id) in displayed.into_iter().enumerate() {
        let execution = state
            .scoped_executions()
            .iter()
            .find(|execution| execution.id == id)
            .expect("visible execution must exist");
        let selected = state.selected_execution_id.as_deref() == Some(id.as_str());
        let marker = if selected { ">" } else { " " };
        let (status, style) = execution_status(execution);
        let handoff = state
            .selected_session_id
            .as_ref()
            .is_some_and(|session_id| {
                execution.initiator.session_id.as_deref() != Some(session_id.as_str())
            });
        let source = if handoff {
            "handoff"
        } else {
            execution
                .initiator
                .name
                .as_deref()
                .unwrap_or(&execution.initiator.kind)
        };
        items.push(ListItem::new(Line::from(vec![
            Span::styled(format!("{marker} {status:<9}"), style),
            Span::styled(
                format!(" {:<7}", truncate(source, 7)),
                Style::default().fg(Color::DarkGray),
            ),
            Span::raw(format!(" {}", execution.command_display)),
        ])));
        state.hit_regions.push(HitRegion {
            area: Rect::new(
                area.x + 1,
                area.y + 1 + row as u16,
                area.width.saturating_sub(2),
                1,
            ),
            target: HitTarget::Execution(id),
        });
    }
    let title = state
        .selected_session()
        .map_or("Executions | all work".to_owned(), |session| {
            format!("Executions | {}", provider_label(&session.agent_kind))
        });
    frame.render_widget(
        List::new(items)
            .block(plain_block(&title).border_style(focus_style(state.focus == Focus::Executions))),
        area,
    );
}

fn render_output(frame: &mut ratatui::Frame<'_>, state: &mut OperatorState, area: Rect) {
    state.hit_regions.push(HitRegion {
        area,
        target: HitTarget::Pane(Focus::Output),
    });
    let parts = Layout::vertical([Constraint::Length(8), Constraint::Min(1)]).split(area);
    let metadata = state.selected_execution().map_or_else(
        || vec![Line::from("No execution selected")],
        |execution| {
            let (status, status_style) = execution_status(execution);
            let request = state
                .detail
                .as_ref()
                .and_then(|detail| detail.turns.last())
                .map(|turn| {
                    truncate(
                        &turn.prompt.split_whitespace().collect::<Vec<_>>().join(" "),
                        72,
                    )
                });
            let source_relation = state
                .selected_session_id
                .as_ref()
                .is_some_and(|session_id| {
                    execution.initiator.session_id.as_deref() != Some(session_id.as_str())
                });
            vec![
                Line::styled(
                    execution.command_display.clone(),
                    Style::default().add_modifier(Modifier::BOLD),
                ),
                Line::from(vec![
                    Span::styled(status, status_style),
                    Span::raw(format!(
                        "  {}  {}",
                        format_outcome(execution.outcome.as_ref()),
                        format_duration(execution.duration_ms.unwrap_or_else(|| {
                            u64::try_from(now_ms().saturating_sub(
                                execution.started_at_ms.unwrap_or(execution.created_at_ms),
                            ))
                            .unwrap_or_default()
                        }))
                    )),
                ]),
                Line::from(format!("cwd  {}", execution.cwd)),
                Line::from(format!(
                    "source  {}{}",
                    if source_relation {
                        "handoff"
                    } else {
                        execution.initiator.kind.as_str()
                    },
                    execution
                        .initiator
                        .session_id
                        .as_deref()
                        .map(|id| format!(" | {}", short_id(id)))
                        .unwrap_or_default()
                )),
                Line::styled(
                    format!("id  {}", execution.id),
                    Style::default().fg(Color::DarkGray),
                ),
                Line::styled(
                    request.map_or_else(
                        || "request  unavailable".into(),
                        |request| format!("request  {request}"),
                    ),
                    Style::default().fg(Color::DarkGray),
                ),
            ]
        },
    );
    frame.render_widget(
        Paragraph::new(metadata)
            .wrap(Wrap { trim: true })
            .block(plain_block("Inspector")),
        parts[0],
    );
    let lines = state.output_lines();
    let viewport_height = usize::from(parts[1].height.saturating_sub(2));
    let max_scroll = lines.len().saturating_sub(viewport_height);
    if state.follow {
        state.output_scroll = max_scroll;
    } else {
        state.output_scroll = state.output_scroll.min(max_scroll);
    }
    let title = if state.follow {
        "Output | follow"
    } else {
        "Output"
    };
    frame.render_widget(
        Paragraph::new(lines)
            .scroll((u16::try_from(state.output_scroll).unwrap_or(u16::MAX), 0))
            .block(plain_block(title).border_style(focus_style(state.focus == Focus::Output))),
        parts[1],
    );
}

fn render_footer(frame: &mut ratatui::Frame<'_>, state: &OperatorState, area: Rect) {
    let (message, style) = if state.editing_search {
        (
            format!("/{}_  Enter apply  Esc close", state.query),
            Style::default().fg(Color::Cyan),
        )
    } else if let Some(error) = &state.error {
        (error.clone(), Style::default().fg(Color::LightRed))
    } else if let Some(notice) = &state.notice {
        (notice.clone(), Style::default().fg(Color::Yellow))
    } else if !state.query.is_empty() {
        (
            format!("filter: {}  Esc clear  / edit", state.query),
            Style::default().fg(Color::Cyan),
        )
    } else {
        (
            "Tab focus  / search  Ctrl-P actions  n new  h handoff  ? help  q quit".into(),
            Style::default().fg(Color::DarkGray),
        )
    };
    frame.render_widget(Paragraph::new(message).style(style), area);
}

fn render_modal(frame: &mut ratatui::Frame<'_>, state: &OperatorState, full: Rect) {
    let Some(modal) = &state.modal else {
        return;
    };
    match modal {
        Modal::Help => {
            let area = centered_rect(68, 18, full);
            frame.render_widget(Clear, area);
            frame.render_widget(
                Paragraph::new(vec![
                    Line::from("Tab / Shift-Tab   move focus"),
                    Line::from("Up/Down or j/k     navigate / scroll"),
                    Line::from("Enter              inspect selection"),
                    Line::from("/                  search everything"),
                    Line::from("Ctrl-P or :        command palette"),
                    Line::from("n / h              new agent / handoff"),
                    Line::from("c                  cancel active execution"),
                    Line::from("o / e              open / export replay"),
                    Line::from("f / r              follow output / refresh"),
                    Line::from("mouse              select and scroll"),
                    Line::from("q                  quit"),
                    Line::from(""),
                    Line::styled("Esc or ? closes help", Style::default().fg(Color::DarkGray)),
                ])
                .block(plain_block("Keyboard help").border_style(Style::default().fg(Color::Cyan))),
                area,
            );
        }
        Modal::ConfirmCancel(execution_id) => {
            let area = centered_rect(58, 7, full);
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
        Modal::Palette { selected } => {
            let area = centered_rect(58, 14, full);
            frame.render_widget(Clear, area);
            let items = PaletteAction::ALL
                .iter()
                .enumerate()
                .map(|(index, action)| {
                    let marker = if index == *selected { ">" } else { " " };
                    ListItem::new(Line::from(vec![
                        Span::styled(
                            format!("{marker} {:<28}", action.label()),
                            if index == *selected {
                                Style::default()
                                    .fg(Color::Cyan)
                                    .add_modifier(Modifier::BOLD)
                            } else {
                                Style::default()
                            },
                        ),
                        Span::styled(action.shortcut(), Style::default().fg(Color::DarkGray)),
                    ]))
                })
                .collect::<Vec<_>>();
            frame.render_widget(
                List::new(items).block(
                    plain_block("Command palette").border_style(Style::default().fg(Color::Cyan)),
                ),
                area,
            );
        }
        Modal::Agent {
            purpose,
            provider,
            field,
            prompt,
        } => {
            let height = if *purpose == AgentModalPurpose::Launch {
                11
            } else {
                12
            };
            let area = centered_rect(66, height, full);
            frame.render_widget(Clear, area);
            let codex = provider_segment("Codex", *provider == AgentProvider::Codex);
            let claude = provider_segment("Claude", *provider == AgentProvider::Claude);
            let mut lines = vec![
                Line::from(match purpose {
                    AgentModalPurpose::Launch => "Start a recorded agent in this workspace",
                    AgentModalPurpose::Handoff { .. } => {
                        "Continue active durable work with another agent"
                    }
                }),
                Line::from(""),
                Line::from(vec![
                    Span::raw("Provider  "),
                    codex,
                    Span::raw("  "),
                    claude,
                ]),
            ];
            if *purpose == AgentModalPurpose::Launch {
                lines.extend([
                    Line::from(""),
                    Line::from(vec![
                        Span::raw("Prompt    "),
                        Span::styled(
                            if prompt.is_empty() {
                                "optional initial request".to_owned()
                            } else {
                                prompt.clone()
                            },
                            if *field == AgentModalField::Prompt {
                                Style::default().fg(Color::Cyan)
                            } else {
                                Style::default().fg(Color::DarkGray)
                            },
                        ),
                    ]),
                ]);
            } else if let AgentModalPurpose::Handoff {
                source_session_id,
                source_provider,
                active_count,
                command_preview,
            } = purpose
            {
                lines.extend([
                    Line::from(""),
                    Line::from(format!(
                        "Source    {}  {}",
                        provider_label(source_provider),
                        short_id(source_session_id)
                    )),
                    Line::from(format!("Active    {active_count} execution(s)")),
                    Line::styled(
                        format!("Command   {command_preview}"),
                        Style::default().fg(Color::DarkGray),
                    ),
                ]);
            }
            lines.extend([
                Line::from(""),
                Line::styled(
                    if *purpose == AgentModalPurpose::Launch {
                        "Left/Right provider  Tab prompt  Enter start  Esc cancel"
                    } else {
                        "Left/Right provider  Enter handoff  Esc cancel"
                    },
                    Style::default().fg(Color::DarkGray),
                ),
            ]);
            let title = if *purpose == AgentModalPurpose::Launch {
                "New agent"
            } else {
                "Handoff"
            };
            frame.render_widget(
                Paragraph::new(lines)
                    .wrap(Wrap { trim: true })
                    .block(plain_block(title).border_style(Style::default().fg(Color::Cyan))),
                area,
            );
        }
    }
}

fn provider_segment(label: &str, selected: bool) -> Span<'static> {
    Span::styled(
        format!("[ {label} ]"),
        if selected {
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::DarkGray)
        },
    )
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

fn execution_failed(execution: &Execution) -> bool {
    matches!(execution_status(execution).0, "failed")
}

fn session_style(state: &AgentSessionState) -> Style {
    match state {
        AgentSessionState::Recording => Style::default().fg(Color::Cyan),
        AgentSessionState::Finished => Style::default().fg(Color::Green),
        AgentSessionState::Interrupted => Style::default().fg(Color::LightRed),
    }
}

fn session_label(state: &AgentSessionState) -> &'static str {
    match state {
        AgentSessionState::Recording => "REC",
        AgentSessionState::Finished => "DONE",
        AgentSessionState::Interrupted => "STOP",
    }
}

fn provider_label(provider: &str) -> String {
    match provider {
        "codex" => "CODEX".into(),
        "claude" => "CLAUDE".into(),
        other => other.to_ascii_uppercase(),
    }
}

fn format_outcome(outcome: Option<&ExecutionOutcome>) -> String {
    match outcome {
        Some(ExecutionOutcome::Exited { code }) => format!("exit {code}"),
        Some(ExecutionOutcome::Signaled { signal }) => format!("signal {signal}"),
        Some(ExecutionOutcome::SpawnError { message }) => format!("spawn error: {message}"),
        Some(ExecutionOutcome::Cancelled { signal }) => signal
            .map(|value| format!("cancelled | signal {value}"))
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

fn format_age(timestamp_ms: i64) -> String {
    let seconds = u64::try_from(now_ms().saturating_sub(timestamp_ms)).unwrap_or_default() / 1_000;
    if seconds < 60 {
        format!("{seconds}s")
    } else if seconds < 3_600 {
        format!("{}m", seconds / 60)
    } else if seconds < 86_400 {
        format!("{}h", seconds / 3_600)
    } else {
        format!("{}d", seconds / 86_400)
    }
}

fn truncate(value: &str, max_chars: usize) -> String {
    let mut characters = value.chars();
    let truncated = characters.by_ref().take(max_chars).collect::<String>();
    if characters.next().is_some() {
        format!("{truncated}...")
    } else {
        truncated
    }
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

fn contains(area: Rect, x: u16, y: u16) -> bool {
    x >= area.x
        && x < area.x.saturating_add(area.width)
        && y >= area.y
        && y < area.y.saturating_add(area.height)
}

fn selection_offset(current: usize, selected: usize, capacity: usize, item_count: usize) -> usize {
    let mut offset = current.min(item_count.saturating_sub(capacity));
    if selected < offset {
        offset = selected;
    } else if selected >= offset.saturating_add(capacity) {
        offset = selected.saturating_add(1).saturating_sub(capacity);
    }
    offset
}

fn export_path(session_id: &str) -> Result<PathBuf> {
    Ok(std::env::current_dir()?.join(format!("loomterm-session-{session_id}.html")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{CommandSpec, Initiator};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn workspace() -> Workspace {
        Workspace {
            id: "workspace".into(),
            name: "loomterm".into(),
            root: "/tmp/loomterm".into(),
            created_at_ms: 1,
        }
    }

    fn session(id: &str, provider: &str, state: AgentSessionState) -> AgentSession {
        AgentSession {
            id: id.into(),
            workspace_id: "workspace".into(),
            state,
            agent_kind: provider.into(),
            name: Some(format!("{provider} work")),
            command: CommandSpec::Argv {
                program: provider.into(),
                args: Vec::new(),
            },
            command_display: provider.into(),
            cwd: "/tmp/loomterm".into(),
            created_at_ms: 10,
            ended_at_ms: None,
            duration_ms: None,
            recorder_pid: 1,
            outcome: None,
            captured_bytes: 0,
            output_truncated: false,
            initial_cols: 80,
            initial_rows: 24,
            cast_path: "session.cast".into(),
            html_path: "session.html".into(),
        }
    }

    fn execution(id: &str, session_id: Option<&str>, state: ExecutionState) -> Execution {
        Execution {
            id: id.into(),
            workspace_id: "workspace".into(),
            state,
            command: CommandSpec::Argv {
                program: "cargo".into(),
                args: vec!["test".into()],
            },
            command_display: format!("cargo test --test {id}"),
            cwd: "/tmp/loomterm".into(),
            env_keys: Vec::new(),
            initiator: Initiator {
                kind: "mcp".into(),
                name: Some("loomterm".into()),
                session_id: session_id.map(str::to_owned),
            },
            created_at_ms: 20,
            started_at_ms: Some(20),
            ended_at_ms: None,
            duration_ms: None,
            pid: Some(1),
            pgid: Some(1),
            outcome: None,
            captured_bytes: 0,
            output_truncated: false,
            last_seq: 0,
        }
    }

    fn state() -> OperatorState {
        let mut state = OperatorState::new(UiOptions {
            workspace: workspace(),
            selected_session_id: None,
            notice: None,
        });
        let sessions = vec![session("session-1", "codex", AgentSessionState::Finished)];
        let executions = vec![
            execution("linked", Some("session-1"), ExecutionState::Running),
            execution("unowned", None, ExecutionState::Finished),
        ];
        state.apply_snapshot(sessions, executions, None);
        state
    }

    #[test]
    fn all_work_includes_sessionless_executions() {
        let state = state();
        assert_eq!(
            state.visible_session_ids(),
            vec![None, Some("session-1".into())]
        );
        assert_eq!(state.visible_execution_ids().len(), 2);
        assert!(
            state
                .visible_execution_ids()
                .contains(&"unowned".to_owned())
        );
    }

    #[test]
    fn search_filters_commands_and_preserves_all_work() {
        let mut state = state();
        state.query = "unowned".into();
        state.ensure_execution_selection();
        assert_eq!(state.visible_execution_ids(), ["unowned"]);
        assert_eq!(state.visible_session_ids().first(), Some(&None));
    }

    #[test]
    fn cancel_requires_an_active_execution() {
        let mut state = state();
        state.selected_execution_id = Some("unowned".into());
        assert_eq!(state.invoke(PaletteAction::Cancel), Command::None);
        assert!(
            state
                .notice
                .as_deref()
                .unwrap()
                .contains("queued or running")
        );
        state.selected_execution_id = Some("linked".into());
        state.invoke(PaletteAction::Cancel);
        assert_eq!(state.modal, Some(Modal::ConfirmCancel("linked".into())));
    }

    #[test]
    fn renders_responsive_layouts_without_overflow() {
        for (width, height) in [(80, 24), (120, 32), (160, 40)] {
            let backend = TestBackend::new(width, height);
            let mut terminal = Terminal::new(backend).unwrap();
            let mut state = state();
            terminal.draw(|frame| render(frame, &mut state)).unwrap();
            let buffer = terminal.backend().buffer();
            let rendered = buffer
                .content()
                .iter()
                .map(|cell| cell.symbol())
                .collect::<String>();
            assert!(rendered.contains("LOOMTERM"));
            assert!(rendered.contains("ALL WORK"));
            assert!(!state.hit_regions.is_empty());
        }
    }

    #[test]
    fn provider_modal_returns_launch_request() {
        let mut state = state();
        state.invoke(PaletteAction::NewAgent);
        let command = state.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(command, Command::Launch(AgentProvider::Codex, None));
    }

    #[test]
    fn target_session_marks_cross_session_execution_as_handoff() {
        let target = session("target", "claude", AgentSessionState::Recording);
        let linked = execution("linked", Some("source"), ExecutionState::Running);
        let detail = AgentSessionDetail {
            session: target.clone(),
            executions: vec![linked.clone()],
            turns: Vec::new(),
            actions: Vec::new(),
        };
        let mut state = OperatorState::new(UiOptions {
            workspace: workspace(),
            selected_session_id: Some(target.id.clone()),
            notice: None,
        });
        state.apply_snapshot(vec![target], vec![linked], Some(detail));
        state.focus = Focus::Executions;

        let backend = TestBackend::new(160, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(frame, &mut state)).unwrap();
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("handoff"));
        assert!(rendered.contains("Executions | CLAUDE"));
    }

    #[test]
    fn list_offset_keeps_the_selection_visible() {
        assert_eq!(selection_offset(0, 12, 5, 20), 8);
        assert_eq!(selection_offset(8, 2, 5, 20), 2);
        assert_eq!(selection_offset(18, 19, 5, 20), 15);
    }
}
