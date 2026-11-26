use std::sync::Arc;

use rmcp::ServerHandler;
use rmcp::handler::server::common::schema_for_input;
use rmcp::model::{
    CallToolRequestMethod, CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock,
    Implementation, JsonObject, ListToolsResult, PaginatedRequestParams, ServerCapabilities,
    ServerConfig, Tool,
};
use rmcp::service::{RequestContext, RoleServer, ServerInitializeError};
use rmcp::transport::async_rw::AsyncRwTransport;
use serde_json::{Value, json};

use crate::config::{Mode, Settings};
use crate::database::{Database, Outcome};
use crate::statement::{
    DeleteRows, DescribeTable, InsertRow, ListTables, SelectRows, Statement, UpdateRows,
};

const MAX_FRAME_BYTES: usize = 1024 * 1024;
const MUTATION_ROLLED_BACK: &str = "mutation exceeded the 100-row limit and was rolled back";

const READ_TOOLS: [&str; 3] = ["list_tables", "describe_table", "select_rows"];
const WRITE_TOOLS: [&str; 3] = ["insert_row", "update_rows", "delete_rows"];

pub struct McpServer {
    database: Database,
    mode: Mode,
    tools: Vec<Tool>,
}

impl McpServer {
    pub fn new(settings: Settings) -> Result<Self, &'static str> {
        let mode = settings.mode;
        let database = Database::new(Arc::new(settings));
        let tools = build_tools()?;
        Ok(Self {
            database,
            mode,
            tools,
        })
    }

    async fn invoke<T>(
        &self,
        name: &'static str,
        arguments: JsonObject,
        prepare: impl FnOnce(T) -> Result<Statement, &'static str>,
    ) -> CallToolResult
    where
        T: serde::de::DeserializeOwned,
    {
        let input = match serde_json::from_value(Value::Object(arguments)) {
            Ok(input) => input,
            Err(_) => return tool_failure(format!("invalid arguments for {name}")),
        };
        let statement = match prepare(input) {
            Ok(statement) => statement,
            Err(message) => return tool_failure(message.to_owned()),
        };
        let mutation = statement.mutation;
        match self.database.execute(statement).await {
            Ok(Outcome::Read(read)) => CallToolResult::structured(json!({
                "columns": read.columns,
                "rows": read.rows,
                "truncated": read.truncated,
            })),
            Ok(Outcome::Mutation(affected)) if affected >= 0 => {
                CallToolResult::structured(json!({"affected_rows": affected}))
            }
            Ok(Outcome::Mutation(_)) => CallToolResult::structured_error(json!({
                "affected_rows": -1,
                "error": MUTATION_ROLLED_BACK,
            })),
            Err(error) => tool_failure(error.message(mutation).to_owned()),
        }
    }
}

impl ServerHandler for McpServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build()).with_server_info(
            Implementation::new(env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION")),
        )
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, rmcp::ErrorData> {
        let tools = self
            .tools
            .iter()
            .filter(|tool| enabled(self.mode, tool.name.as_ref()))
            .cloned()
            .collect();
        Ok(ListToolsResult::with_all_items(tools))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, rmcp::ErrorData> {
        let name = request.name.as_ref();
        if !known_tool(name) {
            return Err(rmcp::ErrorData::method_not_found::<CallToolRequestMethod>());
        }
        if !enabled(self.mode, name) {
            return Ok(CallToolResult::error(vec![ContentBlock::text(format!(
                "tool is not available in {} mode",
                self.mode.name()
            ))])
            .into());
        }

        let arguments = request.arguments.unwrap_or_default();
        let result = match name {
            "list_tables" => {
                self.invoke("list_tables", arguments, |input: ListTables| {
                    input.prepare()
                })
                .await
            }
            "describe_table" => {
                self.invoke("describe_table", arguments, |input: DescribeTable| {
                    input.prepare()
                })
                .await
            }
            "select_rows" => {
                self.invoke("select_rows", arguments, |input: SelectRows| {
                    input.prepare()
                })
                .await
            }
            "insert_row" => {
                self.invoke("insert_row", arguments, |input: InsertRow| input.prepare())
                    .await
            }
            "update_rows" => {
                self.invoke("update_rows", arguments, |input: UpdateRows| {
                    input.prepare()
                })
                .await
            }
            "delete_rows" => {
                self.invoke("delete_rows", arguments, |input: DeleteRows| {
                    input.prepare()
                })
                .await
            }
            _ => return Err(rmcp::ErrorData::method_not_found::<CallToolRequestMethod>()),
        };
        Ok(result.into())
    }
}

pub async fn serve(settings: Settings) -> Result<(), &'static str> {
    let server = McpServer::new(settings)?;
    let (reader, writer) = rmcp::transport::stdio();
    let transport = AsyncRwTransport::<RoleServer, _, _>::new_server(reader, writer)
        .with_max_line_length(MAX_FRAME_BYTES);
    let running = match rmcp::serve_server(server, transport).await {
        Ok(running) => running,
        Err(ServerInitializeError::ConnectionClosed(_)) => return Ok(()),
        Err(_) => return Err("MCP initialization failed"),
    };
    match running.waiting().await {
        Ok(_) => Ok(()),
        Err(_) => Err("MCP service stopped unexpectedly"),
    }
}

fn tool_failure(message: String) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(message)])
}

fn known_tool(name: &str) -> bool {
    READ_TOOLS.contains(&name) || WRITE_TOOLS.contains(&name)
}

fn enabled(mode: Mode, name: &str) -> bool {
    match mode {
        Mode::Read => READ_TOOLS.contains(&name),
        Mode::Write => WRITE_TOOLS.contains(&name),
        Mode::Both => known_tool(name),
    }
}

fn build_tools() -> Result<Vec<Tool>, &'static str> {
    Ok(vec![
        Tool::new(
            "list_tables",
            "List tables and views visible to the configured SQL Server login.",
            schema_for_input::<ListTables>().map_err(|_| "tool schema generation failed")?,
        ),
        Tool::new(
            "describe_table",
            "Describe the columns of a table or view.",
            schema_for_input::<DescribeTable>().map_err(|_| "tool schema generation failed")?,
        ),
        Tool::new(
            "select_rows",
            "Read rows from a table or view with equality filters, ordering, and a bounded limit.",
            schema_for_input::<SelectRows>().map_err(|_| "tool schema generation failed")?,
        ),
        Tool::new(
            "insert_row",
            "Insert one row and report the number of affected rows.",
            schema_for_input::<InsertRow>().map_err(|_| "tool schema generation failed")?,
        ),
        Tool::new(
            "update_rows",
            "Update at most 100 rows matching equality filters.",
            schema_for_input::<UpdateRows>().map_err(|_| "tool schema generation failed")?,
        ),
        Tool::new(
            "delete_rows",
            "Delete at most 100 rows matching equality filters.",
            schema_for_input::<DeleteRows>().map_err(|_| "tool schema generation failed")?,
        ),
    ])
}
