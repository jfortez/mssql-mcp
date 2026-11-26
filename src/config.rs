use std::env;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};

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

#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Transport {
    Stdio,
    Http,
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

#[derive(Args)]
struct ConnectionOptions {
    /// SQL Server hostname (or IP address).
    #[arg(long, env = "MSSQL_HOST", required = true)]
    host: Option<String>,

    /// SQL Server TCP port. Use the configured port for a named instance.
    #[arg(long, env = "MSSQL_PORT", default_value_t = 1433, value_parser = clap::value_parser!(u16).range(1..))]
    port: u16,

    /// SQL Server database name.
    #[arg(long, env = "MSSQL_DATABASE", required = true)]
    database: Option<String>,

    /// SQL login, current Windows identity, or explicit NTLM credentials.
    /// SQL/NTLM use MSSQL_USER and MSSQL_PASSWORD; a `DOMAIN\user` in
    /// MSSQL_USER selects NTLM when this flag is absent.
    #[arg(long, env = "MSSQL_AUTH", value_enum)]
    auth: Option<AuthKind>,

    /// Operation deadline in seconds, including opening a connection.
    #[arg(long, env = "MSSQL_TIMEOUT_SECONDS", default_value_t = 15, value_parser = clap::value_parser!(u16).range(1..=300))]
    timeout_seconds: u16,

    /// Skip TLS certificate validation (development only).
    #[arg(long, env = "MSSQL_TRUST_SERVER_CERTIFICATE")]
    trust_server_certificate: bool,
}

#[derive(Parser)]
#[command(version, about, subcommand_negates_reqs = true)]
pub struct Cli {
    #[command(flatten)]
    connection: ConnectionOptions,

    #[command(subcommand)]
    pub command: Option<Command>,

    /// Expose reading tools, mutation tools, or both.
    #[arg(long, env = "MSSQL_MODE", value_enum, default_value = "read")]
    mode: Mode,

    /// Serve over stdio (default) or as one shared Streamable HTTP daemon.
    /// The HTTP transport requires MSSQL_MCP_TOKEN in the environment.
    #[arg(long, env = "MSSQL_MCP_TRANSPORT", value_enum, default_value = "stdio")]
    transport: Transport,

    /// TCP address the HTTP transport binds once; compatible clients share it.
    #[arg(long, env = "MSSQL_MCP_HTTP_LISTEN", default_value = "127.0.0.1:8765")]
    http_listen: SocketAddr,

    /// Skip the approval prompt for mutation tools (dangerous).
    #[arg(long, env = "MSSQL_MCP_YOLO")]
    yolo: bool,

    /// Validate configuration without connecting to SQL Server.
    #[arg(long)]
    check_config: bool,
}

#[derive(Subcommand)]
pub enum Command {
    /// Export a read-only catalog map once, without starting an MCP transport.
    Map(MapOptions),
}

#[derive(Clone, Copy, ValueEnum)]
pub enum MapFormat {
    Html,
    Json,
}

#[derive(Args)]
pub struct MapOptions {
    #[command(flatten)]
    connection: ConnectionOptions,

    /// Output format.
    #[arg(long, value_enum, default_value = "html")]
    pub format: MapFormat,

    /// Path to a new output file.
    #[arg(long)]
    pub out: PathBuf,

    /// Restrict the map to this schema.
    #[arg(long)]
    pub schema: Option<String>,
}

pub struct Settings {
    pub host: String,
    pub port: u16,
    pub database: String,
    pub auth: Auth,
    pub mode: Mode,
    pub transport: Transport,
    pub http_listen: SocketAddr,
    /// Bearer token guarding the HTTP transport; always `None` for stdio.
    pub token: Option<String>,
    pub timeout_seconds: u16,
    /// Whether mutation tools skip the human approval prompt.
    pub yolo: bool,
    pub trust_server_certificate: bool,
    pub check_config: bool,
}

impl Cli {
    pub fn settings(self) -> Result<Settings, &'static str> {
        let mut settings = self.connection.settings()?;
        settings.mode = self.mode;
        settings.transport = self.transport;
        settings.http_listen = self.http_listen;
        settings.token = match self.transport {
            Transport::Stdio => None,
            Transport::Http => Some(http_token()?),
        };
        settings.yolo = self.yolo;
        settings.check_config = self.check_config;
        Ok(settings)
    }
}

impl MapOptions {
    pub fn settings(&self) -> Result<Settings, &'static str> {
        self.connection.settings()
    }
}

impl ConnectionOptions {
    fn settings(&self) -> Result<Settings, &'static str> {
        let host = self.host.as_deref().ok_or("host is required")?;
        if host.trim().is_empty() || host.chars().any(char::is_whitespace) {
            return Err("host must be a non-empty hostname or IP address");
        }
        let database = self.database.as_deref().ok_or("database is required")?;
        if database.trim().is_empty() {
            return Err("database must not be blank");
        }
        let auth = self.auth()?;
        Ok(Settings {
            host: host.to_owned(),
            port: self.port,
            database: database.to_owned(),
            auth,
            mode: Mode::Read,
            transport: Transport::Stdio,
            http_listen: SocketAddr::from((Ipv4Addr::LOCALHOST, 8765)),
            token: None,
            timeout_seconds: self.timeout_seconds,
            yolo: false,
            trust_server_certificate: self.trust_server_certificate,
            check_config: false,
        })
    }

    /// Resolve the SQL identity once, in the documented order: an explicit
    /// `--auth` always wins, so the backslash in `MSSQL_USER` is a signal only
    /// when the flag is absent and a contradictory pair is an error.
    fn auth(&self) -> Result<Auth, &'static str> {
        match self.auth {
            Some(AuthKind::Windows) if cfg!(windows) => Ok(Auth::Windows),
            Some(AuthKind::Windows) => Err("Windows integrated authentication requires Windows"),
            Some(AuthKind::Sql) => sql_auth(),
            Some(AuthKind::Ntlm) => ntlm_auth(),
            None if user_is_qualified()? => ntlm_auth(),
            None => sql_auth(),
        }
    }
}

const SQL_QUALIFIED_USER: &str = "SQL authentication cannot use a qualified MSSQL_USER: the DOMAIN\\user principal needs --auth ntlm";
const NTLM_UNQUALIFIED_USER: &str =
    "NTLM requires the domain in MSSQL_USER as DOMAIN\\user, not only the user name";
const USER_WITH_EMPTY_PART: &str =
    "MSSQL_USER must be DOMAIN\\user with a non-empty domain and user";

/// Detection recognizes `DOMAIN\user` and nothing else: tiberius and sspi-rs
/// take exactly that spelling, so a UPN would claim support the driver does not
/// prove.
fn user_is_qualified() -> Result<bool, &'static str> {
    let user = required(
        "MSSQL_USER",
        "MSSQL_USER is required for SQL authentication",
    )?;
    Ok(user.contains('\\'))
}

fn sql_auth() -> Result<Auth, &'static str> {
    let user = required(
        "MSSQL_USER",
        "MSSQL_USER is required for SQL authentication",
    )?;
    if user.contains('\\') {
        return Err(SQL_QUALIFIED_USER);
    }
    Ok(Auth::Sql {
        user,
        password: required(
            "MSSQL_PASSWORD",
            "MSSQL_PASSWORD is required for SQL authentication",
        )?,
    })
}

fn ntlm_auth() -> Result<Auth, &'static str> {
    let value = required("MSSQL_USER", "MSSQL_USER is required for NTLM")?;
    let (domain, user) = qualified(&value)?.ok_or(NTLM_UNQUALIFIED_USER)?;
    Ok(Auth::Ntlm {
        domain: domain.to_owned(),
        user: user.to_owned(),
        password: required("MSSQL_PASSWORD", "MSSQL_PASSWORD is required for NTLM")?,
    })
}

/// Split `MSSQL_USER` as `DOMAIN\user` on the first backslash.
///
/// `Ok(None)` means the value has no backslash at all. A backslash with an
/// empty part on either side is an error: NTLM needs both halves.
fn qualified(value: &str) -> Result<Option<(&str, &str)>, &'static str> {
    let Some((domain, user)) = value.split_once('\\') else {
        return Ok(None);
    };
    if domain.is_empty() || user.is_empty() {
        return Err(USER_WITH_EMPTY_PART);
    }
    Ok(Some((domain, user)))
}

/// Accepted only from the environment.
fn http_token() -> Result<String, &'static str> {
    let token = required(
        "MSSQL_MCP_TOKEN",
        "MSSQL_MCP_TOKEN is required for the HTTP transport",
    )?;
    if !(32..=128).contains(&token.len())
        || !token
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
    {
        return Err("MSSQL_MCP_TOKEN must be 32 to 128 characters of A-Z, a-z, 0-9, '_' or '-'");
    }
    Ok(token)
}

fn required(name: &str, message: &'static str) -> Result<String, &'static str> {
    env::var(name).ok().filter(|s| !s.is_empty()).ok_or(message)
}
