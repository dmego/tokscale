use assert_cmd::Command;
use assert_cmd::cargo::cargo_bin_cmd;
use predicates::prelude::*;
use std::fs;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};
use tempfile::TempDir;

// ── Fixture helpers ────────────────────────────────────────────────────────

fn prime_pricing_cache(base: &Path) {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time before unix epoch")
        .as_secs();
    let payload = format!(r#"{{"timestamp":{},"data":{{}}}}"#, now);

    for dir in [
        base.join("Library/Caches/tokscale"),
        base.join(".cache/tokscale"),
    ] {
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("pricing-litellm.json"), &payload).unwrap();
        fs::write(dir.join("pricing-openrouter.json"), &payload).unwrap();
    }
}

/// Create a temporary directory with minimal OpenCode fixture data.
///
/// Layout:
///   <tmp>/.local/share/opencode/storage/message/session1/msg_a.json  (2024-06-15, claude-sonnet-4-20250514, anthropic)
///   <tmp>/.local/share/opencode/storage/message/session1/msg_b.json  (2024-06-15, claude-sonnet-4-20250514, anthropic)
///   <tmp>/.local/share/opencode/storage/message/session2/msg_c.json  (2025-01-10, gpt-4o, openai)
fn create_temp_fixture_dir() -> TempDir {
    let tmp = TempDir::new().expect("failed to create temp dir");
    let base = tmp.path();
    prime_pricing_cache(base);

    // Session 1: two messages on 2024-06-15 using claude-sonnet-4
    let session1 = base.join(".local/share/opencode/storage/message/session1");
    fs::create_dir_all(&session1).unwrap();

    // 2024-06-15 12:00:00 UTC = 1718452800000 ms
    let msg_a = r#"{
        "id": "msg_a",
        "sessionID": "session1",
        "role": "assistant",
        "modelID": "claude-sonnet-4-20250514",
        "providerID": "anthropic",
        "cost": 0.05,
        "tokens": {
            "input": 1000,
            "output": 500,
            "reasoning": 0,
            "cache": { "read": 200, "write": 50 }
        },
        "time": { "created": 1718452800000.0 }
    }"#;
    fs::write(session1.join("msg_a.json"), msg_a).unwrap();

    // Same session, a bit later on the same day
    let msg_b = r#"{
        "id": "msg_b",
        "sessionID": "session1",
        "role": "assistant",
        "modelID": "claude-sonnet-4-20250514",
        "providerID": "anthropic",
        "cost": 0.03,
        "tokens": {
            "input": 800,
            "output": 300,
            "reasoning": 0,
            "cache": { "read": 150, "write": 30 }
        },
        "time": { "created": 1718456400000.0 }
    }"#;
    fs::write(session1.join("msg_b.json"), msg_b).unwrap();

    // Session 2: one message on 2025-01-10 using gpt-4o
    let session2 = base.join(".local/share/opencode/storage/message/session2");
    fs::create_dir_all(&session2).unwrap();

    // 2025-01-10 12:00:00 UTC = 1736510400000 ms
    let msg_c = r#"{
        "id": "msg_c",
        "sessionID": "session2",
        "role": "assistant",
        "modelID": "gpt-4o",
        "providerID": "openai",
        "cost": 0.02,
        "tokens": {
            "input": 600,
            "output": 200,
            "reasoning": 0,
            "cache": { "read": 100, "write": 20 }
        },
        "time": { "created": 1736510400000.0 }
    }"#;
    fs::write(session2.join("msg_c.json"), msg_c).unwrap();

    tmp
}

/// Create an empty fixture dir with no session data.
fn create_empty_fixture_dir() -> TempDir {
    let tmp = TempDir::new().expect("failed to create temp dir");
    let base = tmp.path();
    prime_pricing_cache(base);
    let opencode_dir = base.join(".local/share/opencode/storage/message");
    fs::create_dir_all(opencode_dir).unwrap();
    tmp
}

fn create_timezone_boundary_fixture_dir() -> TempDir {
    let tmp = TempDir::new().expect("failed to create temp dir");
    let base = tmp.path();
    prime_pricing_cache(base);

    let session = base.join(".local/share/opencode/storage/message/session1");
    fs::create_dir_all(&session).unwrap();

    // 2026-03-02 18:00:00 UTC = 2026-03-02 10:00:00 in America/Los_Angeles
    let msg_a = r#"{
        "id": "msg_a",
        "sessionID": "session1",
        "role": "assistant",
        "modelID": "claude-sonnet-4-20250514",
        "providerID": "anthropic",
        "cost": 0.05,
        "tokens": {
            "input": 1000,
            "output": 500,
            "reasoning": 0,
            "cache": { "read": 200, "write": 50 }
        },
        "time": { "created": 1772474400000.0 }
    }"#;
    fs::write(session.join("msg_a.json"), msg_a).unwrap();

    // 2026-03-03 04:30:00 UTC = 2026-03-02 20:30:00 in America/Los_Angeles
    let msg_b = r#"{
        "id": "msg_b",
        "sessionID": "session1",
        "role": "assistant",
        "modelID": "claude-sonnet-4-20250514",
        "providerID": "anthropic",
        "cost": 0.03,
        "tokens": {
            "input": 800,
            "output": 300,
            "reasoning": 0,
            "cache": { "read": 150, "write": 30 }
        },
        "time": { "created": 1772512200000.0 }
    }"#;
    fs::write(session.join("msg_b.json"), msg_b).unwrap();

    tmp
}

/// Build a Command pointing HOME at the given temp dir, with --no-spinner and --opencode flags.
fn cmd_with_home(tmp: &Path) -> Command {
    let mut cmd = cargo_bin_cmd!("tokscale");
    cmd.env("HOME", tmp)
        .env("XDG_DATA_HOME", tmp.join(".local/share"))
        .env("XDG_CACHE_HOME", tmp.join(".cache"));
    cmd
}

// ── Existing tests ─────────────────────────────────────────────────────────

#[test]
fn test_help_command() {
    let mut cmd = cargo_bin_cmd!("tokscale");
    cmd.arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("AI token usage analytics"));
}

fn write_tokscale_settings(base: &Path, content: &str) {
    for dir in [
        base.join(".config/tokscale"),
        base.join("xdg-config/tokscale"),
        base.join("Library/Application Support/tokscale"),
    ] {
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("settings.json"), content).unwrap();
    }
}

fn autosubmit_scheduler_kind_fixture() -> &'static str {
    if cfg!(target_os = "macos") {
        "launchd"
    } else if cfg!(windows) {
        "windowsTask"
    } else {
        "systemdUser"
    }
}

fn autosubmit_scheduler_label() -> &'static str {
    if cfg!(target_os = "macos") {
        "launchd"
    } else if cfg!(windows) {
        "windows-task"
    } else {
        "systemd-user"
    }
}

fn unsupported_autosubmit_scheduler_kind_fixture() -> &'static str {
    if cfg!(target_os = "macos") {
        "systemdUser"
    } else {
        "launchd"
    }
}

fn unsupported_autosubmit_scheduler_label() -> &'static str {
    if cfg!(target_os = "macos") {
        "systemd-user"
    } else {
        "launchd"
    }
}

fn unsupported_autosubmit_scheduler_reason() -> &'static str {
    if cfg!(target_os = "macos") {
        "systemd --user scheduler is not available on this platform"
    } else {
        "launchd scheduler is not available on this platform"
    }
}

#[test]
fn test_submit_help_shows_dry_run() {
    let mut cmd = cargo_bin_cmd!("tokscale");
    cmd.args(["submit", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--dry-run"));
}

#[test]
fn test_autosubmit_help_command() {
    let mut cmd = cargo_bin_cmd!("tokscale");
    cmd.args(["autosubmit", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("scheduled automatic submit"))
        .stdout(predicate::str::contains("enable"))
        .stdout(predicate::str::contains("status"))
        .stdout(predicate::str::contains(
            "Run autosubmit if the saved interval is due",
        ));
}

#[test]
fn test_autosubmit_enable_help_hides_dry_run() {
    let mut cmd = cargo_bin_cmd!("tokscale");
    cmd.args(["autosubmit", "enable", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--interval"))
        .stdout(predicate::str::contains("--month"))
        .stdout(predicate::str::contains("--dry-run").not());
}

#[test]
fn test_autosubmit_enable_rejects_dry_run() {
    let temp = TempDir::new().unwrap();
    let mut cmd = cargo_bin_cmd!("tokscale");
    cmd.env("HOME", temp.path())
        .env("XDG_CONFIG_HOME", temp.path())
        .env("TOKSCALE_AUTOSUBMIT_SKIP_SCHEDULER", "1")
        .args(["autosubmit", "enable", "--interval", "2h", "--dry-run"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "unexpected argument '--dry-run' found",
        ));
}

#[test]
fn test_autosubmit_status_rejects_subcommand_no_spinner() {
    let mut cmd = cargo_bin_cmd!("tokscale");
    cmd.args(["autosubmit", "status", "--no-spinner"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "unexpected argument '--no-spinner' found",
        ));
}

#[test]
fn test_autosubmit_status_reports_disabled_by_default() {
    let temp = TempDir::new().unwrap();
    let mut cmd = cargo_bin_cmd!("tokscale");
    cmd.env("HOME", temp.path())
        .env("XDG_CONFIG_HOME", temp.path())
        .env("TOKSCALE_AUTOSUBMIT_SKIP_SCHEDULER", "1")
        .args(["autosubmit", "status"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Autosubmit status: disabled"));
}

#[test]
fn test_autosubmit_status_reports_platform_scheduler_and_run_command() {
    let temp = TempDir::new().unwrap();
    let settings = format!(
        r#"{{
  "colorPalette": "blue",
  "autoRefreshEnabled": false,
  "autoRefreshMs": 60000,
  "includeUnusedModels": false,
  "nativeTimeoutMs": 300000,
  "autosubmit": {{
    "enabled": true,
    "interval": {{
      "raw": "2h",
      "value": 2,
      "unit": "hours"
    }},
    "submitArgs": {{
      "opencode": true,
      "claude": true,
      "codex": false,
      "gemini": false,
      "cursor": false,
      "amp": false,
      "droid": false,
      "openclaw": false,
      "pi": false,
      "kimi": false,
      "qwen": false,
      "roocode": false,
      "kilocode": false,
      "mux": false,
      "synthetic": false,
      "today": false,
      "week": false,
      "month": false,
      "since": "2024-01-01",
      "until": null,
      "year": null,
      "dryRun": false
    }},
    "scheduler": {{
      "kind": "{}",
      "identifier": "tokscale-autosubmit",
      "heartbeatMinutes": 60,
      "commandPreview": "'/tmp/tokscale' autosubmit run"
    }},
    "createdAt": "2026-03-21T00:00:00Z",
    "lastRunAt": null
  }}
}}"#,
        autosubmit_scheduler_kind_fixture()
    );
    write_tokscale_settings(temp.path(), &settings);

    let mut cmd = cargo_bin_cmd!("tokscale");
    cmd.env("HOME", temp.path())
        .env("XDG_CONFIG_HOME", temp.path().join("xdg-config"))
        .env("TOKSCALE_AUTOSUBMIT_SKIP_SCHEDULER", "1")
        .args(["autosubmit", "status"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Autosubmit status: enabled"))
        .stdout(predicate::str::contains("Interval: 2h"))
        .stdout(predicate::str::contains(format!(
            "Scheduler: {}",
            autosubmit_scheduler_label()
        )))
        .stdout(predicate::str::contains("Scheduler status: installed"))
        .stdout(predicate::str::contains(
            "Scheduler ID: tokscale-autosubmit",
        ))
        .stdout(predicate::str::contains(
            "Command: '/tmp/tokscale' autosubmit run",
        ))
        .stdout(predicate::str::contains(
            "Submit args: --opencode --claude --since 2024-01-01",
        ));
}

#[test]
fn test_autosubmit_status_reports_degraded_when_scheduler_probe_fails() {
    let temp = TempDir::new().unwrap();
    let settings = format!(
        r#"{{
  "colorPalette": "blue",
  "autoRefreshEnabled": false,
  "autoRefreshMs": 60000,
  "includeUnusedModels": false,
  "nativeTimeoutMs": 300000,
  "autosubmit": {{
    "enabled": true,
    "interval": {{
      "raw": "2h",
      "value": 2,
      "unit": "hours"
    }},
    "submitArgs": {{
      "opencode": false,
      "claude": false,
      "codex": false,
      "gemini": false,
      "cursor": false,
      "amp": false,
      "droid": false,
      "openclaw": false,
      "pi": false,
      "kimi": false,
      "qwen": false,
      "roocode": false,
      "kilocode": false,
      "mux": false,
      "synthetic": false,
      "today": false,
      "week": false,
      "month": false,
      "since": null,
      "until": null,
      "year": null,
      "dryRun": false
    }},
    "scheduler": {{
      "kind": "{}",
      "identifier": "tokscale-autosubmit",
      "heartbeatMinutes": 60,
      "commandPreview": "'/tmp/tokscale' autosubmit run"
    }},
    "createdAt": "2026-03-21T00:00:00Z",
    "lastRunAt": null
  }}
}}"#,
        unsupported_autosubmit_scheduler_kind_fixture()
    );
    write_tokscale_settings(temp.path(), &settings);

    let mut cmd = cargo_bin_cmd!("tokscale");
    cmd.env("HOME", temp.path())
        .env("XDG_CONFIG_HOME", temp.path().join("xdg-config"))
        .args(["autosubmit", "status"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Autosubmit status: degraded"))
        .stdout(predicate::str::contains(format!(
            "Scheduler: {}",
            unsupported_autosubmit_scheduler_label()
        )))
        .stdout(predicate::str::contains("Scheduler status: error"))
        .stdout(predicate::str::contains(format!(
            "Reason: {}",
            unsupported_autosubmit_scheduler_reason()
        )));
}

#[test]
fn test_autosubmit_disable_clears_unsupported_scheduler_config() {
    let temp = TempDir::new().unwrap();
    let settings = format!(
        r#"{{
  "colorPalette": "blue",
  "autoRefreshEnabled": false,
  "autoRefreshMs": 60000,
  "includeUnusedModels": false,
  "nativeTimeoutMs": 300000,
  "autosubmit": {{
    "enabled": true,
    "interval": {{
      "raw": "2h",
      "value": 2,
      "unit": "hours"
    }},
    "submitArgs": {{
      "opencode": false,
      "claude": false,
      "codex": false,
      "gemini": false,
      "cursor": false,
      "amp": false,
      "droid": false,
      "openclaw": false,
      "pi": false,
      "kimi": false,
      "qwen": false,
      "roocode": false,
      "kilocode": false,
      "mux": false,
      "synthetic": false,
      "today": false,
      "week": false,
      "month": false,
      "since": null,
      "until": null,
      "year": null,
      "dryRun": false
    }},
    "scheduler": {{
      "kind": "{}",
      "identifier": "tokscale-autosubmit",
      "heartbeatMinutes": 60,
      "commandPreview": "'/tmp/tokscale' autosubmit run"
    }},
    "createdAt": "2026-03-21T00:00:00Z",
    "lastRunAt": null
  }}
}}"#,
        unsupported_autosubmit_scheduler_kind_fixture()
    );
    write_tokscale_settings(temp.path(), &settings);

    let mut disable = cargo_bin_cmd!("tokscale");
    disable
        .env("HOME", temp.path())
        .env("XDG_CONFIG_HOME", temp.path().join("xdg-config"))
        .args(["autosubmit", "disable"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Autosubmit disabled"));

    let mut status = cargo_bin_cmd!("tokscale");
    status
        .env("HOME", temp.path())
        .env("XDG_CONFIG_HOME", temp.path().join("xdg-config"))
        .args(["autosubmit", "status"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Autosubmit status: disabled"));
}

#[test]
fn test_autosubmit_status_fails_on_invalid_settings_json() {
    let temp = TempDir::new().unwrap();
    write_tokscale_settings(temp.path(), "{invalid-json");

    let mut cmd = cargo_bin_cmd!("tokscale");
    cmd.env("HOME", temp.path())
        .env("XDG_CONFIG_HOME", temp.path().join("xdg-config"))
        .env("TOKSCALE_AUTOSUBMIT_SKIP_SCHEDULER", "1")
        .args(["autosubmit", "status"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "Failed to load settings for autosubmit status",
        ));
}

#[test]
fn test_autosubmit_run_logs_minimal_block_without_submit_details() {
    let temp = TempDir::new().unwrap();
    let settings = format!(
        r#"{{
  "colorPalette": "blue",
  "autoRefreshEnabled": false,
  "autoRefreshMs": 60000,
  "includeUnusedModels": false,
  "nativeTimeoutMs": 300000,
  "autosubmit": {{
    "enabled": true,
    "interval": {{
      "raw": "1h",
      "value": 1,
      "unit": "hours"
    }},
    "submitArgs": {{
      "opencode": false,
      "claude": false,
      "codex": false,
      "gemini": false,
      "cursor": false,
      "amp": false,
      "droid": false,
      "openclaw": false,
      "pi": false,
      "kimi": false,
      "qwen": false,
      "roocode": false,
      "kilocode": false,
      "mux": false,
      "synthetic": false,
      "today": false,
      "week": false,
      "month": false,
      "since": null,
      "until": null,
      "year": null,
      "dryRun": false
    }},
    "scheduler": {{
      "kind": "{}",
      "identifier": "tokscale-autosubmit",
      "heartbeatMinutes": 60,
      "commandPreview": "'/tmp/tokscale' autosubmit run"
    }},
    "createdAt": "2026-03-21T00:00:00Z",
    "lastRunAt": null
  }}
}}"#,
        autosubmit_scheduler_kind_fixture()
    );
    write_tokscale_settings(temp.path(), &settings);

    let mut cmd = cargo_bin_cmd!("tokscale");
    cmd.env("HOME", temp.path())
        .env("XDG_CONFIG_HOME", temp.path().join("xdg-config"))
        .args(["autosubmit", "run"])
        .assert()
        .failure()
        .stdout(predicate::str::is_match(r"20\d\d-\d\d-\d\dT").unwrap())
        .stdout(predicate::str::contains("[autosubmit] start"))
        .stdout(predicate::str::contains("status=failed"))
        .stdout(predicate::str::contains("reason=\"Not logged in.\""))
        .stdout(predicate::str::contains("Tokscale - Submit Usage Data").not())
        .stdout(predicate::str::contains("Data to submit:").not())
        .stdout(predicate::str::contains("Successfully submitted!").not())
        .stderr(predicate::str::is_empty());
}

#[test]
fn test_help_short_flag() {
    let mut cmd = cargo_bin_cmd!("tokscale");
    cmd.arg("-h")
        .assert()
        .success()
        .stdout(predicate::str::contains("AI token usage analytics"));
}

#[test]
fn test_version_flag() {
    let mut cmd = cargo_bin_cmd!("tokscale");
    cmd.arg("--version")
        .assert()
        .success()
        .stdout(predicate::str::contains("tokscale"));
}

#[test]
fn test_models_command_help() {
    let mut cmd = cargo_bin_cmd!("tokscale");
    cmd.arg("models")
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("Show model usage report"));
}

#[test]
fn test_monthly_command_help() {
    let mut cmd = cargo_bin_cmd!("tokscale");
    cmd.arg("monthly")
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("Show monthly usage report"));
}

#[test]
fn test_pricing_command_help() {
    let mut cmd = cargo_bin_cmd!("tokscale");
    cmd.arg("pricing")
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("Show pricing for a model"));
}

#[test]
fn test_clients_command_help() {
    let mut cmd = cargo_bin_cmd!("tokscale");
    cmd.arg("clients")
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("Show local scan locations"));
}

#[test]
fn test_graph_command_help() {
    let mut cmd = cargo_bin_cmd!("tokscale");
    cmd.arg("graph")
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("Export contribution graph data"));
}

#[test]
fn test_tui_command_help() {
    let mut cmd = cargo_bin_cmd!("tokscale");
    cmd.arg("tui")
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("Launch interactive TUI"));
}

#[test]
fn test_headless_command_help() {
    let mut cmd = cargo_bin_cmd!("tokscale");
    cmd.arg("headless")
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("Capture subprocess output"));
}

#[test]
fn test_login_command_help() {
    let mut cmd = cargo_bin_cmd!("tokscale");
    cmd.arg("login")
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("Login to Tokscale"));
}

#[test]
fn test_logout_command_help() {
    let mut cmd = cargo_bin_cmd!("tokscale");
    cmd.arg("logout")
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("Logout from Tokscale"));
}

#[test]
fn test_whoami_command_help() {
    let mut cmd = cargo_bin_cmd!("tokscale");
    cmd.arg("whoami")
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("Show current logged in user"));
}

#[test]
fn test_invalid_command() {
    let mut cmd = cargo_bin_cmd!("tokscale");
    cmd.arg("invalid-command").assert().failure();
}

#[test]
fn test_invalid_subcommand() {
    let mut cmd = cargo_bin_cmd!("tokscale");
    cmd.arg("models").arg("invalid-flag").assert().failure();
}

#[test]
fn test_pricing_command_missing_model() {
    let mut cmd = cargo_bin_cmd!("tokscale");
    cmd.arg("pricing").assert().failure();
}

#[test]
fn test_headless_command_missing_client() {
    let mut cmd = cargo_bin_cmd!("tokscale");
    cmd.arg("headless").assert().failure();
}

#[test]
fn test_headless_command_invalid_client() {
    let mut cmd = cargo_bin_cmd!("tokscale");
    cmd.arg("headless")
        .arg("invalid-client")
        .arg("test")
        .assert()
        .failure();
}

#[test]
fn test_models_with_invalid_date_format() {
    let mut cmd = cargo_bin_cmd!("tokscale");
    cmd.arg("models")
        .arg("--light")
        .arg("--since")
        .arg("invalid-date")
        .assert()
        .success();
}

#[test]
fn test_models_with_invalid_year() {
    let mut cmd = cargo_bin_cmd!("tokscale");
    cmd.arg("models")
        .arg("--light")
        .arg("--year")
        .arg("not-a-year")
        .assert()
        .success();
}

#[test]
fn test_global_theme_flag() {
    let mut cmd = cargo_bin_cmd!("tokscale");
    cmd.arg("--theme")
        .arg("blue")
        .arg("--help")
        .assert()
        .success();
}

#[test]
fn test_global_debug_flag() {
    let mut cmd = cargo_bin_cmd!("tokscale");
    cmd.arg("--debug").arg("--help").assert().success();
}

// ── Date filtering tests ───────────────────────────────────────────────────

#[test]
fn test_models_with_since_until_filter() {
    let tmp = create_temp_fixture_dir();
    cmd_with_home(tmp.path())
        .args(["models", "--json", "--opencode", "--no-spinner"])
        .args(["--since", "2024-06-01", "--until", "2024-06-30"])
        .assert()
        .success()
        .stdout(predicate::str::contains("claude-sonnet-4"))
        .stdout(predicate::str::contains("gpt-4o").not());
}

#[test]
fn test_models_with_year_filter() {
    let tmp = create_temp_fixture_dir();
    cmd_with_home(tmp.path())
        .args(["models", "--json", "--opencode", "--no-spinner"])
        .args(["--year", "2024"])
        .assert()
        .success()
        .stdout(predicate::str::contains("claude-sonnet-4"))
        .stdout(predicate::str::contains("gpt-4o").not());
}

#[test]
fn test_monthly_with_date_filters() {
    let tmp = create_temp_fixture_dir();
    cmd_with_home(tmp.path())
        .args(["monthly", "--json", "--opencode", "--no-spinner"])
        .args(["--since", "2025-01-01", "--until", "2025-12-31"])
        .assert()
        .success()
        .stdout(predicate::str::contains("2025-01"));
}

#[test]
fn test_models_with_since_only() {
    let tmp = create_temp_fixture_dir();
    cmd_with_home(tmp.path())
        .args(["models", "--json", "--opencode", "--no-spinner"])
        .args(["--since", "2025-01-01"])
        .assert()
        .success()
        .stdout(predicate::str::contains("gpt-4o"))
        .stdout(predicate::str::contains("anthropic").not());
}

#[test]
fn test_models_with_until_only() {
    let tmp = create_temp_fixture_dir();
    cmd_with_home(tmp.path())
        .args(["models", "--json", "--opencode", "--no-spinner"])
        .args(["--until", "2024-12-31"])
        .assert()
        .success()
        .stdout(predicate::str::contains("claude-sonnet-4"))
        .stdout(predicate::str::contains("gpt-4o").not());
}

#[test]
fn test_models_with_no_matching_date() {
    let tmp = create_temp_fixture_dir();
    let output = cmd_with_home(tmp.path())
        .args(["models", "--json", "--opencode", "--no-spinner"])
        .args(["--since", "2099-01-01", "--until", "2099-12-31"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let entries = json["entries"].as_array().unwrap();
    assert!(
        entries.is_empty(),
        "No entries expected for future date range"
    );
}

#[test]
fn test_graph_single_day_filter_uses_local_timezone_boundaries() {
    let tmp = create_timezone_boundary_fixture_dir();
    let output = cmd_with_home(tmp.path())
        .env("TZ", "America/Los_Angeles")
        .args(["graph", "--opencode", "--no-spinner"])
        .args(["--since", "2026-03-02", "--until", "2026-03-02"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let contributions = json["contributions"].as_array().unwrap();
    assert_eq!(
        contributions.len(),
        1,
        "expected a single local-day bucket, got {:?}",
        contributions
    );
    assert_eq!(contributions[0]["date"].as_str().unwrap(), "2026-03-02");
    assert_eq!(contributions[0]["totals"]["messages"].as_i64().unwrap(), 2);
}

#[test]
fn test_graph_with_year_filter() {
    let tmp = create_temp_fixture_dir();
    let output = cmd_with_home(tmp.path())
        .args(["graph", "--opencode", "--no-spinner"])
        .args(["--year", "2024"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let contributions = json["contributions"].as_array().unwrap();
    for c in contributions {
        let date = c["date"].as_str().unwrap();
        assert!(
            date.starts_with("2024-"),
            "Expected 2024 dates, got {}",
            date
        );
    }
}

// ── Client filtering tests ─────────────────────────────────────────────────

#[test]
fn test_models_with_client_filter_opencode() {
    let tmp = create_temp_fixture_dir();
    let output = cmd_with_home(tmp.path())
        .args(["models", "--json", "--opencode", "--no-spinner"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let entries = json["entries"].as_array().unwrap();
    for entry in entries {
        assert_eq!(entry["client"].as_str().unwrap(), "opencode");
    }
}

#[test]
fn test_models_with_client_filter_multiple() {
    let tmp = create_temp_fixture_dir();
    cmd_with_home(tmp.path())
        .args(["models", "--json", "--opencode", "--claude", "--no-spinner"])
        .assert()
        .success();
}

#[test]
fn test_models_with_all_client_flags() {
    let tmp = create_temp_fixture_dir();
    cmd_with_home(tmp.path())
        .args([
            "models",
            "--json",
            "--no-spinner",
            "--opencode",
            "--claude",
            "--codex",
            "--gemini",
            "--cursor",
            "--amp",
            "--droid",
            "--openclaw",
            "--pi",
        ])
        .assert()
        .success();
}

#[test]
fn test_models_client_and_date_combined() {
    let tmp = create_temp_fixture_dir();
    cmd_with_home(tmp.path())
        .args(["models", "--json", "--opencode", "--no-spinner"])
        .args(["--year", "2025"])
        .assert()
        .success()
        .stdout(predicate::str::contains("gpt-4o"))
        .stdout(predicate::str::contains("anthropic").not());
}

// ── JSON output validation tests ───────────────────────────────────────────

#[test]
fn test_models_json_output() {
    let tmp = create_temp_fixture_dir();
    let output = cmd_with_home(tmp.path())
        .args(["models", "--json", "--opencode", "--no-spinner"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();

    assert!(json.get("groupBy").is_some(), "Missing groupBy field");
    assert!(json.get("entries").is_some(), "Missing entries field");
    assert!(json.get("totalInput").is_some(), "Missing totalInput");
    assert!(json.get("totalOutput").is_some(), "Missing totalOutput");
    assert!(
        json.get("totalCacheRead").is_some(),
        "Missing totalCacheRead"
    );
    assert!(
        json.get("totalCacheWrite").is_some(),
        "Missing totalCacheWrite"
    );
    assert!(json.get("totalMessages").is_some(), "Missing totalMessages");
    assert!(json.get("totalCost").is_some(), "Missing totalCost");
    assert!(
        json.get("processingTimeMs").is_some(),
        "Missing processingTimeMs"
    );

    let entries = json["entries"].as_array().unwrap();
    assert!(!entries.is_empty(), "Should have entries from fixture data");
    let first = &entries[0];
    assert!(first.get("client").is_some());
    assert!(first.get("model").is_some());
    assert!(first.get("provider").is_some());
    assert!(first.get("input").is_some());
    assert!(first.get("output").is_some());
    assert!(first.get("cacheRead").is_some());
    assert!(first.get("cacheWrite").is_some());
    assert!(first.get("cost").is_some());
}

#[test]
fn test_models_json_total_consistency() {
    let tmp = create_temp_fixture_dir();
    let output = cmd_with_home(tmp.path())
        .args(["models", "--json", "--opencode", "--no-spinner"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();

    let entries = json["entries"].as_array().unwrap();
    let sum_input: i64 = entries.iter().map(|e| e["input"].as_i64().unwrap()).sum();
    let sum_output: i64 = entries.iter().map(|e| e["output"].as_i64().unwrap()).sum();
    let total_input = json["totalInput"].as_i64().unwrap();
    let total_output = json["totalOutput"].as_i64().unwrap();

    assert_eq!(
        sum_input, total_input,
        "Sum of entry inputs must match totalInput"
    );
    assert_eq!(
        sum_output, total_output,
        "Sum of entry outputs must match totalOutput"
    );
}

#[test]
fn test_monthly_json_output() {
    let tmp = create_temp_fixture_dir();
    let output = cmd_with_home(tmp.path())
        .args(["monthly", "--json", "--opencode", "--no-spinner"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();

    assert!(json.get("entries").is_some(), "Missing entries field");
    assert!(json.get("totalCost").is_some(), "Missing totalCost field");
    assert!(
        json.get("processingTimeMs").is_some(),
        "Missing processingTimeMs"
    );

    let entries = json["entries"].as_array().unwrap();
    assert!(
        !entries.is_empty(),
        "Should have monthly entries from fixture data"
    );
    let first = &entries[0];
    assert!(first.get("month").is_some());
    assert!(first.get("models").is_some());
    assert!(first.get("input").is_some());
    assert!(first.get("output").is_some());
    assert!(first.get("cacheRead").is_some());
    assert!(first.get("cacheWrite").is_some());
    assert!(first.get("messageCount").is_some());
    assert!(first.get("cost").is_some());
}

#[test]
fn test_monthly_json_with_client_filter() {
    let tmp = create_temp_fixture_dir();
    let output = cmd_with_home(tmp.path())
        .args(["monthly", "--json", "--opencode", "--no-spinner"])
        .args(["--year", "2024"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let entries = json["entries"].as_array().unwrap();
    for entry in entries {
        let month = entry["month"].as_str().unwrap();
        assert!(
            month.starts_with("2024-"),
            "Expected 2024 months only, got {}",
            month
        );
    }
}

#[test]
fn test_graph_json_output() {
    let tmp = create_temp_fixture_dir();
    let output = cmd_with_home(tmp.path())
        .args(["graph", "--opencode", "--no-spinner"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();

    assert!(json.get("meta").is_some(), "Missing meta field");
    assert!(json.get("summary").is_some(), "Missing summary field");
    assert!(json.get("years").is_some(), "Missing years field");
    assert!(
        json.get("contributions").is_some(),
        "Missing contributions field"
    );
}

#[test]
fn test_graph_json_has_meta() {
    let tmp = create_temp_fixture_dir();
    let output = cmd_with_home(tmp.path())
        .args(["graph", "--opencode", "--no-spinner"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let meta = &json["meta"];
    assert!(
        meta.get("generatedAt").is_some(),
        "Missing meta.generatedAt"
    );
    assert!(meta.get("version").is_some(), "Missing meta.version");
    assert!(meta.get("dateRange").is_some(), "Missing meta.dateRange");
}

#[test]
fn test_graph_json_has_summary() {
    let tmp = create_temp_fixture_dir();
    let output = cmd_with_home(tmp.path())
        .args(["graph", "--opencode", "--no-spinner"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let summary = &json["summary"];
    assert!(
        summary.get("totalTokens").is_some(),
        "Missing summary.totalTokens"
    );
    assert!(
        summary.get("totalCost").is_some(),
        "Missing summary.totalCost"
    );
    assert!(
        summary.get("totalDays").is_some(),
        "Missing summary.totalDays"
    );
    assert!(
        summary.get("activeDays").is_some(),
        "Missing summary.activeDays"
    );
    assert!(summary.get("clients").is_some(), "Missing summary.clients");
    assert!(summary.get("models").is_some(), "Missing summary.models");
}

// ── Group-by strategy tests ────────────────────────────────────────────────

#[test]
fn test_models_group_by_default() {
    let tmp = create_temp_fixture_dir();
    let output = cmd_with_home(tmp.path())
        .args(["models", "--json", "--opencode", "--no-spinner"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["groupBy"].as_str().unwrap(), "client,model");
}

#[test]
fn test_models_group_by_model() {
    let tmp = create_temp_fixture_dir();
    let output = cmd_with_home(tmp.path())
        .args(["models", "--json", "--opencode", "--no-spinner"])
        .args(["--group-by", "model"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["groupBy"].as_str().unwrap(), "model");

    let entries = json["entries"].as_array().unwrap();
    let models: Vec<&str> = entries
        .iter()
        .map(|e| e["model"].as_str().unwrap())
        .collect();
    let unique_models: std::collections::HashSet<&&str> = models.iter().collect();
    assert_eq!(
        models.len(),
        unique_models.len(),
        "group-by model should produce unique model entries"
    );
}

#[test]
fn test_models_group_by_client_provider_model() {
    let tmp = create_temp_fixture_dir();
    let output = cmd_with_home(tmp.path())
        .args(["models", "--json", "--opencode", "--no-spinner"])
        .args(["--group-by", "client,provider,model"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["groupBy"].as_str().unwrap(), "client,provider,model");

    let entries = json["entries"].as_array().unwrap();
    for entry in entries {
        assert!(entry.get("client").is_some(), "Entry must have client");
        assert!(entry.get("provider").is_some(), "Entry must have provider");
        assert!(entry.get("model").is_some(), "Entry must have model");
    }
}

#[test]
fn test_models_json_with_group_by_model() {
    let tmp = create_temp_fixture_dir();
    let output = cmd_with_home(tmp.path())
        .args(["models", "--json", "--opencode", "--no-spinner"])
        .args(["--group-by", "model"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let entries = json["entries"].as_array().unwrap();
    for entry in entries {
        assert!(
            entry.get("mergedClients").is_some(),
            "group-by model entries should have mergedClients field"
        );
    }
}

// ── Pricing command tests ──────────────────────────────────────────────────

#[test]
fn test_pricing_command_success() {
    let mut cmd = cargo_bin_cmd!("tokscale");
    cmd.args(["pricing", "claude-sonnet-4-20250514", "--no-spinner"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Pricing for"))
        .stdout(predicate::str::contains("Input"))
        .stdout(predicate::str::contains("Output"));
}

#[test]
fn test_pricing_command_json() {
    let output = cargo_bin_cmd!("tokscale")
        .args([
            "pricing",
            "claude-sonnet-4-20250514",
            "--json",
            "--no-spinner",
        ])
        .output()
        .unwrap();
    assert!(output.status.success());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(json.get("modelId").is_some(), "Missing modelId");
    assert!(json.get("matchedKey").is_some(), "Missing matchedKey");
    assert!(json.get("source").is_some(), "Missing source");
    assert!(json.get("pricing").is_some(), "Missing pricing");

    let pricing = &json["pricing"];
    assert!(pricing.get("inputCostPerToken").is_some());
    assert!(pricing.get("outputCostPerToken").is_some());
}

#[test]
fn test_pricing_command_with_provider() {
    let mut cmd = cargo_bin_cmd!("tokscale");
    cmd.args([
        "pricing",
        "claude-sonnet-4-20250514",
        "--provider",
        "litellm",
        "--no-spinner",
    ])
    .assert()
    .success();
}

#[test]
fn test_pricing_command_invalid_provider() {
    let mut cmd = cargo_bin_cmd!("tokscale");
    cmd.args([
        "pricing",
        "claude-sonnet-4-20250514",
        "--provider",
        "invalid-provider",
        "--no-spinner",
    ])
    .assert()
    .failure();
}

// ── Clients command tests ──────────────────────────────────────────────────

#[test]
fn test_clients_command() {
    let mut cmd = cargo_bin_cmd!("tokscale");
    cmd.arg("clients")
        .assert()
        .success()
        .stdout(predicate::str::contains("OpenCode").or(predicate::str::contains("opencode")))
        .stdout(predicate::str::contains("Claude").or(predicate::str::contains("claude")));
}

#[test]
fn test_clients_json() {
    let output = cargo_bin_cmd!("tokscale")
        .args(["clients", "--json"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(json.is_object(), "Clients JSON should be an object");
    assert!(json.get("clients").is_some(), "Should have 'clients' field");
    assert!(
        json.get("headlessRoots").is_some(),
        "Should have 'headlessRoots' field"
    );
    assert!(json.get("note").is_some(), "Should have 'note' field");

    let arr = json["clients"].as_array().unwrap();
    assert!(!arr.is_empty(), "Should list at least one client");

    let first = &arr[0];
    assert!(
        first.get("client").is_some(),
        "Client entry should have 'client' field"
    );
    assert!(
        first.get("label").is_some(),
        "Client entry should have 'label' field"
    );
    assert!(
        first.get("sessionsPath").is_some(),
        "Client entry should have 'sessionsPath' field"
    );
    assert!(
        first.get("messageCount").is_some(),
        "Client entry should have 'messageCount' field"
    );
}

// ── Light mode tests ───────────────────────────────────────────────────────

#[test]
fn test_models_light_output() {
    let tmp = create_temp_fixture_dir();
    cmd_with_home(tmp.path())
        .args(["models", "--light", "--opencode", "--no-spinner"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Token Usage Report by Model"));
}

#[test]
fn test_monthly_light_output() {
    let tmp = create_temp_fixture_dir();
    cmd_with_home(tmp.path())
        .args(["monthly", "--light", "--opencode", "--no-spinner"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Monthly Token Usage Report"));
}

#[test]
fn test_models_light_with_client_filter() {
    let tmp = create_temp_fixture_dir();
    cmd_with_home(tmp.path())
        .args(["models", "--light", "--opencode", "--no-spinner"])
        .args(["--year", "2024"])
        .assert()
        .success()
        .stdout(predicate::str::contains("2024"));
}

// ── Benchmark flag tests ───────────────────────────────────────────────────

#[test]
fn test_models_benchmark_flag() {
    let tmp = create_temp_fixture_dir();
    cmd_with_home(tmp.path())
        .args([
            "models",
            "--light",
            "--opencode",
            "--no-spinner",
            "--benchmark",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("Processing time"));
}

#[test]
fn test_monthly_benchmark_flag() {
    let tmp = create_temp_fixture_dir();
    cmd_with_home(tmp.path())
        .args([
            "monthly",
            "--light",
            "--opencode",
            "--no-spinner",
            "--benchmark",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("Processing time"));
}

// ── Empty fixture tests ────────────────────────────────────────────────────

#[test]
fn test_models_empty_fixture() {
    let tmp = create_empty_fixture_dir();
    let output = cmd_with_home(tmp.path())
        .args(["models", "--json", "--opencode", "--no-spinner"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let entries = json["entries"].as_array().unwrap();
    assert!(
        entries.is_empty(),
        "Empty fixture should produce no entries"
    );
    assert_eq!(json["totalInput"].as_i64().unwrap(), 0);
    assert_eq!(json["totalOutput"].as_i64().unwrap(), 0);
}

#[test]
fn test_graph_empty_contributions() {
    let tmp = create_empty_fixture_dir();
    let output = cmd_with_home(tmp.path())
        .args(["graph", "--opencode", "--no-spinner"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let contributions = json["contributions"].as_array().unwrap();
    assert!(
        contributions.is_empty(),
        "Empty fixture should produce no contributions"
    );
}

// ── No-spinner flag tests ──────────────────────────────────────────────────

#[test]
fn test_models_no_spinner_flag() {
    let tmp = create_temp_fixture_dir();
    cmd_with_home(tmp.path())
        .args(["models", "--light", "--opencode", "--no-spinner"])
        .assert()
        .success();
}

#[test]
fn test_graph_no_spinner_flag() {
    let tmp = create_temp_fixture_dir();
    cmd_with_home(tmp.path())
        .args(["graph", "--opencode", "--no-spinner"])
        .assert()
        .success();
}

// ── Graph with client filter tests ─────────────────────────────────────────

#[test]
fn test_graph_with_client_filter() {
    let tmp = create_temp_fixture_dir();
    let output = cmd_with_home(tmp.path())
        .args(["graph", "--opencode", "--no-spinner"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let contributions = json["contributions"].as_array().unwrap();
    for c in contributions {
        let clients = c["clients"].as_array().unwrap();
        for cl in clients {
            assert_eq!(
                cl["client"].as_str().unwrap(),
                "opencode",
                "All contributions should be from opencode"
            );
        }
    }
}

// ── Graph output file test ─────────────────────────────────────────────────

#[test]
fn test_graph_output_to_file() {
    let tmp = create_temp_fixture_dir();
    let output_file = tmp.path().join("graph-output.json");
    cmd_with_home(tmp.path())
        .args(["graph", "--opencode", "--no-spinner"])
        .args(["--output", output_file.to_str().unwrap()])
        .assert()
        .success();
    assert!(output_file.exists(), "Output file should be created");
    let content = fs::read_to_string(&output_file).unwrap();
    let json: serde_json::Value = serde_json::from_str(&content).unwrap();
    assert!(json.get("meta").is_some());
    assert!(json.get("contributions").is_some());
}

// ── Root command tests (no subcommand) ─────────────────────────────────────

#[test]
fn test_root_json_output() {
    let tmp = create_temp_fixture_dir();
    let output = cmd_with_home(tmp.path())
        .args(["--json", "--opencode", "--no-spinner"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(json.get("entries").is_some());
    assert!(json.get("totalCost").is_some());
}

#[test]
fn test_root_light_output() {
    let tmp = create_temp_fixture_dir();
    cmd_with_home(tmp.path())
        .args(["--light", "--opencode", "--no-spinner"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Token Usage Report by Model"));
}

#[test]
fn test_root_with_date_filter() {
    let tmp = create_temp_fixture_dir();
    cmd_with_home(tmp.path())
        .args(["--json", "--opencode", "--no-spinner"])
        .args(["--year", "2025"])
        .assert()
        .success()
        .stdout(predicate::str::contains("gpt-4o"));
}

#[test]
fn test_root_with_group_by() {
    let tmp = create_temp_fixture_dir();
    let output = cmd_with_home(tmp.path())
        .args(["--json", "--opencode", "--no-spinner"])
        .args(["--group-by", "model"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["groupBy"].as_str().unwrap(), "model");
}
