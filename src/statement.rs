use std::collections::{BTreeMap, BTreeSet};

use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

const DEFAULT_LIMIT: u16 = 25;
pub(crate) const MAX_ROWS: u16 = 100;
const MAX_ENTRIES: usize = 16;
pub(crate) const MAX_SELECT_COLUMNS: usize = 64;
pub(crate) const DEFAULT_RESULT_COLUMNS: usize = 16;
const PAGE_SIZE: u16 = 100;
const MAX_TEXT_BYTES: usize = 16 * 1024;
const MAX_TEXT_UNITS: usize = 1025;
const IDENTIFIER_ERROR: &str =
    "identifier must contain 1–128 UTF-16 units and no control characters";

/// How the database layer decodes the batch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Execution {
    /// At most one result set, bounded like a read.
    Rows,
    /// The generated mutation status shape with `affected_rows`.
    Status,
    /// One arbitrary batch: zero result sets or one bounded result set.
    Batch,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Statement {
    pub sql: String,
    pub parameters: Vec<Value>,
    pub mutation: bool,
    pub(crate) execution: Execution,
    pub(crate) limit: usize,
    pub(crate) max_columns: usize,
}

impl Statement {
    pub fn read(sql: String, parameters: Vec<Value>, limit: usize) -> Self {
        Self {
            sql,
            parameters,
            mutation: false,
            execution: Execution::Rows,
            limit: limit.clamp(1, usize::from(MAX_ROWS)),
            max_columns: DEFAULT_RESULT_COLUMNS,
        }
    }

    pub fn status(sql: String, parameters: Vec<Value>) -> Self {
        Self {
            sql,
            parameters,
            mutation: true,
            execution: Execution::Status,
            limit: usize::from(MAX_ROWS),
            max_columns: DEFAULT_RESULT_COLUMNS,
        }
    }

    /// One raw batch with no generated wrapper and no affected-row guard.
    pub fn batch(sql: String, limit: usize) -> Self {
        Self {
            sql,
            parameters: Vec::new(),
            mutation: true,
            execution: Execution::Batch,
            limit: limit.clamp(1, usize::from(MAX_ROWS)),
            max_columns: DEFAULT_RESULT_COLUMNS,
        }
    }
}

/// Table or view addressed by schema-qualified name.
#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Table {
    /// Object name without a schema prefix.
    pub name: String,
    /// Owning schema; defaults to dbo.
    pub schema: Option<String>,
}

struct Target {
    schema: String,
    name: String,
}

impl Table {
    fn resolve(&self) -> Result<Target, &'static str> {
        let schema = self.schema.as_deref().unwrap_or("dbo");
        validate_identifier(schema)?;
        validate_identifier(&self.name)?;
        Ok(Target {
            schema: schema.to_owned(),
            name: self.name.clone(),
        })
    }

    /// Resolve base types using the database's identifier collation. An empty
    /// list selects the first columns in catalog order for a bounded sample.
    pub(crate) fn column_types(&self, columns: &[String]) -> Result<Statement, &'static str> {
        let target = self.resolve()?;
        let mut binder = Binder::default();
        let schema = binder.bind(Value::String(target.schema))?;
        let name = binder.bind(Value::String(target.name))?;
        let (selected, join, order) = if columns.is_empty() {
            ("c.name", String::new(), "c.column_id")
        } else {
            let mut values = Vec::with_capacity(columns.len());
            for (index, column) in columns.iter().enumerate() {
                let parameter = binder.bind(Value::String(column.clone()))?;
                values.push(format!("({parameter}, {index})"));
            }
            (
                "requested.name",
                format!(
                    " JOIN (VALUES {}) AS requested(name, ordinal) ON requested.name = c.name",
                    values.join(", ")
                ),
                "requested.ordinal",
            )
        };
        let sql = format!(
            "SELECT TOP ({}) CONVERT(nvarchar(128), {selected}) AS [name], CONVERT(nvarchar(128), COALESCE(TYPE_NAME(c.system_type_id), TYPE_NAME(c.user_type_id))) AS [base_type] \
             FROM sys.columns AS c JOIN sys.objects AS o ON o.object_id = c.object_id \
             JOIN sys.schemas AS s ON s.schema_id = o.schema_id{join} \
             WHERE o.type IN (N'U', N'V') AND s.name = {schema} AND o.name = {name} ORDER BY {order}",
            MAX_SELECT_COLUMNS + 1,
        );
        Ok(Statement::read(sql, binder.parameters, MAX_SELECT_COLUMNS))
    }
}

impl Target {
    fn sql(&self) -> String {
        format!("{}.{}", quote(&self.schema), quote(&self.name))
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SelectRows {
    /// Table or view to read.
    pub table: Table,
    /// Columns to project, 1–64 unique names; every value returns as bounded text.
    #[schemars(length(min = 1, max = 64))]
    pub columns: Vec<String>,
    /// Equality filters combined with AND; null matches SQL NULL.
    #[serde(default)]
    #[schemars(length(max = 16))]
    pub filters: BTreeMap<String, Value>,
    /// Ascending sort columns, at most 16; required when offset is set.
    #[serde(default)]
    #[schemars(length(max = 16))]
    pub order_by: Vec<String>,
    /// Rows to skip; requires order_by.
    #[serde(default)]
    pub offset: u32,
    /// Maximum rows to return, 1–100; defaults to 25.
    #[schemars(range(min = 1, max = 100))]
    pub limit: Option<u16>,
}

impl SelectRows {
    pub fn prepare(&self) -> Result<Statement, &'static str> {
        self.prepare_with_types(&BTreeMap::new())
    }

    pub(crate) fn prepare_with_types(
        &self,
        types: &BTreeMap<String, String>,
    ) -> Result<Statement, &'static str> {
        if self.columns.is_empty() || self.columns.len() > MAX_SELECT_COLUMNS {
            return Err("columns must contain between 1 and 64 names");
        }
        let mut seen = BTreeSet::new();
        for column in &self.columns {
            validate_identifier(column).map_err(|_| {
                "columns must contain valid identifiers (1–128 UTF-16 units, no control characters)"
            })?;
            if !seen.insert(column.as_str()) {
                return Err("columns must not repeat a name");
            }
        }
        let limit = self.limit.unwrap_or(DEFAULT_LIMIT);
        if !(1..=MAX_ROWS).contains(&limit) {
            return Err("limit must be between 1 and 100");
        }
        if self.offset > 0 && self.order_by.is_empty() {
            return Err("offset requires order_by");
        }
        if self.order_by.len() > MAX_ENTRIES {
            return Err("order_by must contain at most 16 names");
        }
        for column in &self.order_by {
            validate_identifier(column).map_err(|_| "order_by must contain valid identifiers")?;
        }
        if self.filters.len() > MAX_ENTRIES {
            return Err("filters must contain at most 16 entries");
        }
        validate_identifier(self.table.schema.as_deref().unwrap_or("dbo"))
            .map_err(|_| "table.schema must be a valid identifier")?;
        validate_identifier(&self.table.name)
            .map_err(|_| "table.name must be a valid identifier")?;
        let target = self.table.resolve()?;
        let projection = self
            .columns
            .iter()
            .map(|column| projected(column, types.get(column).map(String::as_str)))
            .collect::<Vec<_>>()
            .join(", ");
        let mut binder = Binder::default();
        // One lookahead row tells the decoder that another page exists.
        let fetch = Value::from(limit + 1);
        let top = if self.order_by.is_empty() {
            format!("TOP ({}) ", binder.bind(fetch.clone())?)
        } else {
            String::new()
        };
        let mut sql = format!("SELECT {top}{projection} FROM {} AS [t]", target.sql());
        sql.push_str(
            &where_clause(&self.filters, &mut binder).map_err(|error| match error {
                INTEGER_RANGE_ERROR => {
                    "filters: integer exceeds signed 64-bit range; pass it as a string"
                }
                IDENTIFIER_ERROR => "filters must use valid column identifiers",
                _ => "filters must use scalar values (text up to 16384 bytes)",
            })?,
        );
        if !self.order_by.is_empty() {
            let order = self
                .order_by
                .iter()
                .map(|column| format!("[t].{}", quote(column)))
                .collect::<Vec<_>>()
                .join(", ");
            let offset = binder.bind(Value::from(self.offset))?;
            let fetch = binder.bind(fetch)?;
            sql.push_str(&format!(
                " ORDER BY {order} OFFSET {offset} ROWS FETCH NEXT {fetch} ROWS ONLY"
            ));
        }
        let mut statement = Statement::read(sql, binder.parameters, usize::from(limit));
        statement.max_columns = MAX_SELECT_COLUMNS;
        Ok(statement)
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SampleRows {
    /// Table or view to sample.
    pub table: Table,
    /// Maximum rows to sample, 1–25; defaults to 5.
    #[schemars(range(min = 1, max = 25))]
    pub limit: Option<u16>,
}

impl SampleRows {
    pub(crate) fn validate(&self) -> Result<u16, &'static str> {
        validate_identifier(self.table.schema.as_deref().unwrap_or("dbo"))
            .map_err(|_| "table.schema must be a valid identifier")?;
        validate_identifier(&self.table.name)
            .map_err(|_| "table.name must be a valid identifier")?;
        let limit = self.limit.unwrap_or(5);
        if !(1..=25).contains(&limit) {
            return Err("limit must be between 1 and 25");
        }
        Ok(limit)
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ListTables {
    /// Restrict the listing to this schema.
    pub schema: Option<String>,
    /// Records to skip.
    #[serde(default)]
    pub offset: u32,
}

const LIST_TABLE_COLUMNS: [(&str, &str); 4] = [
    ("schema", "CONVERT(nvarchar(128), s.name)"),
    ("name", "CONVERT(nvarchar(128), o.name)"),
    ("type", "CONVERT(nvarchar(60), o.type_desc)"),
    ("object_id", "CONVERT(nvarchar(20), o.object_id)"),
];

impl ListTables {
    pub fn prepare(&self) -> Result<Statement, &'static str> {
        let mut binder = Binder::default();
        let offset = binder.bind(Value::from(self.offset))?;
        let schema = match &self.schema {
            Some(schema) => {
                validate_identifier(schema)?;
                let placeholder = binder.bind(Value::String(schema.clone()))?;
                format!(" AND s.name = {placeholder}")
            }
            None => String::new(),
        };
        let sql = format!(
            "SELECT {} FROM sys.objects AS o JOIN sys.schemas AS s ON s.schema_id = o.schema_id WHERE o.is_ms_shipped = 0 AND o.type IN (N'U', N'V'){schema} ORDER BY s.name, o.name OFFSET {offset} ROWS FETCH NEXT {PAGE_SIZE} ROWS ONLY",
            projection(&LIST_TABLE_COLUMNS),
        );
        Ok(Statement::read(
            sql,
            binder.parameters,
            usize::from(MAX_ROWS),
        ))
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DescribeTable {
    /// Table or view whose columns are described.
    pub table: Table,
    /// Records to skip.
    #[serde(default)]
    pub offset: u32,
}

const DESCRIBE_COLUMNS: [(&str, &str); 8] = [
    ("name", "CONVERT(nvarchar(128), c.name)"),
    (
        "type",
        "CONVERT(nvarchar(128), COALESCE(TYPE_NAME(c.user_type_id), TYPE_NAME(c.system_type_id)))",
    ),
    (
        "max_length",
        "CONVERT(nvarchar(20), CASE WHEN c.system_type_id IN (231, 239) AND c.max_length >= 0 THEN c.max_length / 2 ELSE c.max_length END)",
    ),
    ("precision", "CONVERT(nvarchar(20), c.precision)"),
    ("scale", "CONVERT(nvarchar(20), c.scale)"),
    ("is_nullable", "CONVERT(nvarchar(5), c.is_nullable)"),
    ("ordinal", "CONVERT(nvarchar(20), c.column_id)"),
    (
        "base_type",
        "CONVERT(nvarchar(128), TYPE_NAME(c.system_type_id))",
    ),
];

impl DescribeTable {
    pub fn prepare(&self) -> Result<Statement, &'static str> {
        let target = self.table.resolve()?;
        let mut binder = Binder::default();
        let schema = binder.bind(Value::String(target.schema))?;
        let name = binder.bind(Value::String(target.name))?;
        let offset = binder.bind(Value::from(self.offset))?;
        let sql = format!(
            "SELECT {} FROM sys.columns AS c JOIN sys.objects AS o ON o.object_id = c.object_id JOIN sys.schemas AS s ON s.schema_id = o.schema_id WHERE o.is_ms_shipped = 0 AND o.type IN (N'U', N'V') AND s.name = {schema} AND o.name = {name} ORDER BY c.column_id OFFSET {offset} ROWS FETCH NEXT {PAGE_SIZE} ROWS ONLY",
            projection(&DESCRIBE_COLUMNS),
        );
        Ok(Statement::read(
            sql,
            binder.parameters,
            usize::from(MAX_ROWS),
        ))
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct InsertRow {
    /// Table or view that receives the row.
    pub table: Table,
    /// Column values to insert, 1–16 entries. SQL Server can implicitly convert numbers to text; use JSON strings to specify the exact text.
    #[schemars(length(max = 16))]
    pub values: BTreeMap<String, Value>,
}

impl InsertRow {
    pub fn prepare(&self) -> Result<Statement, &'static str> {
        let target = self.table.resolve()?;
        validate_values(&self.values)?;
        let mut binder = Binder::default();
        let mut columns = Vec::with_capacity(self.values.len());
        let mut placeholders = Vec::with_capacity(self.values.len());
        for (column, value) in &self.values {
            validate_identifier(column)?;
            columns.push(quote(column));
            placeholders.push(binder.bind(value.clone())?);
        }
        let body = format!(
            "INSERT INTO {} ({}) VALUES ({})",
            target.sql(),
            columns.join(", "),
            placeholders.join(", ")
        );
        Ok(mutation_statement(body, &binder))
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct UpdateRows {
    /// Table or view whose rows are updated.
    pub table: Table,
    /// Column values to set, 1–16 entries.
    pub values: BTreeMap<String, Value>,
    /// Equality filters combined with AND; at least one is required.
    #[serde(default)]
    pub filters: BTreeMap<String, Value>,
}

impl UpdateRows {
    pub fn prepare(&self) -> Result<Statement, &'static str> {
        let target = self.table.resolve()?;
        validate_values(&self.values)?;
        validate_mutation_filters(&self.filters)?;
        let mut binder = Binder::default();
        let mut assignments = Vec::with_capacity(self.values.len());
        for (column, value) in &self.values {
            validate_identifier(column)?;
            let placeholder = binder.bind(value.clone())?;
            assignments.push(format!("{} = {placeholder}", quote(column)));
        }
        let filters = where_clause(&self.filters, &mut binder)?;
        let body = format!(
            "UPDATE TOP ({}) {} SET {}{filters}",
            MAX_ROWS + 1,
            target.sql(),
            assignments.join(", "),
        );
        Ok(mutation_statement(body, &binder))
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DeleteRows {
    /// Table or view whose rows are deleted.
    pub table: Table,
    /// Equality filters combined with AND; at least one is required.
    #[serde(default)]
    pub filters: BTreeMap<String, Value>,
}

impl DeleteRows {
    pub fn prepare(&self) -> Result<Statement, &'static str> {
        let target = self.table.resolve()?;
        validate_mutation_filters(&self.filters)?;
        let mut binder = Binder::default();
        let filters = where_clause(&self.filters, &mut binder)?;
        let body = format!(
            "DELETE TOP ({}) FROM {}{filters}",
            MAX_ROWS + 1,
            target.sql()
        );
        Ok(mutation_statement(body, &binder))
    }
}

fn mutation_statement(body: String, binder: &Binder) -> Statement {
    let guard = format!(
        "DECLARE @mcp_affected int = @@ROWCOUNT;\nIF @mcp_affected > {MAX_ROWS}\n    ROLLBACK TRANSACTION\nELSE\n    COMMIT TRANSACTION;"
    );
    let affected = format!(
        "SELECT CONVERT(nvarchar(20), CASE WHEN @mcp_affected > {MAX_ROWS} THEN -1 ELSE @mcp_affected END) AS [affected_rows];"
    );
    let sql = format!(
        "SET NOCOUNT ON; SET XACT_ABORT ON; BEGIN TRANSACTION;\n{body};\n{guard}\n{affected}"
    );
    Statement::status(sql, binder.parameters.clone())
}

#[derive(Default)]
pub(crate) struct Binder {
    pub(crate) parameters: Vec<Value>,
}

impl Binder {
    pub(crate) fn bind(&mut self, value: Value) -> Result<String, &'static str> {
        validate_value(&value)?;
        self.parameters.push(value);
        Ok(format!("@P{}", self.parameters.len()))
    }
}

const INTEGER_RANGE_ERROR: &str = "integer exceeds signed 64-bit range; pass it as a string";

fn validate_value(value: &Value) -> Result<(), &'static str> {
    match value {
        Value::Null | Value::Bool(_) => Ok(()),
        Value::Number(number) if number.is_i64() || number.is_f64() => Ok(()),
        Value::Number(_) => Err(INTEGER_RANGE_ERROR),
        Value::String(text) if text.len() <= MAX_TEXT_BYTES => Ok(()),
        Value::String(_) => Err("string value must be at most 16384 bytes"),
        Value::Array(_) | Value::Object(_) => {
            Err("compound values are not supported; pass a scalar")
        }
    }
}

pub(crate) fn validate_identifier(value: &str) -> Result<(), &'static str> {
    if value.is_empty() || value.encode_utf16().count() > 128 || value.chars().any(char::is_control)
    {
        return Err(IDENTIFIER_ERROR);
    }
    Ok(())
}

fn validate_values(values: &BTreeMap<String, Value>) -> Result<(), &'static str> {
    if values.is_empty() || values.len() > MAX_ENTRIES {
        return Err("values must contain between 1 and 16 entries");
    }
    Ok(())
}

fn validate_mutation_filters(filters: &BTreeMap<String, Value>) -> Result<(), &'static str> {
    if filters.is_empty() {
        return Err("mutation requires at least one filter");
    }
    if filters.len() > MAX_ENTRIES {
        return Err("filters must contain at most 16 entries");
    }
    Ok(())
}

fn where_clause(
    filters: &BTreeMap<String, Value>,
    binder: &mut Binder,
) -> Result<String, &'static str> {
    if filters.is_empty() {
        return Ok(String::new());
    }
    let mut predicates = Vec::with_capacity(filters.len());
    for (column, value) in filters {
        validate_identifier(column)?;
        let column = quote(column);
        if value.is_null() {
            predicates.push(format!("{column} IS NULL"));
        } else {
            let placeholder = binder.bind(value.clone())?;
            predicates.push(format!("{column} = {placeholder}"));
        }
    }
    Ok(format!(" WHERE {}", predicates.join(" AND ")))
}

fn projected(column: &str, base_type: Option<&str>) -> String {
    let quoted = quote(column);
    let conversion = match base_type {
        Some("binary" | "varbinary" | "image" | "timestamp" | "rowversion") => {
            format!("CONVERT(varchar(max), {quoted}, 2)")
        }
        Some("xml") => format!("CONVERT(nvarchar(max), {quoted})"),
        _ => format!("CONVERT(nvarchar(max), {quoted}, 126)"),
    };
    format!("LEFT({conversion}, {MAX_TEXT_UNITS}) AS {quoted}")
}

fn projection(columns: &[(&str, &str)]) -> String {
    columns
        .iter()
        .map(|(alias, expression)| format!("{expression} AS {}", quote(alias)))
        .collect::<Vec<_>>()
        .join(", ")
}

fn quote(identifier: &str) -> String {
    format!("[{}]", identifier.replace(']', "]]"))
}
