//! Runs the built `vaern` binary against the fixtures in `tests/sandbox_run/`.
//!
//! Unit tests inside the engine cannot cover process exit codes or what lands
//! on stdout and stderr, so these tests start the real binary as a child
//! process and assert on exactly those three outputs.

use std::error::Error;
use std::path::PathBuf;
use std::process::{Command, Output};

type TestResult = Result<(), Box<dyn Error>>;

fn sandbox_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/sandbox_run")
}

fn vaern(args: &[&str]) -> Result<Output, Box<dyn Error>> {
    Ok(Command::new(env!("CARGO_BIN_EXE_vaern"))
        .args(args)
        .current_dir(sandbox_dir())
        .env("VAERN_SANDBOX_VALUE", "from-host")
        .output()?)
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).replace("\r\n", "\n")
}

#[test]
fn version_reports_release_number() -> TestResult {
    let out = vaern(&["--version"])?;
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(text(&out.stdout).trim(), "vaern 0.0.6");
    Ok(())
}

#[test]
fn typescript_types_are_stripped() -> TestResult {
    let out = vaern(&["run", "hello.ts"])?;
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(text(&out.stdout), "Hello from Vaern (Runtime)!\n");
    assert_eq!(text(&out.stderr), "");
    Ok(())
}

#[test]
fn script_arguments_reach_process_argv() -> TestResult {
    let out = vaern(&["run", "argv_env.ts", "first-arg"])?;
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(text(&out.stdout), "Arg: first-arg\n");
    Ok(())
}

#[test]
fn host_environment_reaches_process_env() -> TestResult {
    let out = vaern(&["run", "env_test.ts"])?;
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(text(&out.stdout), "Env: from-host\n");
    Ok(())
}

#[test]
fn process_exit_sets_the_exit_code() -> TestResult {
    let out = vaern(&["run", "exit_test.ts"])?;
    assert_eq!(out.status.code(), Some(42));
    assert_eq!(text(&out.stdout), "");
    Ok(())
}

#[test]
fn syntax_error_reports_location_without_panic() -> TestResult {
    let out = vaern(&["run", "bad_syntax.ts"])?;
    assert_eq!(out.status.code(), Some(1));
    let stderr = text(&out.stderr);
    assert!(stderr.contains("bad_syntax.ts:1:"), "{stderr}");
    assert!(!stderr.contains("panicked"), "{stderr}");
    Ok(())
}

#[test]
fn uncaught_exception_reports_location_and_fails() -> TestResult {
    let out = vaern(&["run", "throws.js"])?;
    assert_eq!(out.status.code(), Some(1));
    let stderr = text(&out.stderr);
    assert!(stderr.contains("Error: boom"), "{stderr}");
    assert!(stderr.contains("throws.js:1:"), "{stderr}");
    assert!(!stderr.contains("panicked"), "{stderr}");
    Ok(())
}

#[test]
fn throwing_timer_callback_fails() -> TestResult {
    let out = vaern(&["run", "timer_throws.js"])?;
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(text(&out.stdout), "");
    let stderr = text(&out.stderr);
    assert!(stderr.contains("Error: timer boom"), "{stderr}");
    assert!(stderr.contains("timer_throws.js:2:"), "{stderr}");
    assert!(!stderr.contains("panicked"), "{stderr}");
    Ok(())
}

/// A fresh journal home for one test, outside the sandbox directory.
fn journal_home(test: &str) -> Result<PathBuf, Box<dyn Error>> {
    let dir = std::env::temp_dir().join(format!("vaern-cli-{}-{test}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

fn vaern_home(args: &[&str], home: &std::path::Path) -> Result<Output, Box<dyn Error>> {
    Ok(Command::new(env!("CARGO_BIN_EXE_vaern"))
        .args(args)
        .current_dir(sandbox_dir())
        .env("VAERN_HOME", home)
        .output()?)
}

#[test]
fn run_is_journalled_and_verifies() -> TestResult {
    let home = journal_home("verify")?;
    let out = vaern_home(&["run", "hello.ts"], &home)?;
    assert_eq!(out.status.code(), Some(0));

    let check = vaern_home(&["journal", "verify", "latest"], &home)?;
    assert_eq!(check.status.code(), Some(0), "{}", text(&check.stderr));
    let report = text(&check.stdout);
    assert!(report.contains("records, sealed"), "{report}");
    assert!(report.contains("final hash "), "{report}");

    // Every printed line is in the journal, in order, before run.end.
    let id = std::fs::read_to_string(home.join("runs/latest"))?;
    let journal = std::fs::read_to_string(home.join("runs").join(id.trim()).join("journal.jsonl"))?;
    let stdout = text(&out.stdout);
    let first = stdout.lines().next().unwrap_or_default();
    assert!(journal.contains("\"kind\":\"console\""), "{journal}");
    assert!(journal.contains(first), "{first} missing from {journal}");
    assert!(
        journal
            .lines()
            .last()
            .unwrap_or_default()
            .contains("\"kind\":\"run.end\",\"at_us\"")
    );

    // One changed byte breaks verification.
    let path = home.join("runs").join(id.trim()).join("journal.jsonl");
    std::fs::write(
        &path,
        journal.replacen("\"kind\":\"console\"", "\"kind\":\"consolE\"", 1),
    )?;
    let check = vaern_home(&["journal", "verify", "latest"], &home)?;
    assert_eq!(check.status.code(), Some(1));
    assert!(text(&check.stderr).contains("fails verification at record"));
    Ok(())
}

#[test]
fn process_exit_seals_the_journal_with_its_code() -> TestResult {
    let home = journal_home("exit")?;
    let out = vaern_home(&["run", "exit_test.ts"], &home)?;
    assert_eq!(out.status.code(), Some(42));
    let check = vaern_home(&["journal", "verify", "latest"], &home)?;
    assert_eq!(check.status.code(), Some(0), "{}", text(&check.stdout));
    let id = std::fs::read_to_string(home.join("runs/latest"))?;
    let journal = std::fs::read_to_string(home.join("runs").join(id.trim()).join("journal.jsonl"))?;
    assert!(journal.contains("\"kind\":\"run.end\""), "{journal}");
    assert!(journal.contains("\"exit_code\":42"), "{journal}");
    Ok(())
}

#[test]
fn no_journal_writes_nothing() -> TestResult {
    let home = journal_home("off")?;
    let out = vaern_home(&["run", "--no-journal", "hello.ts"], &home)?;
    assert_eq!(out.status.code(), Some(0));
    assert!(!home.join("runs").exists());
    let check = vaern_home(&["journal", "verify", "latest"], &home)?;
    assert_eq!(check.status.code(), Some(1));
    Ok(())
}
