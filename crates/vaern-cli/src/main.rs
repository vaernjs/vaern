//! Command-line interface for the Vaern runtime.
//!
//! Owns argument parsing, help output, version reporting, launching the
//! native runtime engine to execute TypeScript/JavaScript files, and the
//! commands that read run journals.

use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::process::ExitCode;
use vaern_core::{QuickJsRuntime, Runtime, RuntimeOptions, budget, journal};

#[derive(Parser, Debug)]
#[command(
    name = "vaern",
    version = env!("VAERN_VERSION"),
    about = "An agent-native JavaScript and TypeScript runtime.",
    long_about = "Vaern is an agent-native JavaScript and TypeScript runtime. Each run's effects are recorded in a hash-chained journal under .vaern/runs."
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Run a JavaScript or TypeScript file
    Run {
        /// Do not write a journal for this run
        #[arg(long)]
        no_journal: bool,
        /// Most the run may reserve for model calls and priced tools, in
        /// estimated USD; a script's budget() cannot raise it
        #[arg(long, value_name = "USD", value_parser = parse_usd)]
        budget_usd: Option<u64>,
        /// Most the run may reserve in model tokens; a script's budget()
        /// cannot raise it
        #[arg(long, value_name = "TOKENS")]
        budget_tokens: Option<u64>,
        /// Target script file to execute
        file: PathBuf,
        /// Arguments passed to the script
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Inspect run journals
    Journal {
        #[command(subcommand)]
        command: JournalCommand,
    },
}

#[derive(Subcommand, Debug)]
enum JournalCommand {
    /// Check a run's hash chain: exit 0 when intact and sealed, 2 when intact
    /// but unsealed, 1 when it does not verify
    Verify {
        /// Run id from .vaern/runs, or `latest`
        id: String,
    },
}

/// `VAERN_HOME` when set, else `.vaern` in the working directory.
fn journal_home() -> PathBuf {
    match std::env::var_os("VAERN_HOME") {
        Some(home) if !home.is_empty() => PathBuf::from(home),
        _ => std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(".vaern"),
    }
}

/// A USD amount as nano-USD, the unit budgets count in.
fn parse_usd(text: &str) -> Result<u64, String> {
    match text.parse::<f64>() {
        Ok(usd) if usd.is_finite() && (0.0..=1.0e9).contains(&usd) => {
            Ok((usd * 1.0e9).round() as u64)
        }
        _ => Err(format!(
            "'{text}' is not an amount in USD; give a number such as 0.25"
        )),
    }
}

fn run(no_journal: bool, caps: budget::Limits, file: PathBuf, args: Vec<String>) -> ExitCode {
    let mut argv = vec!["vaern".to_string(), file.display().to_string()];
    argv.extend(args);

    let options = RuntimeOptions { argv };
    let mut runtime = match QuickJsRuntime::new(options) {
        Ok(rt) => rt,
        Err(err) => {
            eprintln!("{err}");
            return ExitCode::FAILURE;
        }
    };
    if !no_journal {
        runtime.set_journal_home(Some(journal_home()));
    }
    // Named budget ledgers live under the same home, journal or not.
    runtime.set_budget(caps, Some(journal_home()));

    match runtime.run_file(&file) {
        Ok(0) => ExitCode::SUCCESS,
        Ok(code) => ExitCode::from(code as u8),
        Err(err) => {
            eprintln!("{err}");
            ExitCode::FAILURE
        }
    }
}

fn verify(id: &str) -> ExitCode {
    let path = match journal::locate(&journal_home(), id) {
        Ok(path) => path,
        Err(err) => {
            eprintln!("{err}");
            return ExitCode::FAILURE;
        }
    };
    match journal::verify(&path) {
        Ok(v) => {
            let state = match (v.sealed, v.torn_tail) {
                (true, _) => "sealed",
                (false, true) => "unsealed, last line cut off (the run was interrupted)",
                (false, false) => "unsealed (the run was interrupted, or its end was removed)",
            };
            println!(
                "{}: {} records, {state}\nfinal hash {}",
                path.display(),
                v.records,
                v.final_hash
            );
            if v.sealed {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(2)
            }
        }
        Err(err) => {
            eprintln!("{err}");
            ExitCode::FAILURE
        }
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.command {
        Commands::Run {
            no_journal,
            budget_usd,
            budget_tokens,
            file,
            args,
        } => {
            let caps = budget::Limits {
                nano: budget_usd,
                tokens: budget_tokens,
            };
            run(no_journal, caps, file, args)
        }
        Commands::Journal {
            command: JournalCommand::Verify { id },
        } => verify(&id),
    }
}
