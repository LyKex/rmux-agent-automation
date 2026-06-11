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

#[derive(Debug, Parser)]
#[command(name = "claude-rmux-runner")]
#[command(about = "Run interactive Claude in an isolated rmux-owned session")]
struct Cli {
    #[arg(long)]
    workspace: PathBuf,
    #[arg(long)]
    prompt_file: PathBuf,
    #[arg(long)]
    result_file: PathBuf,
    #[arg(long)]
    trace_file: PathBuf,
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
    timeout: Duration,
    model: Option<String>,
    session_name: String,
    claude_bin: String,
    rmux_bin: Option<String>,
    final_message_file: Option<PathBuf>,
    permission_mode: PermissionMode,
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
        if let Some(path) = &cli.final_message_file {
            if let Some(parent) = path.parent().filter(|path| !path.as_os_str().is_empty()) {
                ensure_directory(parent, "--final-message-file parent")?;
            }
        }

        Ok(Self {
            workspace: absolutize_existing(cli.workspace)?,
            prompt_file: absolutize_existing(cli.prompt_file)?,
            result_file: absolutize_maybe_missing(cli.result_file)?,
            trace_file: absolutize_maybe_missing(cli.trace_file)?,
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

#[derive(Serialize)]
struct Metadata {
    entrypoint: &'static str,
    workspace: String,
    session: String,
    pane: String,
    result_file: String,
    result_file_exists: bool,
    exit_reason: ExitReason,
    exit_code: u8,
    claude_command: Vec<String>,
    permission_mode: String,
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
            };
            let _ = write_trace(&config, &outcome, Some(&error.to_string()));
            eprintln!("setup failure: {error}");
            emit_metadata_and_exit(&config, &outcome, false);
            return ExitCode::from(outcome.exit_reason.code());
        }
    };

    emit_metadata_and_exit(&config, &outcome, true);
    ExitCode::from(outcome.exit_reason.code())
}

fn emit_metadata_and_exit(config: &Config, outcome: &RunOutcome, write_clean_trace: bool) {
    if let Some(path) = &config.final_message_file {
        let _ = fs::write(path, &outcome.final_visible_text);
    }

    if write_clean_trace {
        let _ = write_trace(config, outcome, None);
    }

    let metadata = metadata_for(config, outcome);
    match serde_json::to_string(&metadata) {
        Ok(json) => println!("{json}"),
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
    let claude_command = claude_argv(
        &claude_path,
        config.model.as_deref(),
        &config.permission_mode,
    );

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

    let keyboard = pane.keyboard();
    keyboard.type_text(&prompt).await?;
    // type_text delivers the prompt as one literal blob; Claude's TUI treats it
    // as a paste and coalesces it. Pressing Enter before that paste window closes
    // gets absorbed as another newline instead of submitting, so wait for the
    // pane to settle first, then submit.
    pane.wait_until_stable_for(Duration::from_millis(500))
        .timeout(Duration::from_secs(30))
        .await?;
    keyboard.press("Enter").await?;

    let exit_reason = wait_for_completion(&pane, &config.result_file, config.timeout).await;
    let final_visible_text = pane
        .snapshot()
        .await
        .map(|snapshot| snapshot.visible_text())
        .unwrap_or_default();

    owned.cleanup().await?;

    Ok(RunOutcome {
        exit_reason,
        session: config.session_name.clone(),
        pane: "0:0".to_owned(),
        claude_command,
        final_visible_text,
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
                if result_file.exists() {
                    return ExitReason::Completed;
                }
                if pane_has_exited(pane).await.unwrap_or(true) {
                    if result_file.exists() {
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

fn claude_argv(
    claude_path: &str,
    model: Option<&str>,
    permission_mode: &PermissionMode,
) -> Vec<String> {
    let mut argv = vec![
        claude_path.to_owned(),
        "--bare".to_owned(),
        "--safe-mode".to_owned(),
        "--disable-slash-commands".to_owned(),
        "--strict-mcp-config".to_owned(),
        "--permission-mode".to_owned(),
        permission_mode.claude_value().to_owned(),
    ];
    if let Some(model) = model {
        argv.push("--model".to_owned());
        argv.push(model.to_owned());
    }
    argv
}

fn write_trace(config: &Config, outcome: &RunOutcome, setup_error: Option<&str>) -> io::Result<()> {
    let mut lines = Vec::new();
    lines.push(format!("timestamp={}", timestamp()));
    lines.push(format!("entrypoint={ENTRYPOINT}"));
    lines.push(format!("workspace={}", config.workspace.display()));
    lines.push(format!("prompt_file={}", config.prompt_file.display()));
    lines.push(format!("result_file={}", config.result_file.display()));
    lines.push(format!("trace_file={}", config.trace_file.display()));
    lines.push(format!("session={}", outcome.session));
    lines.push(format!("pane={}", outcome.pane));
    lines.push(format!(
        "permission_mode={}",
        config.permission_mode.claude_value()
    ));
    lines.push(format!(
        "claude_command={}",
        serde_json::to_string(&outcome.claude_command).unwrap_or_default()
    ));
    if let Some(rmux_bin) = &config.rmux_bin {
        lines.push(format!("rmux_bin={rmux_bin}"));
    }
    lines.push("prompt_send_event=sent_exact_prompt_file_bytes_via_keyboard".to_owned());
    lines.push("prompt_submit_event=pressed_enter_after_prompt".to_owned());
    lines.push(format!(
        "result_file_exists={}",
        config.result_file.exists()
    ));
    lines.push(format!("exit_reason={:?}", outcome.exit_reason));
    lines.push(format!("exit_code={}", outcome.exit_reason.code()));
    if let Some(error) = setup_error {
        lines.push(format!("setup_error={error}"));
    }
    lines.push("terminal_snapshot_begin".to_owned());
    lines.push(outcome.final_visible_text.clone());
    lines.push("terminal_snapshot_end".to_owned());
    lines.push(String::new());
    fs::write(&config.trace_file, lines.join("\n"))
}

fn metadata_for(config: &Config, outcome: &RunOutcome) -> Metadata {
    Metadata {
        entrypoint: ENTRYPOINT,
        workspace: path_string(&config.workspace),
        session: outcome.session.clone(),
        pane: outcome.pane.clone(),
        result_file: path_string(&config.result_file),
        result_file_exists: config.result_file.exists(),
        exit_reason: outcome.exit_reason,
        exit_code: outcome.exit_reason.code(),
        claude_command: outcome.claude_command.clone(),
        permission_mode: config.permission_mode.claude_value().to_owned(),
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

fn timestamp() -> String {
    format!("{}", epoch_millis())
}

fn epoch_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

fn resolve_executable(command: &str) -> Result<String, String> {
    let path = Path::new(command);
    if path.components().count() > 1 {
        return executable_path(path)
            .map(path_to_string)
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
            return Some(path_to_string(path));
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

fn path_to_string(path: PathBuf) -> String {
    path.to_string_lossy().into_owned()
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
        );
        assert_eq!(argv[0], "/usr/bin/claude");
        assert!(argv.contains(&"--bare".to_owned()));
        assert!(argv.contains(&"--safe-mode".to_owned()));
        assert!(argv.contains(&"--disable-slash-commands".to_owned()));
        assert!(argv.contains(&"--strict-mcp-config".to_owned()));
        assert!(argv
            .windows(2)
            .any(|pair| pair == ["--permission-mode", "acceptEdits"]));
        assert!(argv.windows(2).any(|pair| pair == ["--model", "sonnet"]));
    }

    #[test]
    fn metadata_json_has_required_shape() {
        let config = Config {
            workspace: PathBuf::from("/tmp/ws"),
            prompt_file: PathBuf::from("/tmp/prompt"),
            result_file: PathBuf::from("/tmp/result"),
            trace_file: PathBuf::from("/tmp/trace"),
            timeout: Duration::from_secs(1),
            model: None,
            session_name: "s".to_owned(),
            claude_bin: "claude".to_owned(),
            rmux_bin: None,
            final_message_file: None,
            permission_mode: PermissionMode::Default,
        };
        let outcome = RunOutcome {
            exit_reason: ExitReason::Timeout,
            session: "s".to_owned(),
            pane: "0:0".to_owned(),
            claude_command: vec!["claude".to_owned()],
            final_visible_text: String::new(),
        };
        let value = serde_json::to_value(metadata_for(&config, &outcome)).unwrap();
        assert_eq!(value["entrypoint"], ENTRYPOINT);
        assert_eq!(value["exit_code"], 2);
        assert_eq!(value["permission_mode"], "default");
        assert_eq!(value["result_file_exists"], false);
    }

    #[test]
    fn resolve_executable_finds_absolute_executable() {
        let exe = env::current_exe().unwrap();
        assert_eq!(
            resolve_executable(exe.to_str().unwrap()).unwrap(),
            path_to_string(exe)
        );
    }

    #[test]
    fn resolve_executable_in_supplied_path_finds_command() {
        let exe = env::current_exe().unwrap();
        let name = exe.file_name().unwrap().to_string_lossy().to_string();
        let directory = exe.parent().unwrap().to_path_buf();
        assert_eq!(
            resolve_executable_in_path(&name, [directory]).unwrap(),
            path_to_string(exe)
        );
    }

    #[test]
    fn prompt_with_shell_metacharacters_is_not_in_argv() {
        let prompt = std::ffi::OsString::from("hello ' ; $(rm -rf /)\nnext");
        let argv = claude_argv("claude", None, &PermissionMode::Default);
        assert!(!argv.iter().any(|arg| arg == &prompt.to_string_lossy()));
    }
}
