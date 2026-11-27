use std::process::ExitCode;

use clap::Parser;
use mssql_mcp::config::{Cli, Command};

mod map_command;
mod map_html;

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    if let Some(Command::Map(ref options)) = cli.command {
        let settings = match options.settings() {
            Ok(settings) => settings,
            Err(error) => {
                eprintln!("Configuration error: {error}");
                return ExitCode::from(2);
            }
        };
        return match map_command::export(options, settings).await {
            Ok(()) => {
                println!("Schema map written to {}", options.out.display());
                ExitCode::SUCCESS
            }
            Err(error) => {
                eprintln!("Schema map error: {error}");
                ExitCode::FAILURE
            }
        };
    }
    let (settings, trust_elicitation) = match cli.settings() {
        Ok(configuration) => configuration,
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
    match mssql_mcp::server::serve(settings, trust_elicitation).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("MCP server error: {error}");
            ExitCode::FAILURE
        }
    }
}
