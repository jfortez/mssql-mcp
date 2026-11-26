use std::borrow::Cow;
use std::sync::Arc;
use std::time::Duration;

use futures_util::TryStreamExt;
use serde_json::Value;
use tiberius::{
    AuthMethod, Client, Column, ColumnData, Config, EncryptionLevel, Query, QueryItem, QueryStream,
    Row,
};
use tokio::net::TcpStream;
use tokio::sync::Semaphore;
use tokio_util::compat::{Compat, TokioAsyncWriteCompatExt};

use crate::config::{Auth, Settings};
use crate::statement::Statement;

const MAX_IN_FLIGHT: usize = 4;
const MAX_ROWS: usize = 100;
const MAX_COLUMNS: usize = 16;
const MAX_CELL_UNITS: usize = 1024;
const MUTATION_COLUMN: &str = "affected_rows";

type Connection = Client<Compat<TcpStream>>;

pub struct ReadOutput {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Option<String>>>,
    pub truncated: bool,
}

pub enum Outcome {
    Read(ReadOutput),
    Mutation(i64),
}

/// Sanitized failure classes; the original driver error is never exposed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DbError {
    Busy,
    /// The connection, TLS or login handshake failed before any statement ran.
    Connect,
    Timeout,
    Failed,
    UnsupportedAuth,
    UnexpectedResult,
}

impl DbError {
    pub const fn message(self, mutation: bool) -> &'static str {
        match (self, mutation) {
            (Self::Busy, _) => "too many database operations are in progress",
            (Self::Timeout, true) => {
                "database operation timed out; the mutation outcome is unknown"
            }
            (Self::Timeout, false) => "database operation timed out",
            (Self::Connect, _) => "database operation failed",
            (Self::Failed, true) => "database operation failed; the mutation outcome is unknown",
            (Self::Failed, false) => "database operation failed",
            (Self::UnsupportedAuth, _) => "authentication method is not available in this build",
            (Self::UnexpectedResult, true) => {
                "database returned an unexpected result; the mutation outcome is unknown"
            }
            (Self::UnexpectedResult, false) => "database returned an unexpected result",
        }
    }
}

pub struct Database {
    settings: Arc<Settings>,
    connections: Arc<Semaphore>,
}

impl Database {
    pub fn new(settings: Arc<Settings>) -> Self {
        Self {
            settings,
            connections: Arc::new(Semaphore::new(MAX_IN_FLIGHT)),
        }
    }

    pub async fn execute(&self, statement: Statement) -> Result<Outcome, DbError> {
        let permit = self
            .connections
            .clone()
            .try_acquire_owned()
            .map_err(|_| DbError::Busy)?;
        let deadline = Duration::from_secs(self.settings.timeout_seconds.into());
        let result = tokio::time::timeout(deadline, self.run(statement)).await;
        drop(permit);
        match result {
            Ok(result) => result,
            Err(_) => Err(DbError::Timeout),
        }
    }

    async fn run(&self, statement: Statement) -> Result<Outcome, DbError> {
        let mutation = statement.mutation;
        let mut client = self.connect().await?;
        let mut query = Query::new(statement.sql);
        for parameter in statement.parameters {
            query.bind(json_to_sql(parameter)?);
        }
        let mut stream = query
            .query(&mut client)
            .await
            .map_err(|_| DbError::Failed)?;
        if mutation {
            read_mutation(&mut stream).await
        } else {
            read_rows(&mut stream).await
        }
    }

    async fn connect(&self) -> Result<Connection, DbError> {
        let mut config = Config::new();
        config.host(self.settings.host.as_str());
        config.port(self.settings.port);
        config.database(self.settings.database.as_str());
        config.encryption(EncryptionLevel::Required);
        if self.settings.trust_server_certificate {
            config.trust_cert();
        }
        config.authentication(auth_method(&self.settings.auth)?);

        let stream = TcpStream::connect((self.settings.host.as_str(), self.settings.port))
            .await
            .map_err(|_| DbError::Connect)?;
        stream.set_nodelay(true).map_err(|_| DbError::Connect)?;
        Client::connect(config, stream.compat_write())
            .await
            .map_err(|_| DbError::Connect)
    }
}

fn auth_method(auth: &Auth) -> Result<AuthMethod, DbError> {
    match auth {
        Auth::Sql { user, password } => Ok(AuthMethod::sql_server(user, password)),
        Auth::Windows => integrated_auth(),
        Auth::Ntlm {
            domain,
            user,
            password,
        } => ntlm_auth(domain, user, password),
    }
}

#[cfg(windows)]
fn integrated_auth() -> Result<AuthMethod, DbError> {
    Ok(AuthMethod::Integrated)
}

#[cfg(not(windows))]
fn integrated_auth() -> Result<AuthMethod, DbError> {
    Err(DbError::UnsupportedAuth)
}

#[cfg(any(unix, windows))]
fn ntlm_auth(domain: &str, user: &str, password: &str) -> Result<AuthMethod, DbError> {
    Ok(AuthMethod::windows(format!("{domain}\\{user}"), password))
}

#[cfg(not(any(unix, windows)))]
fn ntlm_auth(_domain: &str, _user: &str, _password: &str) -> Result<AuthMethod, DbError> {
    Err(DbError::UnsupportedAuth)
}

fn json_to_sql(value: Value) -> Result<ColumnData<'static>, DbError> {
    match value {
        Value::Null => Ok(ColumnData::String(None)),
        Value::Bool(value) => Ok(ColumnData::Bit(Some(value))),
        Value::Number(number) => {
            if let Some(value) = number.as_i64() {
                Ok(ColumnData::I64(Some(value)))
            } else if let Some(value) = number.as_f64() {
                Ok(ColumnData::F64(Some(value)))
            } else {
                Err(DbError::Failed)
            }
        }
        Value::String(value) => Ok(ColumnData::String(Some(Cow::Owned(value)))),
        Value::Array(_) | Value::Object(_) => Err(DbError::Failed),
    }
}

async fn read_rows(stream: &mut QueryStream<'_>) -> Result<Outcome, DbError> {
    let mut columns: Option<Vec<String>> = None;
    let mut rows: Vec<Vec<Option<String>>> = Vec::new();
    let mut truncated = false;

    while let Some(item) = stream.try_next().await.map_err(|_| DbError::Failed)? {
        match item {
            QueryItem::Metadata(metadata) => {
                if columns.is_some() {
                    return Err(DbError::UnexpectedResult);
                }
                let names = metadata
                    .columns()
                    .iter()
                    .map(|column| column.name().to_owned())
                    .collect::<Vec<_>>();
                if names.is_empty() || names.len() > MAX_COLUMNS {
                    return Err(DbError::UnexpectedResult);
                }
                columns = Some(names);
            }
            QueryItem::Row(row) => {
                let Some(names) = columns.as_ref() else {
                    return Err(DbError::UnexpectedResult);
                };
                if row.len() != names.len() {
                    return Err(DbError::UnexpectedResult);
                }
                if rows.len() == MAX_ROWS {
                    truncated = true;
                    break;
                }
                let mut values = Vec::with_capacity(names.len());
                for (index, _) in names.iter().enumerate() {
                    let data = row
                        .get_column_data(index)
                        .map_err(|_| DbError::UnexpectedResult)?;
                    let (text, cell_truncated) = cell_text(data)?;
                    truncated |= cell_truncated;
                    values.push(text);
                }
                rows.push(values);
            }
        }
    }

    let Some(columns) = columns else {
        return Err(DbError::UnexpectedResult);
    };
    Ok(Outcome::Read(ReadOutput {
        columns,
        rows,
        truncated,
    }))
}

async fn read_mutation(stream: &mut QueryStream<'_>) -> Result<Outcome, DbError> {
    let mut current_is_status = false;
    let mut status_rows: Option<Vec<Row>> = None;
    let mut status_sets = 0usize;

    while let Some(item) = stream.try_next().await.map_err(|_| DbError::Failed)? {
        match item {
            QueryItem::Metadata(metadata) => {
                current_is_status = is_status_columns(metadata.columns());
                if current_is_status {
                    status_sets += 1;
                    if status_sets > 1 {
                        return Err(DbError::UnexpectedResult);
                    }
                    status_rows = Some(Vec::new());
                } else {
                    status_rows = None;
                }
            }
            QueryItem::Row(row) => {
                if current_is_status
                    && let Some(rows) = status_rows.as_mut()
                    && rows.len() < 2
                {
                    rows.push(row);
                }
            }
        }
    }

    if status_sets != 1 {
        return Err(DbError::UnexpectedResult);
    }
    let Some(rows) = status_rows else {
        return Err(DbError::UnexpectedResult);
    };
    if rows.len() != 1 {
        return Err(DbError::UnexpectedResult);
    }
    let row = &rows[0];
    if row.len() != 1 {
        return Err(DbError::UnexpectedResult);
    }
    let data = row
        .get_column_data(0)
        .map_err(|_| DbError::UnexpectedResult)?;
    let ColumnData::String(Some(value)) = data else {
        return Err(DbError::UnexpectedResult);
    };
    let affected = value
        .parse::<i64>()
        .map_err(|_| DbError::UnexpectedResult)?;
    if !(-1..=MAX_ROWS as i64).contains(&affected) {
        return Err(DbError::UnexpectedResult);
    }
    Ok(Outcome::Mutation(affected))
}

fn is_status_columns(columns: &[Column]) -> bool {
    columns.len() == 1 && columns[0].name() == MUTATION_COLUMN
}

fn cell_text(data: &ColumnData<'_>) -> Result<(Option<String>, bool), DbError> {
    let ColumnData::String(value) = data else {
        return Err(DbError::UnexpectedResult);
    };
    match value {
        None => Ok((None, false)),
        Some(text) if text.encode_utf16().count() > MAX_CELL_UNITS => {
            Ok((Some(truncate_cell(text)), true))
        }
        Some(text) => Ok((Some(text.to_string()), false)),
    }
}

fn truncate_cell(text: &str) -> String {
    let mut units = text.encode_utf16().take(MAX_CELL_UNITS).collect::<Vec<_>>();
    if units
        .last()
        .is_some_and(|unit| (0xD800..=0xDBFF).contains(unit))
    {
        units.pop();
    }
    String::from_utf16_lossy(&units)
}
