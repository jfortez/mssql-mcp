use std::collections::BTreeMap;
use std::future::Future;

use serde::Deserialize;
use serde::de::DeserializeOwned;

use super::*;
use crate::catalog::{
    CatalogError, ObjectDependencies, ObjectName, SchemaMap, SearchObjects, TableStats,
};
use crate::database::ReadOutput;
use crate::statement::SampleRows;

impl Scoped for SchemaMap {
    fn scope(&self) -> Scope<'_> {
        match (&self.table, self.schema.as_deref()) {
            (Some(table), schema) => object_scope(table, schema),
            (None, Some(schema)) => Scope::schema(schema),
            (None, None) => Scope::catalog(),
        }
    }
}

impl Scoped for ObjectDependencies {
    fn scope(&self) -> Scope<'_> {
        object_scope(&self.object, None)
    }
}

impl Scoped for TableStats {
    fn scope(&self) -> Scope<'_> {
        object_scope(&self.table, None)
    }
}

impl Scoped for SearchObjects {
    fn scope(&self) -> Scope<'_> {
        self.schema
            .as_deref()
            .map_or_else(Scope::catalog, Scope::schema)
    }
}

fn object_scope<'a>(object: &'a ObjectName, schema: Option<&'a str>) -> Scope<'a> {
    Scope::table(
        object.schema.as_deref().or(schema).unwrap_or("dbo"),
        &object.name,
    )
}

impl McpServer {
    pub(super) async fn describe_table(
        &self,
        arguments: JsonObject,
        started: Instant,
    ) -> CallToolResult {
        let tool = Known::DescribeTable;
        let name = tool.name();
        let input = match serde_json::from_value::<DescribeTable>(Value::Object(arguments)) {
            Ok(input) => input,
            Err(_) => {
                return self.failed(
                    name,
                    started,
                    None,
                    ToolError::invalid_arguments(tool),
                    None,
                );
            }
        };
        let statement = match input.prepare() {
            Ok(statement) => statement,
            Err(message) => {
                return self.failed(
                    name,
                    started,
                    None,
                    ToolError::invalid_input(tool, message),
                    None,
                );
            }
        };
        let scope = input.scope();
        let result = async {
            let read = self.read_output(statement).await?;
            // An empty non-first page is valid if the object still exists.
            if read.rows.is_empty() {
                if input.offset == 0 {
                    return Err(ToolError::not_found());
                }
                let metadata = self.read_columns(&input.table, &[]).await?;
                if metadata.rows.is_empty() {
                    return Err(ToolError::not_found());
                }
            }
            Ok(read)
        }
        .await;
        self.finish_rows(tool, started, scope, result, false)
    }

    pub(super) async fn select_rows(
        &self,
        arguments: JsonObject,
        started: Instant,
    ) -> CallToolResult {
        let tool = Known::SelectRows;
        let name = tool.name();
        let raw = Value::Object(arguments);
        let input = match SelectRows::deserialize(&raw) {
            Ok(input) => input,
            Err(_) => {
                let mut error = ToolError::invalid_arguments(tool);
                error.message = format!("invalid select_rows field: {}", select_field(&raw));
                return self.failed(name, started, None, error, None);
            }
        };
        if let Err(message) = input.prepare() {
            return self.failed(
                name,
                started,
                None,
                ToolError::invalid_input(tool, message),
                None,
            );
        }
        let result = async {
            let metadata = self.read_columns(&input.table, &input.columns).await?;
            self.projected_rows(&input, metadata).await
        }
        .await;
        self.finish_rows(tool, started, input.scope(), result, false)
    }

    pub(super) async fn sample_rows(
        &self,
        arguments: JsonObject,
        started: Instant,
    ) -> CallToolResult {
        let tool = Known::SampleRows;
        let name = tool.name();
        let input = match serde_json::from_value::<SampleRows>(Value::Object(arguments)) {
            Ok(input) => input,
            Err(_) => {
                return self.failed(
                    name,
                    started,
                    None,
                    ToolError::invalid_arguments(tool),
                    None,
                );
            }
        };
        let limit = match input.validate() {
            Ok(limit) => limit,
            Err(message) => {
                return self.failed(
                    name,
                    started,
                    None,
                    ToolError::invalid_input(tool, message),
                    None,
                );
            }
        };
        let scope = table_scope(&input.table);
        let metadata = match self.read_columns(&input.table, &[]).await {
            Ok(read) if read.rows.is_empty() => {
                return self.failed(name, started, Some(scope), ToolError::not_found(), None);
            }
            Ok(read) => read,
            Err(error) => return self.failed(name, started, Some(scope), error, None),
        };
        let omitted = metadata.truncated;
        let columns = metadata
            .rows
            .iter()
            .filter_map(|row| row.first().and_then(Option::as_ref).cloned())
            .collect();
        let sample = SelectRows {
            table: input.table.clone(),
            columns,
            filters: BTreeMap::new(),
            order_by: Vec::new(),
            offset: 0,
            limit: Some(limit),
        };
        self.finish_rows(
            tool,
            started,
            scope,
            self.projected_rows(&sample, metadata).await,
            omitted,
        )
    }

    async fn read_columns(
        &self,
        table: &Table,
        columns: &[String],
    ) -> Result<ReadOutput, ToolError> {
        let statement = table
            .column_types(columns)
            .map_err(|message| ToolError::invalid_input(Known::SelectRows, message))?;
        self.read_output(statement).await
    }

    async fn projected_rows(
        &self,
        input: &SelectRows,
        metadata: ReadOutput,
    ) -> Result<ReadOutput, ToolError> {
        let mut types = BTreeMap::new();
        for mut row in metadata.rows {
            if let [Some(name), Some(base)] = row.as_mut_slice() {
                types.insert(std::mem::take(name), std::mem::take(base));
            } else {
                return Err(ToolError::from_database(DbError::UnexpectedResult, false));
            }
        }
        let statement = input
            .prepare_with_types(&types)
            .map_err(|message| ToolError::invalid_input(Known::SelectRows, message))?;
        self.read_output(statement).await
    }

    async fn read_output(&self, statement: Statement) -> Result<ReadOutput, ToolError> {
        match self.database.execute(statement).await {
            Ok(Outcome::Read(read)) => Ok(read),
            Ok(_) => Err(ToolError::from_database(DbError::UnexpectedResult, false)),
            Err(error) => Err(ToolError::from_database(error, false)),
        }
    }

    fn finish_rows(
        &self,
        tool: Known,
        started: Instant,
        scope: Scope<'_>,
        result: Result<ReadOutput, ToolError>,
        columns_omitted: bool,
    ) -> CallToolResult {
        let read = match result {
            Ok(read) => read,
            Err(error) => return self.failed(tool.name(), started, Some(scope), error, None),
        };
        let mut payload = json!({"columns": read.columns, "rows": read.rows, "truncated": read.truncated || columns_omitted});
        if columns_omitted {
            payload["columns_omitted"] = json!(true);
        }
        self.emit_catalog_read(tool, started, scope, payload)
    }

    pub(super) async fn catalog_call<T, Plan, Page, Exec>(
        &self,
        tool: Known,
        arguments: JsonObject,
        started: Instant,
        plan: impl FnOnce(&T) -> Result<Plan, &'static str>,
        execute: impl FnOnce(Plan) -> Exec,
    ) -> CallToolResult
    where
        T: DeserializeOwned + Scoped + Send,
        Page: serde::Serialize,
        Exec: Future<Output = Result<Page, CatalogError>>,
    {
        let name = tool.name();
        let input = match serde_json::from_value(Value::Object(arguments)) {
            Ok(input) => input,
            Err(_) => {
                return self.failed(
                    name,
                    started,
                    None,
                    ToolError::invalid_arguments(tool),
                    None,
                );
            }
        };
        let plan = match plan(&input) {
            Ok(plan) => plan,
            Err(message) => {
                return self.failed(
                    name,
                    started,
                    None,
                    ToolError::invalid_input(tool, message),
                    None,
                );
            }
        };
        let scope = input.scope();
        match execute(plan).await {
            Ok(page) => match serde_json::to_value(page) {
                Ok(payload) => self.emit_catalog_read(tool, started, scope, payload),
                Err(_) => self.failed(
                    name,
                    started,
                    Some(scope),
                    ToolError::from_database(DbError::UnexpectedResult, false),
                    None,
                ),
            },
            Err(error) => {
                let error = match error {
                    CatalogError::NotFound => ToolError::not_found(),
                    CatalogError::UnexpectedResult => {
                        ToolError::from_database(DbError::UnexpectedResult, false)
                    }
                    CatalogError::Database(error) => ToolError::from_database(error, false),
                };
                self.failed(name, started, Some(scope), error, None)
            }
        }
    }

    fn emit_catalog_read(
        &self,
        tool: Known,
        started: Instant,
        scope: Scope<'_>,
        payload: Value,
    ) -> CallToolResult {
        let rows = ["rows", "entries", "hits", "edges", "indexes"]
            .iter()
            .find_map(|field| payload[*field].as_array().map(Vec::len));
        self.record(
            Some(tool.name()),
            started,
            Status::Ok,
            None,
            Some(scope),
            Details {
                rows,
                truncated: payload["truncated"].as_bool(),
                ..Details::default()
            },
        );
        CallToolResult::structured(payload)
    }
}

fn select_field(raw: &Value) -> String {
    let Some(object) = raw.as_object() else {
        return "arguments".to_owned();
    };
    let Some(table) = raw["table"].as_object() else {
        return "table".to_owned();
    };
    if !raw["table"]["name"].is_string() {
        return "table.name".to_owned();
    }
    if table
        .get("schema")
        .is_some_and(|value| !(value.is_null() || value.is_string()))
    {
        return "table.schema".to_owned();
    }
    if let Some(key) = table
        .keys()
        .find(|key| !matches!(key.as_str(), "name" | "schema"))
    {
        return format!("table.{}", safe_field_name(key));
    }
    if Vec::<String>::deserialize(&raw["columns"]).is_err() {
        return "columns".to_owned();
    }
    if object
        .get("filters")
        .is_some_and(|value| !value.is_object())
    {
        return "filters".to_owned();
    }
    if object
        .get("order_by")
        .is_some_and(|value| Vec::<String>::deserialize(value).is_err())
    {
        return "order_by".to_owned();
    }
    if object
        .get("offset")
        .is_some_and(|value| u32::deserialize(value).is_err())
    {
        return "offset".to_owned();
    }
    if Option::<u16>::deserialize(&raw["limit"]).is_err() {
        return "limit".to_owned();
    }
    object
        .keys()
        .find(|key| {
            !matches!(
                key.as_str(),
                "table" | "columns" | "filters" | "order_by" | "offset" | "limit"
            )
        })
        .map_or_else(|| "arguments".to_owned(), |key| safe_field_name(key))
}

fn safe_field_name(key: &str) -> String {
    key.chars()
        .filter(|character| !character.is_control())
        .take(128)
        .collect()
}
