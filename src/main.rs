use std::process::ExitCode;

use clap::Parser;
use mssql_mcp::config::Cli;

fn main() -> ExitCode {
    let settings = match Cli::parse().settings() {
        Ok(settings) => settings,
        Err(error) => {
            eprintln!("Configuration error: {error}");
            return ExitCode::from(2);
        }
    };
    if settings.check_config {
        println!(
            "Configuration valid (auth={}, mode={})",
            settings.auth.name(),
            settings.mode.name()
        );
        return ExitCode::SUCCESS;
    }
    eprintln!("MCP transport is not available yet");
    ExitCode::FAILURE
}
