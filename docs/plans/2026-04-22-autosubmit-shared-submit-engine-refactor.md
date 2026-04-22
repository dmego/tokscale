# Autosubmit Minimal-Intrusion Shared Submit Engine Refactor Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace autosubmit's subprocess-based `tokscale submit` bridge with a shared in-process submit execution path, while keeping the current scheduler design and avoiding broad file or module restructuring.

**Architecture:** Keep `SubmitFilterArgs`, `SubmitCommandArgs`, and the existing submit implementation in `crates/tokscale-cli/src/main.rs`. Introduce a small internal execution mode split inside `main.rs` so interactive `submit` and background `autosubmit` share the same core submit logic but differ on prompt/printing/cache-warming behavior. Update `crates/tokscale-cli/src/commands/autosubmit.rs` to call the new in-process submit entrypoint directly and delete the subprocess bridge helpers.

**Tech Stack:** Rust, Clap, Tokio runtime, reqwest, anyhow, existing `tokscale-core` graph generation APIs, existing autosubmit unit tests, existing CLI integration tests.

---

## Scope Guard

- Keep the public autosubmit CLI surface unchanged: `enable|disable|status|run`.
- Keep the public submit CLI surface unchanged.
- Keep the current interval model unchanged: user-facing `Nh|Nd`, scheduler heartbeat remains separate.
- Do not create `crates/tokscale-cli/src/commands/submit.rs`.
- Do not modify `crates/tokscale-cli/src/commands/mod.rs`.
- Limit the write set to:
  - `crates/tokscale-cli/src/main.rs`
  - `crates/tokscale-cli/src/commands/autosubmit.rs`
  - existing test files only when required by the refactor

## File Structure

### Modify

- `crates/tokscale-cli/src/main.rs`
  - Keep the existing submit code in place.
  - Add a minimal shared execution mode split and an autosubmit-safe entrypoint.
  - Remove subprocess-only submit machine-error helpers after autosubmit stops depending on them.
- `crates/tokscale-cli/src/commands/autosubmit.rs`
  - Replace subprocess submit execution with direct in-process submit execution.
  - Delete the subprocess bridge helpers.
  - Fix the submit-flag formatter so status rendering stays in sync with all current submit filters.
- `crates/tokscale-cli/tests/cli_tests.rs`
  - Only modify if the existing integration suite does not already cover a refactor regression.

### Keep Unchanged

- `crates/tokscale-cli/src/commands/mod.rs`
- scheduler install/probe/uninstall logic in `crates/tokscale-cli/src/commands/autosubmit.rs`
- settings schema in `crates/tokscale-cli/src/tui/settings.rs`

### Test Surfaces

- `crates/tokscale-cli/src/main.rs`
  - Extend the existing in-file unit tests for the new execution mode helpers.
- `crates/tokscale-cli/src/commands/autosubmit.rs`
  - Keep using the existing autosubmit run/status/lock regression tests.
  - Add one targeted regression test for complete filter rendering.
- `crates/tokscale-cli/tests/cli_tests.rs`
  - Reuse the current CLI contract tests first; only add more if the refactor changes an uncovered edge.

---

### Task 1: Add A Shared Submit Execution Mode Inside `main.rs`

**Files:**
- Modify: `crates/tokscale-cli/src/main.rs`
- Test: `crates/tokscale-cli/src/main.rs`

- [ ] **Step 1: Write the failing unit tests for the new submit execution mode helpers**

```rust
#[test]
fn test_submit_run_mode_interactive_flags() {
    let mode = SubmitRunMode::InteractiveCli;
    assert!(mode.should_prompt_for_star());
    assert!(mode.should_print_user_output());
    assert!(mode.should_warm_tui_cache());
}

#[test]
fn test_submit_run_mode_autosubmit_flags() {
    let mode = SubmitRunMode::Autosubmit;
    assert!(!mode.should_prompt_for_star());
    assert!(!mode.should_print_user_output());
    assert!(!mode.should_warm_tui_cache());
}
```

- [ ] **Step 2: Run the focused tests to verify the helper type does not exist yet**

Run:

```bash
cargo test -p tokscale-cli test_submit_run_mode_interactive_flags -- --nocapture
cargo test -p tokscale-cli test_submit_run_mode_autosubmit_flags -- --nocapture
```

Expected:

```text
FAIL: cannot find type `SubmitRunMode`
```

- [ ] **Step 3: Add a minimal internal mode enum and submit outcome type in `main.rs`**

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SubmitRunMode {
    InteractiveCli,
    Autosubmit,
}

impl SubmitRunMode {
    fn should_prompt_for_star(self) -> bool {
        matches!(self, Self::InteractiveCli)
    }

    fn should_print_user_output(self) -> bool {
        matches!(self, Self::InteractiveCli)
    }

    fn should_warm_tui_cache(self) -> bool {
        matches!(self, Self::InteractiveCli)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SubmitExecutionOutcome {
    submitted: bool,
}
```

- [ ] **Step 4: Refactor the current submit path so the core execution does not call `std::process::exit()`**

Replace the current monolithic flow with a shared internal helper:

```rust
fn execute_submit_command(
    clients: Option<Vec<String>>,
    since: Option<String>,
    until: Option<String>,
    year: Option<String>,
    dry_run: bool,
    mode: SubmitRunMode,
) -> Result<SubmitExecutionOutcome> {
    let credentials = auth::load_credentials()
        .ok_or_else(|| anyhow::anyhow!("Not logged in."))?;

    if mode.should_prompt_for_star()
        && std::io::stdin().is_terminal()
        && std::io::stdout().is_terminal()
    {
        let _ = prompt_star_repo(&credentials.username);
    }

    if mode.should_print_user_output() {
        println!("\n  {}\n", "Tokscale - Submit Usage Data".cyan());
    }

    // keep the existing Cursor sync, graph generation, UTC date capping,
    // request execution, and response parsing logic here
    // but convert all previous `std::process::exit(1)` branches into `Err(...)`

    if graph_result.summary.total_tokens == 0 {
        if mode.should_print_user_output() {
            println!("{}", "  No usage data found to submit.\n".yellow());
        }
        return Ok(SubmitExecutionOutcome { submitted: false });
    }

    if dry_run {
        if mode.should_print_user_output() {
            println!("{}", "  Dry run - not submitting data.\n".yellow());
        }
        return Ok(SubmitExecutionOutcome { submitted: false });
    }

    Ok(SubmitExecutionOutcome { submitted: true })
}
```

- [ ] **Step 5: Keep the existing public submit wrapper but route it through the shared helper**

```rust
pub fn run_submit_with_args(args: &SubmitCommandArgs) -> Result<()> {
    let clients = build_client_filter(args.filters.client_flags());
    let (since, until) = build_date_filter(
        args.filters.today,
        args.filters.week,
        args.filters.month,
        args.filters.since.clone(),
        args.filters.until.clone(),
    );
    let year = normalize_year_filter(
        args.filters.today,
        args.filters.week,
        args.filters.month,
        args.filters.year.clone(),
    );

    match execute_submit_command(
        clients,
        since,
        until,
        year,
        args.dry_run,
        SubmitRunMode::InteractiveCli,
    ) {
        Ok(outcome) => {
            if outcome.submitted {
                spawn_warm_tui_cache_detached();
            }
            Ok(())
        }
        Err(err) => {
            use colored::Colorize;
            eprintln!("\n  {}", format!("Error: {}", err).red());
            println!();
            std::process::exit(1);
        }
    }
}
```

- [ ] **Step 6: Add the autosubmit-safe shared entrypoint without moving code to a new file**

```rust
pub(crate) fn run_submit_with_args_autosubmit(args: &SubmitCommandArgs) -> Result<()> {
    let clients = build_client_filter(args.filters.client_flags());
    let (since, until) = build_date_filter(
        args.filters.today,
        args.filters.week,
        args.filters.month,
        args.filters.since.clone(),
        args.filters.until.clone(),
    );
    let year = normalize_year_filter(
        args.filters.today,
        args.filters.week,
        args.filters.month,
        args.filters.year.clone(),
    );

    execute_submit_command(
        clients,
        since,
        until,
        year,
        args.dry_run,
        SubmitRunMode::Autosubmit,
    )
    .map(|_| ())
}
```

- [ ] **Step 7: Re-run the focused mode tests**

Run:

```bash
cargo test -p tokscale-cli test_submit_run_mode_interactive_flags -- --nocapture
cargo test -p tokscale-cli test_submit_run_mode_autosubmit_flags -- --nocapture
```

Expected:

```text
PASS: both mode helper tests succeed
```

- [ ] **Step 8: Commit the internal shared engine split**

```bash
git add crates/tokscale-cli/src/main.rs
git commit -m "refactor(cli): share submit execution with autosubmit mode"
```

### Task 2: Rewire Autosubmit To Use The Shared In-Process Submit Path

**Files:**
- Modify: `crates/tokscale-cli/src/commands/autosubmit.rs`
- Modify: `crates/tokscale-cli/src/main.rs`
- Test: `crates/tokscale-cli/src/commands/autosubmit.rs`

- [ ] **Step 1: Write the failing regression test for complete filter rendering**

```rust
#[test]
fn format_submit_args_includes_recent_client_flags() {
    let args = SubmitCommandArgs {
        filters: SubmitFilterArgs {
            copilot: true,
            hermes: true,
            kilo: true,
            crush: true,
            ..Default::default()
        },
        dry_run: false,
    };

    let rendered = format_submit_args(&args);

    assert!(rendered.contains("--copilot"));
    assert!(rendered.contains("--hermes"));
    assert!(rendered.contains("--kilo"));
    assert!(rendered.contains("--crush"));
}
```

- [ ] **Step 2: Run the focused autosubmit tests to verify the existing bridge still leaks**

Run:

```bash
cargo test -p tokscale-cli format_submit_args_includes_recent_client_flags -- --nocapture
cargo test -p tokscale-cli run_logs_minimal_block_for_successful_run -- --nocapture
```

Expected:

```text
FAIL: `format_submit_args_includes_recent_client_flags` is missing one or more flags
PASS or FAIL: the logging test remains the baseline for the later rewire
```

- [ ] **Step 3: Replace the default autosubmit submitter with the shared in-process entrypoint**

In `crates/tokscale-cli/src/commands/autosubmit.rs`, replace the subprocess default:

```rust
use crate::{run_submit_with_args_autosubmit, SubmitCommandArgs, SubmitFilterArgs};

pub fn run_autosubmit_run() -> Result<()> {
    run_autosubmit_run_with_submitter(run_submit_with_args_autosubmit)
}
```

- [ ] **Step 4: Delete the subprocess bridge helpers from `autosubmit.rs`**

Remove these functions entirely once the direct call compiles:

```rust
fn extract_submit_failure_reason(output: &Output) -> String { ... }
fn submit_args_to_cli_args(args: &SubmitCommandArgs) -> Vec<String> { ... }
fn run_submit_quiet_via_cli(args: &SubmitCommandArgs) -> Result<()> { ... }
```

Also remove their now-unused imports:

```rust
use std::process::{Command, Output, Stdio};
use crate::SUBMIT_MACHINE_ERROR_PREFIX;
```

- [ ] **Step 5: Expand the autosubmit filter formatter to stay aligned with all current submit flags**

Update `submit_filter_bool_flag_entries()` to include the missing filters:

```rust
fn submit_filter_bool_flag_entries(filters: &SubmitFilterArgs) -> [(&'static str, bool); 22] {
    [
        ("--opencode", filters.opencode),
        ("--claude", filters.claude),
        ("--codex", filters.codex),
        ("--copilot", filters.copilot),
        ("--gemini", filters.gemini),
        ("--cursor", filters.cursor),
        ("--amp", filters.amp),
        ("--droid", filters.droid),
        ("--openclaw", filters.openclaw),
        ("--hermes", filters.hermes),
        ("--pi", filters.pi),
        ("--kimi", filters.kimi),
        ("--qwen", filters.qwen),
        ("--roocode", filters.roocode),
        ("--kilocode", filters.kilocode),
        ("--kilo", filters.kilo),
        ("--mux", filters.mux),
        ("--crush", filters.crush),
        ("--synthetic", filters.synthetic),
        ("--today", filters.today),
        ("--week", filters.week),
        ("--month", filters.month),
    ]
}
```

- [ ] **Step 6: Remove now-obsolete submit machine-error helpers from `main.rs`**

Delete these once `autosubmit.rs` no longer references them:

```rust
pub const SUBMIT_MACHINE_ERROR_PREFIX: &str = "__TOKSCALE_SUBMIT_ERROR__:";
fn submit_machine_error_contract_enabled() -> bool { ... }
fn emit_submit_machine_error(reason: &str) { ... }
```

- [ ] **Step 7: Re-run the autosubmit unit suite**

Run:

```bash
cargo test -p tokscale-cli autosubmit -- --nocapture
```

Expected:

```text
PASS: existing autosubmit tests remain green
PASS: `format_submit_args_includes_recent_client_flags` remains green
```

- [ ] **Step 8: Commit the autosubmit rewire**

```bash
git add crates/tokscale-cli/src/main.rs \
        crates/tokscale-cli/src/commands/autosubmit.rs
git commit -m "refactor(cli): run autosubmit through shared submit path"
```

### Task 3: Validate The Minimal-Intrusion Refactor Boundary

**Files:**
- Modify: `crates/tokscale-cli/tests/cli_tests.rs` only if a missing regression test is discovered
- Modify: `crates/tokscale-cli/src/main.rs` only for small cleanup from test feedback
- Modify: `crates/tokscale-cli/src/commands/autosubmit.rs` only for small cleanup from test feedback

- [ ] **Step 1: Reuse the current CLI contract tests before adding any new integration coverage**

Run:

```bash
cargo test -p tokscale-cli test_submit_help_shows_dry_run -- --nocapture
cargo test -p tokscale-cli test_autosubmit_help_command -- --nocapture
cargo test -p tokscale-cli test_autosubmit_run_logs_minimal_block_without_submit_details -- --nocapture
```

Expected:

```text
PASS: submit help still shows `--dry-run`
PASS: autosubmit help text is unchanged
PASS: autosubmit run still emits only the minimal autosubmit log block
```

- [ ] **Step 2: Only if one important edge is still uncovered, add exactly one integration test**

Use this test only if the existing suite misses a regression after the refactor:

```rust
#[test]
fn test_autosubmit_run_stays_quiet_on_submit_success() {
    let mut cmd = command();
    cmd.env("TOKSCALE_AUTOSUBMIT_SKIP_SCHEDULER", "1")
        .args(["autosubmit", "run"])
        .assert()
        .stdout(predicate::str::contains("[autosubmit] start"))
        .stdout(predicate::str::contains("Tokscale - Submit Usage Data").not());
}
```

- [ ] **Step 3: Run formatting and the full tokscale-cli test suite**

Run:

```bash
cargo fmt --all
cargo test -p tokscale-cli
```

Expected:

```text
PASS: formatting is clean
PASS: tokscale-cli unit and integration tests all pass
```

- [ ] **Step 4: Verify the refactor stayed inside the intended write set**

Run:

```bash
git diff --stat
rg -n "SUBMIT_MACHINE_ERROR_PREFIX|submit_args_to_cli_args|run_submit_quiet_via_cli|extract_submit_failure_reason" crates/tokscale-cli/src -S
```

Expected:

```text
git diff only shows `main.rs`, `autosubmit.rs`, and tests if needed
rg returns no matches for the deleted subprocess bridge helpers
```

- [ ] **Step 5: Commit the validation pass**

```bash
git add crates/tokscale-cli/src/main.rs \
        crates/tokscale-cli/src/commands/autosubmit.rs \
        crates/tokscale-cli/tests/cli_tests.rs
git commit -m "test(cli): verify minimal shared autosubmit refactor"
```

---

## Implementation Notes

- This plan intentionally does **not** introduce a new submit module. The point is to reduce bridge complexity without turning this PR into a broad file-organization refactor.
- The shared submit engine here means “shared execution path inside `main.rs`”, not “new submit architecture”.
- Interactive `submit` should keep its current user-facing output and cache-warming behavior.
- Autosubmit should stop inheriting interactive-only side effects:
  - no subprocess submit invocation
  - no machine-readable stderr prefix contract
  - no detached TUI cache warming
  - no interactive banner or prompt output
- `autosubmit status` must keep using `format_submit_args()` and therefore needs the full current filter map.

## Self-Review

- Spec coverage:
  - Minimal-intrusion shared engine in `main.rs` is covered by Task 1.
  - Direct autosubmit wiring and bridge deletion are covered by Task 2.
  - Boundary validation and contract verification are covered by Task 3.
- Placeholder scan:
  - No `TODO`, `TBD`, or “implement later” placeholders remain.
  - Every task contains concrete file paths, code blocks, commands, and expected outcomes.
- Type consistency:
  - `SubmitRunMode`, `SubmitExecutionOutcome`, `execute_submit_command`, and `run_submit_with_args_autosubmit` use one consistent naming scheme throughout the plan.
