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
    assert_eq!(text(&out.stdout).trim(), "vaern 0.0.3");
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
