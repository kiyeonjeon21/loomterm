use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use base64::Engine;
use clap::{Args, Parser, Subcommand, ValueEnum};
use loomterm::client::DaemonClient;
use loomterm::config::{AppPaths, Settings};
use loomterm::model::{
    AgentSessionFinish, AgentSessionRequest, AgentSessionState, CommandSpec, Execution,
    ExecutionEvent, ExecutionEventPayload, ExecutionOutcome, ExecutionRequest, ExecutionStats,
    Initiator, new_id, now_ms,
};
use loomterm::session::{
    RecordSpec, SessionArtifacts, export_cast, open_html, record, terminal_size, write_replay_html,
};
use loomterm::{Error, Result};

#[derive(Debug, Parser)]
#[command(
    name = "loom",
    version,
    about = "Structured command execution for coding agents"
)]
struct Cli {
    #[arg(long, global = true, help = "Emit JSON or JSON Lines")]
    json: bool,
    #[arg(long, global = true, help = "Do not start loomd automatically")]
    no_autostart: bool,
    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    #[command(subcommand)]
    Workspace(WorkspaceCommand),
    Run(RunArgs),
    Get {
        execution_id: String,
    },
    Logs(LogsArgs),
    Cancel {
        execution_id: String,
    },
    List {
        #[arg(short, long)]
        workspace: Option<String>,
        #[arg(short, long, default_value_t = 100)]
        limit: u32,
    },
    Stats(StatsArgs),
    #[command(subcommand)]
    Session(SessionCommand),
    #[command(subcommand)]
    Daemon(DaemonCommand),
    Doctor,
}

#[derive(Debug, Subcommand)]
enum WorkspaceCommand {
    Add {
        path: PathBuf,
        #[arg(short, long)]
        name: Option<String>,
    },
    List,
    Remove {
        workspace: String,
    },
}

#[derive(Debug, Subcommand)]
enum SessionCommand {
    /// Record an interactive agent through a PTY and generate a local replay.
    Record(SessionRecordArgs),
    /// List recent recorded agent sessions.
    List {
        #[arg(short, long)]
        workspace: Option<String>,
        #[arg(short, long, default_value_t = 100)]
        limit: u32,
    },
    /// Show one session and its correlated executions.
    Get { session_id: String },
    /// Regenerate the replay and open it in the default browser.
    Open { session_id: String },
    /// Export a self-contained HTML replay or an asciicast file.
    Export(SessionExportArgs),
    /// Delete a finished session and its recording artifacts.
    Delete { session_id: String },
}

#[derive(Debug, Args)]
struct SessionRecordArgs {
    #[arg(short, long)]
    workspace: Option<String>,
    #[arg(short, long)]
    name: Option<String>,
    #[arg(long, value_enum, default_value_t = AgentKindArg::Auto)]
    agent: AgentKindArg,
    #[arg(long)]
    capture_limit_bytes: Option<u64>,
    #[arg(
        trailing_var_arg = true,
        allow_hyphen_values = true,
        required = true,
        value_name = "COMMAND"
    )]
    argv: Vec<String>,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum AgentKindArg {
    Auto,
    Codex,
    Claude,
    Generic,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum SessionExportFormat {
    Html,
    Cast,
}

#[derive(Debug, Args)]
struct SessionExportArgs {
    session_id: String,
    #[arg(long, value_enum, default_value_t = SessionExportFormat::Html)]
    format: SessionExportFormat,
    #[arg(short, long)]
    output: PathBuf,
    #[arg(long)]
    redact: Vec<String>,
}

#[derive(Debug, Args)]
struct RunArgs {
    #[arg(short, long)]
    workspace: Option<String>,
    #[arg(long)]
    cwd: Option<String>,
    #[arg(
        long,
        conflicts_with = "argv",
        help = "Run an explicit /bin/sh -c command"
    )]
    shell: Option<String>,
    #[arg(long, requires = "shell")]
    shell_program: Option<String>,
    #[arg(
        trailing_var_arg = true,
        allow_hyphen_values = true,
        required_unless_present = "shell",
        value_name = "COMMAND"
    )]
    argv: Vec<String>,
    #[arg(long = "env", value_parser = parse_env)]
    env: Vec<(String, String)>,
    #[arg(long, value_name = "PATH", help = "Read initial stdin from PATH, or -")]
    stdin_file: Option<PathBuf>,
    #[arg(long)]
    capture_limit_bytes: Option<u64>,
    #[arg(long, help = "Return the execution id without waiting")]
    detach: bool,
}

#[derive(Debug, Args)]
struct LogsArgs {
    execution_id: String,
    #[arg(short, long)]
    follow: bool,
    #[arg(long, default_value_t = 0)]
    after_seq: u64,
    #[arg(long, default_value_t = 1024 * 1024)]
    max_bytes: usize,
}

#[derive(Debug, Args)]
struct StatsArgs {
    #[arg(short, long)]
    workspace: Option<String>,
    #[arg(
        long,
        default_value_t = 7,
        value_parser = clap::value_parser!(u32).range(1..=3650),
        help = "Number of recent days to summarize"
    )]
    days: u32,
}

#[derive(Debug, Subcommand)]
enum DaemonCommand {
    Start,
    Status,
    Stop {
        #[arg(long)]
        force: bool,
    },
    Restart {
        #[arg(
            long,
            help = "Restart even when active execution count is non-zero or unknown"
        )]
        force: bool,
    },
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    match run(cli).await {
        Ok(code) => std::process::exit(code),
        Err(error) => {
            eprintln!("loom: {error}");
            std::process::exit(1);
        }
    }
}

async fn run(cli: Cli) -> Result<i32> {
    let paths = AppPaths::discover()?;
    paths.ensure()?;
    let client = match &cli.command {
        Commands::Daemon(DaemonCommand::Start) => DaemonClient::connect_or_start(&paths).await?,
        Commands::Daemon(
            DaemonCommand::Status | DaemonCommand::Stop { .. } | DaemonCommand::Restart { .. },
        ) => DaemonClient::new(&paths.socket),
        _ if cli.no_autostart => DaemonClient::new(&paths.socket),
        _ => DaemonClient::connect_or_start(&paths).await?,
    };

    match cli.command {
        Commands::Workspace(command) => {
            handle_workspace(&client, command, cli.json).await?;
            Ok(0)
        }
        Commands::Run(args) => run_command(&client, args, cli.json).await,
        Commands::Get { execution_id } => {
            let execution = client.get(execution_id).await?;
            print_value(&execution, cli.json)?;
            Ok(0)
        }
        Commands::Logs(args) => {
            follow_logs(&client, args, cli.json).await?;
            Ok(0)
        }
        Commands::Cancel { execution_id } => {
            let execution = client.cancel(execution_id).await?;
            print_value(&execution, cli.json)?;
            Ok(0)
        }
        Commands::List { workspace, limit } => {
            let executions = client.list(workspace, limit).await?;
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&executions)?);
            } else {
                for execution in executions {
                    println!(
                        "{:<12} {:<11} {:<24} {}",
                        short_id(&execution.id),
                        execution.state.as_str(),
                        workspace_label(&execution.workspace_id),
                        execution.command_display
                    );
                }
            }
            Ok(0)
        }
        Commands::Stats(args) => {
            let workspace = resolve_workspace(&client, args.workspace.as_deref()).await?;
            let window_ms = i64::from(args.days) * 24 * 60 * 60 * 1_000;
            let stats = client
                .stats(workspace, now_ms().saturating_sub(window_ms))
                .await?;
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&stats)?);
            } else {
                println!("{}", format_stats(&stats));
            }
            Ok(0)
        }
        Commands::Session(command) => handle_session(&client, &paths, command, cli.json).await,
        Commands::Daemon(command) => {
            match command {
                DaemonCommand::Start | DaemonCommand::Status => {
                    print_value(&client.health().await?, cli.json)?;
                }
                DaemonCommand::Stop { force } => {
                    client.shutdown_with_force(force).await?;
                    if !cli.json {
                        println!("loomd stopped");
                    }
                }
                DaemonCommand::Restart { force } => {
                    restart_daemon(&client, &paths, force, cli.json).await?;
                }
            }
            Ok(0)
        }
        Commands::Doctor => {
            let health = client.health().await?;
            let workspaces = client.list_workspaces().await?;
            if cli.json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "health": health,
                        "workspaces": workspaces,
                    }))?
                );
            } else {
                println!("daemon: ok (pid {})", health.daemon_pid);
                println!("protocol: v{}", health.protocol_version);
                println!(
                    "server: {}",
                    health.server_version.as_deref().unwrap_or("unknown")
                );
                println!("capabilities: {}", health.capabilities.join(", "));
                println!(
                    "active executions: {}",
                    health
                        .active_executions
                        .map_or_else(|| "unknown".into(), |count| count.to_string())
                );
                println!(
                    "active sessions: {}",
                    health
                        .active_sessions
                        .map_or_else(|| "unknown".into(), |count| count.to_string())
                );
                println!("socket: {}", health.socket_path);
                println!("database: {}", health.database_path);
                println!("workspaces: {}", workspaces.len());
            }
            Ok(0)
        }
    }
}

async fn restart_daemon(
    client: &DaemonClient,
    paths: &AppPaths,
    force: bool,
    json: bool,
) -> Result<()> {
    let health = client.health().await?;
    if !force {
        match health.active_executions {
            Some(0) => {}
            Some(count) => {
                return Err(Error::InvalidRequest(format!(
                    "daemon has {count} active execution(s); wait for them or use --force"
                )));
            }
            None => {
                return Err(Error::InvalidRequest(
                    "daemon does not report active executions; use --force to restart it".into(),
                ));
            }
        }
        match health.active_sessions {
            Some(0) => {}
            Some(count) => {
                return Err(Error::InvalidRequest(format!(
                    "daemon has {count} active agent session(s); wait for them or use --force"
                )));
            }
            None => {
                return Err(Error::InvalidRequest(
                    "daemon does not report active agent sessions; use --force to restart it"
                        .into(),
                ));
            }
        }
    }

    client.shutdown_with_force(force).await?;
    for _ in 0..600 {
        if !paths.socket.exists() {
            let restarted = DaemonClient::connect_or_start(paths).await?;
            print_value(&restarted.health().await?, json)?;
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    Err(Error::Timeout)
}

async fn handle_workspace(
    client: &DaemonClient,
    command: WorkspaceCommand,
    json: bool,
) -> Result<()> {
    match command {
        WorkspaceCommand::Add { path, name } => {
            let canonical = path.canonicalize()?;
            let name = name.unwrap_or_else(|| {
                canonical
                    .file_name()
                    .and_then(|value| value.to_str())
                    .unwrap_or("workspace")
                    .to_owned()
            });
            let workspace = client
                .add_workspace(name, canonical.to_string_lossy().into_owned())
                .await?;
            print_value(&workspace, json)?;
        }
        WorkspaceCommand::List => {
            let workspaces = client.list_workspaces().await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&workspaces)?);
            } else {
                for workspace in workspaces {
                    println!(
                        "{:<20} {:<12} {}",
                        workspace.name,
                        short_id(&workspace.id),
                        workspace.root
                    );
                }
            }
        }
        WorkspaceCommand::Remove { workspace } => {
            client.remove_workspace(workspace).await?;
        }
    }
    Ok(())
}

async fn handle_session(
    client: &DaemonClient,
    paths: &AppPaths,
    command: SessionCommand,
    json: bool,
) -> Result<i32> {
    match command {
        SessionCommand::Record(args) => record_session(client, paths, args, json).await,
        SessionCommand::List { workspace, limit } => {
            let sessions = client.list_agent_sessions(workspace, limit).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&sessions)?);
            } else {
                for session in sessions {
                    println!(
                        "{:<12} {:<12} {:<10} {}",
                        short_id(&session.id),
                        session.state.as_str(),
                        session.agent_kind,
                        session.name.as_deref().unwrap_or(&session.command_display),
                    );
                }
            }
            Ok(0)
        }
        SessionCommand::Get { session_id } => {
            let detail = client.get_agent_session(session_id).await?;
            print_value(&detail, json)?;
            Ok(0)
        }
        SessionCommand::Open { session_id } => {
            let detail = client.get_agent_session(session_id).await?;
            let cast = Path::new(&detail.session.cast_path);
            let html = Path::new(&detail.session.html_path);
            write_replay_html(&detail, cast, html, &[])?;
            open_html(html)?;
            if !json {
                println!("opened {}", html.display());
            }
            Ok(0)
        }
        SessionCommand::Export(args) => {
            let detail = client.get_agent_session(args.session_id).await?;
            if args.redact.is_empty() {
                eprintln!(
                    "loom: export may contain prompts, command output, paths, and other sensitive data"
                );
            }
            match args.format {
                SessionExportFormat::Html => write_replay_html(
                    &detail,
                    Path::new(&detail.session.cast_path),
                    &args.output,
                    &args.redact,
                )?,
                SessionExportFormat::Cast => export_cast(
                    Path::new(&detail.session.cast_path),
                    &args.output,
                    &args.redact,
                )?,
            }
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "session_id": detail.session.id,
                        "output": args.output,
                    }))?
                );
            } else {
                println!("exported {}", args.output.display());
            }
            Ok(0)
        }
        SessionCommand::Delete { session_id } => {
            client.delete_agent_session(session_id).await?;
            Ok(0)
        }
    }
}

async fn record_session(
    client: &DaemonClient,
    paths: &AppPaths,
    args: SessionRecordArgs,
    json: bool,
) -> Result<i32> {
    if json {
        return Err(Error::InvalidRequest(
            "--json is not supported by interactive session recording".into(),
        ));
    }
    let capture_limit_bytes = args
        .capture_limit_bytes
        .unwrap_or(Settings::load(paths)?.capture_limit_bytes);
    let cwd = std::env::current_dir()?.canonicalize()?;
    let workspace_id = resolve_workspace(client, args.workspace.as_deref()).await?;
    let (initial_cols, initial_rows) = terminal_size()?;
    let mut argv = args.argv.into_iter();
    let program = argv
        .next()
        .ok_or_else(|| Error::InvalidRequest("missing agent command".into()))?;
    let command = CommandSpec::Argv {
        program,
        args: argv.collect(),
    };
    let agent_kind = resolve_agent_kind(args.agent, &command);
    let session_id = new_id();
    let artifacts = SessionArtifacts::create(paths, &session_id)?;
    let request = AgentSessionRequest {
        id: session_id.clone(),
        workspace_id,
        agent_kind: agent_kind.clone(),
        name: args.name,
        command: command.clone(),
        cwd: cwd.to_string_lossy().into_owned(),
        recorder_pid: std::process::id(),
        initial_cols,
        initial_rows,
        cast_path: artifacts.cast_path.to_string_lossy().into_owned(),
        html_path: artifacts.html_path.to_string_lossy().into_owned(),
    };
    if let Err(error) = client.create_agent_session(request).await {
        let _ = std::fs::remove_dir_all(&artifacts.directory);
        return Err(error);
    }
    let spec = RecordSpec {
        command,
        cwd,
        session_id: session_id.clone(),
        agent_kind,
        cast_path: artifacts.cast_path.clone(),
        initial_cols,
        initial_rows,
        capture_limit_bytes,
    };
    let result = tokio::task::spawn_blocking(move || record(spec))
        .await
        .map_err(|error| Error::Config(format!("session recorder task failed: {error}")))?;

    let result = match result {
        Ok(result) => result,
        Err(error) => {
            let _ = finish_session_resilient(
                client,
                paths,
                session_id.clone(),
                AgentSessionFinish {
                    state: AgentSessionState::Interrupted,
                    outcome: ExecutionOutcome::Interrupted {
                        reason: error.to_string(),
                    },
                    captured_bytes: 0,
                    output_truncated: false,
                },
            )
            .await;
            return Err(error);
        }
    };
    let state = if matches!(result.outcome, ExecutionOutcome::Interrupted { .. }) {
        AgentSessionState::Interrupted
    } else {
        AgentSessionState::Finished
    };
    finish_session_resilient(
        client,
        paths,
        session_id.clone(),
        AgentSessionFinish {
            state,
            outcome: result.outcome.clone(),
            captured_bytes: result.captured_bytes,
            output_truncated: result.output_truncated,
        },
    )
    .await?;
    let active_client = DaemonClient::connect_or_start(paths).await?;
    let detail = active_client.get_agent_session(session_id.clone()).await?;
    write_replay_html(&detail, &artifacts.cast_path, &artifacts.html_path, &[])?;
    eprintln!("\nloom: session {session_id}");
    eprintln!("loom: replay {}", artifacts.html_path.display());
    eprintln!("loom: cast {}", artifacts.cast_path.display());
    Ok(result.exit_code)
}

async fn finish_session_resilient(
    client: &DaemonClient,
    paths: &AppPaths,
    session_id: String,
    finish: AgentSessionFinish,
) -> Result<loomterm::model::AgentSession> {
    match client
        .finish_agent_session(session_id.clone(), finish.clone())
        .await
    {
        Ok(session) => Ok(session),
        Err(Error::DaemonUnavailable(_)) => {
            DaemonClient::connect_or_start(paths)
                .await?
                .finish_agent_session(session_id, finish)
                .await
        }
        Err(error) => Err(error),
    }
}

fn resolve_agent_kind(requested: AgentKindArg, command: &CommandSpec) -> String {
    match requested {
        AgentKindArg::Codex => return "codex".into(),
        AgentKindArg::Claude => return "claude".into(),
        AgentKindArg::Generic => return "generic".into(),
        AgentKindArg::Auto => {}
    }
    let CommandSpec::Argv { program, .. } = command else {
        return "generic".into();
    };
    let name = Path::new(program)
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or(program);
    match name {
        "codex" => "codex",
        "claude" => "claude",
        _ => "generic",
    }
    .into()
}

async fn run_command(client: &DaemonClient, args: RunArgs, json: bool) -> Result<i32> {
    let workspace_id = resolve_workspace(client, args.workspace.as_deref()).await?;
    let command = match args.shell {
        Some(command) => CommandSpec::Shell {
            command,
            shell: args.shell_program,
        },
        None => {
            let mut argv = args.argv.into_iter();
            CommandSpec::Argv {
                program: argv
                    .next()
                    .ok_or_else(|| Error::InvalidRequest("missing command".into()))?,
                args: argv.collect(),
            }
        }
    };
    let stdin_base64 = args
        .stdin_file
        .as_deref()
        .map(read_stdin)
        .transpose()?
        .map(|data| base64::engine::general_purpose::STANDARD.encode(data));
    let request = ExecutionRequest {
        workspace_id,
        cwd: args.cwd,
        command,
        env: args.env.into_iter().collect::<BTreeMap<_, _>>(),
        stdin_base64,
        initiator: Initiator {
            kind: "cli".into(),
            name: Some("loom".into()),
            session_id: std::env::var("LOOMTERM_SESSION_ID")
                .ok()
                .filter(|value| !value.is_empty()),
        },
        capture_limit_bytes: args.capture_limit_bytes,
    };
    let execution = client.execute(request).await?;
    if args.detach {
        print_value(&execution, json)?;
        return Ok(0);
    }
    if json {
        print_json_line("execution", &execution)?;
    }
    let final_execution = stream_until_finished(client, &execution.id, 0, json).await?;
    if json {
        print_json_line("result", &final_execution)?;
    }
    Ok(process_exit_code(&final_execution))
}

async fn stream_until_finished(
    client: &DaemonClient,
    id: &str,
    cursor: u64,
    json: bool,
) -> Result<Execution> {
    let mut subscription = client.subscribe(id.into(), cursor).await?;
    while let Some(event) = subscription.next_event().await? {
        let terminal = matches!(&event.payload, ExecutionEventPayload::Finished { .. });
        render_event(&event, json)?;
        if terminal {
            return client.get(id.into()).await;
        }
    }
    let execution = client.get(id.into()).await?;
    if execution.state.is_terminal() {
        Ok(execution)
    } else {
        Err(Error::Protocol(
            "execution subscription closed before completion".into(),
        ))
    }
}

async fn follow_logs(client: &DaemonClient, args: LogsArgs, json: bool) -> Result<()> {
    if args.follow {
        let mut subscription = client
            .subscribe(args.execution_id.clone(), args.after_seq)
            .await?;
        while let Some(event) = subscription.next_event().await? {
            render_event(&event, json)?;
        }
        return Ok(());
    }
    let mut cursor = args.after_seq;
    loop {
        let read = client
            .read_output(args.execution_id.clone(), cursor, args.max_bytes)
            .await?;
        let response = (read.events, read.next_seq, read.has_more);
        for event in &response.0 {
            render_event(event, json)?;
        }
        cursor = response.1;
        if !response.2 {
            return Ok(());
        }
    }
}

fn render_event(event: &ExecutionEvent, json: bool) -> Result<()> {
    if json {
        return print_json_line("event", event);
    }
    match &event.payload {
        ExecutionEventPayload::Output {
            stream,
            data_base64,
            ..
        } => {
            let data = base64::engine::general_purpose::STANDARD
                .decode(data_base64)
                .map_err(|error| Error::Protocol(format!("invalid output encoding: {error}")))?;
            match stream {
                loomterm::model::OutputStream::Stdout => {
                    std::io::stdout().write_all(&data)?;
                    std::io::stdout().flush()?;
                }
                loomterm::model::OutputStream::Stderr => {
                    std::io::stderr().write_all(&data)?;
                    std::io::stderr().flush()?;
                }
            }
        }
        ExecutionEventPayload::CaptureTruncated { limit_bytes } => {
            eprintln!("loom: output capture truncated at {limit_bytes} bytes");
        }
        ExecutionEventPayload::Started { .. } | ExecutionEventPayload::Finished { .. } => {}
    }
    Ok(())
}

async fn resolve_workspace(client: &DaemonClient, requested: Option<&str>) -> Result<String> {
    if let Some(requested) = requested {
        return Ok(requested.into());
    }
    let cwd = std::env::current_dir()?.canonicalize()?;
    client
        .list_workspaces()
        .await?
        .into_iter()
        .filter(|workspace| cwd.starts_with(Path::new(&workspace.root)))
        .max_by_key(|workspace| workspace.root.len())
        .map(|workspace| workspace.id)
        .ok_or_else(|| {
            Error::InvalidRequest(
                "current directory is not in a registered workspace; run `loom workspace add .`"
                    .into(),
            )
        })
}

fn process_exit_code(execution: &Execution) -> i32 {
    match &execution.outcome {
        Some(ExecutionOutcome::Exited { code }) => *code,
        Some(ExecutionOutcome::Signaled { signal }) => 128 + signal,
        Some(ExecutionOutcome::Cancelled { signal }) => 128 + signal.unwrap_or(15),
        Some(ExecutionOutcome::SpawnError { .. }) => 126,
        Some(ExecutionOutcome::Interrupted { .. }) | None => 1,
    }
}

fn read_stdin(path: &Path) -> Result<Vec<u8>> {
    let mut data = Vec::new();
    if path == Path::new("-") {
        std::io::stdin().read_to_end(&mut data)?;
    } else {
        std::fs::File::open(path)?.read_to_end(&mut data)?;
    }
    Ok(data)
}

fn parse_env(value: &str) -> std::result::Result<(String, String), String> {
    let (key, value) = value
        .split_once('=')
        .ok_or_else(|| "expected KEY=VALUE".to_owned())?;
    if key.is_empty() || key.contains('=') {
        return Err("environment key must be non-empty".into());
    }
    Ok((key.into(), value.into()))
}

fn print_value<T: serde::Serialize + std::fmt::Debug>(value: &T, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(value)?);
    } else {
        println!("{value:#?}");
    }
    Ok(())
}

fn print_json_line<T: serde::Serialize>(kind: &str, value: &T) -> Result<()> {
    println!(
        "{}",
        serde_json::to_string(&serde_json::json!({"type": kind, "value": value}))?
    );
    Ok(())
}

fn short_id(id: &str) -> &str {
    id.get(..8).unwrap_or(id)
}

fn workspace_label(id: &str) -> String {
    short_id(id).to_owned()
}

fn format_stats(stats: &ExecutionStats) -> String {
    let initiators = if stats.by_initiator.is_empty() {
        "none".to_owned()
    } else {
        stats
            .by_initiator
            .iter()
            .map(|item| format!("{} {}", item.kind, item.count))
            .collect::<Vec<_>>()
            .join(", ")
    };
    format!(
        concat!(
            "Workspace: {} ({})\n",
            "Window: {}..{} (epoch ms)\n",
            "Executions: {}\n",
            "Status:\n",
            "  queued: {}\n",
            "  running: {}\n",
            "  exited 0: {}\n",
            "  exited nonzero: {}\n",
            "  signaled: {}\n",
            "  spawn error: {}\n",
            "  cancelled: {}\n",
            "  interrupted: {}\n",
            "  unknown terminal: {}\n",
            "Initiators: {}\n",
            "Captured output: {}\n",
            "Truncated executions: {}\n",
            "Duration samples: {}\n",
            "Duration p50: {}\n",
            "Duration p95: {}",
        ),
        stats.workspace.name,
        short_id(&stats.workspace.id),
        stats.since_ms,
        stats.until_ms,
        stats.total,
        stats.status.queued,
        stats.status.running,
        stats.status.exited_zero,
        stats.status.exited_nonzero,
        stats.status.signaled,
        stats.status.spawn_error,
        stats.status.cancelled,
        stats.status.interrupted,
        stats.status.unknown_terminal,
        initiators,
        format_bytes(stats.captured_bytes),
        stats.truncated_executions,
        stats.duration_samples,
        format_duration(stats.duration_p50_ms),
        format_duration(stats.duration_p95_ms),
    )
}

fn format_bytes(bytes: u64) -> String {
    const UNITS: [(&str, u64); 4] = [
        ("TiB", 1024_u64.pow(4)),
        ("GiB", 1024_u64.pow(3)),
        ("MiB", 1024_u64.pow(2)),
        ("KiB", 1024),
    ];
    for (unit, divisor) in UNITS {
        if bytes >= divisor {
            return format!(
                "{:.1} {unit} ({bytes} bytes)",
                bytes as f64 / divisor as f64
            );
        }
    }
    format!("{bytes} bytes")
}

fn format_duration(value: Option<u64>) -> String {
    value.map_or_else(|| "n/a".into(), |milliseconds| format!("{milliseconds} ms"))
}

#[cfg(test)]
mod tests {
    use loomterm::model::{ExecutionStats, ExecutionStatusCounts, InitiatorStats, Workspace};

    use super::*;

    #[test]
    fn formats_stats_for_humans() {
        let stats = ExecutionStats {
            workspace: Workspace {
                id: "01234567-rest".into(),
                name: "loomterm".into(),
                root: "/tmp/loomterm".into(),
                created_at_ms: 0,
            },
            since_ms: 100,
            until_ms: 200,
            total: 3,
            status: ExecutionStatusCounts {
                exited_zero: 2,
                exited_nonzero: 1,
                ..ExecutionStatusCounts::default()
            },
            by_initiator: vec![InitiatorStats {
                kind: "cli".into(),
                count: 3,
            }],
            captured_bytes: 1536,
            truncated_executions: 1,
            duration_samples: 3,
            duration_p50_ms: Some(12),
            duration_p95_ms: Some(90),
        };

        let output = format_stats(&stats);
        assert!(output.contains("Workspace: loomterm (01234567)"));
        assert!(output.contains("Executions: 3"));
        assert!(output.contains("Status:\n  queued: 0\n  running: 0"));
        assert!(output.contains("Initiators: cli 3"));
        assert!(output.contains("Captured output: 1.5 KiB (1536 bytes)"));
        assert!(output.contains("Duration p95: 90 ms"));
    }

    #[test]
    fn formats_empty_duration_and_small_bytes() {
        assert_eq!(format_duration(None), "n/a");
        assert_eq!(format_bytes(0), "0 bytes");
        assert_eq!(format_bytes(1023), "1023 bytes");
    }
}
