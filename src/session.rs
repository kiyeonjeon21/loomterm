//! Interactive agent session recording and replay export.

use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::fd::AsFd;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::Engine;
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, size};
use fs4::FileExt;
use nix::errno::Errno;
use nix::poll::{PollFd, PollFlags, poll};
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use serde_json::Value;
use signal_hook::consts::signal::{SIGHUP, SIGINT, SIGTERM, SIGWINCH};

use crate::config::AppPaths;
use crate::model::{AgentSessionDetail, CommandSpec, ExecutionOutcome};
use crate::{Error, Result};

const PLAYER_JS: &str = include_str!("../vendor/asciinema-player/asciinema-player.min.js");
const PLAYER_CSS: &str = include_str!("../vendor/asciinema-player/asciinema-player.css");

pub struct SessionArtifacts {
    pub directory: PathBuf,
    pub cast_path: PathBuf,
    pub html_path: PathBuf,
    _lock: File,
}

impl SessionArtifacts {
    pub fn create(paths: &AppPaths, session_id: &str) -> Result<Self> {
        let directory = paths.sessions_dir.join(session_id);
        std::fs::create_dir(&directory)?;
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700))?;
        let lock_path = directory.join("active.lock");
        let lock = private_file(&lock_path, true)?;
        FileExt::lock(&lock)?;
        Ok(Self {
            cast_path: directory.join("recording.cast"),
            html_path: directory.join("replay.html"),
            directory,
            _lock: lock,
        })
    }
}

pub struct RecordSpec {
    pub command: CommandSpec,
    pub cwd: PathBuf,
    pub session_id: String,
    pub agent_kind: String,
    pub cast_path: PathBuf,
    pub initial_cols: u16,
    pub initial_rows: u16,
    pub capture_limit_bytes: u64,
    pub env: BTreeMap<String, String>,
}

pub struct RecordResult {
    pub outcome: ExecutionOutcome,
    pub captured_bytes: u64,
    pub output_truncated: bool,
    pub exit_code: i32,
}

pub fn terminal_size() -> Result<(u16, u16)> {
    let (cols, rows) = size()?;
    Ok((
        if cols == 0 { 80 } else { cols },
        if rows == 0 { 24 } else { rows },
    ))
}

pub fn record(spec: RecordSpec) -> Result<RecordResult> {
    if !std::io::IsTerminal::is_terminal(&std::io::stdin())
        || !std::io::IsTerminal::is_terminal(&std::io::stdout())
    {
        return Err(Error::InvalidRequest(
            "session recording requires terminal stdin and stdout".into(),
        ));
    }

    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows: spec.initial_rows,
            cols: spec.initial_cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|error| Error::Config(format!("failed to open PTY: {error}")))?;
    let cast_file = private_file(&spec.cast_path, true)?;
    let cast = Arc::new(Mutex::new(CastWriter::new(
        cast_file,
        spec.initial_cols,
        spec.initial_rows,
        &spec.command.display(),
    )?));
    let mut command = command_builder(&spec.command)?;
    command.cwd(&spec.cwd);
    command.env("LOOMTERM_SESSION_ID", &spec.session_id);
    command.env("LOOMTERM_AGENT_KIND", &spec.agent_kind);
    for (key, value) in &spec.env {
        command.env(key, value);
    }
    let child = pair
        .slave
        .spawn_command(command)
        .map_err(|error| Error::Config(format!("failed to start agent: {error}")))?;
    let mut child = ChildGuard::new(child);
    drop(pair.slave);
    let captured = Arc::new(AtomicU64::new(0));
    let truncated = Arc::new(AtomicBool::new(false));

    let reader = pair
        .master
        .try_clone_reader()
        .map_err(|error| Error::Config(format!("failed to clone PTY reader: {error}")))?;
    let output_cast = Arc::clone(&cast);
    let output_captured = Arc::clone(&captured);
    let output_truncated = Arc::clone(&truncated);
    let capture_limit = spec.capture_limit_bytes;
    let output_thread = std::thread::spawn(move || {
        capture_output(
            reader,
            output_cast,
            output_captured,
            output_truncated,
            capture_limit,
        )
    });

    let writer = pair
        .master
        .take_writer()
        .map_err(|error| Error::Config(format!("failed to open PTY writer: {error}")))?;
    let _raw_mode = RawModeGuard::enable()?;
    let input_relay = InputRelay::start(writer);
    let resized = Arc::new(AtomicBool::new(false));
    let terminated = Arc::new(AtomicBool::new(false));
    signal_hook::flag::register(SIGWINCH, Arc::clone(&resized))?;
    signal_hook::flag::register(SIGTERM, Arc::clone(&terminated))?;
    signal_hook::flag::register(SIGHUP, Arc::clone(&terminated))?;
    signal_hook::flag::register(SIGINT, Arc::clone(&terminated))?;

    let mut interrupted = false;
    let status = loop {
        if resized.swap(false, Ordering::SeqCst)
            && let Ok((cols, rows)) = size()
            && cols > 0
            && rows > 0
        {
            let dimensions = PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            };
            let _ = pair.master.resize(dimensions);
            cast.lock()
                .map_err(|_| Error::Config("cast writer lock was poisoned".into()))?
                .event("r", format!("{cols}x{rows}"))?;
        }
        if input_relay.is_closed() || !std::io::IsTerminal::is_terminal(&std::io::stdin()) {
            interrupted = true;
            terminate_child_group(&mut *child)?;
        }
        if terminated.swap(false, Ordering::SeqCst) {
            interrupted = true;
            terminate_child_group(&mut *child)?;
        }
        if let Some(status) = child.try_wait()? {
            break status;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    child.disarm();

    input_relay.finish()?;
    output_thread
        .join()
        .map_err(|_| Error::Config("PTY output thread panicked".into()))??;
    let outcome = if interrupted {
        ExecutionOutcome::Interrupted {
            reason: "session recorder received a termination signal".into(),
        }
    } else if let Some(signal) = status.signal() {
        ExecutionOutcome::Signaled {
            signal: signal_number(signal),
        }
    } else {
        ExecutionOutcome::Exited {
            code: status.exit_code() as i32,
        }
    };
    let exit_code = match &outcome {
        ExecutionOutcome::Exited { code } => *code,
        ExecutionOutcome::Signaled { signal } => 128 + *signal,
        ExecutionOutcome::Interrupted { .. } => 130,
        _ => 1,
    };
    cast.lock()
        .map_err(|_| Error::Config("cast writer lock was poisoned".into()))?
        .event("x", exit_code.to_string())?;

    Ok(RecordResult {
        outcome,
        captured_bytes: captured.load(Ordering::Relaxed),
        output_truncated: truncated.load(Ordering::Relaxed),
        exit_code,
    })
}

struct InputRelay {
    stop: Arc<AtomicBool>,
    closed: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<Result<()>>>,
}

impl InputRelay {
    fn start<W>(mut writer: W) -> Self
    where
        W: Write + Send + 'static,
    {
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let closed = Arc::new(AtomicBool::new(false));
        let thread_closed = Arc::clone(&closed);
        let handle = std::thread::spawn(move || {
            let result = relay_input(&mut writer, &thread_stop);
            thread_closed.store(true, Ordering::Release);
            result
        });
        Self {
            stop,
            closed,
            handle: Some(handle),
        }
    }

    fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    fn finish(mut self) -> Result<()> {
        self.stop.store(true, Ordering::Relaxed);
        self.join()
    }

    fn join(&mut self) -> Result<()> {
        let Some(handle) = self.handle.take() else {
            return Ok(());
        };
        handle
            .join()
            .map_err(|_| Error::Config("PTY input thread panicked".into()))?
    }
}

impl Drop for InputRelay {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = self.join();
    }
}

fn relay_input(writer: &mut dyn Write, stop: &AtomicBool) -> Result<()> {
    let stdin = std::io::stdin();
    let mut stdin = stdin.lock();
    let mut buffer = [0_u8; 8192];
    while !stop.load(Ordering::Relaxed) {
        let events = {
            let mut poll_fd = [PollFd::new(stdin.as_fd(), PollFlags::POLLIN)];
            match poll(&mut poll_fd, 50_u16) {
                Ok(0) => continue,
                Ok(_) => poll_fd[0].revents().unwrap_or_else(PollFlags::empty),
                Err(Errno::EINTR) => continue,
                Err(error) => {
                    return Err(Error::Io(std::io::Error::from_raw_os_error(error as i32)));
                }
            }
        };
        if events.intersects(PollFlags::POLLERR | PollFlags::POLLHUP | PollFlags::POLLNVAL) {
            return Ok(());
        }
        if !events.contains(PollFlags::POLLIN) {
            continue;
        }
        let count = stdin.read(&mut buffer)?;
        if count == 0 || writer.write_all(&buffer[..count]).is_err() {
            return Ok(());
        }
        if writer.flush().is_err() {
            return Ok(());
        }
    }
    Ok(())
}

fn command_builder(command: &CommandSpec) -> Result<CommandBuilder> {
    match command {
        CommandSpec::Argv { program, args } => {
            let mut builder = CommandBuilder::new(program);
            builder.args(args);
            Ok(builder)
        }
        CommandSpec::Shell { command, shell } => {
            let mut builder = CommandBuilder::new(shell.as_deref().unwrap_or("/bin/sh"));
            builder.args(["-c", command]);
            Ok(builder)
        }
    }
}

fn terminate_child_group(child: &mut dyn portable_pty::Child) -> Result<()> {
    if let Some(pid) = child.process_id() {
        let group = nix::unistd::Pid::from_raw(-(pid as i32));
        let _ = nix::sys::signal::kill(group, nix::sys::signal::Signal::SIGTERM);
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if child.try_wait()?.is_some() {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    child.kill()?;
    let _ = child.wait()?;
    Ok(())
}

struct ChildGuard {
    child: Box<dyn portable_pty::Child + Send + Sync>,
    armed: bool,
}

impl ChildGuard {
    fn new(child: Box<dyn portable_pty::Child + Send + Sync>) -> Self {
        Self { child, armed: true }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl std::ops::Deref for ChildGuard {
    type Target = dyn portable_pty::Child + Send + Sync;

    fn deref(&self) -> &Self::Target {
        &*self.child
    }
}

impl std::ops::DerefMut for ChildGuard {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut *self.child
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if self.armed {
            let _ = terminate_child_group(&mut *self.child);
        }
    }
}

fn signal_number(signal: &str) -> i32 {
    let signal = signal.to_ascii_lowercase();
    if signal.contains("hangup") {
        1
    } else if signal.contains("interrupt") {
        2
    } else if signal.contains("quit") {
        3
    } else if signal.contains("kill") {
        9
    } else if signal.contains("term") {
        15
    } else {
        1
    }
}

fn capture_output(
    mut reader: Box<dyn Read + Send>,
    cast: Arc<Mutex<CastWriter>>,
    captured: Arc<AtomicU64>,
    truncated: Arc<AtomicBool>,
    limit: u64,
) -> Result<()> {
    let mut stdout = std::io::stdout().lock();
    let mut decoder = Utf8Decoder::default();
    let mut buffer = [0_u8; 32 * 1024];
    loop {
        let count = reader.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        stdout.write_all(&buffer[..count])?;
        stdout.flush()?;
        let current = captured.load(Ordering::Relaxed);
        let remaining = limit.saturating_sub(current) as usize;
        let record_count = remaining.min(count);
        if record_count > 0 {
            captured.fetch_add(record_count as u64, Ordering::Relaxed);
            for text in decoder.push(&buffer[..record_count]) {
                cast.lock()
                    .map_err(|_| Error::Config("cast writer lock was poisoned".into()))?
                    .event("o", text)?;
            }
        }
        if record_count < count {
            truncated.store(true, Ordering::Relaxed);
        }
    }
    if let Some(text) = decoder.finish() {
        cast.lock()
            .map_err(|_| Error::Config("cast writer lock was poisoned".into()))?
            .event("o", text)?;
    }
    Ok(())
}

struct CastWriter {
    file: File,
    started: Instant,
    previous: Instant,
}

impl CastWriter {
    fn new(file: File, cols: u16, rows: u16, command: &str) -> Result<Self> {
        let now = Instant::now();
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let header = serde_json::json!({
            "version": 3,
            "term": {"cols": cols, "rows": rows, "type": std::env::var("TERM").unwrap_or_else(|_| "xterm-256color".into())},
            "timestamp": timestamp,
            "command": command,
            "title": "Loomterm agent session",
        });
        let mut writer = Self {
            file,
            started: now,
            previous: now,
        };
        serde_json::to_writer(&mut writer.file, &header)?;
        writer.file.write_all(b"\n")?;
        writer.file.flush()?;
        Ok(writer)
    }

    fn event(&mut self, code: &str, data: String) -> Result<()> {
        let now = Instant::now();
        let interval = if self.previous == self.started {
            now.duration_since(self.started).as_secs_f64()
        } else {
            now.duration_since(self.previous).as_secs_f64()
        };
        self.previous = now;
        serde_json::to_writer(&mut self.file, &(round_millis(interval), code, data))?;
        self.file.write_all(b"\n")?;
        self.file.flush()?;
        Ok(())
    }
}

fn round_millis(value: f64) -> f64 {
    (value * 1000.0).round() / 1000.0
}

#[derive(Default)]
struct Utf8Decoder {
    pending: Vec<u8>,
}

impl Utf8Decoder {
    fn push(&mut self, bytes: &[u8]) -> Vec<String> {
        self.pending.extend_from_slice(bytes);
        let mut output = Vec::new();
        loop {
            match std::str::from_utf8(&self.pending) {
                Ok(text) => {
                    if !text.is_empty() {
                        output.push(text.to_owned());
                    }
                    self.pending.clear();
                    break;
                }
                Err(error) => {
                    let valid = error.valid_up_to();
                    if valid > 0 {
                        output.push(String::from_utf8_lossy(&self.pending[..valid]).into_owned());
                        self.pending.drain(..valid);
                    }
                    if let Some(length) = error.error_len() {
                        output.push("�".into());
                        self.pending.drain(..length);
                    } else {
                        break;
                    }
                }
            }
        }
        output
    }

    fn finish(self) -> Option<String> {
        (!self.pending.is_empty()).then(|| String::from_utf8_lossy(&self.pending).into_owned())
    }
}

struct RawModeGuard;

impl RawModeGuard {
    fn enable() -> Result<Self> {
        enable_raw_mode()?;
        Ok(Self)
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
    }
}

pub fn write_replay_html(
    detail: &AgentSessionDetail,
    cast_path: &Path,
    output: &Path,
    redactions: &[String],
) -> Result<()> {
    let cast = sanitize_cast(cast_path, redactions)?;
    let mut detail = detail.clone();
    redact_detail(&mut detail, redactions);
    let cast_base64 = base64::engine::general_purpose::STANDARD.encode(cast);
    let detail_base64 =
        base64::engine::general_purpose::STANDARD.encode(serde_json::to_vec(&detail)?);
    let html = html_document(&cast_base64, &detail_base64);
    write_private(output, html.as_bytes())
}

pub fn export_cast(cast_path: &Path, output: &Path, redactions: &[String]) -> Result<()> {
    let cast = sanitize_cast(cast_path, redactions)?;
    write_private(output, cast.as_bytes())
}

pub fn open_html(path: &Path) -> Result<()> {
    #[cfg(target_os = "macos")]
    let program = "open";
    #[cfg(not(target_os = "macos"))]
    let program = "xdg-open";
    std::process::Command::new(program).arg(path).spawn()?;
    Ok(())
}

fn sanitize_cast(path: &Path, redactions: &[String]) -> Result<String> {
    let home = std::env::var("HOME").ok();
    let mut replacements = Vec::new();
    if let Some(home) = home.filter(|value| !value.is_empty()) {
        replacements.push((home, "~".to_owned()));
    }
    replacements.extend(
        redactions
            .iter()
            .filter(|value| !value.is_empty())
            .cloned()
            .map(|value| (value, "[REDACTED]".to_owned())),
    );
    let input = BufReader::new(File::open(path)?);
    let mut values = Vec::new();
    for line in input.lines() {
        values.push(serde_json::from_str::<Value>(&line?)?);
    }
    redact_output_stream(&mut values, &replacements);
    let mut output = String::new();
    for mut value in values {
        redact_json(&mut value, &replacements);
        output.push_str(&serde_json::to_string(&value)?);
        output.push('\n');
    }
    Ok(output)
}

fn redact_output_stream(values: &mut [Value], replacements: &[(String, String)]) {
    let mut stream = String::new();
    let mut ranges = Vec::new();
    for (index, value) in values.iter().enumerate() {
        let Some(event) = value.as_array() else {
            continue;
        };
        if event.get(1).and_then(Value::as_str) != Some("o") {
            continue;
        }
        let Some(data) = event.get(2).and_then(Value::as_str) else {
            continue;
        };
        let start = stream.len();
        stream.push_str(data);
        ranges.push((index, start, stream.len()));
    }
    if stream.is_empty() || replacements.is_empty() {
        return;
    }

    let mut matches = Vec::new();
    let mut position = 0;
    while position < stream.len() {
        let matched = replacements
            .iter()
            .filter(|(pattern, _)| !pattern.is_empty() && stream[position..].starts_with(pattern))
            .max_by_key(|(pattern, _)| pattern.len());
        if let Some((pattern, replacement)) = matched {
            matches.push((position, position + pattern.len(), replacement.as_str()));
            position += pattern.len();
        } else {
            position += stream[position..].chars().next().map_or(1, char::len_utf8);
        }
    }

    for (index, start, end) in ranges {
        let mut redacted = String::new();
        let mut cursor = start;
        for (match_start, match_end, replacement) in &matches {
            if *match_end <= start || *match_start >= end {
                continue;
            }
            if *match_start >= start {
                if cursor < *match_start {
                    redacted.push_str(&stream[cursor..*match_start]);
                }
                redacted.push_str(replacement);
            }
            cursor = cursor.max(*match_end);
        }
        if cursor < end {
            redacted.push_str(&stream[cursor..end]);
        }
        if let Some(event) = values[index].as_array_mut() {
            event[2] = Value::String(redacted);
        }
    }
}

fn redact_detail(detail: &mut AgentSessionDetail, redactions: &[String]) {
    let mut replacements = Vec::new();
    if let Ok(home) = std::env::var("HOME")
        && !home.is_empty()
    {
        replacements.push((home, "~".to_owned()));
    }
    replacements.extend(
        redactions
            .iter()
            .filter(|value| !value.is_empty())
            .cloned()
            .map(|value| (value, "[REDACTED]".to_owned())),
    );
    let mut value = serde_json::to_value(&*detail).unwrap_or(Value::Null);
    redact_json(&mut value, &replacements);
    if let Ok(redacted) = serde_json::from_value(value) {
        *detail = redacted;
    }
}

fn redact_json(value: &mut Value, replacements: &[(String, String)]) {
    match value {
        Value::String(text) => {
            for (pattern, replacement) in replacements {
                *text = text.replace(pattern, replacement);
            }
        }
        Value::Array(values) => {
            for value in values {
                redact_json(value, replacements);
            }
        }
        Value::Object(values) => {
            for value in values.values_mut() {
                redact_json(value, replacements);
            }
        }
        _ => {}
    }
}

fn html_document(cast_base64: &str, detail_base64: &str) -> String {
    format!(
        r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>Loomterm session replay</title>
<style>{PLAYER_CSS}
:root {{ color-scheme: dark; font-family: Inter, ui-sans-serif, system-ui, sans-serif; }}
* {{ box-sizing: border-box; }}
body {{ margin: 0; background: #101214; color: #f3f4f6; min-height: 100vh; }}
header {{ border-bottom: 1px solid #30343a; padding: 18px 24px; display: flex; gap: 18px; align-items: baseline; }}
h1 {{ font-size: 18px; margin: 0; letter-spacing: 0; }}
#meta {{ color: #9ca3af; font: 13px ui-monospace, monospace; }}
main {{ display: grid; grid-template-columns: minmax(0, 1fr) 340px; min-height: calc(100vh - 62px); }}
#player-wrap {{ padding: 24px; min-width: 0; overflow: auto; }}
#player {{ max-width: 1180px; margin: 0 auto; }}
aside {{ border-left: 1px solid #30343a; padding: 20px 0; overflow: auto; }}
h2 {{ font-size: 13px; text-transform: uppercase; color: #9ca3af; padding: 0 18px 12px; margin: 0; letter-spacing: 0; }}
.execution {{ appearance: none; background: transparent; color: inherit; border: 0; border-top: 1px solid #262a2f; display: block; text-align: left; width: 100%; padding: 13px 18px; cursor: pointer; }}
.request {{ appearance: none; background: #171a1d; color: inherit; border: 0; border-top: 1px solid #30343a; display: block; text-align: left; width: 100%; padding: 13px 18px; cursor: pointer; }}
.execution:hover, .execution:focus-visible, .request:hover, .request:focus-visible {{ background: #1f2428; outline: none; }}
.command {{ display: block; font: 13px ui-monospace, monospace; white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }}
.prompt {{ display: -webkit-box; font-size: 13px; line-height: 1.45; overflow: hidden; -webkit-line-clamp: 3; -webkit-box-orient: vertical; }}
.facts {{ display: flex; justify-content: space-between; margin-top: 7px; color: #9ca3af; font-size: 12px; }}
.empty {{ color: #9ca3af; padding: 10px 18px; font-size: 13px; }}
@media (max-width: 820px) {{ main {{ grid-template-columns: 1fr; }} aside {{ border-left: 0; border-top: 1px solid #30343a; }} #player-wrap {{ padding: 14px; }} }}
</style>
</head>
<body>
<header><h1 id="title">Loomterm session</h1><div id="meta"></div></header>
<main><section id="player-wrap"><div id="player"></div></section><aside><h2>Agent requests</h2><div id="requests"></div><h2 style="padding-top:20px">Executions</h2><div id="timeline"></div></aside></main>
<script>{PLAYER_JS}</script>
<script>
const decode = value => new TextDecoder().decode(Uint8Array.from(atob(value), c => c.charCodeAt(0)));
const cast = decode('{cast_base64}');
const detail = JSON.parse(decode('{detail_base64}'));
const player = AsciinemaPlayer.create({{data: cast}}, document.getElementById('player'), {{fit: 'width', idleTimeLimit: 2, autoplay: true}});
document.getElementById('title').textContent = detail.session.name || `${{detail.session.agent_kind}} session`;
document.getElementById('meta').textContent = `${{detail.session.state}} · ${{Math.round((detail.session.duration_ms || 0) / 1000)}}s`;
const timeline = document.getElementById('timeline');
const requests = document.getElementById('requests');
if (!(detail.turns || []).length) requests.innerHTML = '<div class="empty">No structured agent requests captured.</div>';
for (const turn of detail.turns || []) {{
  const button = document.createElement('button');
  const actions = (detail.actions || []).filter(action => action.turn_id === turn.id);
  const offset = Math.max(0, (turn.created_at_ms - detail.session.created_at_ms) / 1000);
  button.className = 'request';
  button.innerHTML = `<span class="prompt"></span><span class="facts"><span>${{turn.provider}} · ${{turn.state}}</span><span>${{actions.length}} actions</span></span>`;
  button.querySelector('.prompt').textContent = turn.prompt || 'Prompt unavailable';
  button.addEventListener('click', () => player.seek(offset));
  requests.appendChild(button);
}}
if (!detail.executions.length) timeline.innerHTML = '<div class="empty">No correlated Loomterm executions.</div>';
for (const execution of detail.executions) {{
  const button = document.createElement('button');
  button.className = 'execution';
  const offset = Math.max(0, (execution.created_at_ms - detail.session.created_at_ms) / 1000);
  const relation = execution.initiator.session_id === detail.session.id ? execution.state : `${{execution.state}} · handoff`;
  button.innerHTML = `<span class="command"></span><span class="facts"><span>${{relation}}</span><span>${{execution.duration_ms || 0}} ms</span></span>`;
  button.querySelector('.command').textContent = execution.command_display;
  button.addEventListener('click', () => player.seek(offset));
  timeline.appendChild(button);
}}
</script>
</body>
</html>"#
    )
}

fn private_file(path: &Path, truncate: bool) -> Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(truncate)
        .open(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(file)
}

fn write_private(path: &Path, data: &[u8]) -> Result<()> {
    let mut file = private_file(path, true)?;
    file.write_all(data)?;
    file.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf8_decoder_preserves_split_sequences() {
        let mut decoder = Utf8Decoder::default();
        assert!(decoder.push(&[0xed, 0x95]).is_empty());
        assert_eq!(decoder.push(&[0x9c]), vec!["한"]);
        assert_eq!(decoder.finish(), None);
    }

    #[test]
    fn redaction_walks_nested_json() {
        let mut value = serde_json::json!({"a": ["secret", {"b": "x-secret-y"}]});
        redact_json(&mut value, &[("secret".into(), "[REDACTED]".into())]);
        assert_eq!(value["a"][0], "[REDACTED]");
        assert_eq!(value["a"][1]["b"], "x-[REDACTED]-y");
    }

    #[test]
    fn redaction_crosses_cast_event_boundaries() {
        let mut values = vec![
            serde_json::json!({"version": 3, "term": {"cols": 80, "rows": 24}}),
            serde_json::json!([0.1, "o", "sec"]),
            serde_json::json!([0.1, "o", "ret value"]),
        ];
        redact_output_stream(&mut values, &[("secret".into(), "[REDACTED]".into())]);
        assert_eq!(values[1][2], "[REDACTED]");
        assert_eq!(values[2][2], " value");
    }
}
