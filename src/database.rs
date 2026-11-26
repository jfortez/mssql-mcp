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
use crate::statement::{Execution, Statement};

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
    /// One raw batch with no result set, so no rows or affected count is known.
    Executed,
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
        let mut client = self.connect().await?;
        let mut stream = if statement.execution == Execution::Batch {
            client
                .simple_query(statement.sql)
                .await
                .map_err(|_| DbError::Failed)?
        } else {
            let mut query = Query::new(statement.sql);
            for parameter in statement.parameters {
                query.bind(json_to_sql(parameter)?);
            }
            query
                .query(&mut client)
                .await
                .map_err(|_| DbError::Failed)?
        };
        match statement.execution {
            Execution::Rows => read_rows(&mut stream, statement.limit).await,
            Execution::Status => read_mutation(&mut stream).await,
            Execution::Batch => read_batch(&mut stream, statement.limit).await,
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

async fn read_rows(stream: &mut QueryStream<'_>, limit: usize) -> Result<Outcome, DbError> {
    let mut rows = BoundedRows::new(limit);

    while let Some(item) = stream.try_next().await.map_err(|_| DbError::Failed)? {
        match item {
            QueryItem::Metadata(metadata) => {
                rows.metadata(column_names(metadata.columns()))?;
            }
            QueryItem::Row(row) => {
                let width = rows.width().ok_or(DbError::UnexpectedResult)?;
                if row.len() != width {
                    return Err(DbError::UnexpectedResult);
                }
                if rows.push(read_cells(&row, width)?)? {
                    break;
                }
            }
        }
    }

    rows.finish().map(Outcome::Read)
}

/// One arbitrary batch: no result set reports successful execution, exactly one
/// bounded result set returns rows, and more than one is ambiguous and rejected.
async fn read_batch(stream: &mut QueryStream<'_>, limit: usize) -> Result<Outcome, DbError> {
    let mut rows = BoundedRows::new(limit);
    let mut result_sets = 0usize;
    let mut full = false;

    while let Some(item) = stream.try_next().await.map_err(|_| DbError::Failed)? {
        match item {
            QueryItem::Metadata(metadata) => {
                result_sets += 1;
                rows.metadata(column_names(metadata.columns()))?;
            }
            QueryItem::Row(row) => {
                if result_sets == 0 {
                    return Err(DbError::UnexpectedResult);
                }
                if full {
                    continue;
                }
                let width = rows.width().ok_or(DbError::UnexpectedResult)?;
                if row.len() != width {
                    return Err(DbError::UnexpectedResult);
                }
                full = rows.push(read_cells(&row, width)?)?;
            }
        }
    }

    if result_sets == 0 {
        return Ok(Outcome::Executed);
    }
    rows.finish().map(Outcome::Read)
}

/// Bounded result state shared by `select_rows` and the raw `query` tool.
struct BoundedRows {
    columns: Option<Vec<String>>,
    rows: Vec<Vec<Option<String>>>,
    truncated: bool,
    limit: usize,
}

impl BoundedRows {
    fn new(limit: usize) -> Self {
        Self {
            columns: None,
            rows: Vec::new(),
            truncated: false,
            limit: limit.clamp(1, MAX_ROWS),
        }
    }

    fn metadata(&mut self, names: Vec<String>) -> Result<(), DbError> {
        if self.columns.is_some() || names.is_empty() || names.len() > MAX_COLUMNS {
            return Err(DbError::UnexpectedResult);
        }
        self.columns = Some(names);
        Ok(())
    }

    fn width(&self) -> Option<usize> {
        self.columns.as_ref().map(Vec::len)
    }

    /// Store one row, or report that the row cap was reached and reading should
    /// stop storing. The caller keeps draining the stream so an extra result
    /// set is still detected.
    fn push(&mut self, cells: Vec<(Option<String>, bool)>) -> Result<bool, DbError> {
        let width = self.width().ok_or(DbError::UnexpectedResult)?;
        if cells.len() != width {
            return Err(DbError::UnexpectedResult);
        }
        if self.rows.len() == self.limit {
            self.truncated = true;
            return Ok(true);
        }
        for (_, cell_truncated) in &cells {
            self.truncated |= *cell_truncated;
        }
        self.rows
            .push(cells.into_iter().map(|(text, _)| text).collect());
        Ok(false)
    }

    fn finish(self) -> Result<ReadOutput, DbError> {
        let Some(columns) = self.columns else {
            return Err(DbError::UnexpectedResult);
        };
        Ok(ReadOutput {
            columns,
            rows: self.rows,
            truncated: self.truncated,
        })
    }
}

fn column_names(columns: &[Column]) -> Vec<String> {
    columns
        .iter()
        .map(|column| column.name().to_owned())
        .collect()
}

fn read_cells(row: &Row, width: usize) -> Result<Vec<(Option<String>, bool)>, DbError> {
    let mut cells = Vec::with_capacity(width);
    for index in 0..width {
        let data = row
            .get_column_data(index)
            .map_err(|_| DbError::UnexpectedResult)?;
        cells.push(cell_text(data));
    }
    Ok(cells)
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

/// Render one driver value as bounded text. `select_rows` sends strings, while
/// the raw `query` tool receives whatever types the batch returns, so every
/// driver variant is rendered rather than rejected.
fn cell_text(data: &ColumnData<'_>) -> (Option<String>, bool) {
    match data {
        ColumnData::U8(value) => scalar(*value),
        ColumnData::I16(value) => scalar(*value),
        ColumnData::I32(value) => scalar(*value),
        ColumnData::I64(value) => scalar(*value),
        ColumnData::F32(value) => scalar(*value),
        ColumnData::F64(value) => scalar(*value),
        ColumnData::Bit(value) => bounded(
            value.map(|value| if value { "1" } else { "0" }.to_owned()),
            false,
        ),
        ColumnData::String(value) => bounded(value.as_ref().map(|text| text.to_string()), false),
        ColumnData::Guid(value) => scalar(*value),
        ColumnData::Binary(value) => match value {
            None => (None, false),
            Some(bytes) => binary_text(bytes),
        },
        ColumnData::Numeric(value) => scalar(*value),
        ColumnData::Xml(value) => bounded(value.as_ref().map(|xml| xml.to_string()), false),
        ColumnData::DateTime(value) => bounded(
            value.map(|value| datetime_text(i64::from(value.days()), value.seconds_fragments())),
            false,
        ),
        ColumnData::SmallDateTime(value) => bounded(
            value.map(|value| {
                datetime_text(
                    i64::from(value.days()),
                    u32::from(value.seconds_fragments()),
                )
            }),
            false,
        ),
        ColumnData::Time(value) => bounded(
            value.map(|value| time_text(value.increments(), value.scale())),
            false,
        ),
        ColumnData::Date(value) => {
            bounded(value.map(|value| date_text(i64::from(value.days()))), false)
        }
        ColumnData::DateTime2(value) => bounded(
            value.map(|value| {
                datetime2_text(
                    value.date().days(),
                    value.time().increments(),
                    value.time().scale(),
                )
            }),
            false,
        ),
        ColumnData::DateTimeOffset(value) => bounded(
            value.map(|value| {
                let offset = i32::from(value.offset());
                let sign = if offset < 0 { '-' } else { '+' };
                let minutes = offset.unsigned_abs();
                format!(
                    "{}{sign}{:02}:{:02}",
                    datetime2_text(
                        value.datetime2().date().days(),
                        value.datetime2().time().increments(),
                        value.datetime2().time().scale(),
                    ),
                    minutes / 60,
                    minutes % 60,
                )
            }),
            false,
        ),
    }
}

fn scalar<T: ToString>(value: Option<T>) -> (Option<String>, bool) {
    bounded(value.map(|value| value.to_string()), false)
}

fn bounded(value: Option<String>, truncated: bool) -> (Option<String>, bool) {
    match value {
        None => (None, false),
        Some(text) if text.encode_utf16().count() > MAX_CELL_UNITS => {
            (Some(truncate_cell(&text)), true)
        }
        Some(text) => (Some(text), truncated),
    }
}

fn binary_text(bytes: &[u8]) -> (Option<String>, bool) {
    let mut text = String::with_capacity(MAX_CELL_UNITS + 2);
    text.push_str("0x");
    let mut truncated = false;
    for byte in bytes {
        if text.len() + 2 > MAX_CELL_UNITS {
            truncated = true;
            break;
        }
        text.push_str(&format!("{byte:02X}"));
    }
    bounded(Some(text), truncated)
}

fn date_text(days: i64) -> String {
    let (year, month, day) = civil_from_days(days);
    format!("{year:04}-{month:02}-{day:02}")
}

fn datetime_text(days_since_1900: i64, fragments: u32) -> String {
    let date = date_text(days_since_1900 + 693_595);
    let seconds = fragments / 300;
    let millis = (fragments % 300) * 1000 / 300;
    format!(
        "{date} {:02}:{:02}:{:02}.{millis:03}",
        seconds / 3600,
        (seconds % 3600) / 60,
        seconds % 60,
    )
}

fn datetime2_text(days: u32, increments: u64, scale: u8) -> String {
    format!(
        "{} {}",
        date_text(i64::from(days)),
        time_text(increments, scale)
    )
}

fn time_text(increments: u64, scale: u8) -> String {
    let scale = scale.min(9);
    let divisor = 10u64.pow(u32::from(scale));
    let total_seconds = increments / divisor;
    let fraction = increments % divisor;
    let hours = total_seconds / 3600;
    let minutes = (total_seconds % 3600) / 60;
    let seconds = total_seconds % 60;
    if scale == 0 {
        return format!("{hours:02}:{minutes:02}:{seconds:02}");
    }
    let digits = usize::from(scale);
    format!("{hours:02}:{minutes:02}:{seconds:02}.{fraction:0digits$}")
}

/// Days since 1 January year 1 to a civil date.
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 306; // days since 1 March year 0
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let mut year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let day = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let month = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    if month <= 2 {
        year += 1;
    }
    (year, month, day)
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

#[cfg(test)]
mod tests {
    use super::*;

    fn cell(text: &str) -> (Option<String>, bool) {
        cell_text(&ColumnData::String(Some(Cow::Owned(text.to_owned()))))
    }

    fn one_column() -> BoundedRows {
        let mut rows = BoundedRows::new(MAX_ROWS);
        rows.metadata(vec!["id".to_owned()]).expect("one column");
        rows
    }

    #[test]
    fn seventeen_columns_are_rejected() {
        let mut rows = BoundedRows::new(MAX_ROWS);
        let names = (0..=MAX_COLUMNS).map(|index| format!("c{index}")).collect();
        assert_eq!(rows.metadata(names), Err(DbError::UnexpectedResult));
    }

    #[test]
    fn sixteen_columns_are_accepted() {
        let mut rows = BoundedRows::new(MAX_ROWS);
        let names = (0..MAX_COLUMNS).map(|index| format!("c{index}")).collect();
        rows.metadata(names).expect("sixteen columns");
        assert_eq!(rows.width(), Some(MAX_COLUMNS));
    }

    #[test]
    fn a_second_result_set_is_ambiguous() {
        let mut rows = one_column();
        assert_eq!(
            rows.metadata(vec!["id".to_owned()]),
            Err(DbError::UnexpectedResult)
        );
    }

    #[test]
    fn the_row_cap_sets_truncated() {
        let mut rows = one_column();
        for _ in 0..MAX_ROWS {
            assert_eq!(rows.push(vec![cell("1")]), Ok(false));
        }
        assert_eq!(rows.push(vec![cell("1")]), Ok(true));
        let read = rows.finish().expect("bounded read");
        assert_eq!(read.rows.len(), MAX_ROWS);
        assert!(read.truncated);
    }

    #[test]
    fn a_requested_limit_lowers_the_row_cap() {
        let mut rows = BoundedRows::new(2);
        rows.metadata(vec!["id".to_owned()]).expect("one column");
        assert_eq!(rows.push(vec![cell("1")]), Ok(false));
        assert_eq!(rows.push(vec![cell("2")]), Ok(false));
        assert_eq!(rows.push(vec![cell("3")]), Ok(true));
        let read = rows.finish().expect("bounded read");
        assert_eq!(read.rows.len(), 2);
        assert!(read.truncated);
    }

    #[test]
    fn oversized_cells_are_truncated_at_the_cell_bound() {
        let (text, truncated) = cell(&"x".repeat(MAX_CELL_UNITS + 10));
        assert!(truncated);
        let text = text.expect("string cell");
        assert_eq!(text.encode_utf16().count(), MAX_CELL_UNITS);

        let mut rows = one_column();
        assert_eq!(rows.push(vec![(Some(text), truncated)]), Ok(false));
        assert!(rows.finish().expect("bounded read").truncated);
    }

    #[test]
    fn rows_before_metadata_are_rejected() {
        let mut rows = BoundedRows::new(MAX_ROWS);
        assert_eq!(rows.push(vec![cell("1")]), Err(DbError::UnexpectedResult));
    }

    #[test]
    fn driver_types_render_as_text() {
        assert_eq!(
            cell_text(&ColumnData::I32(Some(-7))),
            (Some("-7".to_owned()), false)
        );
        assert_eq!(
            cell_text(&ColumnData::Bit(Some(true))),
            (Some("1".to_owned()), false)
        );
        assert_eq!(
            cell_text(&ColumnData::Binary(Some(Cow::Borrowed(&[0x00, 0xAB])))),
            (Some("0x00AB".to_owned()), false)
        );
        assert_eq!(
            cell_text(&ColumnData::Date(Some(tiberius::time::Date::new(0)))),
            (Some("0001-01-01".to_owned()), false)
        );
        assert_eq!(
            cell_text(&ColumnData::Time(Some(tiberius::time::Time::new(0, 3)))),
            (Some("00:00:00.000".to_owned()), false)
        );
        assert_eq!(
            cell_text(&ColumnData::Time(Some(tiberius::time::Time::new(1_234, 3)))),
            (Some("00:00:01.234".to_owned()), false)
        );
        assert_eq!(date_text(719_162), "1970-01-01");
        assert_eq!(date_text(639_905), "1753-01-01");
        assert_eq!(datetime_text(0, 0), "1900-01-01 00:00:00.000");
        assert_eq!(datetime_text(0, 12_960_000), "1900-01-01 12:00:00.000");
    }
}
