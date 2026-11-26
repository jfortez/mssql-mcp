use std::env;

use clap::{Parser, ValueEnum};

#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Mode {
    Read,
    Write,
    Both,
}

impl Mode {
    pub const fn name(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Write => "write",
            Self::Both => "both",
        }
    }
}

#[derive(Clone, Copy, ValueEnum)]
enum AuthKind {
    Sql,
    Windows,
    Ntlm,
}

pub enum Auth {
    Sql {
        user: String,
        password: String,
    },
    Windows,
    Ntlm {
        domain: String,
        user: String,
        password: String,
    },
}

impl Auth {
    pub const fn name(&self) -> &'static str {
        match self {
            Self::Sql { .. } => "sql",
            Self::Windows => "windows",
            Self::Ntlm { .. } => "ntlm",
        }
    }
}

#[derive(Parser)]
#[command(version, about)]
pub struct Cli {
    /// SQL Server hostname (or IP address).
    #[arg(long, env = "MSSQL_HOST")]
    host: String,

    /// SQL Server TCP port. Use the configured port for a named instance.
    #[arg(long, env = "MSSQL_PORT", default_value_t = 1433, value_parser = clap::value_parser!(u16).range(1..))]
    port: u16,

    /// SQL Server database name.
    #[arg(long, env = "MSSQL_DATABASE")]
    database: String,

    /// SQL login, current Windows identity, or explicit NTLM credentials.
    /// SQL/NTLM use MSSQL_USER and MSSQL_PASSWORD; NTLM also uses MSSQL_DOMAIN.
    #[arg(long, env = "MSSQL_AUTH", value_enum, default_value = "sql")]
    auth: AuthKind,

    /// Expose reading tools, mutation tools, or both.
    #[arg(long, env = "MSSQL_MODE", value_enum, default_value = "read")]
    mode: Mode,

    /// Operation deadline in seconds, including opening a connection.
    #[arg(long, env = "MSSQL_TIMEOUT_SECONDS", default_value_t = 15, value_parser = clap::value_parser!(u16).range(1..=300))]
    timeout_seconds: u16,

    /// Skip TLS certificate validation (development only).
    #[arg(long, env = "MSSQL_TRUST_SERVER_CERTIFICATE")]
    trust_server_certificate: bool,

    /// Validate configuration without connecting to SQL Server.
    #[arg(long)]
    check_config: bool,
}

pub struct Settings {
    pub host: String,
    pub port: u16,
    pub database: String,
    pub auth: Auth,
    pub mode: Mode,
    pub timeout_seconds: u16,
    pub trust_server_certificate: bool,
    pub check_config: bool,
}

impl Cli {
    pub fn settings(self) -> Result<Settings, &'static str> {
        if self.host.trim().is_empty() || self.host.chars().any(char::is_whitespace) {
            return Err("host must be a non-empty hostname or IP address");
        }
        if self.database.trim().is_empty() {
            return Err("database must not be blank");
        }
        let auth = match self.auth {
            AuthKind::Sql => Auth::Sql {
                user: required(
                    "MSSQL_USER",
                    "MSSQL_USER is required for SQL authentication",
                )?,
                password: required(
                    "MSSQL_PASSWORD",
                    "MSSQL_PASSWORD is required for SQL authentication",
                )?,
            },
            AuthKind::Windows if cfg!(windows) => Auth::Windows,
            AuthKind::Windows => return Err("Windows integrated authentication requires Windows"),
            AuthKind::Ntlm => {
                let domain = required("MSSQL_DOMAIN", "MSSQL_DOMAIN is required for NTLM")?;
                let user = required("MSSQL_USER", "MSSQL_USER is required for NTLM")?;
                if user.contains(['\\', '@']) || domain.contains(['\\', '@']) {
                    return Err(
                        "NTLM requires separate, unqualified MSSQL_DOMAIN and MSSQL_USER values",
                    );
                }
                Auth::Ntlm {
                    domain,
                    user,
                    password: required("MSSQL_PASSWORD", "MSSQL_PASSWORD is required for NTLM")?,
                }
            }
        };
        Ok(Settings {
            host: self.host,
            port: self.port,
            database: self.database,
            auth,
            mode: self.mode,
            timeout_seconds: self.timeout_seconds,
            trust_server_certificate: self.trust_server_certificate,
            check_config: self.check_config,
        })
    }
}

fn required(name: &str, message: &'static str) -> Result<String, &'static str> {
    env::var(name).ok().filter(|s| !s.is_empty()).ok_or(message)
}
