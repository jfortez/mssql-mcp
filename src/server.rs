use std::sync::Arc;
use std::time::Instant;

use rmcp::ServerHandler;
use rmcp::handler::server::common::schema_for_input;
use rmcp::model::{
    CallToolRequestMethod, CallToolRequestParams, CallToolResponse, CallToolResult, ConstString,
    ContentBlock, ErrorCode, Implementation, JsonObject, ListToolsResult, PaginatedRequestParams,
    ServerCapabilities, ServerConfig, Tool,
};
use rmcp::service::{RequestContext, RoleServer, ServerInitializeError};
use rmcp::transport::async_rw::AsyncRwTransport;
use serde_json::{Value, json};

use crate::audit::{self, Scope, Status};
use crate::config::{Mode, Settings, Transport};
use crate::database::{Database, DbError, Outcome};
use crate::statement::{
    DeleteRows, DescribeTable, InsertRow, ListTables, SelectRows, Statement, Table, UpdateRows,
};

const MAX_FRAME_BYTES: usize = 1024 * 1024;
const MUTATION_ROLLED_BACK: &str = "mutation exceeded the 100-row limit and was rolled back";

const MODE_UNAVAILABLE: &str = "mode_unavailable";
const INVALID_ARGUMENTS: &str = "invalid_arguments";
const INVALID_INPUT: &str = "invalid_input";
const UNKNOWN_TOOL: &str = "unknown_tool";
const DATABASE_BUSY: &str = "database_busy";
const CONNECTION_FAILED: &str = "connection_failed";
const AUTH_UNAVAILABLE: &str = "authentication_unavailable";
const QUERY_FAILED: &str = "query_failed";
const TIMEOUT: &str = "timeout";
const UNRECOGNIZED_RESULT: &str = "unrecognized_result";
const MUTATION_ROLLED_BACK_CODE: &str = "mutation_rolled_back";

const NOT_ATTEMPTED: &str = "not_attempted";
const ROLLED_BACK: &str = "rolled_back";
const UNKNOWN_OUTCOME: &str = "unknown";

const MODE_ACTION: &str = "start the server with a mode that enables this tool";
const ARGUMENTS_ACTION: &str = "match the tool inputSchema and retry";
const INPUT_ACTION: &str = "correct the arguments and retry";
const BUSY_ACTION: &str = "retry after the other database operations finish";
const CONNECTION_ACTION: &str =
    "check that the server is reachable and that TLS and the credentials are accepted";
const AUTH_ACTION: &str = "use an authentication method available in this build and platform";
const RETRY_READ_ACTION: &str =
    "check the database permissions and the requested objects, then retry";
const VERIFY_STATE_ACTION: &str = "verify the database state before retrying the operation";
const ROLLBACK_ACTION: &str = "narrow the filters so at most 100 rows change, then retry";

#[derive(Clone, Copy)]
enum Known {
    ListTables,
    DescribeTable,
    SelectRows,
    InsertRow,
    UpdateRows,
    DeleteRows,
}

const KNOWN: [Known; 6] = [
    Known::ListTables,
    Known::DescribeTable,
    Known::SelectRows,
    Known::InsertRow,
    Known::UpdateRows,
    Known::DeleteRows,
];

impl Known {
    const fn name(self) -> &'static str {
        match self {
            Self::ListTables => "list_tables",
            Self::DescribeTable => "describe_table",
            Self::SelectRows => "select_rows",
            Self::InsertRow => "insert_row",
            Self::UpdateRows => "update_rows",
            Self::DeleteRows => "delete_rows",
        }
    }

    const fn is_mutation(self) -> bool {
        matches!(self, Self::InsertRow | Self::UpdateRows | Self::DeleteRows)
    }

    const fn allowed(self, mode: Mode) -> bool {
        match mode {
            Mode::Read => !self.is_mutation(),
            Mode::Write => self.is_mutation(),
            Mode::Both => true,
        }
    }

    fn from_name(name: &str) -> Option<Self> {
        KNOWN.into_iter().find(|known| known.name() == name)
    }
}

trait Scoped {
    fn scope(&self) -> Scope<'_>;
}

impl Scoped for ListTables {
    fn scope(&self) -> Scope<'_> {
        match self.schema.as_deref() {
            Some(schema) => Scope::schema(schema),
            None => Scope::catalog(),
        }
    }
}

impl Scoped for DescribeTable {
    fn scope(&self) -> Scope<'_> {
        table_scope(&self.table)
    }
}

impl Scoped for SelectRows {
    fn scope(&self) -> Scope<'_> {
        table_scope(&self.table)
    }
}

impl Scoped for InsertRow {
    fn scope(&self) -> Scope<'_> {
        table_scope(&self.table)
    }
}

impl Scoped for UpdateRows {
    fn scope(&self) -> Scope<'_> {
        table_scope(&self.table)
    }
}

impl Scoped for DeleteRows {
    fn scope(&self) -> Scope<'_> {
        table_scope(&self.table)
    }
}

fn table_scope(table: &Table) -> Scope<'_> {
    Scope::table(table.schema.as_deref().unwrap_or("dbo"), &table.name)
}

struct ToolError {
    code: &'static str,
    message: String,
    action: &'static str,
    retryable: bool,
    mutation_outcome: Option<&'static str>,
}

impl ToolError {
    fn mode_unavailable(mode: Mode, mutation: bool) -> Self {
        Self {
            code: MODE_UNAVAILABLE,
            message: format!("tool is not available in {} mode", mode.name()),
            action: MODE_ACTION,
            retryable: false,
            mutation_outcome: mutation.then_some(NOT_ATTEMPTED),
        }
    }

    fn invalid_arguments(tool: Known) -> Self {
        Self {
            code: INVALID_ARGUMENTS,
            message: format!("invalid arguments for {}", tool.name()),
            action: ARGUMENTS_ACTION,
            retryable: false,
            mutation_outcome: tool.is_mutation().then_some(NOT_ATTEMPTED),
        }
    }

    fn invalid_input(tool: Known, message: &'static str) -> Self {
        Self {
            code: INVALID_INPUT,
            message: message.to_owned(),
            action: INPUT_ACTION,
            retryable: false,
            mutation_outcome: tool.is_mutation().then_some(NOT_ATTEMPTED),
        }
    }

    fn from_database(error: DbError, mutation: bool) -> Self {
        let (code, action, retryable) = match error {
            DbError::Busy => (DATABASE_BUSY, BUSY_ACTION, true),
            DbError::Connect => (CONNECTION_FAILED, CONNECTION_ACTION, false),
            DbError::UnsupportedAuth => (AUTH_UNAVAILABLE, AUTH_ACTION, false),
            DbError::Failed => (QUERY_FAILED, retry_action(mutation), false),
            DbError::Timeout => (TIMEOUT, retry_action(mutation), false),
            DbError::UnexpectedResult => (UNRECOGNIZED_RESULT, retry_action(mutation), false),
        };
        Self {
            code,
            message: error.message(mutation).to_owned(),
            action,
            retryable,
            mutation_outcome: mutation.then_some(match error {
                DbError::Busy | DbError::Connect | DbError::UnsupportedAuth => NOT_ATTEMPTED,
                DbError::Failed | DbError::Timeout | DbError::UnexpectedResult => UNKNOWN_OUTCOME,
            }),
        }
    }

    fn into_result(self) -> CallToolResult {
        let mut structured = json!({
            "code": self.code,
            "message": &self.message,
            "action": self.action,
            "retryable": self.retryable,
        });
        if let Some(outcome) = self.mutation_outcome {
            structured["mutation_outcome"] = Value::String(outcome.to_owned());
        }
        let mut result = CallToolResult::error(vec![ContentBlock::text(self.message)]);
        result.structured_content = Some(structured);
        result
    }
}

fn retry_action(mutation: bool) -> &'static str {
    if mutation {
        VERIFY_STATE_ACTION
    } else {
        RETRY_READ_ACTION
    }
}

fn rolled_back() -> CallToolResult {
    let legacy = json!({"affected_rows": -1, "error": MUTATION_ROLLED_BACK});
    let mut result = CallToolResult::error(vec![ContentBlock::text(legacy.to_string())]);
    result.structured_content = Some(json!({
        "affected_rows": -1,
        "error": MUTATION_ROLLED_BACK,
        "code": MUTATION_ROLLED_BACK_CODE,
        "message": MUTATION_ROLLED_BACK,
        "action": ROLLBACK_ACTION,
        "retryable": false,
        "mutation_outcome": ROLLED_BACK,
    }));
    result
}

#[derive(Default)]
struct Counts {
    rows: Option<usize>,
    affected_rows: Option<i64>,
    truncated: Option<bool>,
}

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
        tool: Known,
        arguments: JsonObject,
        started: Instant,
        prepare: impl FnOnce(&T) -> Result<Statement, &'static str>,
    ) -> CallToolResult
    where
        T: serde::de::DeserializeOwned + Scoped + Send,
    {
        let name = tool.name();
        let input = match serde_json::from_value(Value::Object(arguments)) {
            Ok(input) => input,
            Err(_) => return self.failed(name, started, None, ToolError::invalid_arguments(tool)),
        };
        let statement = match prepare(&input) {
            Ok(statement) => statement,
            Err(message) => {
                return self.failed(name, started, None, ToolError::invalid_input(tool, message));
            }
        };
        let mutation = statement.mutation;
        let scope = input.scope();
        match self.database.execute(statement).await {
            Ok(Outcome::Read(read)) => {
                self.record(
                    Some(name),
                    started,
                    Status::Ok,
                    None,
                    Some(scope),
                    Counts {
                        rows: Some(read.rows.len()),
                        truncated: Some(read.truncated),
                        ..Counts::default()
                    },
                );
                CallToolResult::structured(json!({
                    "columns": read.columns,
                    "rows": read.rows,
                    "truncated": read.truncated,
                }))
            }
            Ok(Outcome::Mutation(affected)) if affected >= 0 => {
                self.record(
                    Some(name),
                    started,
                    Status::Ok,
                    None,
                    Some(scope),
                    Counts {
                        affected_rows: Some(affected),
                        ..Counts::default()
                    },
                );
                CallToolResult::structured(json!({"affected_rows": affected}))
            }
            Ok(Outcome::Mutation(_)) => {
                self.record(
                    Some(name),
                    started,
                    Status::Error,
                    Some(MUTATION_ROLLED_BACK_CODE),
                    Some(scope),
                    Counts {
                        affected_rows: Some(-1),
                        ..Counts::default()
                    },
                );
                rolled_back()
            }
            Err(error) => self.failed(
                name,
                started,
                Some(scope),
                ToolError::from_database(error, mutation),
            ),
        }
    }

    fn failed(
        &self,
        tool: &'static str,
        started: Instant,
        scope: Option<Scope<'_>>,
        error: ToolError,
    ) -> CallToolResult {
        self.record(
            Some(tool),
            started,
            Status::Error,
            Some(error.code),
            scope,
            Counts::default(),
        );
        error.into_result()
    }

    fn record(
        &self,
        tool: Option<&'static str>,
        started: Instant,
        status: Status,
        code: Option<&'static str>,
        scope: Option<Scope<'_>>,
        counts: Counts,
    ) {
        audit::call(audit::CallRecord {
            tool,
            mode: self.mode,
            status,
            code,
            duration: started.elapsed(),
            scope,
            rows: counts.rows,
            affected_rows: counts.affected_rows,
            truncated: counts.truncated,
        });
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
            .filter(|tool| {
                Known::from_name(tool.name.as_ref()).is_some_and(|known| known.allowed(self.mode))
            })
            .cloned()
            .collect();
        Ok(ListToolsResult::with_all_items(tools))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, rmcp::ErrorData> {
        let started = Instant::now();
        let Some(tool) = Known::from_name(request.name.as_ref()) else {
            self.record(
                None,
                started,
                Status::Error,
                Some(UNKNOWN_TOOL),
                None,
                Counts::default(),
            );
            return Err(rmcp::ErrorData::new(
                ErrorCode::METHOD_NOT_FOUND,
                CallToolRequestMethod::VALUE,
                Some(json!({"code": UNKNOWN_TOOL})),
            ));
        };
        if !tool.allowed(self.mode) {
            let error = ToolError::mode_unavailable(self.mode, tool.is_mutation());
            return Ok(self.failed(tool.name(), started, None, error).into());
        }

        let arguments = request.arguments.unwrap_or_default();
        let result = match tool {
            Known::ListTables => {
                self.invoke(tool, arguments, started, |input: &ListTables| {
                    input.prepare()
                })
                .await
            }
            Known::DescribeTable => {
                self.invoke(tool, arguments, started, |input: &DescribeTable| {
                    input.prepare()
                })
                .await
            }
            Known::SelectRows => {
                self.invoke(tool, arguments, started, |input: &SelectRows| {
                    input.prepare()
                })
                .await
            }
            Known::InsertRow => {
                self.invoke(tool, arguments, started, |input: &InsertRow| {
                    input.prepare()
                })
                .await
            }
            Known::UpdateRows => {
                self.invoke(tool, arguments, started, |input: &UpdateRows| {
                    input.prepare()
                })
                .await
            }
            Known::DeleteRows => {
                self.invoke(tool, arguments, started, |input: &DeleteRows| {
                    input.prepare()
                })
                .await
            }
        };
        Ok(result.into())
    }
}

pub async fn serve(settings: Settings) -> Result<(), String> {
    audit::startup(
        settings.mode,
        settings.auth.name(),
        !settings.trust_server_certificate,
    );
    let result = match settings.transport {
        Transport::Stdio => run_stdio(settings).await,
        Transport::Http => crate::http::run(settings).await,
    };
    audit::shutdown(if result.is_ok() {
        Status::Ok
    } else {
        Status::Error
    });
    result
}

async fn run_stdio(settings: Settings) -> Result<(), String> {
    let server = McpServer::new(settings).map_err(str::to_owned)?;
    let (reader, writer) = rmcp::transport::stdio();
    let transport = AsyncRwTransport::<RoleServer, _, _>::new_server(reader, writer)
        .with_max_line_length(MAX_FRAME_BYTES);
    let running = match rmcp::serve_server(server, transport).await {
        Ok(running) => running,
        Err(ServerInitializeError::ConnectionClosed(_)) => return Ok(()),
        Err(_) => return Err("MCP initialization failed".to_owned()),
    };
    match running.waiting().await {
        Ok(_) => Ok(()),
        Err(_) => Err("MCP service stopped unexpectedly".to_owned()),
    }
}

fn build_tools() -> Result<Vec<Tool>, &'static str> {
    Ok(vec![
        Tool::new(
            Known::ListTables.name(),
            "List tables and views visible to the configured SQL Server login.",
            schema_for_input::<ListTables>().map_err(|_| "tool schema generation failed")?,
        ),
        Tool::new(
            Known::DescribeTable.name(),
            "Describe the columns of a table or view.",
            schema_for_input::<DescribeTable>().map_err(|_| "tool schema generation failed")?,
        ),
        Tool::new(
            Known::SelectRows.name(),
            "Read rows from a table or view with equality filters, ordering, and a bounded limit.",
            schema_for_input::<SelectRows>().map_err(|_| "tool schema generation failed")?,
        ),
        Tool::new(
            Known::InsertRow.name(),
            "Insert one row and report the number of affected rows.",
            schema_for_input::<InsertRow>().map_err(|_| "tool schema generation failed")?,
        ),
        Tool::new(
            Known::UpdateRows.name(),
            "Update at most 100 rows matching equality filters.",
            schema_for_input::<UpdateRows>().map_err(|_| "tool schema generation failed")?,
        ),
        Tool::new(
            Known::DeleteRows.name(),
            "Delete at most 100 rows matching equality filters.",
            schema_for_input::<DeleteRows>().map_err(|_| "tool schema generation failed")?,
        ),
    ])
}
