use std::env;
use std::error::Error;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use clap::{error::ErrorKind, Parser, ValueEnum};
use rmux_sdk::{CleanupPolicy, PaneProcessState, Rmux, SessionName, TerminalSizeSpec};
use serde::Serialize;
use tokio::time::{self, Instant};

const ENTRYPOINT: &str = "claude_interactive_rmux";
const RMUX_DAEMON_BINARY_ENV: &str = "RMUX_SDK_DAEMON_BINARY";
// Bracketed-paste envelope. The pane forwards send_text bytes literally, so a
// bare newline in the prompt reads as Enter and submits a partial prompt.
// Wrapping the payload in these markers tells Claude's TUI to buffer the whole
// block — newlines included — as pasted content.
const BRACKETED_PASTE_START: &str = "\u{1b}[200~";
const BRACKETED_PASTE_END: &str = "\u{1b}[201~";
// Env vars Claude Code sets for processes it spawns. When the runner itself runs
// under a Claude session, a nested Claude inherits these, detects a *child*
// session, and suppresses its on-disk `<id>.jsonl` transcript — the very file we
// harvest. We unset them for the spawned Claude so it persists a normal session.
const CLAUDE_NESTING_ENV: &[&str] = &[
    "CLAUDECODE",
    "CLAUDE_CODE_ENTRYPOINT",
    "CLAUDE_CODE_CHILD_SESSION",
    "CLAUDE_CODE_SESSION_ID",
    "CLAUDE_CODE_EXECPATH",
    "CLAUDE_EFFORT",
];

#[derive(Debug, Parser)]
#[command(name = "claude-rmux-runner")]
#[command(about = "Run interactive Claude in an isolated rmux-owned session")]
struct Cli {
    #[arg(long)]
    workspace: PathBuf,
    #[arg(long)]
    prompt_file: PathBuf,
    /// File the agent writes when it finishes; its existence (with non-whitespace
    /// content) signals completion. Contents are the caller's contract and opaque
    /// to the runner — a bare output-dir path, JSON, or anything else. When it
    /// holds a bare existing path, that path is recorded in the metadata as a
    /// best effort.
    #[arg(long)]
    result_file: PathBuf,
    /// Merged run metadata, written as the same JSON object printed to stdout.
    #[arg(long)]
    trace_file: PathBuf,
    /// Raw session transcript copy (`.jsonl`). Defaults to `trace.jsonl` beside
    /// `--trace-file`. Holds Claude's real `<id>.jsonl`, or — as a fallback — a
    /// single JSON line wrapping the final terminal snapshot.
    #[arg(long)]
    transcript_file: Option<PathBuf>,
    #[arg(long)]
    timeout_seconds: u64,
    #[arg(long)]
    model: Option<String>,
    #[arg(long)]
    session_name: Option<String>,
    #[arg(long, default_value = "claude")]
    claude_bin: String,
    #[arg(long)]
    rmux_bin: Option<String>,
    #[arg(long)]
    final_message_file: Option<PathBuf>,
    #[arg(long, value_enum, default_value_t = PermissionMode::AcceptEdits)]
    permission_mode: PermissionMode,
}

#[derive(Clone, Debug, Eq, PartialEq, ValueEnum)]
enum PermissionMode {
    #[value(name = "acceptEdits")]
    AcceptEdits,
    #[value(name = "bypassPermissions")]
    BypassPermissions,
    #[value(name = "default")]
    Default,
}

impl PermissionMode {
    fn claude_value(&self) -> &'static str {
        match self {
            Self::AcceptEdits => "acceptEdits",
            Self::BypassPermissions => "bypassPermissions",
            Self::Default => "default",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum ExitReason {
    Completed,
    ClaudeExited,
    Timeout,
    SetupFailure,
    Interrupted,
}

impl ExitReason {
    fn code(self) -> u8 {
        match self {
            Self::Completed => 0,
            Self::ClaudeExited => 1,
            Self::Timeout => 2,
            Self::SetupFailure => 3,
            Self::Interrupted => 4,
        }
    }
}

#[derive(Debug)]
struct Config {
    workspace: PathBuf,
    prompt_file: PathBuf,
    result_file: PathBuf,
    trace_file: PathBuf,
    transcript_file: PathBuf,
    timeout: Duration,
    model: Option<String>,
    session_name: String,
    claude_bin: String,
    rmux_bin: Option<String>,
    final_message_file: Option<PathBuf>,
    permission_mode: PermissionMode,
    // Session id we force on Claude with `--session-id`, so we can locate the
    // exact `<id>.jsonl` transcript it writes under the projects config dir.
    claude_session_id: String,
}

impl Config {
    fn from_cli(cli: Cli) -> Result<Self, SetupError> {
        if cli.timeout_seconds == 0 {
            return Err(SetupError::new(
                "--timeout-seconds must be greater than zero",
            ));
        }
        ensure_directory(&cli.workspace, "--workspace")?;
        ensure_file(&cli.prompt_file, "--prompt-file")?;
        if let Some(parent) = cli
            .trace_file
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
        {
            ensure_directory(parent, "--trace-file parent")?;
        }
        if let Some(path) = &cli.transcript_file {
            if let Some(parent) = path.parent().filter(|path| !path.as_os_str().is_empty()) {
                ensure_directory(parent, "--transcript-file parent")?;
            }
        }
        if let Some(path) = &cli.final_message_file {
            if let Some(parent) = path.parent().filter(|path| !path.as_os_str().is_empty()) {
                ensure_directory(parent, "--final-message-file parent")?;
            }
        }

        let trace_file = absolutize_maybe_missing(cli.trace_file)?;
        // Default the transcript beside the metadata file so a single --trace-file
        // still yields both artifacts.
        let transcript_file = match cli.transcript_file {
            Some(path) => absolutize_maybe_missing(path)?,
            None => trace_file
                .parent()
                .unwrap_or_else(|| Path::new("."))
                .join("trace.jsonl"),
        };

        Ok(Self {
            workspace: absolutize_existing(cli.workspace)?,
            prompt_file: absolutize_existing(cli.prompt_file)?,
            result_file: absolutize_maybe_missing(cli.result_file)?,
            trace_file,
            transcript_file,
            timeout: Duration::from_secs(cli.timeout_seconds),
            model: cli.model,
            session_name: cli.session_name.unwrap_or_else(unique_session_name),
            claude_bin: cli.claude_bin,
            rmux_bin: cli.rmux_bin,
            final_message_file: cli
                .final_message_file
                .map(absolutize_maybe_missing)
                .transpose()?,
            permission_mode: cli.permission_mode,
            claude_session_id: new_uuid(),
        })
    }
}

#[derive(Debug)]
struct RunOutcome {
    exit_reason: ExitReason,
    session: String,
    pane: String,
    claude_command: Vec<String>,
    final_visible_text: String,
    final_output_dir: Option<String>,
    // Real Claude session transcript, harvested from `<id>.jsonl` after the run.
    // `None` when the file could not be located/read (trace falls back to the
    // terminal snapshot).
    transcript_path: Option<PathBuf>,
    transcript_text: Option<String>,
}

#[derive(Debug)]
struct SetupError {
    message: String,
}

impl SetupError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for SetupError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for SetupError {}

impl From<io::Error> for SetupError {
    fn from(error: io::Error) -> Self {
        Self::new(error.to_string())
    }
}

/// The single merged run record: printed to stdout and written verbatim to
/// `--trace-file`. Supersedes the old key=value trace log. The transcript itself
/// is not embedded here — it lives in `--transcript-file` (see `transcript_*`).
#[derive(Serialize)]
struct Metadata {
    entrypoint: &'static str,
    timestamp_ms: u128,
    workspace: String,
    prompt_file: String,
    result_file: String,
    result_file_exists: bool,
    final_output_dir: Option<String>,
    session: String,
    claude_session_id: String,
    pane: String,
    permission_mode: String,
    claude_command: Vec<String>,
    rmux_bin: Option<String>,
    prompt_send_event: &'static str,
    prompt_submit_event: &'static str,
    exit_reason: ExitReason,
    exit_code: u8,
    setup_error: Option<String>,
    // Where the runner wrote the transcript copy, its on-disk source (Claude's
    // `<id>.jsonl`), and which of the two got captured.
    transcript_file: String,
    transcript_jsonl_path: Option<String>,
    transcript_source: &'static str,
    isolation_notes: Vec<String>,
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(error) => {
            let code = if matches!(
                error.kind(),
                ErrorKind::DisplayHelp | ErrorKind::DisplayVersion
            ) {
                0
            } else {
                ExitReason::SetupFailure.code()
            };
            let _ = error.print();
            return ExitCode::from(code);
        }
    };
    let config = match Config::from_cli(cli) {
        Ok(config) => config,
        Err(error) => {
            eprintln!("setup failure: {error}");
            return ExitCode::from(ExitReason::SetupFailure.code());
        }
    };

    let outcome = match run(&config).await {
        Ok(outcome) => outcome,
        Err(error) => {
            let outcome = RunOutcome {
                exit_reason: ExitReason::SetupFailure,
                session: config.session_name.clone(),
                pane: "0:0".to_owned(),
                claude_command: Vec::new(),
                final_visible_text: String::new(),
                final_output_dir: None,
                transcript_path: None,
                transcript_text: None,
            };
            eprintln!("setup failure: {error}");
            emit(&config, &outcome, Some(&error.to_string()));
            return ExitCode::from(outcome.exit_reason.code());
        }
    };

    emit(&config, &outcome, None);
    ExitCode::from(outcome.exit_reason.code())
}

/// Write all run artifacts: the transcript `.jsonl` copy, then the merged
/// metadata JSON — printed to stdout and written verbatim to `--trace-file`.
fn emit(config: &Config, outcome: &RunOutcome, setup_error: Option<&str>) {
    if let Some(path) = &config.final_message_file {
        let _ = fs::write(path, &outcome.final_visible_text);
    }

    let transcript_source = write_transcript_file(config, outcome);
    let metadata = metadata_for(config, outcome, setup_error, transcript_source);
    match serde_json::to_string(&metadata) {
        Ok(json) => {
            println!("{json}");
            let _ = fs::write(&config.trace_file, format!("{json}\n"));
        }
        Err(error) => eprintln!("metadata serialization failure: {error}"),
    }
}

async fn run(config: &Config) -> Result<RunOutcome, Box<dyn Error>> {
    let claude_path = resolve_executable(&config.claude_bin).map_err(|message| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!("{message}. Pass an absolute path with --claude-bin /path/to/claude"),
        )
    })?;
    let rmux_path = config
        .rmux_bin
        .as_deref()
        .map(resolve_executable)
        .transpose()
        .map_err(|message| io::Error::new(io::ErrorKind::NotFound, message))?;
    if let Some(path) = &rmux_path {
        env::set_var(RMUX_DAEMON_BINARY_ENV, path);
    }

    let prompt_bytes = fs::read(&config.prompt_file)?;
    let prompt = String::from_utf8(prompt_bytes).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("--prompt-file must contain UTF-8 text for rmux keyboard input: {error}"),
        )
    })?;
    let claude_command = spawn_command(claude_argv(
        &claude_path,
        config.model.as_deref(),
        &config.permission_mode,
        &config.claude_session_id,
    ));

    let rmux = Rmux::builder()
        .default_timeout(Duration::from_secs(10))
        .connect_or_start()
        .await?;
    let session_name = SessionName::new(config.session_name.clone())?;
    let mut owned = rmux
        .owned_session(session_name)
        .cleanup_policy(CleanupPolicy::KillOnOwnerExit)
        .await?;
    let session = owned.session();
    let pane = session.pane(0, 0);

    pane.resize(TerminalSizeSpec::new(120, 36)).await?;
    pane.spawn(claude_command.clone())
        .cwd(&config.workspace)
        .kill_existing(true)
        .keep_alive_on_exit(true)
        .title("agent:claude-rmux-runner")
        .await?;
    pane.wait_until_stable_for(Duration::from_millis(500))
        .timeout(Duration::from_secs(30))
        .await?;

    accept_workspace_trust_if_prompted(&pane).await?;
    send_and_submit_prompt(&pane, &prompt).await?;

    let exit_reason = wait_for_completion(&pane, &config.result_file, config.timeout).await;
    let final_visible_text = pane
        .snapshot()
        .await
        .map(|snapshot| snapshot.visible_text())
        .unwrap_or_default();
    let final_output_dir = resolve_result_dir(&config.result_file);

    // Interactive Claude flushes its `<id>.jsonl` transcript on graceful exit,
    // not on the SIGKILL that session teardown delivers. Quit Claude cleanly and
    // wait for the process to leave before cleanup, so the transcript is on disk.
    quit_claude_gracefully(&pane).await;

    owned.cleanup().await?;

    // Harvest the real session transcript. The `<id>.jsonl` lives under the
    // projects config dir (outside rmux), so session teardown never touches it.
    let (transcript_path, transcript_text) = harvest_transcript(&config.claude_session_id);

    Ok(RunOutcome {
        exit_reason,
        session: config.session_name.clone(),
        pane: "0:0".to_owned(),
        claude_command,
        final_visible_text,
        final_output_dir,
        transcript_path,
        transcript_text,
    })
}

/// Accept Claude's workspace-trust dialog when a fresh, untrusted cwd triggers
/// it. This must run before any prompt is pasted: the dialog only listens for
/// its own keys, and the paste's `ESC` bytes would be read as an Escape/cancel.
/// A trusted workspace shows no dialog, so this is a no-op there.
async fn accept_workspace_trust_if_prompted(pane: &rmux_sdk::Pane) -> rmux_sdk::Result<()> {
    const ATTEMPTS: usize = 6;

    for _ in 0..ATTEMPTS {
        let visible = pane
            .snapshot()
            .await
            .map(|snapshot| snapshot.visible_text())
            .unwrap_or_default();
        if !has_trust_prompt(&visible) {
            return Ok(());
        }
        // "Yes, I trust this folder" is the highlighted default; Enter confirms.
        pane.keyboard().press("Enter").await?;
        pane.wait_until_stable_for(Duration::from_millis(500))
            .timeout(Duration::from_secs(30))
            .await?;
    }
    Ok(())
}

/// Whether the pane is showing the workspace-trust safety prompt.
fn has_trust_prompt(visible_text: &str) -> bool {
    visible_text.contains("trust this folder")
}

/// Paste the prompt into the TUI and submit it, confirming each step.
///
/// The prompt is delivered inside a bracketed-paste envelope so embedded
/// newlines land in the input buffer instead of submitting the prompt line by
/// line. Two things are flaky while Claude's startup screen is still repainting:
/// the paste can be dropped entirely, and an Enter can be absorbed as a newline
/// rather than a submit. So we drive a small state machine off pane snapshots:
/// re-paste until the prompt is actually sitting in the input box, then press
/// Enter until that box clears (the prompt was accepted).
async fn send_and_submit_prompt(pane: &rmux_sdk::Pane, prompt: &str) -> rmux_sdk::Result<()> {
    const ATTEMPTS: usize = 12;

    let keyboard = pane.keyboard();
    let needle: String = prompt
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("")
        .chars()
        .take(24)
        .collect();

    // Let the startup screen finish drawing before the first paste.
    pane.wait_until_stable_for(Duration::from_millis(500))
        .timeout(Duration::from_secs(30))
        .await?;
    keyboard.type_text(bracketed_paste(prompt)).await?;

    let mut landed = false;
    for _ in 0..ATTEMPTS {
        time::sleep(Duration::from_millis(600)).await;
        let visible = pane
            .snapshot()
            .await
            .map(|snapshot| snapshot.visible_text())
            .unwrap_or_default();

        if input_box_retains(&visible, &needle) {
            // The prompt is in the input box; submit it (retry if a prior Enter
            // was swallowed and the prompt is still sitting there).
            landed = true;
            keyboard.press("Enter").await?;
        } else if landed {
            // The prompt was in the box and is now gone → it was submitted.
            return Ok(());
        } else {
            // The paste was dropped by a startup repaint. Clear any partial
            // input and paste again.
            keyboard.press("C-u").await?;
            keyboard.type_text(bracketed_paste(prompt)).await?;
        }
    }
    Ok(())
}

/// Whether the prompt still sits unsent in the input box.
///
/// The input line carries the `❯` prompt marker; submitting clears it. A prompt
/// that is still pending shows up on that line one of two ways: inline for a
/// short prompt, or collapsed to a `[Pasted text #N +M lines]` placeholder for a
/// multi-line paste. Only the input line uses the `❯` marker (the sent message
/// is echoed back without it), so a marker line still carrying either form means
/// the submit has not gone through.
fn input_box_retains(visible_text: &str, needle: &str) -> bool {
    visible_text
        .lines()
        .filter(|line| line.trim_start().starts_with('❯'))
        .any(|line| {
            line.contains("[Pasted text") || (!needle.is_empty() && line.contains(needle))
        })
}

async fn wait_for_completion(
    pane: &rmux_sdk::Pane,
    result_file: &Path,
    timeout: Duration,
) -> ExitReason {
    let deadline = Instant::now() + timeout;
    let mut interval = time::interval(Duration::from_millis(250));
    let mut ctrl_c = Box::pin(tokio::signal::ctrl_c());
    let timeout_sleep = time::sleep_until(deadline);
    tokio::pin!(timeout_sleep);

    loop {
        tokio::select! {
            _ = interval.tick() => {
                if result_is_complete(result_file) {
                    return ExitReason::Completed;
                }
                if pane_has_exited(pane).await.unwrap_or(true) {
                    if result_is_complete(result_file) {
                        return ExitReason::Completed;
                    }
                    return ExitReason::ClaudeExited;
                }
            }
            _ = &mut timeout_sleep => return ExitReason::Timeout,
            _ = &mut ctrl_c => return ExitReason::Interrupted,
        }
    }
}

/// The run is complete once the agent has created a non-empty result-file. Its
/// contents are the caller's contract and opaque to the runner — we detect only
/// that the file exists and holds more than whitespace. The non-empty guard
/// rejects a zero-byte or not-yet-written create that precedes the real write. A
/// bare path, a JSON document, or any other body all count as complete.
fn result_is_complete(result_file: &Path) -> bool {
    // Read raw bytes, not UTF-8: the contents are opaque, so a non-UTF-8 body is
    // still a valid completion marker. Complete iff any byte is non-whitespace.
    fs::read(result_file)
        .map(|bytes| bytes.iter().any(|b| !b.is_ascii_whitespace()))
        .unwrap_or(false)
}

/// Best-effort read of a final output directory recorded in the result file, for
/// the emitted metadata only — this no longer gates completion (see
/// [`result_is_complete`]). Returns the trimmed contents when they name a path
/// that exists on disk; `None` otherwise (e.g. a structured/JSON result file).
fn resolve_result_dir(result_file: &Path) -> Option<String> {
    let contents = fs::read_to_string(result_file).ok()?;
    let path = contents.trim();
    if path.is_empty() || !Path::new(path).exists() {
        return None;
    }
    Some(path.to_owned())
}

/// Quit the interactive Claude TUI cleanly so it flushes its session `.jsonl`
/// transcript. Two quick Ctrl-C presses trigger the confirmed exit; we then wait
/// (bounded) for the pane process to leave and the async writer to settle. Best
/// effort — on timeout the caller falls back to the terminal snapshot.
async fn quit_claude_gracefully(pane: &rmux_sdk::Pane) {
    const QUIT_DEADLINE: Duration = Duration::from_secs(15);

    let keyboard = pane.keyboard();
    for _ in 0..2 {
        let _ = keyboard.press("C-c").await;
        time::sleep(Duration::from_millis(150)).await;
    }

    let deadline = Instant::now() + QUIT_DEADLINE;
    while Instant::now() < deadline {
        if pane_has_exited(pane).await.unwrap_or(false) {
            // Let the transcript writer finish its final flush before teardown.
            time::sleep(Duration::from_millis(500)).await;
            return;
        }
        time::sleep(Duration::from_millis(250)).await;
    }
}

async fn pane_has_exited(pane: &rmux_sdk::Pane) -> rmux_sdk::Result<bool> {
    let info = pane.info().await?;
    let target = pane.target();
    let Some(pane_info) = info
        .panes
        .iter()
        .find(|pane_info| pane_info.index == target.pane_index && pane_info.command.is_some())
    else {
        return Ok(true);
    };
    Ok(matches!(pane_info.process, PaneProcessState::Exited))
}

fn bracketed_paste(text: &str) -> String {
    format!("{BRACKETED_PASTE_START}{text}{BRACKETED_PASTE_END}")
}

fn claude_argv(
    claude_path: &str,
    model: Option<&str>,
    permission_mode: &PermissionMode,
    session_id: &str,
) -> Vec<String> {
    let mut argv = vec![
        claude_path.to_owned(),
        // --bare is intentionally absent: it never reads OAuth/keychain
        // credentials, so interactive logins fail. --safe-mode keeps the
        // customization isolation (no CLAUDE.md, hooks, plugins, MCP) while
        // auth works normally.
        "--safe-mode".to_owned(),
        "--disable-slash-commands".to_owned(),
        "--strict-mcp-config".to_owned(),
        // Pin the session id so we can find the `<id>.jsonl` transcript on disk
        // afterward. Interactive Claude flushes that file on graceful exit, so
        // the runner quits Claude cleanly before tearing the session down.
        "--session-id".to_owned(),
        session_id.to_owned(),
        "--permission-mode".to_owned(),
        permission_mode.claude_value().to_owned(),
    ];
    if let Some(model) = model {
        argv.push("--model".to_owned());
        argv.push(model.to_owned());
    }
    argv
}

/// Wrap the Claude argv in `env -u …` so the nesting env vars are cleared right
/// before Claude execs. This runs inside the rmux pane, so it strips the vars
/// regardless of whether the pane inherited them from the runner or the daemon,
/// letting the spawned Claude persist a normal session transcript.
fn spawn_command(claude_argv: Vec<String>) -> Vec<String> {
    let mut command = vec!["env".to_owned()];
    for name in CLAUDE_NESTING_ENV {
        command.push("-u".to_owned());
        command.push((*name).to_owned());
    }
    command.extend(claude_argv);
    command
}

/// Write `--transcript-file`, returning which source it captured. Prefers
/// Claude's real `<id>.jsonl` (copied verbatim); if that was not harvested,
/// falls back to a single JSON line wrapping the final terminal snapshot so the
/// file is still valid JSONL. Returns `"none"` when neither is available.
fn write_transcript_file(config: &Config, outcome: &RunOutcome) -> &'static str {
    let (content, source) = match &outcome.transcript_text {
        Some(text) => (text.clone(), "session_jsonl"),
        None if !outcome.final_visible_text.is_empty() => {
            let line = serde_json::json!({
                "type": "terminal_snapshot",
                "text": outcome.final_visible_text,
            });
            (format!("{line}\n"), "terminal_snapshot")
        }
        None => (String::new(), "none"),
    };
    let _ = fs::write(&config.transcript_file, content);
    source
}

fn metadata_for(
    config: &Config,
    outcome: &RunOutcome,
    setup_error: Option<&str>,
    transcript_source: &'static str,
) -> Metadata {
    Metadata {
        entrypoint: ENTRYPOINT,
        timestamp_ms: epoch_millis(),
        workspace: path_string(&config.workspace),
        prompt_file: path_string(&config.prompt_file),
        result_file: path_string(&config.result_file),
        result_file_exists: config.result_file.exists(),
        final_output_dir: outcome.final_output_dir.clone(),
        session: outcome.session.clone(),
        claude_session_id: config.claude_session_id.clone(),
        pane: outcome.pane.clone(),
        permission_mode: config.permission_mode.claude_value().to_owned(),
        claude_command: outcome.claude_command.clone(),
        rmux_bin: config.rmux_bin.clone(),
        prompt_send_event: "sent_exact_prompt_file_bytes_via_bracketed_paste",
        prompt_submit_event: "pressed_enter_after_prompt",
        exit_reason: outcome.exit_reason,
        exit_code: outcome.exit_reason.code(),
        setup_error: setup_error.map(str::to_owned),
        transcript_file: path_string(&config.transcript_file),
        transcript_jsonl_path: outcome
            .transcript_path
            .as_ref()
            .map(|path| path.display().to_string()),
        transcript_source,
        isolation_notes: vec![
            "created an owned rmux session and cleaned up only that session".to_owned(),
            "launched Claude with cwd set to --workspace using structured argv".to_owned(),
            "interactive Claude may still apply user-level account settings outside rmux control"
                .to_owned(),
        ],
    }
}

fn ensure_directory(path: &Path, flag: &str) -> Result<(), SetupError> {
    let metadata = path.metadata().map_err(|error| {
        SetupError::new(format!(
            "{flag} {} is not accessible: {error}",
            path.display()
        ))
    })?;
    if metadata.is_dir() {
        Ok(())
    } else {
        Err(SetupError::new(format!(
            "{flag} {} is not a directory",
            path.display()
        )))
    }
}

fn ensure_file(path: &Path, flag: &str) -> Result<(), SetupError> {
    let metadata = path.metadata().map_err(|error| {
        SetupError::new(format!(
            "{flag} {} is not accessible: {error}",
            path.display()
        ))
    })?;
    if metadata.is_file() {
        Ok(())
    } else {
        Err(SetupError::new(format!(
            "{flag} {} is not a file",
            path.display()
        )))
    }
}

fn absolutize_existing(path: PathBuf) -> Result<PathBuf, SetupError> {
    path.canonicalize().map_err(SetupError::from)
}

fn absolutize_maybe_missing(path: PathBuf) -> Result<PathBuf, SetupError> {
    if path.is_absolute() {
        Ok(path)
    } else {
        Ok(env::current_dir()?.join(path))
    }
}

fn unique_session_name() -> String {
    format!(
        "claude-rmux-runner-{}-{}",
        std::process::id(),
        epoch_millis()
    )
}

fn epoch_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

/// A fresh v4-style UUID for `--session-id`. Reads the kernel UUID source (the
/// canonical form Claude expects); falls back to a synthetic id only if that is
/// unavailable, which does not happen on the Linux hosts this runner targets.
fn new_uuid() -> String {
    fs::read_to_string("/proc/sys/kernel/random/uuid")
        .map(|value| value.trim().to_owned())
        .unwrap_or_else(|_| format!("rmux-{}-{}", std::process::id(), epoch_millis()))
}

/// Root under which Claude stores per-project session `.jsonl` files.
/// `CLAUDE_CONFIG_DIR` relocates it; otherwise it is `~/.claude/projects`.
fn claude_projects_root() -> Option<PathBuf> {
    if let Some(dir) = env::var_os("CLAUDE_CONFIG_DIR") {
        return Some(PathBuf::from(dir).join("projects"));
    }
    env::var_os("HOME").map(|home| PathBuf::from(home).join(".claude").join("projects"))
}

/// Locate and read the session transcript Claude wrote for `session_id`.
///
/// Claude names the file `<session_id>.jsonl` and files it under a per-cwd
/// project directory (`projects/<munged-cwd>/`). Rather than reproduce that
/// path munging, we scan the projects root for the uniquely named file — the
/// run owns the id, so at most one directory holds it.
fn harvest_transcript(session_id: &str) -> (Option<PathBuf>, Option<String>) {
    let Some(root) = claude_projects_root() else {
        return (None, None);
    };
    let file_name = format!("{session_id}.jsonl");
    let Ok(entries) = fs::read_dir(&root) else {
        return (None, None);
    };
    for entry in entries.flatten() {
        let candidate = entry.path().join(&file_name);
        if candidate.is_file() {
            let text = fs::read_to_string(&candidate).ok();
            return (Some(candidate), text);
        }
    }
    (None, None)
}

fn resolve_executable(command: &str) -> Result<String, String> {
    let path = Path::new(command);
    if path.components().count() > 1 {
        return executable_path(path)
            .map(|path| path_string(&path))
            .ok_or_else(|| format!("executable not found or not executable: {command}"));
    }

    let path_var = env::var_os("PATH").ok_or_else(|| "PATH is not set".to_owned())?;
    resolve_executable_in_path(command, env::split_paths(&path_var))
        .ok_or_else(|| format!("executable {command:?} was not found in PATH"))
}

fn resolve_executable_in_path<I>(command: &str, directories: I) -> Option<String>
where
    I: IntoIterator<Item = PathBuf>,
{
    for directory in directories {
        let candidate = directory.join(command);
        if let Some(path) = executable_path(&candidate) {
            return Some(path_string(&path));
        }
    }
    None
}

fn executable_path(path: &Path) -> Option<PathBuf> {
    let metadata = path.metadata().ok()?;
    if !metadata.is_file() {
        return None;
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o111 == 0 {
            return None;
        }
    }

    Some(path.to_path_buf())
}

fn path_string(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_declares_required_arguments() {
        Cli::command().debug_assert();
        let result = Cli::try_parse_from(["runner"]);
        assert!(result.is_err());
    }

    #[test]
    fn cli_parses_optional_arguments() {
        let cli = Cli::try_parse_from([
            "runner",
            "--workspace",
            "/tmp/ws",
            "--prompt-file",
            "/tmp/prompt.txt",
            "--result-file",
            "/tmp/result",
            "--trace-file",
            "/tmp/trace",
            "--timeout-seconds",
            "60",
            "--model",
            "opus",
            "--session-name",
            "session",
            "--claude-bin",
            "/bin/claude",
            "--rmux-bin",
            "/bin/rmux",
            "--final-message-file",
            "/tmp/final",
            "--permission-mode",
            "bypassPermissions",
        ])
        .unwrap();
        assert_eq!(cli.model.as_deref(), Some("opus"));
        assert_eq!(cli.session_name.as_deref(), Some("session"));
        assert_eq!(cli.permission_mode, PermissionMode::BypassPermissions);
    }

    #[test]
    fn permission_mode_maps_to_claude_values() {
        assert_eq!(PermissionMode::AcceptEdits.claude_value(), "acceptEdits");
        assert_eq!(
            PermissionMode::BypassPermissions.claude_value(),
            "bypassPermissions"
        );
        assert_eq!(PermissionMode::Default.claude_value(), "default");
    }

    #[test]
    fn exit_reason_maps_to_required_codes() {
        assert_eq!(ExitReason::Completed.code(), 0);
        assert_eq!(ExitReason::ClaudeExited.code(), 1);
        assert_eq!(ExitReason::Timeout.code(), 2);
        assert_eq!(ExitReason::SetupFailure.code(), 3);
        assert_eq!(ExitReason::Interrupted.code(), 4);
    }

    #[test]
    fn claude_command_contains_interactive_flags_and_model() {
        let argv = claude_argv(
            "/usr/bin/claude",
            Some("sonnet"),
            &PermissionMode::AcceptEdits,
            "11111111-2222-3333-4444-555555555555",
        );
        assert_eq!(argv[0], "/usr/bin/claude");
        assert!(!argv.contains(&"--bare".to_owned()));
        assert!(argv.contains(&"--safe-mode".to_owned()));
        assert!(argv.contains(&"--disable-slash-commands".to_owned()));
        assert!(argv.contains(&"--strict-mcp-config".to_owned()));
        assert!(argv
            .windows(2)
            .any(|pair| pair == ["--permission-mode", "acceptEdits"]));
        assert!(argv.windows(2).any(|pair| pair == ["--model", "sonnet"]));
        assert!(argv.windows(2).any(
            |pair| pair == ["--session-id", "11111111-2222-3333-4444-555555555555"]
        ));
    }

    #[test]
    fn spawn_command_strips_claude_nesting_env_before_claude() {
        let argv = claude_argv("/usr/bin/claude", None, &PermissionMode::Default, "sid");
        let command = spawn_command(argv.clone());
        assert_eq!(command[0], "env");
        // Every nesting var is unset via a `-u NAME` pair ahead of the binary.
        for name in CLAUDE_NESTING_ENV {
            assert!(command
                .windows(2)
                .any(|pair| pair == ["-u", *name]));
        }
        // The Claude argv is preserved intact after the `env` prefix.
        let claude_start = command.iter().position(|arg| arg == "/usr/bin/claude").unwrap();
        assert_eq!(&command[claude_start..], argv.as_slice());
    }

    #[test]
    fn metadata_json_has_required_shape() {
        let config = Config {
            workspace: PathBuf::from("/tmp/ws"),
            prompt_file: PathBuf::from("/tmp/prompt"),
            result_file: PathBuf::from("/tmp/result"),
            trace_file: PathBuf::from("/tmp/trace"),
            transcript_file: PathBuf::from("/tmp/trace.jsonl"),
            timeout: Duration::from_secs(1),
            model: None,
            session_name: "s".to_owned(),
            claude_bin: "claude".to_owned(),
            rmux_bin: None,
            final_message_file: None,
            permission_mode: PermissionMode::Default,
            claude_session_id: "test-session-id".to_owned(),
        };
        let outcome = RunOutcome {
            exit_reason: ExitReason::Timeout,
            session: "s".to_owned(),
            pane: "0:0".to_owned(),
            claude_command: vec!["claude".to_owned()],
            final_visible_text: String::new(),
            final_output_dir: None,
            transcript_path: None,
            transcript_text: None,
        };
        let value =
            serde_json::to_value(metadata_for(&config, &outcome, None, "none")).unwrap();
        assert_eq!(value["entrypoint"], ENTRYPOINT);
        assert_eq!(value["exit_code"], 2);
        assert_eq!(value["permission_mode"], "default");
        assert_eq!(value["result_file_exists"], false);
        assert_eq!(value["final_output_dir"], serde_json::Value::Null);
        assert_eq!(value["claude_session_id"], "test-session-id");
        assert_eq!(value["transcript_jsonl_path"], serde_json::Value::Null);
        assert_eq!(value["transcript_file"], "/tmp/trace.jsonl");
        assert_eq!(value["transcript_source"], "none");
        assert_eq!(value["setup_error"], serde_json::Value::Null);
    }

    #[test]
    fn transcript_file_defaults_beside_trace_file() {
        let cli = Cli::try_parse_from([
            "runner",
            "--workspace",
            "/tmp",
            "--prompt-file",
            "/etc/hostname",
            "--result-file",
            "/tmp/result",
            "--trace-file",
            "/tmp/meta.json",
            "--timeout-seconds",
            "60",
        ])
        .unwrap();
        assert!(cli.transcript_file.is_none());
        let config = Config::from_cli(cli).unwrap();
        assert_eq!(config.transcript_file, PathBuf::from("/tmp/trace.jsonl"));
    }

    #[test]
    fn resolve_executable_finds_absolute_executable() {
        let exe = env::current_exe().unwrap();
        assert_eq!(
            resolve_executable(exe.to_str().unwrap()).unwrap(),
            path_string(&exe)
        );
    }

    #[test]
    fn resolve_executable_in_supplied_path_finds_command() {
        let exe = env::current_exe().unwrap();
        let name = exe.file_name().unwrap().to_string_lossy().to_string();
        let directory = exe.parent().unwrap().to_path_buf();
        assert_eq!(
            resolve_executable_in_path(&name, [directory]).unwrap(),
            path_string(&exe)
        );
    }

    #[test]
    fn bracketed_paste_wraps_prompt_with_markers() {
        let wrapped = bracketed_paste("line one\nline two");
        assert_eq!(wrapped, "\u{1b}[200~line one\nline two\u{1b}[201~");
        assert!(wrapped.starts_with(BRACKETED_PASTE_START));
        assert!(wrapped.ends_with(BRACKETED_PASTE_END));
        // The prompt bytes, including the embedded newline, survive verbatim.
        assert!(wrapped.contains("line one\nline two"));
    }

    #[test]
    fn input_box_retains_detects_unsent_prompt() {
        let needle = "You are running inside";
        // Prompt still sitting in the input box (marker line at the bottom).
        let pending = "\
            some transcript above\n\
            ────────────\n\
            ❯ You are running inside a fresh workspace\n\
              do exactly these steps\n\
            ────────────";
        assert!(input_box_retains(pending, needle));

        // Submitted: input box cleared to its placeholder. The sent message is
        // echoed back higher up in the transcript, but without the ❯ marker, so
        // it must not be mistaken for pending input.
        let submitted = "\
            > You are running inside a fresh workspace (sent message echo)\n\
            ... assistant working ...\n\
            ────────────\n\
            ❯ Try \"write a test\"\n\
            ────────────";
        assert!(!input_box_retains(submitted, needle));

        // Empty needle never matches an inline prompt.
        assert!(!input_box_retains(pending, ""));

        // A multi-line paste is collapsed to a placeholder on the input line;
        // that still counts as pending even without the needle text.
        let collapsed = "\
            ... assistant idle ...\n\
            ────────────\n\
            ❯ [Pasted text #1 +7 lines]\n\
            ────────────";
        assert!(input_box_retains(collapsed, needle));
        assert!(input_box_retains(collapsed, ""));
    }

    #[test]
    fn has_trust_prompt_detects_dialog() {
        let dialog = "\
            Quick safety check: Is this a project you created or one you trust?\n\
            ❯ 1. Yes, I trust this folder\n\
              2. No, exit\n\
            Enter to confirm · Esc to cancel";
        assert!(has_trust_prompt(dialog));
        assert!(!has_trust_prompt("❯ Try \"write a test\""));
    }

    #[test]
    fn result_is_complete_keys_on_nonempty_existence() {
        let dir = env::temp_dir().join(format!(
            "claude-rmux-result-{}-{}",
            std::process::id(),
            epoch_millis()
        ));
        fs::create_dir_all(&dir).unwrap();
        let result_file = dir.join("agent_result.txt");

        // Missing file → not complete.
        assert!(!result_is_complete(&result_file));

        // Empty / whitespace-only → not complete (guards a pre-write create).
        fs::write(&result_file, "   \n").unwrap();
        assert!(!result_is_complete(&result_file));

        // Bare path that does not exist → complete: contents are opaque now.
        fs::write(&result_file, "/no/such/path/at/all").unwrap();
        assert!(result_is_complete(&result_file));

        // Structured (JSON) result file → complete.
        fs::write(&result_file, "{\n  \"final_output_dir\": \"/x\"\n}\n").unwrap();
        assert!(result_is_complete(&result_file));

        // Non-UTF-8 body with a non-whitespace byte → complete (contents opaque).
        fs::write(&result_file, [0x20, 0xff, 0x0a]).unwrap();
        assert!(result_is_complete(&result_file));

        // Bare existing path → still complete, and resolvable for the metadata.
        fs::write(&result_file, format!("{}\n", dir.display())).unwrap();
        assert!(result_is_complete(&result_file));
        assert_eq!(
            resolve_result_dir(&result_file).as_deref(),
            Some(dir.to_string_lossy().as_ref())
        );

        // A JSON body is not a resolvable dir → best-effort resolve yields None.
        fs::write(&result_file, "{ \"a\": 1 }").unwrap();
        assert_eq!(resolve_result_dir(&result_file), None);

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn prompt_with_shell_metacharacters_is_not_in_argv() {
        let prompt = std::ffi::OsString::from("hello ' ; $(rm -rf /)\nnext");
        let argv = claude_argv("claude", None, &PermissionMode::Default, "sid");
        assert!(!argv.iter().any(|arg| arg == &prompt.to_string_lossy()));
    }
}
