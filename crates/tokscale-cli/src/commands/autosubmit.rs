use std::fmt;
use std::fs;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

use anyhow::{Context, Result, anyhow, bail};
use chrono::{DateTime, Duration, Local, Utc};
use clap::Subcommand;
use serde::{Deserialize, Serialize};

use crate::tui::settings::Settings;
use crate::{SUBMIT_MACHINE_ERROR_PREFIX, SubmitCommandArgs, SubmitFilterArgs};

const AUTOSUBMIT_CRON_MARKER: &str = "# tokscale-autosubmit";
const AUTOSUBMIT_TASK_NAME: &str = "tokscale-autosubmit";
const AUTOSUBMIT_LAUNCHD_LABEL: &str = "com.tokscale.autosubmit";
const DEFAULT_HEARTBEAT_MINUTES: u32 = 60;
const STALE_LOCK_MAX_AGE_SECS: i64 = 24 * 60 * 60;

#[derive(Debug, Clone, Subcommand)]
pub enum AutosubmitCommands {
    #[command(about = "Enable scheduled automatic submit")]
    Enable {
        #[arg(
            long,
            value_name = "INTERVAL",
            help = "Autosubmit interval like 2h or 3d"
        )]
        interval: String,
        #[command(flatten)]
        submit: SubmitFilterArgs,
    },
    #[command(about = "Disable scheduled automatic submit")]
    Disable,
    #[command(about = "Show autosubmit status")]
    Status,
    #[command(about = "Run autosubmit if the saved interval is due")]
    Run,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AutosubmitConfig {
    pub enabled: bool,
    pub interval: IntervalSpec,
    pub submit_args: SubmitCommandArgs,
    pub scheduler: SchedulerMetadata,
    pub created_at: DateTime<Utc>,
    pub last_run_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SchedulerMetadata {
    pub kind: SchedulerKind,
    pub identifier: String,
    pub heartbeat_minutes: u32,
    pub command_preview: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum SchedulerKind {
    Launchd,
    SystemdUser,
    Cron,
    WindowsTask,
}

impl fmt::Display for SchedulerKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SchedulerKind::Launchd => write!(f, "launchd"),
            SchedulerKind::SystemdUser => write!(f, "systemd-user"),
            SchedulerKind::Cron => write!(f, "cron"),
            SchedulerKind::WindowsTask => write!(f, "windows-task"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct IntervalSpec {
    pub raw: String,
    pub value: u32,
    pub unit: IntervalUnit,
}

impl IntervalSpec {
    pub fn duration(&self) -> Duration {
        match self.unit {
            IntervalUnit::Hours => Duration::hours(self.value as i64),
            IntervalUnit::Days => Duration::days(self.value as i64),
        }
    }
}

impl fmt::Display for IntervalSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.raw)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum IntervalUnit {
    Hours,
    Days,
}

pub fn handle_autosubmit_command(command: AutosubmitCommands) -> Result<()> {
    match command {
        AutosubmitCommands::Enable { interval, submit } => run_autosubmit_enable(&interval, submit),
        AutosubmitCommands::Disable => run_autosubmit_disable(),
        AutosubmitCommands::Status => run_autosubmit_status(),
        AutosubmitCommands::Run => {
            if run_autosubmit_run().is_err() {
                std::process::exit(1);
            }
            Ok(())
        }
    }
}

pub fn parse_interval_spec(raw: &str) -> Result<IntervalSpec> {
    let trimmed = raw.trim().to_ascii_lowercase();
    if trimmed.len() < 2 {
        bail!("Invalid interval '{raw}'. Use formats like 2h or 3d.");
    }

    let (value_part, unit_part) = trimmed.split_at(trimmed.len() - 1);
    let value: u32 = value_part.parse().with_context(|| {
        format!("Invalid interval '{raw}'. Interval value must be a positive integer.")
    })?;
    if value == 0 {
        bail!("Invalid interval '{raw}'. Interval value must be greater than zero.");
    }

    let unit = match unit_part {
        "h" => IntervalUnit::Hours,
        "d" => IntervalUnit::Days,
        _ => bail!("Invalid interval '{raw}'. Only h and d suffixes are supported."),
    };

    Ok(IntervalSpec {
        raw: trimmed,
        value,
        unit,
    })
}

fn tokscale_config_dir() -> Result<PathBuf> {
    let config_dir = dirs::config_dir()
        .ok_or_else(|| anyhow!("Could not find config directory"))?
        .join("tokscale");
    if !config_dir.exists() {
        fs::create_dir_all(&config_dir)?;
    }
    Ok(config_dir)
}

fn autosubmit_identifier(kind: SchedulerKind) -> String {
    std::env::var("TOKSCALE_AUTOSUBMIT_IDENTIFIER").unwrap_or_else(|_| {
        match kind {
            SchedulerKind::Launchd => AUTOSUBMIT_LAUNCHD_LABEL,
            SchedulerKind::SystemdUser | SchedulerKind::Cron | SchedulerKind::WindowsTask => {
                AUTOSUBMIT_TASK_NAME
            }
        }
        .to_string()
    })
}

fn autosubmit_log_path() -> Result<PathBuf> {
    Ok(tokscale_config_dir()?.join("autosubmit.log"))
}

fn autosubmit_lock_path() -> Result<PathBuf> {
    Ok(tokscale_config_dir()?.join("autosubmit.lock"))
}

fn shell_escape(value: &str) -> String {
    if value.is_empty() {
        return "''".to_string();
    }
    format!("'{}'", value.replace('\'', "'\\''"))
}

pub fn build_command_preview(executable: &std::path::Path) -> String {
    format!(
        "{} autosubmit run",
        shell_escape(&executable.display().to_string())
    )
}

fn sanitize_submit_command_args(args: &SubmitCommandArgs) -> SubmitCommandArgs {
    let mut sanitized = args.clone();
    sanitized.dry_run = false;
    sanitized
}

fn orphan_scheduler_kinds_for_platform(os: &str) -> Vec<SchedulerKind> {
    match os {
        "macos" => vec![SchedulerKind::Launchd],
        "windows" => vec![SchedulerKind::WindowsTask],
        "linux" => vec![SchedulerKind::SystemdUser, SchedulerKind::Cron],
        _ => vec![SchedulerKind::Cron],
    }
}

fn scheduler_kind_supported_on_platform(kind: &SchedulerKind, os: &str) -> bool {
    match kind {
        SchedulerKind::Launchd => os == "macos",
        SchedulerKind::SystemdUser => os == "linux",
        SchedulerKind::Cron => os != "windows",
        SchedulerKind::WindowsTask => os == "windows",
    }
}

fn scheduler_kind_supported_on_current_platform(kind: &SchedulerKind) -> bool {
    scheduler_kind_supported_on_platform(kind, std::env::consts::OS)
}

fn orphan_scheduler_candidates(executable: &std::path::Path) -> Vec<AutosubmitConfig> {
    let command_preview = build_command_preview(executable);
    orphan_scheduler_kinds_for_platform(std::env::consts::OS)
        .into_iter()
        .map(|kind| AutosubmitConfig {
            enabled: true,
            interval: IntervalSpec {
                raw: "1h".to_string(),
                value: 1,
                unit: IntervalUnit::Hours,
            },
            submit_args: SubmitCommandArgs::default(),
            scheduler: SchedulerMetadata {
                identifier: autosubmit_identifier(kind.clone()),
                kind,
                heartbeat_minutes: DEFAULT_HEARTBEAT_MINUTES,
                command_preview: command_preview.clone(),
            },
            created_at: Utc::now(),
            last_run_at: None,
        })
        .collect()
}

fn detect_orphan_schedulers_with_probe<F>(
    executable: &std::path::Path,
    mut probe: F,
) -> Vec<AutosubmitConfig>
where
    F: FnMut(&AutosubmitConfig) -> SchedulerProbeResult,
{
    let mut orphaned = Vec::new();
    for candidate in orphan_scheduler_candidates(executable) {
        if matches!(probe(&candidate), SchedulerProbeResult::Installed) {
            orphaned.push(candidate);
        }
    }
    orphaned
}

fn format_scheduler_targets(configs: &[AutosubmitConfig]) -> String {
    configs
        .iter()
        .map(|config| {
            format!(
                "{} '{}'",
                config.scheduler.kind, config.scheduler.identifier
            )
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn orphan_scheduler_reason(configs: &[AutosubmitConfig]) -> String {
    if configs.len() == 1 {
        format!(
            "Autosubmit settings are missing but scheduler is still installed: {}",
            format_scheduler_targets(configs)
        )
    } else {
        format!(
            "Autosubmit settings are missing but schedulers are still installed: {}",
            format_scheduler_targets(configs)
        )
    }
}

#[cfg(any(windows, test))]
fn render_windows_task_command(executable: &std::path::Path) -> Result<String> {
    let log_path = autosubmit_log_path()?;
    Ok(format!(
        r#"cmd /C "\"{}\" autosubmit run >> \"{}\" 2>&1""#,
        executable.display(),
        log_path.display()
    ))
}

fn select_scheduler_kind_for_platform(os: &str, systemd_user_available: bool) -> SchedulerKind {
    match os {
        "macos" => SchedulerKind::Launchd,
        "windows" => SchedulerKind::WindowsTask,
        "linux" if systemd_user_available => SchedulerKind::SystemdUser,
        _ => SchedulerKind::Cron,
    }
}

fn systemd_user_available() -> bool {
    if !cfg!(target_os = "linux") {
        return false;
    }

    Command::new("systemctl")
        .args(["--user", "show-environment"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

pub fn scheduler_kind() -> SchedulerKind {
    select_scheduler_kind_for_platform(std::env::consts::OS, systemd_user_available())
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum SchedulerProbeResult {
    Installed,
    Missing,
    Error(String),
}

impl SchedulerProbeResult {
    fn status_label(&self) -> &'static str {
        match self {
            SchedulerProbeResult::Installed => "installed",
            SchedulerProbeResult::Missing => "missing",
            SchedulerProbeResult::Error(_) => "error",
        }
    }

    fn reason(&self, config: &AutosubmitConfig) -> Option<String> {
        match self {
            SchedulerProbeResult::Installed => None,
            SchedulerProbeResult::Missing => Some(format!(
                "{} scheduler '{}' is not installed",
                config.scheduler.kind, config.scheduler.identifier
            )),
            SchedulerProbeResult::Error(reason) => Some(reason.clone()),
        }
    }
}

fn command_output_summary(output: &Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if !stderr.is_empty() {
        return stderr;
    }

    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if !stdout.is_empty() {
        return stdout;
    }

    format!("command exited with status {}", output.status)
}

fn output_mentions_any(output: &Output, needles: &[&str]) -> bool {
    let combined = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
    .to_ascii_lowercase();

    needles.iter().any(|needle| combined.contains(needle))
}

fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[cfg(target_os = "linux")]
fn systemd_exec_escape(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('%', "%%")
}

fn scheduler_shell_command(executable: &std::path::Path, log_path: &std::path::Path) -> String {
    format!(
        "{} autosubmit run >> {} 2>&1",
        shell_escape(&executable.display().to_string()),
        shell_escape(&log_path.display().to_string())
    )
}

fn launchd_agents_dir() -> Result<PathBuf> {
    let dir = dirs::home_dir()
        .ok_or_else(|| anyhow!("Could not determine home directory"))?
        .join("Library/LaunchAgents");
    if !dir.exists() {
        fs::create_dir_all(&dir)?;
    }
    Ok(dir)
}

fn launchd_plist_path(identifier: &str) -> Result<PathBuf> {
    Ok(launchd_agents_dir()?.join(format!("{identifier}.plist")))
}

#[cfg(target_os = "linux")]
fn systemd_user_dir() -> Result<PathBuf> {
    let dir = dirs::config_dir()
        .ok_or_else(|| anyhow!("Could not find config directory"))?
        .join("systemd/user");
    if !dir.exists() {
        fs::create_dir_all(&dir)?;
    }
    Ok(dir)
}

#[cfg(target_os = "linux")]
fn systemd_service_unit(identifier: &str) -> String {
    format!("{identifier}.service")
}

#[cfg(target_os = "linux")]
fn systemd_timer_unit(identifier: &str) -> String {
    format!("{identifier}.timer")
}

#[cfg(target_os = "linux")]
fn systemd_service_path(identifier: &str) -> Result<PathBuf> {
    Ok(systemd_user_dir()?.join(systemd_service_unit(identifier)))
}

#[cfg(target_os = "linux")]
fn systemd_timer_path(identifier: &str) -> Result<PathBuf> {
    Ok(systemd_user_dir()?.join(systemd_timer_unit(identifier)))
}

fn render_launchd_plist(config: &AutosubmitConfig, executable: &std::path::Path) -> Result<String> {
    let log_path = autosubmit_log_path()?;
    let interval_seconds = config.scheduler.heartbeat_minutes.saturating_mul(60);
    let executable_path = xml_escape(&executable.display().to_string());
    let log_path_value = xml_escape(&log_path.display().to_string());
    let mut environment_variables = String::new();
    if let Ok(home) = std::env::var("HOME") {
        environment_variables.push_str("  <key>EnvironmentVariables</key>\n  <dict>\n");
        environment_variables.push_str(&format!(
            "    <key>HOME</key>\n    <string>{}</string>\n",
            xml_escape(&home)
        ));
        if let Ok(xdg_config_home) = std::env::var("XDG_CONFIG_HOME") {
            environment_variables.push_str(&format!(
                "    <key>XDG_CONFIG_HOME</key>\n    <string>{}</string>\n",
                xml_escape(&xdg_config_home)
            ));
        }
        environment_variables.push_str("  </dict>\n");
    }

    Ok(format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>{label}</string>
  <key>Program</key>
  <string>{executable_path}</string>
  <key>ProgramArguments</key>
  <array>
    <string>{executable_path}</string>
    <string>autosubmit</string>
    <string>run</string>
  </array>
  <key>StandardOutPath</key>
  <string>{log_path_value}</string>
  <key>StandardErrorPath</key>
  <string>{log_path_value}</string>
  <key>RunAtLoad</key>
  <true/>
  <key>StartInterval</key>
  <integer>{interval_seconds}</integer>
{environment_variables}</dict>
</plist>
"#,
        label = xml_escape(&config.scheduler.identifier),
        executable_path = executable_path,
        log_path_value = log_path_value,
        interval_seconds = interval_seconds,
        environment_variables = environment_variables,
    ))
}

#[cfg(target_os = "linux")]
fn render_systemd_service(
    _config: &AutosubmitConfig,
    executable: &std::path::Path,
) -> Result<String> {
    let log_path = autosubmit_log_path()?;
    let shell_command = scheduler_shell_command(executable, &log_path);

    Ok(format!(
        "[Unit]\nDescription=Tokscale autosubmit runner\n\n[Service]\nType=oneshot\nExecStart=/bin/sh -lc \"{}\"\n",
        systemd_exec_escape(&shell_command),
    ))
}

#[cfg(target_os = "linux")]
fn render_systemd_timer(config: &AutosubmitConfig) -> String {
    format!(
        "[Unit]\nDescription=Tokscale autosubmit timer\n\n[Timer]\nOnStartupSec=1m\nOnUnitActiveSec={}m\nUnit={}\n\n[Install]\nWantedBy=timers.target\n",
        config.scheduler.heartbeat_minutes,
        systemd_service_unit(&config.scheduler.identifier),
    )
}

#[derive(Debug)]
struct AutosubmitRunLock {
    path: PathBuf,
    token: String,
}

impl Drop for AutosubmitRunLock {
    fn drop(&mut self) {
        let should_remove = fs::read_to_string(&self.path)
            .ok()
            .and_then(|content| parse_lock_file(&content).1.map(str::to_string))
            .map(|token| token == self.token)
            .unwrap_or(true);
        if should_remove {
            let _ = fs::remove_file(&self.path);
        }
    }
}

fn parse_lock_file(content: &str) -> (Option<i64>, Option<&str>) {
    let mut lines = content.lines();
    let timestamp = lines
        .next()
        .and_then(|line| line.trim().parse::<i64>().ok());
    let token = lines.next().map(str::trim).filter(|line| !line.is_empty());
    (timestamp, token)
}

fn generate_lock_token() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    format!("{}-{nanos}", std::process::id())
}

fn write_lock_file(file: &mut fs::File, token: &str) -> Result<()> {
    file.set_len(0)?;
    writeln!(file, "{}", Utc::now().timestamp())?;
    write!(file, "{token}")?;
    file.sync_all()?;
    Ok(())
}

fn is_stale_lock(path: &PathBuf) -> bool {
    match fs::read_to_string(path) {
        Ok(content) => match parse_lock_file(&content).0 {
            Some(timestamp) => Utc::now().timestamp() - timestamp > STALE_LOCK_MAX_AGE_SECS,
            None => fs::metadata(path)
                .and_then(|meta| meta.modified())
                .ok()
                .and_then(|modified| modified.elapsed().ok())
                .map(|age| age.as_secs() as i64 > STALE_LOCK_MAX_AGE_SECS)
                .unwrap_or(false),
        },
        Err(_) => true,
    }
}

fn acquire_run_lock() -> Result<AutosubmitRunLock> {
    let path = autosubmit_lock_path()?;
    for _ in 0..2 {
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(mut file) => {
                let token = generate_lock_token();
                write_lock_file(&mut file, &token)?;
                return Ok(AutosubmitRunLock { path, token });
            }
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                if is_stale_lock(&path) {
                    let _ = fs::remove_file(&path);
                    continue;
                }
                bail!("Autosubmit is already running");
            }
            Err(err) => return Err(err).context("Failed to create autosubmit lock file"),
        }
    }

    bail!("Failed to recover stale autosubmit lock")
}

#[derive(Debug)]
enum AutosubmitRunOutcome {
    Success,
    Skipped(String),
}

fn normalize_reason_for_log(reason: &str) -> String {
    reason.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn escape_reason_for_log(reason: &str) -> String {
    normalize_reason_for_log(reason)
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
}

fn autosubmit_start_line(started_at: DateTime<Local>) -> String {
    format!("{} [autosubmit] start", started_at.to_rfc3339())
}

fn autosubmit_finish_line(
    finished_at: DateTime<Local>,
    status: &str,
    reason: Option<&str>,
) -> String {
    let mut line = format!(
        "{} [autosubmit] finish status={status}",
        finished_at.to_rfc3339(),
    );
    if let Some(reason) = reason {
        line.push_str(&format!(" reason=\"{}\"", escape_reason_for_log(reason)));
    }
    line
}

fn strip_ansi_codes(input: &str) -> String {
    let mut result = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();

    while let Some(ch) = chars.next() {
        if ch == '\u{1b}' && matches!(chars.peek(), Some('[')) {
            chars.next();
            for next in chars.by_ref() {
                if ('@'..='~').contains(&next) {
                    break;
                }
            }
            continue;
        }
        result.push(ch);
    }

    result
}

fn extract_submit_failure_reason(output: &Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let combined = strip_ansi_codes(&format!("{stderr}\n{stdout}"));
    let lines = combined
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>();

    if let Some(line) = lines
        .iter()
        .rev()
        .find_map(|line| line.strip_prefix(SUBMIT_MACHINE_ERROR_PREFIX))
    {
        return line.trim().to_string();
    }
    if let Some(line) = lines.iter().rev().find_map(|line| {
        line.strip_prefix("Error: ")
            .or_else(|| line.strip_prefix("Error:"))
    }) {
        return line.trim().to_string();
    }
    if let Some(line) = lines.iter().rev().find(|line| **line == "Not logged in.") {
        return (*line).to_string();
    }
    lines
        .last()
        .map(|line| (*line).to_string())
        .unwrap_or_else(|| format!("submit exited with status {}", output.status))
}

fn submit_args_to_cli_args(args: &SubmitCommandArgs) -> Vec<String> {
    let filters = &args.filters;
    let mut cli_args = vec!["submit".to_string()];

    for (flag, enabled) in [
        ("--opencode", filters.opencode),
        ("--claude", filters.claude),
        ("--codex", filters.codex),
        ("--gemini", filters.gemini),
        ("--cursor", filters.cursor),
        ("--amp", filters.amp),
        ("--droid", filters.droid),
        ("--openclaw", filters.openclaw),
        ("--pi", filters.pi),
        ("--kimi", filters.kimi),
        ("--qwen", filters.qwen),
        ("--roocode", filters.roocode),
        ("--kilocode", filters.kilocode),
        ("--mux", filters.mux),
        ("--synthetic", filters.synthetic),
        ("--today", filters.today),
        ("--week", filters.week),
        ("--month", filters.month),
    ] {
        if enabled {
            cli_args.push(flag.to_string());
        }
    }

    for (flag, value) in [
        ("--since", filters.since.as_ref()),
        ("--until", filters.until.as_ref()),
        ("--year", filters.year.as_ref()),
    ] {
        if let Some(value) = value {
            cli_args.push(flag.to_string());
            cli_args.push(value.clone());
        }
    }

    if args.dry_run {
        cli_args.push("--dry-run".to_string());
    }

    cli_args
}

fn run_submit_quiet_via_cli(args: &SubmitCommandArgs) -> Result<()> {
    let executable = std::env::current_exe()
        .context("Failed to resolve tokscale executable path for autosubmit")?;
    let output = Command::new(executable)
        .env("TOKSCALE_MACHINE_READABLE_SUBMIT_ERRORS", "1")
        .args(submit_args_to_cli_args(args))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .context("Failed to launch tokscale submit for autosubmit")?;

    if output.status.success() {
        Ok(())
    } else {
        bail!("{}", extract_submit_failure_reason(&output));
    }
}

fn run_autosubmit_run_with_submitter<F>(submitter: F) -> Result<()>
where
    F: FnOnce(&SubmitCommandArgs) -> Result<()>,
{
    run_autosubmit_run_with_submitter_and_logger(submitter, |line| println!("{line}"))
}

fn run_autosubmit_run_with_submitter_and_logger<F, G>(submitter: F, mut logger: G) -> Result<()>
where
    F: FnOnce(&SubmitCommandArgs) -> Result<()>,
    G: FnMut(&str),
{
    let started_at = Local::now();
    logger(&autosubmit_start_line(started_at));

    let result = (|| -> Result<AutosubmitRunOutcome> {
        let _lock = acquire_run_lock()?;
        let mut settings =
            Settings::load_strict().context("Failed to load settings for autosubmit run")?;
        let Some(mut config) = settings.autosubmit.clone() else {
            bail!("Autosubmit is not enabled");
        };

        if !config.enabled {
            bail!("Autosubmit is disabled");
        }

        if !is_due(&config, Utc::now()) {
            return Ok(AutosubmitRunOutcome::Skipped(format!(
                "interval {} is not due yet",
                config.interval.raw
            )));
        }

        let previous_last_run_at = config.last_run_at;
        let started_at = Utc::now();
        config.submit_args = sanitize_submit_command_args(&config.submit_args);
        config.last_run_at = Some(started_at);
        let submit_args = config.submit_args.clone();
        settings.autosubmit = Some(config);
        settings.save()?;

        if let Err(err) = submitter(&submit_args) {
            if let Some(config) = settings.autosubmit.as_mut() {
                config.last_run_at = previous_last_run_at;
            }
            let _ = settings.save();
            return Err(err);
        }

        Ok(AutosubmitRunOutcome::Success)
    })();

    let finished_at = Local::now();
    match &result {
        Ok(AutosubmitRunOutcome::Success) => {
            logger(&autosubmit_finish_line(finished_at, "success", None));
        }
        Ok(AutosubmitRunOutcome::Skipped(reason)) => {
            logger(&autosubmit_finish_line(
                finished_at,
                "skipped",
                Some(reason.as_str()),
            ));
        }
        Err(err) => {
            let reason = err.to_string();
            logger(&autosubmit_finish_line(
                finished_at,
                "failed",
                Some(&reason),
            ));
        }
    }

    result.map(|_| ())
}

fn same_scheduler_target(left: &AutosubmitConfig, right: &AutosubmitConfig) -> bool {
    left.scheduler.kind == right.scheduler.kind
        && left.scheduler.identifier == right.scheduler.identifier
}

fn rollback_enable_scheduler(
    previous: Option<&AutosubmitConfig>,
    new_config: &AutosubmitConfig,
    executable: &std::path::Path,
) {
    match previous {
        Some(previous_config) if same_scheduler_target(previous_config, new_config) => {
            let _ = install_scheduler(previous_config, executable);
        }
        Some(_) | None => {
            let _ = uninstall_scheduler(new_config);
        }
    }
}

fn scheduler_requires_saved_settings_before_install(kind: &SchedulerKind) -> bool {
    matches!(kind, SchedulerKind::Launchd)
}

pub fn run_autosubmit_enable(interval_raw: &str, submit: SubmitFilterArgs) -> Result<()> {
    let _lock = acquire_run_lock()?;
    let interval = parse_interval_spec(interval_raw)?;
    let executable =
        std::env::current_exe().context("Failed to resolve tokscale executable path")?;
    let heartbeat_minutes = DEFAULT_HEARTBEAT_MINUTES;
    let kind = scheduler_kind();
    let identifier = autosubmit_identifier(kind.clone());
    let command_preview = build_command_preview(&executable);
    let scheduler = SchedulerMetadata {
        kind,
        identifier: identifier.clone(),
        heartbeat_minutes,
        command_preview,
    };
    let created_at = Utc::now();
    let config = AutosubmitConfig {
        enabled: true,
        interval,
        submit_args: SubmitCommandArgs {
            filters: submit,
            dry_run: false,
        },
        scheduler,
        created_at,
        last_run_at: None,
    };

    let mut settings =
        Settings::load_strict().context("Failed to load settings for autosubmit enable")?;
    let previous = settings.autosubmit.clone();

    if scheduler_requires_saved_settings_before_install(&config.scheduler.kind) {
        settings.autosubmit = Some(config.clone());
        if let Err(err) = settings.save() {
            settings.autosubmit = previous;
            return Err(err).context("Failed to save settings for autosubmit enable");
        }
    }

    if let Err(err) = install_scheduler(&config, &executable) {
        if scheduler_requires_saved_settings_before_install(&config.scheduler.kind) {
            save_settings_after_failed_enable(&mut settings, previous.clone());
        }
        rollback_enable_scheduler(previous.as_ref(), &config, &executable);
        return Err(err);
    }

    let probe = probe_scheduler(&config);
    if !matches!(probe, SchedulerProbeResult::Installed) {
        if scheduler_requires_saved_settings_before_install(&config.scheduler.kind) {
            save_settings_after_failed_enable(&mut settings, previous.clone());
        }
        rollback_enable_scheduler(previous.as_ref(), &config, &executable);
        bail!(
            "Failed to verify autosubmit scheduler after install: {}",
            probe
                .reason(&config)
                .unwrap_or_else(|| "unknown scheduler error".to_string())
        );
    }

    if !scheduler_requires_saved_settings_before_install(&config.scheduler.kind) {
        settings.autosubmit = Some(config.clone());
        if let Err(err) = settings.save() {
            rollback_enable_scheduler(previous.as_ref(), &config, &executable);
            settings.autosubmit = previous;
            return Err(err).context("Failed to save settings for autosubmit enable");
        }
    }

    if let Some(previous_config) = previous.as_ref() {
        if !same_scheduler_target(previous_config, &config) {
            if let Err(err) = uninstall_scheduler(previous_config) {
                settings.autosubmit = previous.clone();
                let _ = settings.save();
                let rollback_err = uninstall_scheduler(&config).err();
                if let Some(rollback_err) = rollback_err {
                    return Err(err).context(format!(
                        "Failed to remove previous autosubmit scheduler and failed to roll back new scheduler: {rollback_err}"
                    ));
                }
                return Err(err).context("Failed to remove previous autosubmit scheduler");
            }
        }
    }

    println!("\n  Autosubmit enabled.");
    println!("  Interval: {}", config.interval.raw);
    println!("  Scheduler: {}", config.scheduler.kind);
    println!("  Scheduler status: installed");
    println!("  Command: {}\n", config.scheduler.command_preview);
    Ok(())
}

fn save_settings_after_failed_enable(settings: &mut Settings, previous: Option<AutosubmitConfig>) {
    settings.autosubmit = previous;
    let _ = settings.save();
}

fn should_skip_scheduler_side_effects() -> bool {
    std::env::var("TOKSCALE_AUTOSUBMIT_SKIP_SCHEDULER")
        .map(|value| matches!(value.as_str(), "1" | "true" | "yes"))
        .unwrap_or(false)
}

pub fn run_autosubmit_disable() -> Result<()> {
    let _lock = acquire_run_lock()?;
    let executable =
        std::env::current_exe().context("Failed to resolve tokscale executable path")?;
    let mut settings =
        Settings::load_strict().context("Failed to load settings for autosubmit disable")?;
    let orphaned = if should_skip_scheduler_side_effects() {
        Vec::new()
    } else {
        detect_orphan_schedulers_with_probe(&executable, probe_scheduler)
    };

    let Some(config) = settings.autosubmit.clone() else {
        if orphaned.is_empty() {
            println!("\n  Autosubmit is not enabled.\n");
            return Ok(());
        }

        for orphan in &orphaned {
            uninstall_scheduler(orphan)?;
        }

        println!("\n  Autosubmit disabled.");
        println!("  Removed orphan scheduler(s): {}\n", format_scheduler_targets(&orphaned));
        return Ok(());
    };

    if scheduler_kind_supported_on_current_platform(&config.scheduler.kind) {
        uninstall_scheduler(&config)?;
    }

    settings.autosubmit = None;
    if let Err(err) = settings.save() {
        settings.autosubmit = Some(config.clone());
        let _ = settings.save();
        if scheduler_kind_supported_on_current_platform(&config.scheduler.kind) {
            let rollback_err = install_scheduler(&config, &executable).err();
            if let Some(rollback_err) = rollback_err {
                return Err(err).context(format!(
                    "Failed to save settings for autosubmit disable and failed to restore scheduler: {rollback_err}"
                ));
            }
        }
        return Err(err).context("Failed to save settings for autosubmit disable");
    }

    println!("\n  Autosubmit disabled.\n");
    Ok(())
}

pub fn run_autosubmit_status() -> Result<()> {
    let executable =
        std::env::current_exe().context("Failed to resolve tokscale executable path")?;
    let settings =
        Settings::load_strict().context("Failed to load settings for autosubmit status")?;
    match settings.autosubmit {
        Some(config) if config.enabled => {
            let probe = probe_scheduler(&config);
            let overall_status = if matches!(probe, SchedulerProbeResult::Installed) {
                "enabled"
            } else {
                "degraded"
            };

            println!("\n  Autosubmit status: {overall_status}");
            println!("  Interval: {}", config.interval.raw);
            println!("  Scheduler: {}", config.scheduler.kind);
            println!("  Scheduler status: {}", probe.status_label());
            println!("  Scheduler ID: {}", config.scheduler.identifier);
            println!("  Command: {}", config.scheduler.command_preview);
            if let Some(reason) = probe.reason(&config) {
                println!("  Reason: {}", reason);
            }
            println!(
                "  Submit args: {}\n",
                format_submit_args(&config.submit_args)
            );
        }
        _ => {
            let orphaned = if should_skip_scheduler_side_effects() {
                Vec::new()
            } else {
                detect_orphan_schedulers_with_probe(&executable, probe_scheduler)
            };

            if orphaned.is_empty() {
                println!("\n  Autosubmit status: disabled\n");
            } else {
                println!("\n  Autosubmit status: degraded");
                if orphaned.len() == 1 {
                    println!("  Scheduler: {}", orphaned[0].scheduler.kind);
                    println!("  Scheduler status: installed");
                    println!("  Scheduler ID: {}", orphaned[0].scheduler.identifier);
                } else {
                    println!("  Scheduler status: installed");
                }
                println!("  Reason: {}\n", orphan_scheduler_reason(&orphaned));
            }
        }
    }
    Ok(())
}

pub fn run_autosubmit_run() -> Result<()> {
    run_autosubmit_run_with_submitter(run_submit_quiet_via_cli)
}

pub fn is_due(config: &AutosubmitConfig, now: DateTime<Utc>) -> bool {
    let anchor = config.last_run_at.unwrap_or(config.created_at);
    now >= anchor + config.interval.duration()
}

pub fn format_submit_args(args: &SubmitCommandArgs) -> String {
    let filters = &args.filters;
    let mut flags = Vec::new();
    for (name, enabled) in [
        ("--opencode", filters.opencode),
        ("--claude", filters.claude),
        ("--codex", filters.codex),
        ("--gemini", filters.gemini),
        ("--cursor", filters.cursor),
        ("--amp", filters.amp),
        ("--droid", filters.droid),
        ("--openclaw", filters.openclaw),
        ("--pi", filters.pi),
        ("--kimi", filters.kimi),
        ("--qwen", filters.qwen),
        ("--roocode", filters.roocode),
        ("--kilocode", filters.kilocode),
        ("--mux", filters.mux),
        ("--synthetic", filters.synthetic),
        ("--today", filters.today),
        ("--week", filters.week),
        ("--month", filters.month),
    ] {
        if enabled {
            flags.push(name.to_string());
        }
    }
    if let Some(value) = &filters.since {
        flags.push(format!("--since {}", value));
    }
    if let Some(value) = &filters.until {
        flags.push(format!("--until {}", value));
    }
    if let Some(value) = &filters.year {
        flags.push(format!("--year {}", value));
    }

    if flags.is_empty() {
        "(all submit defaults)".to_string()
    } else {
        flags.join(" ")
    }
}

fn install_scheduler(config: &AutosubmitConfig, executable: &std::path::Path) -> Result<()> {
    if should_skip_scheduler_side_effects() {
        return Ok(());
    }

    match config.scheduler.kind {
        SchedulerKind::Launchd => install_launchd_scheduler(config, executable),
        SchedulerKind::SystemdUser => install_systemd_user_scheduler(config, executable),
        SchedulerKind::Cron => install_cron_scheduler(config, executable),
        SchedulerKind::WindowsTask => install_windows_scheduler(config, executable),
    }
}

fn uninstall_scheduler(config: &AutosubmitConfig) -> Result<()> {
    if should_skip_scheduler_side_effects() {
        return Ok(());
    }

    match config.scheduler.kind {
        SchedulerKind::Launchd => uninstall_launchd_scheduler(&config.scheduler.identifier),
        SchedulerKind::SystemdUser => {
            uninstall_systemd_user_scheduler(&config.scheduler.identifier)
        }
        SchedulerKind::Cron => uninstall_cron_scheduler(&config.scheduler.identifier),
        SchedulerKind::WindowsTask => uninstall_windows_scheduler(&config.scheduler.identifier),
    }
}

fn probe_scheduler(config: &AutosubmitConfig) -> SchedulerProbeResult {
    if should_skip_scheduler_side_effects() {
        return SchedulerProbeResult::Installed;
    }

    match config.scheduler.kind {
        SchedulerKind::Launchd => probe_launchd_scheduler(&config.scheduler.identifier),
        SchedulerKind::SystemdUser => probe_systemd_user_scheduler(&config.scheduler.identifier),
        SchedulerKind::Cron => probe_cron_scheduler(&config.scheduler.identifier),
        SchedulerKind::WindowsTask => probe_windows_scheduler(&config.scheduler.identifier),
    }
}

#[cfg(target_os = "macos")]
fn launchd_domain() -> Result<String> {
    let output = Command::new("/usr/bin/id")
        .arg("-u")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .context("Failed to resolve current macOS user id for launchd")?;
    if !output.status.success() {
        bail!(
            "Failed to resolve current macOS user id for launchd: {}",
            command_output_summary(&output)
        );
    }

    let uid = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if uid.is_empty() {
        bail!("Failed to resolve current macOS user id for launchd");
    }

    Ok(format!("gui/{uid}"))
}

#[cfg(target_os = "macos")]
fn launchd_service_target(identifier: &str) -> Result<String> {
    Ok(format!("{}/{}", launchd_domain()?, identifier))
}

#[cfg(target_os = "macos")]
fn install_launchd_scheduler(
    config: &AutosubmitConfig,
    executable: &std::path::Path,
) -> Result<()> {
    let plist_path = launchd_plist_path(&config.scheduler.identifier)?;
    fs::write(&plist_path, render_launchd_plist(config, executable)?)
        .context("Failed to write launchd autosubmit agent")?;

    if let Ok(service_target) = launchd_service_target(&config.scheduler.identifier) {
        let _ = Command::new("launchctl")
            .arg("bootout")
            .arg(service_target)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }

    let domain = launchd_domain()?;
    let output = Command::new("launchctl")
        .arg("bootstrap")
        .arg(&domain)
        .arg(&plist_path)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .context("Failed to create launchd autosubmit agent")?;
    if !output.status.success() {
        let _ = fs::remove_file(&plist_path);
        bail!(
            "Failed to create launchd autosubmit agent: {}",
            command_output_summary(&output)
        );
    }
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn install_launchd_scheduler(
    _config: &AutosubmitConfig,
    _executable: &std::path::Path,
) -> Result<()> {
    bail!("launchd scheduler is not available on this platform")
}

#[cfg(target_os = "macos")]
fn uninstall_launchd_scheduler(identifier: &str) -> Result<()> {
    let plist_path = launchd_plist_path(identifier)?;
    let service_target = launchd_service_target(identifier)?;
    let output = Command::new("launchctl")
        .arg("bootout")
        .arg(&service_target)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .context("Failed to unload launchd autosubmit agent")?;
    if !output.status.success()
        && !output_mentions_any(
            &output,
            &[
                "could not find service",
                "service not found",
                "no such process",
            ],
        )
    {
        bail!(
            "Failed to unload launchd autosubmit agent: {}",
            command_output_summary(&output)
        );
    }

    if plist_path.exists() {
        fs::remove_file(&plist_path).context("Failed to remove launchd autosubmit agent file")?;
    }
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn uninstall_launchd_scheduler(_identifier: &str) -> Result<()> {
    bail!("launchd scheduler is not available on this platform")
}

#[cfg(target_os = "macos")]
fn probe_launchd_scheduler(identifier: &str) -> SchedulerProbeResult {
    let plist_path = match launchd_plist_path(identifier) {
        Ok(path) => path,
        Err(err) => return SchedulerProbeResult::Error(err.to_string()),
    };
    if !plist_path.exists() {
        return SchedulerProbeResult::Missing;
    }

    let service_target = match launchd_service_target(identifier) {
        Ok(target) => target,
        Err(err) => return SchedulerProbeResult::Error(err.to_string()),
    };
    match Command::new("launchctl")
        .arg("print")
        .arg(&service_target)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
    {
        Ok(output) if output.status.success() => SchedulerProbeResult::Installed,
        Ok(output)
            if output_mentions_any(
                &output,
                &[
                    "could not find service",
                    "service not found",
                    "no such process",
                ],
            ) =>
        {
            SchedulerProbeResult::Missing
        }
        Ok(output) => SchedulerProbeResult::Error(format!(
            "Failed to inspect launchd scheduler '{}': {}",
            identifier,
            command_output_summary(&output)
        )),
        Err(err) => SchedulerProbeResult::Error(format!(
            "Failed to inspect launchd scheduler '{}': {}",
            identifier, err
        )),
    }
}

#[cfg(not(target_os = "macos"))]
fn probe_launchd_scheduler(_identifier: &str) -> SchedulerProbeResult {
    SchedulerProbeResult::Error("launchd scheduler is not available on this platform".to_string())
}

#[cfg(target_os = "linux")]
fn install_systemd_user_scheduler(
    config: &AutosubmitConfig,
    executable: &std::path::Path,
) -> Result<()> {
    let service_path = systemd_service_path(&config.scheduler.identifier)?;
    let timer_path = systemd_timer_path(&config.scheduler.identifier)?;
    fs::write(&service_path, render_systemd_service(config, executable)?)
        .context("Failed to write systemd --user service for autosubmit")?;
    fs::write(&timer_path, render_systemd_timer(config))
        .context("Failed to write systemd --user timer for autosubmit")?;

    let daemon_reload = Command::new("systemctl")
        .args(["--user", "daemon-reload"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .context("Failed to reload systemd --user daemon")?;
    if !daemon_reload.status.success() {
        let _ = fs::remove_file(&service_path);
        let _ = fs::remove_file(&timer_path);
        bail!(
            "Failed to reload systemd --user daemon: {}",
            command_output_summary(&daemon_reload)
        );
    }

    let timer_unit = systemd_timer_unit(&config.scheduler.identifier);
    let enable_output = Command::new("systemctl")
        .args(["--user", "enable", "--now", &timer_unit])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .context("Failed to enable systemd --user autosubmit timer")?;
    if !enable_output.status.success() {
        let _ = fs::remove_file(&service_path);
        let _ = fs::remove_file(&timer_path);
        let _ = Command::new("systemctl")
            .args(["--user", "daemon-reload"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        bail!(
            "Failed to enable systemd --user autosubmit timer: {}",
            command_output_summary(&enable_output)
        );
    }

    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn install_systemd_user_scheduler(
    _config: &AutosubmitConfig,
    _executable: &std::path::Path,
) -> Result<()> {
    bail!("systemd --user scheduler is not available on this platform")
}

#[cfg(target_os = "linux")]
fn uninstall_systemd_user_scheduler(identifier: &str) -> Result<()> {
    let timer_unit = systemd_timer_unit(identifier);
    let service_path = systemd_service_path(identifier)?;
    let timer_path = systemd_timer_path(identifier)?;

    let disable_output = Command::new("systemctl")
        .args(["--user", "disable", "--now", &timer_unit])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .context("Failed to disable systemd --user autosubmit timer")?;
    if !disable_output.status.success()
        && !output_mentions_any(
            &disable_output,
            &[
                "not found",
                "not loaded",
                "could not be found",
                "no such file",
            ],
        )
    {
        bail!(
            "Failed to disable systemd --user autosubmit timer: {}",
            command_output_summary(&disable_output)
        );
    }

    if service_path.exists() {
        fs::remove_file(&service_path)
            .context("Failed to remove systemd --user autosubmit service file")?;
    }
    if timer_path.exists() {
        fs::remove_file(&timer_path)
            .context("Failed to remove systemd --user autosubmit timer file")?;
    }

    let daemon_reload = Command::new("systemctl")
        .args(["--user", "daemon-reload"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .context("Failed to reload systemd --user daemon")?;
    if !daemon_reload.status.success() {
        bail!(
            "Failed to reload systemd --user daemon: {}",
            command_output_summary(&daemon_reload)
        );
    }

    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn uninstall_systemd_user_scheduler(_identifier: &str) -> Result<()> {
    bail!("systemd --user scheduler is not available on this platform")
}

#[cfg(target_os = "linux")]
fn probe_systemd_user_scheduler(identifier: &str) -> SchedulerProbeResult {
    let service_path = match systemd_service_path(identifier) {
        Ok(path) => path,
        Err(err) => return SchedulerProbeResult::Error(err.to_string()),
    };
    let timer_path = match systemd_timer_path(identifier) {
        Ok(path) => path,
        Err(err) => return SchedulerProbeResult::Error(err.to_string()),
    };
    if !service_path.exists() && !timer_path.exists() {
        return SchedulerProbeResult::Missing;
    }

    let timer_unit = systemd_timer_unit(identifier);
    match Command::new("systemctl")
        .args(["--user", "is-enabled", &timer_unit])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
    {
        Ok(output) if output.status.success() => SchedulerProbeResult::Installed,
        Ok(output)
            if output_mentions_any(
                &output,
                &["not found", "could not be found", "no such file"],
            ) =>
        {
            SchedulerProbeResult::Missing
        }
        Ok(output) if output_mentions_any(&output, &["disabled"]) => {
            SchedulerProbeResult::Error(format!(
                "systemd --user timer '{}' is installed but not enabled",
                timer_unit
            ))
        }
        Ok(output) => SchedulerProbeResult::Error(format!(
            "Failed to inspect systemd --user timer '{}': {}",
            timer_unit,
            command_output_summary(&output)
        )),
        Err(err) => SchedulerProbeResult::Error(format!(
            "Failed to inspect systemd --user timer '{}': {}",
            timer_unit, err
        )),
    }
}

#[cfg(not(target_os = "linux"))]
fn probe_systemd_user_scheduler(_identifier: &str) -> SchedulerProbeResult {
    SchedulerProbeResult::Error(
        "systemd --user scheduler is not available on this platform".to_string(),
    )
}

fn install_cron_scheduler(config: &AutosubmitConfig, executable: &std::path::Path) -> Result<()> {
    let minute = (config.created_at.timestamp() % 60 + 60) % 60;
    let schedule = format!("{} * * * *", minute);
    let log_path = autosubmit_log_path()?;
    let command = scheduler_shell_command(executable, &log_path);
    let block = format!(
        "{marker} {id}\n{schedule} {command}\n",
        marker = AUTOSUBMIT_CRON_MARKER,
        id = config.scheduler.identifier,
        schedule = schedule,
        command = command,
    );

    let existing = read_crontab()?;
    let cleaned = strip_cron_block(&existing, &config.scheduler.identifier);
    let mut merged = cleaned.trim_end().to_string();
    if !merged.is_empty() {
        merged.push('\n');
    }
    merged.push_str(&block);
    write_crontab(&merged)
}

fn uninstall_cron_scheduler(identifier: &str) -> Result<()> {
    let existing = read_crontab()?;
    let cleaned = strip_cron_block(&existing, identifier);
    write_crontab(cleaned.trim_end())
}

fn probe_cron_scheduler(identifier: &str) -> SchedulerProbeResult {
    match read_crontab() {
        Ok(content) => probe_cron_scheduler_content(&content, identifier),
        Err(err) => SchedulerProbeResult::Error(err.to_string()),
    }
}

fn read_crontab() -> Result<String> {
    let output = Command::new("crontab")
        .arg("-l")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .context("Failed to inspect crontab")?;
    if output.status.success() {
        return Ok(String::from_utf8_lossy(&output.stdout).to_string());
    }
    let stderr = String::from_utf8_lossy(&output.stderr).to_ascii_lowercase();
    if stderr.contains("no crontab") {
        Ok(String::new())
    } else {
        bail!(
            "Failed to inspect crontab: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )
    }
}

fn write_crontab(content: &str) -> Result<()> {
    let mut child = Command::new("crontab")
        .arg("-")
        .stdin(Stdio::piped())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .context("Failed to launch crontab")?;

    use std::io::Write;
    if let Some(stdin) = child.stdin.as_mut() {
        stdin.write_all(content.as_bytes())?;
    }
    let status = child.wait()?;
    if !status.success() {
        bail!("Failed to update crontab");
    }
    Ok(())
}

fn strip_cron_block(content: &str, identifier: &str) -> String {
    let marker = format!("{AUTOSUBMIT_CRON_MARKER} {identifier}");
    let mut result = Vec::new();
    let mut lines = content.lines().peekable();
    while let Some(line) = lines.next() {
        if line.trim() == marker.trim() {
            match lines.peek() {
                Some(next_line) if is_autosubmit_cron_command_line(next_line) => {
                    lines.next();
                }
                _ => {}
            }
            continue;
        }
        result.push(line);
    }
    result.join("\n")
}

fn probe_cron_scheduler_content(content: &str, identifier: &str) -> SchedulerProbeResult {
    let marker = format!("{AUTOSUBMIT_CRON_MARKER} {identifier}");
    let mut lines = content.lines().peekable();
    while let Some(line) = lines.next() {
        if line.trim() != marker.trim() {
            continue;
        }

        if let Some(next_line) = lines.peek() {
            if is_autosubmit_cron_command_line(next_line) {
                return SchedulerProbeResult::Installed;
            }
        }
    }

    SchedulerProbeResult::Missing
}

fn is_autosubmit_cron_command_line(line: &str) -> bool {
    let trimmed = line.trim();
    trimmed.contains("tokscale")
        && trimmed.contains(" autosubmit run")
        && trimmed.contains(">>")
        && trimmed.contains("2>&1")
}

#[cfg(windows)]
fn install_windows_scheduler(
    config: &AutosubmitConfig,
    executable: &std::path::Path,
) -> Result<()> {
    let start = config.created_at.with_timezone(&chrono::Local);
    let start_time = start.format("%H:%M").to_string();
    let task_name = &config.scheduler.identifier;
    let task_run = render_windows_task_command(executable)?;
    let output = Command::new("schtasks")
        .args([
            "/Create",
            "/F",
            "/SC",
            "HOURLY",
            "/MO",
            "1",
            "/TN",
            task_name,
            "/TR",
            &task_run,
            "/ST",
            &start_time,
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .context("Failed to create Windows scheduled task")?;
    if !output.status.success() {
        bail!(
            "Failed to create Windows scheduled task: {}",
            command_output_summary(&output)
        );
    }
    Ok(())
}

#[cfg(not(windows))]
fn install_windows_scheduler(
    _config: &AutosubmitConfig,
    _executable: &std::path::Path,
) -> Result<()> {
    bail!("Windows scheduler is not available on this platform")
}

#[cfg(windows)]
fn uninstall_windows_scheduler(identifier: &str) -> Result<()> {
    let output = Command::new("schtasks")
        .args(["/Delete", "/F", "/TN", identifier])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .context("Failed to delete Windows scheduled task")?;
    if !output.status.success()
        && !output_mentions_any(
            &output,
            &[
                "cannot find the file specified",
                "cannot find the task",
                "system cannot find the file specified",
            ],
        )
    {
        bail!(
            "Failed to delete Windows scheduled task: {}",
            command_output_summary(&output)
        );
    }
    Ok(())
}

#[cfg(not(windows))]
fn uninstall_windows_scheduler(_identifier: &str) -> Result<()> {
    bail!("Windows scheduler is not available on this platform")
}

#[cfg(windows)]
fn probe_windows_scheduler(identifier: &str) -> SchedulerProbeResult {
    match Command::new("schtasks")
        .args(["/Query", "/TN", identifier])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
    {
        Ok(output) if output.status.success() => SchedulerProbeResult::Installed,
        Ok(output)
            if output_mentions_any(
                &output,
                &[
                    "cannot find the file specified",
                    "cannot find the task",
                    "system cannot find the file specified",
                ],
            ) =>
        {
            SchedulerProbeResult::Missing
        }
        Ok(output) => SchedulerProbeResult::Error(format!(
            "Failed to inspect Windows scheduled task '{}': {}",
            identifier,
            command_output_summary(&output)
        )),
        Err(err) => SchedulerProbeResult::Error(format!(
            "Failed to inspect Windows scheduled task '{}': {}",
            identifier, err
        )),
    }
}

#[cfg(not(windows))]
fn probe_windows_scheduler(_identifier: &str) -> SchedulerProbeResult {
    SchedulerProbeResult::Error("Windows scheduler is not available on this platform".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::anyhow;
    use serial_test::serial;
    use std::ffi::OsString;
    use std::process::Output;
    use std::sync::{Arc, Mutex};
    use tempfile::TempDir;

    #[cfg(unix)]
    use std::os::unix::process::ExitStatusExt;
    #[cfg(windows)]
    use std::os::windows::process::ExitStatusExt;

    struct TestEnvGuard {
        _temp: TempDir,
        previous_home: Option<OsString>,
        previous_xdg_config_home: Option<OsString>,
        previous_skip_scheduler: Option<OsString>,
        previous_path: Option<OsString>,
    }

    impl TestEnvGuard {
        fn force_scheduler_command_failure(&self) {
            std::env::remove_var("TOKSCALE_AUTOSUBMIT_SKIP_SCHEDULER");
            std::env::set_var("PATH", "/definitely-missing");
        }

        fn allow_scheduler_side_effects(&self) {
            std::env::remove_var("TOKSCALE_AUTOSUBMIT_SKIP_SCHEDULER");
        }
    }

    impl Drop for TestEnvGuard {
        fn drop(&mut self) {
            match &self.previous_home {
                Some(value) => std::env::set_var("HOME", value),
                None => std::env::remove_var("HOME"),
            }
            match &self.previous_xdg_config_home {
                Some(value) => std::env::set_var("XDG_CONFIG_HOME", value),
                None => std::env::remove_var("XDG_CONFIG_HOME"),
            }
            match &self.previous_skip_scheduler {
                Some(value) => std::env::set_var("TOKSCALE_AUTOSUBMIT_SKIP_SCHEDULER", value),
                None => std::env::remove_var("TOKSCALE_AUTOSUBMIT_SKIP_SCHEDULER"),
            }
            match &self.previous_path {
                Some(value) => std::env::set_var("PATH", value),
                None => std::env::remove_var("PATH"),
            }
        }
    }

    fn with_temp_config_dir() -> TestEnvGuard {
        let temp = TempDir::new().unwrap();
        let previous_home = std::env::var_os("HOME");
        let previous_xdg_config_home = std::env::var_os("XDG_CONFIG_HOME");
        let previous_skip_scheduler = std::env::var_os("TOKSCALE_AUTOSUBMIT_SKIP_SCHEDULER");
        let previous_path = std::env::var_os("PATH");
        let home = temp.path();
        let config_home = temp.path().join("xdg-config");
        std::fs::create_dir_all(&config_home).unwrap();
        std::fs::create_dir_all(home.join("Library/Application Support")).unwrap();
        std::env::set_var("HOME", home);
        std::env::set_var("XDG_CONFIG_HOME", &config_home);
        std::env::set_var("TOKSCALE_AUTOSUBMIT_SKIP_SCHEDULER", "1");
        TestEnvGuard {
            _temp: temp,
            previous_home,
            previous_xdg_config_home,
            previous_skip_scheduler,
            previous_path,
        }
    }

    fn sample_submit_args() -> SubmitCommandArgs {
        SubmitCommandArgs {
            filters: SubmitFilterArgs {
                codex: true,
                month: true,
                ..SubmitFilterArgs::default()
            },
            ..SubmitCommandArgs::default()
        }
    }

    fn unsupported_scheduler_kind_for_platform() -> SchedulerKind {
        if cfg!(target_os = "macos") {
            SchedulerKind::SystemdUser
        } else if cfg!(windows) {
            SchedulerKind::Launchd
        } else {
            SchedulerKind::Launchd
        }
    }

    fn failed_output(stdout: &str, stderr: &str) -> Output {
        #[cfg(unix)]
        let status = std::process::ExitStatus::from_raw(1 << 8);
        #[cfg(windows)]
        let status = std::process::ExitStatus::from_raw(1);

        Output {
            status,
            stdout: stdout.as_bytes().to_vec(),
            stderr: stderr.as_bytes().to_vec(),
        }
    }

    #[test]
    fn parse_interval_accepts_hour_and_day_suffixes() {
        let hours = parse_interval_spec("2h").unwrap();
        let days = parse_interval_spec("3d").unwrap();
        assert_eq!(hours.unit, IntervalUnit::Hours);
        assert_eq!(days.value, 3);
    }

    #[test]
    fn parse_interval_rejects_invalid_suffixes() {
        assert!(parse_interval_spec("15m").is_err());
        assert!(parse_interval_spec("0h").is_err());
    }

    #[test]
    fn select_scheduler_kind_matches_platform_policy() {
        assert_eq!(
            select_scheduler_kind_for_platform("macos", false),
            SchedulerKind::Launchd
        );
        assert_eq!(
            select_scheduler_kind_for_platform("windows", false),
            SchedulerKind::WindowsTask
        );
        assert_eq!(
            select_scheduler_kind_for_platform("linux", true),
            SchedulerKind::SystemdUser
        );
        assert_eq!(
            select_scheduler_kind_for_platform("linux", false),
            SchedulerKind::Cron
        );
    }

    #[test]
    #[serial]
    fn autosubmit_identifier_uses_platform_default_label() {
        let previous_identifier = std::env::var_os("TOKSCALE_AUTOSUBMIT_IDENTIFIER");
        std::env::remove_var("TOKSCALE_AUTOSUBMIT_IDENTIFIER");

        assert_eq!(
            autosubmit_identifier(SchedulerKind::Launchd),
            AUTOSUBMIT_LAUNCHD_LABEL
        );
        assert_eq!(
            autosubmit_identifier(SchedulerKind::Cron),
            AUTOSUBMIT_TASK_NAME
        );

        match previous_identifier {
            Some(value) => std::env::set_var("TOKSCALE_AUTOSUBMIT_IDENTIFIER", value),
            None => std::env::remove_var("TOKSCALE_AUTOSUBMIT_IDENTIFIER"),
        }
    }

    #[test]
    fn build_submit_template_keeps_dry_run_off() {
        let args = sample_submit_args();
        assert!(!args.dry_run);
    }

    #[test]
    fn extract_submit_failure_reason_prefers_machine_readable_prefix() {
        let output = failed_output(
            "",
            "__TOKSCALE_SUBMIT_ERROR__:machine-stable reason\nError: human-facing reason",
        );

        let reason = extract_submit_failure_reason(&output);

        assert_eq!(reason, "machine-stable reason");
    }

    #[test]
    #[serial]
    fn render_windows_task_command_redirects_to_autosubmit_log() {
        let _env = with_temp_config_dir();

        let command =
            render_windows_task_command(std::path::Path::new("C:/tokscale/tokscale.exe")).unwrap();

        assert!(command.contains("autosubmit run"));
        assert!(command.contains("autosubmit.log"));
        assert!(command.contains("2>&1"));
    }

    #[test]
    #[serial]
    fn render_launchd_plist_includes_run_at_load_and_interval() {
        let _env = with_temp_config_dir();
        let config = AutosubmitConfig {
            enabled: true,
            interval: parse_interval_spec("2h").unwrap(),
            submit_args: sample_submit_args(),
            scheduler: SchedulerMetadata {
                kind: SchedulerKind::Launchd,
                identifier: AUTOSUBMIT_LAUNCHD_LABEL.to_string(),
                heartbeat_minutes: DEFAULT_HEARTBEAT_MINUTES,
                command_preview: "tokscale autosubmit run".to_string(),
            },
            created_at: Utc::now(),
            last_run_at: None,
        };

        let plist = render_launchd_plist(&config, std::path::Path::new("/tmp/tokscale")).unwrap();

        assert!(plist.contains("<key>RunAtLoad</key>"));
        assert!(plist.contains("<key>StartInterval</key>"));
        assert!(plist.contains("<key>EnvironmentVariables</key>"));
        assert!(plist.contains("<key>HOME</key>"));
        assert!(plist.contains(AUTOSUBMIT_LAUNCHD_LABEL));
        assert!(plist.contains("<key>StandardOutPath</key>"));
        assert!(plist.contains("<key>StandardErrorPath</key>"));
        assert!(plist.contains("/tmp/tokscale"));
        assert!(!plist.contains("/bin/sh"));
        assert!(!plist.contains("<string>-lc</string>"));
        assert!(plist.contains("<string>autosubmit</string>"));
        assert!(plist.contains("<string>run</string>"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn render_systemd_timer_uses_hourly_heartbeat() {
        let config = AutosubmitConfig {
            enabled: true,
            interval: parse_interval_spec("2h").unwrap(),
            submit_args: sample_submit_args(),
            scheduler: SchedulerMetadata {
                kind: SchedulerKind::SystemdUser,
                identifier: AUTOSUBMIT_TASK_NAME.to_string(),
                heartbeat_minutes: DEFAULT_HEARTBEAT_MINUTES,
                command_preview: "tokscale autosubmit run".to_string(),
            },
            created_at: Utc::now(),
            last_run_at: None,
        };

        let timer = render_systemd_timer(&config);

        assert!(timer.contains("OnStartupSec=1m"));
        assert!(timer.contains("OnUnitActiveSec=60m"));
        assert!(timer.contains("WantedBy=timers.target"));
    }

    #[test]
    #[serial]
    fn settings_roundtrip_preserves_autosubmit_config() {
        let _env = with_temp_config_dir();
        let submit_args = sample_submit_args();
        let config = AutosubmitConfig {
            enabled: true,
            interval: parse_interval_spec("2h").unwrap(),
            submit_args,
            scheduler: SchedulerMetadata {
                kind: scheduler_kind(),
                identifier: "tokscale-autosubmit".to_string(),
                heartbeat_minutes: DEFAULT_HEARTBEAT_MINUTES,
                command_preview: "tokscale autosubmit run".to_string(),
            },
            created_at: Utc::now(),
            last_run_at: None,
        };
        let mut settings = Settings::default();
        settings.autosubmit = Some(config.clone());
        settings.save().unwrap();
        let loaded = Settings::load();
        assert_eq!(loaded.autosubmit.unwrap().interval.raw, config.interval.raw);
    }

    #[test]
    fn format_submit_args_shows_selected_flags() {
        let args = sample_submit_args();
        let text = format_submit_args(&args);
        assert!(text.contains("--codex"));
        assert!(text.contains("--month"));
    }

    #[test]
    fn due_logic_uses_last_run_or_creation_time() {
        let submit_args = sample_submit_args();
        let config = AutosubmitConfig {
            enabled: true,
            interval: parse_interval_spec("2h").unwrap(),
            submit_args,
            scheduler: SchedulerMetadata {
                kind: scheduler_kind(),
                identifier: "tokscale-autosubmit".to_string(),
                heartbeat_minutes: DEFAULT_HEARTBEAT_MINUTES,
                command_preview: "tokscale autosubmit run".to_string(),
            },
            created_at: Utc::now(),
            last_run_at: None,
        };
        assert!(!is_due(&config, config.created_at + Duration::minutes(119)));
        assert!(is_due(&config, config.created_at + Duration::minutes(120)));
    }

    #[test]
    fn strip_cron_block_only_removes_marked_block() {
        let content = [
            "MAILTO=user@example.com",
            "# tokscale-autosubmit tokscale-autosubmit",
            "17 * * * * '/usr/local/bin/tokscale' autosubmit run >> '/tmp/tokscale.log' 2>&1",
            "0 1 * * * /usr/bin/backup autosubmit run >> /tmp/backup.log 2>&1",
        ]
        .join("\n");

        let cleaned = strip_cron_block(&content, "tokscale-autosubmit");

        assert!(cleaned.contains("MAILTO=user@example.com"));
        assert!(cleaned.contains("/usr/bin/backup autosubmit run >> /tmp/backup.log 2>&1"));
        assert!(!cleaned.contains("# tokscale-autosubmit tokscale-autosubmit"));
        assert!(
            !cleaned
                .contains("'/usr/local/bin/tokscale' autosubmit run >> '/tmp/tokscale.log' 2>&1")
        );
    }

    #[test]
    fn strip_cron_block_removes_marker_without_touching_unrelated_next_line() {
        let content = [
            "MAILTO=user@example.com",
            "# tokscale-autosubmit tokscale-autosubmit",
            "0 1 * * * /usr/bin/backup >/tmp/backup.log 2>&1",
        ]
        .join("\n");

        let cleaned = strip_cron_block(&content, "tokscale-autosubmit");

        assert!(cleaned.contains("MAILTO=user@example.com"));
        assert!(cleaned.contains("0 1 * * * /usr/bin/backup >/tmp/backup.log 2>&1"));
        assert!(!cleaned.contains("# tokscale-autosubmit tokscale-autosubmit"));
    }

    #[test]
    fn cron_probe_requires_valid_command_after_marker() {
        let marker_only_dirty = [
            "# tokscale-autosubmit tokscale-autosubmit",
            "0 1 * * * /usr/bin/backup >/tmp/backup.log 2>&1",
        ]
        .join("\n");
        let valid = [
            "# tokscale-autosubmit tokscale-autosubmit",
            "17 * * * * '/usr/local/bin/tokscale' autosubmit run >> '/tmp/tokscale.log' 2>&1",
        ]
        .join("\n");

        assert_eq!(
            probe_cron_scheduler_content(&marker_only_dirty, "tokscale-autosubmit"),
            SchedulerProbeResult::Missing
        );
        assert_eq!(
            probe_cron_scheduler_content(&valid, "tokscale-autosubmit"),
            SchedulerProbeResult::Installed
        );
    }

    #[test]
    #[serial]
    fn acquire_run_lock_rejects_second_holder() {
        let _env = with_temp_config_dir();
        let _first = acquire_run_lock().unwrap();
        assert!(acquire_run_lock().is_err());
    }

    #[test]
    #[serial]
    fn stale_lock_file_is_recovered() {
        let _env = with_temp_config_dir();
        let lock_path = autosubmit_lock_path().unwrap();
        std::fs::write(
            &lock_path,
            format!(
                "{}",
                chrono::Utc::now().timestamp() - STALE_LOCK_MAX_AGE_SECS - 1
            ),
        )
        .unwrap();

        let _lock = acquire_run_lock().unwrap();
    }

    #[test]
    #[serial]
    fn fresh_empty_lock_file_is_not_treated_as_stale() {
        let _env = with_temp_config_dir();
        let lock_path = autosubmit_lock_path().unwrap();
        std::fs::write(&lock_path, "").unwrap();

        let err = acquire_run_lock().unwrap_err();

        assert!(err.to_string().contains("Autosubmit is already running"));
    }

    #[test]
    #[serial]
    fn stale_owner_drop_does_not_remove_reclaimed_lock() {
        let _env = with_temp_config_dir();
        let lock_path = autosubmit_lock_path().unwrap();
        std::fs::write(
            &lock_path,
            format!(
                "{}",
                chrono::Utc::now().timestamp() - STALE_LOCK_MAX_AGE_SECS - 1
            ),
        )
        .unwrap();

        let _current_lock = acquire_run_lock().unwrap();
        drop(AutosubmitRunLock {
            path: lock_path.clone(),
            token: "old-owner".to_string(),
        });

        assert!(lock_path.exists());
    }

    #[test]
    #[serial]
    fn enable_restores_previous_settings_when_scheduler_install_fails() {
        let env = with_temp_config_dir();
        env.force_scheduler_command_failure();

        let existing = AutosubmitConfig {
            enabled: true,
            interval: parse_interval_spec("1d").unwrap(),
            submit_args: sample_submit_args(),
            scheduler: SchedulerMetadata {
                kind: scheduler_kind(),
                identifier: "existing-autosubmit".to_string(),
                heartbeat_minutes: DEFAULT_HEARTBEAT_MINUTES,
                command_preview: "tokscale autosubmit run".to_string(),
            },
            created_at: Utc::now(),
            last_run_at: None,
        };
        let mut settings = Settings::default();
        settings.autosubmit = Some(existing.clone());
        settings.save().unwrap();

        let result = run_autosubmit_enable("2h", SubmitFilterArgs::default());

        assert!(result.is_err());
        let restored = Settings::load().autosubmit.unwrap();
        assert_eq!(restored.interval.raw, existing.interval.raw);
        assert_eq!(restored.scheduler.identifier, existing.scheduler.identifier);
    }

    #[test]
    #[serial]
    fn disable_restores_settings_when_scheduler_uninstall_fails() {
        let env = with_temp_config_dir();
        env.force_scheduler_command_failure();

        let config = AutosubmitConfig {
            enabled: true,
            interval: parse_interval_spec("2h").unwrap(),
            submit_args: sample_submit_args(),
            scheduler: SchedulerMetadata {
                kind: scheduler_kind(),
                identifier: "tokscale-autosubmit".to_string(),
                heartbeat_minutes: DEFAULT_HEARTBEAT_MINUTES,
                command_preview: "tokscale autosubmit run".to_string(),
            },
            created_at: Utc::now(),
            last_run_at: None,
        };
        let mut settings = Settings::default();
        settings.autosubmit = Some(config);
        settings.save().unwrap();

        let result = run_autosubmit_disable();

        assert!(result.is_err());
        assert!(Settings::load().autosubmit.is_some());
    }

    #[test]
    #[serial]
    fn disable_clears_saved_config_for_unsupported_scheduler_kind() {
        let env = with_temp_config_dir();
        env.allow_scheduler_side_effects();

        let config = AutosubmitConfig {
            enabled: true,
            interval: parse_interval_spec("2h").unwrap(),
            submit_args: sample_submit_args(),
            scheduler: SchedulerMetadata {
                kind: unsupported_scheduler_kind_for_platform(),
                identifier: "tokscale-autosubmit".to_string(),
                heartbeat_minutes: DEFAULT_HEARTBEAT_MINUTES,
                command_preview: "tokscale autosubmit run".to_string(),
            },
            created_at: Utc::now(),
            last_run_at: None,
        };
        let mut settings = Settings::default();
        settings.autosubmit = Some(config);
        settings.save().unwrap();

        let result = run_autosubmit_disable();

        assert!(result.is_ok());
        assert!(Settings::load_strict().unwrap().autosubmit.is_none());
    }

    #[test]
    #[serial]
    fn disable_rejects_while_autosubmit_run_is_in_progress() {
        let _env = with_temp_config_dir();
        let config = AutosubmitConfig {
            enabled: true,
            interval: parse_interval_spec("2h").unwrap(),
            submit_args: sample_submit_args(),
            scheduler: SchedulerMetadata {
                kind: scheduler_kind(),
                identifier: "tokscale-autosubmit".to_string(),
                heartbeat_minutes: DEFAULT_HEARTBEAT_MINUTES,
                command_preview: "tokscale autosubmit run".to_string(),
            },
            created_at: Utc::now() - Duration::hours(3),
            last_run_at: None,
        };
        let mut settings = Settings::default();
        settings.autosubmit = Some(config);
        settings.save().unwrap();

        let entered_submitter = Arc::new(std::sync::Barrier::new(2));
        let release_submitter = Arc::new(std::sync::Barrier::new(2));
        let entered_submitter_clone = Arc::clone(&entered_submitter);
        let release_submitter_clone = Arc::clone(&release_submitter);

        let run_handle = std::thread::spawn(move || {
            run_autosubmit_run_with_submitter_and_logger(
                |_| {
                    entered_submitter_clone.wait();
                    release_submitter_clone.wait();
                    Err(anyhow!("submit failed"))
                },
                |_| {},
            )
        });

        entered_submitter.wait();

        let disable_result = run_autosubmit_disable();
        assert!(disable_result.is_err());
        assert!(
            disable_result
                .unwrap_err()
                .to_string()
                .contains("Autosubmit is already running")
        );

        release_submitter.wait();
        assert!(run_handle.join().unwrap().is_err());
        assert!(Settings::load_strict().unwrap().autosubmit.is_some());
    }

    #[test]
    fn detect_orphan_schedulers_when_settings_are_missing() {
        let executable = std::path::Path::new("/tmp/tokscale");
        let settings = Settings::default();
        let expected_kind = orphan_scheduler_kinds_for_platform(std::env::consts::OS)
            .into_iter()
            .next()
            .unwrap();

        let orphaned = detect_orphan_schedulers_with_probe(executable, |candidate| {
            if candidate.scheduler.kind == expected_kind {
                SchedulerProbeResult::Installed
            } else {
                SchedulerProbeResult::Missing
            }
        });

        assert_eq!(settings.autosubmit, None);
        assert_eq!(orphaned.len(), 1);
        assert_eq!(orphaned[0].scheduler.kind, expected_kind);
    }

    #[test]
    #[serial]
    fn run_persists_lease_before_submit_and_rolls_back_on_failure() {
        let _env = with_temp_config_dir();
        let config = AutosubmitConfig {
            enabled: true,
            interval: parse_interval_spec("2h").unwrap(),
            submit_args: sample_submit_args(),
            scheduler: SchedulerMetadata {
                kind: scheduler_kind(),
                identifier: "tokscale-autosubmit".to_string(),
                heartbeat_minutes: DEFAULT_HEARTBEAT_MINUTES,
                command_preview: "tokscale autosubmit run".to_string(),
            },
            created_at: Utc::now() - Duration::hours(3),
            last_run_at: None,
        };
        let mut settings = Settings::default();
        settings.autosubmit = Some(config);
        settings.save().unwrap();

        let seen_last_run_at = Arc::new(Mutex::new(None));
        let seen_last_run_at_clone = Arc::clone(&seen_last_run_at);
        let result = run_autosubmit_run_with_submitter_and_logger(
            |_| {
                let loaded = Settings::load_strict().unwrap();
                let last_run_at = loaded.autosubmit.unwrap().last_run_at;
                *seen_last_run_at_clone.lock().unwrap() = last_run_at;
                Err(anyhow!("submit failed"))
            },
            |_| {},
        );

        assert!(result.is_err());
        assert!(seen_last_run_at.lock().unwrap().is_some());
        let loaded = Settings::load_strict().unwrap();
        assert!(loaded.autosubmit.unwrap().last_run_at.is_none());
    }

    #[test]
    #[serial]
    fn run_ignores_persisted_dry_run_flag() {
        let _env = with_temp_config_dir();
        let mut submit_args = sample_submit_args();
        submit_args.dry_run = true;

        let config = AutosubmitConfig {
            enabled: true,
            interval: parse_interval_spec("2h").unwrap(),
            submit_args,
            scheduler: SchedulerMetadata {
                kind: scheduler_kind(),
                identifier: "tokscale-autosubmit".to_string(),
                heartbeat_minutes: DEFAULT_HEARTBEAT_MINUTES,
                command_preview: "tokscale autosubmit run".to_string(),
            },
            created_at: Utc::now() - Duration::hours(3),
            last_run_at: None,
        };
        let mut settings = Settings::default();
        settings.autosubmit = Some(config);
        settings.save().unwrap();

        let seen_dry_run = Arc::new(Mutex::new(None));
        let seen_dry_run_clone = Arc::clone(&seen_dry_run);
        let result = run_autosubmit_run_with_submitter_and_logger(
            |args| {
                *seen_dry_run_clone.lock().unwrap() = Some(args.dry_run);
                Ok(())
            },
            |_| {},
        );

        assert!(result.is_ok());
        assert_eq!(*seen_dry_run.lock().unwrap(), Some(false));
        let loaded = Settings::load_strict().unwrap();
        assert!(!loaded.autosubmit.unwrap().submit_args.dry_run);
    }

    #[test]
    #[serial]
    fn run_logs_minimal_block_for_skipped_run() {
        let _env = with_temp_config_dir();
        let config = AutosubmitConfig {
            enabled: true,
            interval: parse_interval_spec("2h").unwrap(),
            submit_args: sample_submit_args(),
            scheduler: SchedulerMetadata {
                kind: scheduler_kind(),
                identifier: "tokscale-autosubmit".to_string(),
                heartbeat_minutes: DEFAULT_HEARTBEAT_MINUTES,
                command_preview: "tokscale autosubmit run".to_string(),
            },
            created_at: Utc::now(),
            last_run_at: None,
        };
        let mut settings = Settings::default();
        settings.autosubmit = Some(config);
        settings.save().unwrap();

        let mut logs = Vec::new();
        let result = run_autosubmit_run_with_submitter_and_logger(
            |_| panic!("submitter should not run when autosubmit is not due"),
            |line: &str| logs.push(line.to_string()),
        );

        assert!(result.is_ok());
        assert_eq!(logs.len(), 2);
        assert!(logs[0].starts_with("20"));
        assert!(logs[0].contains("[autosubmit] start"));
        assert!(logs[1].contains("status=skipped"));
        assert!(logs[1].contains("reason=\"interval 2h is not due yet\""));
    }

    #[test]
    #[serial]
    fn run_logs_minimal_block_for_successful_run() {
        let _env = with_temp_config_dir();
        let config = AutosubmitConfig {
            enabled: true,
            interval: parse_interval_spec("2h").unwrap(),
            submit_args: sample_submit_args(),
            scheduler: SchedulerMetadata {
                kind: scheduler_kind(),
                identifier: "tokscale-autosubmit".to_string(),
                heartbeat_minutes: DEFAULT_HEARTBEAT_MINUTES,
                command_preview: "tokscale autosubmit run".to_string(),
            },
            created_at: Utc::now() - Duration::hours(3),
            last_run_at: None,
        };
        let mut settings = Settings::default();
        settings.autosubmit = Some(config);
        settings.save().unwrap();

        let mut logs = Vec::new();
        let result = run_autosubmit_run_with_submitter_and_logger(
            |_| Ok(()),
            |line: &str| logs.push(line.to_string()),
        );

        assert!(result.is_ok());
        assert_eq!(logs.len(), 2);
        assert!(logs[0].starts_with("20"));
        assert!(logs[0].contains("[autosubmit] start"));
        assert!(logs[1].contains("status=success"));
        assert!(!logs[1].contains("reason="));
    }

    #[test]
    #[serial]
    fn run_logs_minimal_block_for_failed_run() {
        let _env = with_temp_config_dir();
        let config = AutosubmitConfig {
            enabled: true,
            interval: parse_interval_spec("2h").unwrap(),
            submit_args: sample_submit_args(),
            scheduler: SchedulerMetadata {
                kind: scheduler_kind(),
                identifier: "tokscale-autosubmit".to_string(),
                heartbeat_minutes: DEFAULT_HEARTBEAT_MINUTES,
                command_preview: "tokscale autosubmit run".to_string(),
            },
            created_at: Utc::now() - Duration::hours(3),
            last_run_at: None,
        };
        let mut settings = Settings::default();
        settings.autosubmit = Some(config);
        settings.save().unwrap();

        let mut logs = Vec::new();
        let result = run_autosubmit_run_with_submitter_and_logger(
            |_| Err(anyhow!("submit failed")),
            |line: &str| logs.push(line.to_string()),
        );

        assert!(result.is_err());
        assert_eq!(logs.len(), 2);
        assert!(logs[0].starts_with("20"));
        assert!(logs[0].contains("[autosubmit] start"));
        assert!(logs[1].contains("status=failed"));
        assert!(logs[1].contains("reason=\"submit failed\""));
    }
}
