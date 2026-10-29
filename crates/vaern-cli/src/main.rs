//! Command-line interface for the Vaern runtime.
//!
//! Owns argument parsing, help output, version reporting, and launching the
//! native runtime engine to execute TypeScript/JavaScript files.

use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::process::ExitCode;
use vaern_core::{QuickJsRuntime, Runtime, RuntimeOptions};

#[derive(Parser, Debug)]
#[command(
    name = "vaern",
    version = env!("VAERN_VERSION"),
    about = "An agent-native JavaScript and TypeScript runtime.",
    long_about = "Vaern is an agent-native JavaScript and TypeScript runtime with capability sandboxing, deterministic replay, and sovereignty enforcement."
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Run a JavaScript or TypeScript file
    Run {
        /// Target script file to execute
        file: PathBuf,
        /// Arguments passed to the script
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.command {
        Commands::Run { file, args } => {
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

            match runtime.run_file(&file) {
                Ok(code) => {
                    if code == 0 {
                        ExitCode::SUCCESS
                    } else {
                        ExitCode::from(code as u8)
                    }
                }
                Err(err) => {
                    eprintln!("{err}");
                    ExitCode::FAILURE
                }
            }
        }
    }
}
