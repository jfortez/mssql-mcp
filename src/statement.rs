use std::collections::{BTreeMap, BTreeSet};

use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

const DEFAULT_LIMIT: u16 = 25;
const MAX_ROWS: u16 = 100;
const MAX_ENTRIES: usize = 16;
const PAGE_SIZE: u16 = 100;
const MAX_TEXT_BYTES: usize = 16 * 1024;
const MAX_TEXT_UNITS: usize = 1025;
const IDENTIFIER_ERROR: &str =
    "identifier must contain 1–128 UTF-16 units and no control characters";

#[derive(Debug, Clone, PartialEq)]
pub struct Statement {
    pub sql: String,
    pub parameters: Vec<Value>,
    pub mutation: bool,
}

/// Table or view addressed by schema-qualified name.
#[derive(Debug, Deserialize, JsonSchema)]
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
    /// Columns to project, 1–16 unique names; every value returns as bounded text.
    pub columns: Vec<String>,
    /// Equality filters combined with AND; null matches SQL NULL.
    #[serde(default)]
    pub filters: BTreeMap<String, Value>,
    /// Ascending sort columns, at most 16; required when offset is set.
    #[serde(default)]
    pub order_by: Vec<String>,
    /// Rows to skip; requires order_by.
    #[serde(default)]
    pub offset: u32,
    /// Maximum rows to return, 1–100; defaults to 25.
    pub limit: Option<u16>,
}

impl SelectRows {
    pub fn prepare(&self) -> Result<Statement, &'static str> {
        if self.columns.is_empty() || self.columns.len() > MAX_ENTRIES {
            return Err("columns must contain between 1 and 16 names");
        }
        let mut seen = BTreeSet::new();
        for column in &self.columns {
            validate_identifier(column)?;
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
            validate_identifier(column)?;
        }
        if self.filters.len() > MAX_ENTRIES {
            return Err("filters must contain at most 16 entries");
        }
        let target = self.table.resolve()?;
        let projection = self
            .columns
            .iter()
            .map(|column| projected(column))
            .collect::<Vec<_>>()
            .join(", ");
        let mut binder = Binder::default();
        let top = if self.order_by.is_empty() {
            format!("TOP ({}) ", binder.bind(Value::from(limit))?)
        } else {
            String::new()
        };
        let mut sql = format!("SELECT {top}{projection} FROM {} AS [t]", target.sql());
        sql.push_str(&where_clause(&self.filters, &mut binder)?);
        if !self.order_by.is_empty() {
            let order = self
                .order_by
                .iter()
                .map(|column| format!("[t].{}", quote(column)))
                .collect::<Vec<_>>()
                .join(", ");
            let offset = binder.bind(Value::from(self.offset))?;
            let limit = binder.bind(Value::from(limit))?;
            sql.push_str(&format!(
                " ORDER BY {order} OFFSET {offset} ROWS FETCH NEXT {limit} ROWS ONLY"
            ));
        }
        Ok(Statement {
            sql,
            parameters: binder.parameters,
            mutation: false,
        })
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
        Ok(Statement {
            sql,
            parameters: binder.parameters,
            mutation: false,
        })
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

const DESCRIBE_COLUMNS: [(&str, &str); 7] = [
    ("name", "CONVERT(nvarchar(128), c.name)"),
    ("type", "CONVERT(nvarchar(128), TYPE_NAME(c.user_type_id))"),
    ("max_length", "CONVERT(nvarchar(20), c.max_length)"),
    ("precision", "CONVERT(nvarchar(20), c.precision)"),
    ("scale", "CONVERT(nvarchar(20), c.scale)"),
    ("is_nullable", "CONVERT(nvarchar(5), c.is_nullable)"),
    ("ordinal", "CONVERT(nvarchar(20), c.column_id)"),
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
        Ok(Statement {
            sql,
            parameters: binder.parameters,
            mutation: false,
        })
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct InsertRow {
    /// Table or view that receives the row.
    pub table: Table,
    /// Column values to insert, 1–16 entries.
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
    Statement {
        sql,
        parameters: binder.parameters.clone(),
        mutation: true,
    }
}

#[derive(Default)]
struct Binder {
    parameters: Vec<Value>,
}

impl Binder {
    fn bind(&mut self, value: Value) -> Result<String, &'static str> {
        validate_value(&value)?;
        self.parameters.push(value);
        Ok(format!("@P{}", self.parameters.len()))
    }
}

fn validate_value(value: &Value) -> Result<(), &'static str> {
    match value {
        Value::Null | Value::Bool(_) => Ok(()),
        Value::Number(number) if number.is_i64() || number.is_f64() => Ok(()),
        Value::Number(_) => Err("integer exceeds signed 64-bit range; pass it as a string"),
        Value::String(text) if text.len() <= MAX_TEXT_BYTES => Ok(()),
        Value::String(_) => Err("string value must be at most 16384 bytes"),
        Value::Array(_) | Value::Object(_) => {
            Err("compound values are not supported; pass a scalar")
        }
    }
}

fn validate_identifier(value: &str) -> Result<(), &'static str> {
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

fn projected(column: &str) -> String {
    let quoted = quote(column);
    format!("LEFT(CONVERT(nvarchar(max), {quoted}, 126), {MAX_TEXT_UNITS}) AS {quoted}")
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
