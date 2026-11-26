use std::sync::Arc;
use std::time::Instant;

use rmcp::ServerHandler;
use rmcp::handler::server::common::schema_for_input;
use rmcp::model::{
    CallToolRequestMethod, CallToolRequestParams, CallToolResponse, CallToolResult, ConstString,
    ContentBlock, ErrorCode, Implementation, JsonObject, ListToolsResult, PaginatedRequestParams,
    ServerCapabilities, ServerConfig, Tool, ToolAnnotations,
};
use rmcp::service::{RequestContext, RoleServer, ServerInitializeError};
use rmcp::transport::async_rw::AsyncRwTransport;
use serde_json::{Value, json};

use crate::approval::{self, Approves, Decision, Subject};
use crate::audit::{self, Scope, Status};
use crate::config::{Mode, Settings, Transport};
use crate::database::{Database, DbError, Outcome};
use crate::query::Query;
use crate::schema::{Catalog, ExploreSchema};
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
const APPROVAL_DENIED: &str = "approval_denied";
const APPROVAL_UNAVAILABLE: &str = "approval_unavailable";

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
const APPROVAL_ACTION: &str =
    "accept the approval prompt for the mutation, or start the server with --yolo";
const APPROVAL_UNAVAILABLE_ACTION: &str =
    "use a client that supports MCP elicitation, or start the server with --yolo";
const APPROVAL_UNAVAILABLE_MESSAGE: &str = concat!(
    "the client cannot approve mutations; use a client that supports elicitation ",
    "or start the server with --yolo"
);

#[derive(Clone, Copy)]
enum Known {
    ListTables,
    DescribeTable,
    SelectRows,
    InsertRow,
    UpdateRows,
    DeleteRows,
    Query,
    ExploreSchema,
}

const KNOWN: [Known; 8] = [
    Known::ListTables,
    Known::DescribeTable,
    Known::SelectRows,
    Known::InsertRow,
    Known::UpdateRows,
    Known::DeleteRows,
    Known::Query,
    Known::ExploreSchema,
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
            Self::Query => "query",
            Self::ExploreSchema => "explore_schema",
        }
    }

    /// Tools that may change the database and therefore ask for approval.
    const fn is_mutation(self) -> bool {
        matches!(
            self,
            Self::InsertRow | Self::UpdateRows | Self::DeleteRows | Self::Query
        )
    }

    const fn allowed(self, mode: Mode) -> bool {
        match mode {
            Mode::Read => matches!(
                self,
                Self::ListTables | Self::DescribeTable | Self::SelectRows | Self::ExploreSchema
            ),
            Mode::Write => matches!(
                self,
                Self::InsertRow
                    | Self::UpdateRows
                    | Self::DeleteRows
                    | Self::Query
                    | Self::ExploreSchema
            ),
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

    fn approval_denied() -> Self {
        Self {
            code: APPROVAL_DENIED,
            message: "the mutation was not approved".to_owned(),
            action: APPROVAL_ACTION,
            retryable: false,
            mutation_outcome: Some(NOT_ATTEMPTED),
        }
    }

    fn approval_unavailable() -> Self {
        Self {
            code: APPROVAL_UNAVAILABLE,
            message: APPROVAL_UNAVAILABLE_MESSAGE.to_owned(),
            action: APPROVAL_UNAVAILABLE_ACTION,
            retryable: false,
            mutation_outcome: Some(NOT_ATTEMPTED),
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

/// Bounded result details of one call; `approval` is set only for mutations
/// that reached the gate.
#[derive(Default)]
struct Details {
    rows: Option<usize>,
    affected_rows: Option<i64>,
    truncated: Option<bool>,
    approval: Option<&'static str>,
}

pub struct McpServer {
    database: Database,
    mode: Mode,
    yolo: bool,
    tools: Vec<Tool>,
}

impl McpServer {
    pub fn new(settings: Settings) -> Result<Self, &'static str> {
        let mode = settings.mode;
        let yolo = settings.yolo;
        let database = Database::new(Arc::new(settings));
        let tools = build_tools()?;
        Ok(Self {
            database,
            mode,
            yolo,
            tools,
        })
    }

    async fn invoke<T>(
        &self,
        tool: Known,
        arguments: JsonObject,
        context: &RequestContext<RoleServer>,
        started: Instant,
        prepare: impl FnOnce(&T) -> Result<Statement, &'static str>,
    ) -> CallToolResult
    where
        T: serde::de::DeserializeOwned + Scoped + Approves + Send,
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
        let statement = match prepare(&input) {
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
        let mutation = statement.mutation;
        let scope = input.scope();
        // Approval runs before any connection is opened, so a database failure
        // can never be mistaken for an approval failure.
        let approval = if mutation {
            let decision = match input.approval() {
                Some(subject) => {
                    match self
                        .approve(name, &subject, context, started, Some(scope))
                        .await
                    {
                        Ok(decision) => decision,
                        Err(result) => return result,
                    }
                }
                None => {
                    let error = ToolError::approval_unavailable();
                    return self.failed(
                        name,
                        started,
                        Some(scope),
                        error,
                        Some(Decision::Unavailable.audit_name()),
                    );
                }
            };
            Some(decision)
        } else {
            None
        };
        match self.database.execute(statement).await {
            Ok(Outcome::Read(read)) => {
                self.record(
                    Some(name),
                    started,
                    Status::Ok,
                    None,
                    Some(scope),
                    Details {
                        rows: Some(read.rows.len()),
                        truncated: Some(read.truncated),
                        approval,
                        ..Details::default()
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
                    Details {
                        affected_rows: Some(affected),
                        approval,
                        ..Details::default()
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
                    Details {
                        affected_rows: Some(-1),
                        approval,
                        ..Details::default()
                    },
                );
                rolled_back()
            }
            Ok(Outcome::Executed) => self.failed(
                name,
                started,
                Some(scope),
                ToolError::from_database(DbError::UnexpectedResult, mutation),
                approval,
            ),
            Err(error) => self.failed(
                name,
                started,
                Some(scope),
                ToolError::from_database(error, mutation),
                approval,
            ),
        }
    }

    /// Ask for approval and map a refusal into the recorded tool error. The
    /// caller returns the error result when this fails.
    async fn approve(
        &self,
        tool: &'static str,
        subject: &Subject<'_>,
        context: &RequestContext<RoleServer>,
        started: Instant,
        scope: Option<Scope<'_>>,
    ) -> Result<&'static str, CallToolResult> {
        let decision = tokio::select! {
            decision = approval::gate(&context.peer, subject, self.yolo) => decision,
            _ = context.ct.cancelled() => {
                return Err(self.failed(
                    tool,
                    started,
                    scope,
                    ToolError::approval_denied(),
                    Some(Decision::Denied.audit_name()),
                ));
            }
        };
        if context.ct.is_cancelled() {
            return Err(self.failed(
                tool,
                started,
                scope,
                ToolError::approval_denied(),
                Some(decision.audit_name()),
            ));
        }
        if decision.permitted() {
            return Ok(decision.audit_name());
        }
        let error = if decision == Decision::Denied {
            ToolError::approval_denied()
        } else {
            ToolError::approval_unavailable()
        };
        Err(self.failed(tool, started, scope, error, Some(decision.audit_name())))
    }

    /// One raw batch: approval always precedes the connection and the prompt
    /// carries the exact SQL. No scope can be validated from arbitrary text.
    async fn query(
        &self,
        arguments: JsonObject,
        context: &RequestContext<RoleServer>,
        started: Instant,
    ) -> CallToolResult {
        let name = Known::Query.name();
        let input = match serde_json::from_value::<Query>(Value::Object(arguments)) {
            Ok(input) => input,
            Err(_) => {
                return self.failed(
                    name,
                    started,
                    None,
                    ToolError::invalid_arguments(Known::Query),
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
                    ToolError::invalid_input(Known::Query, message),
                    None,
                );
            }
        };
        let subject = Subject::batch(name, &input.sql);
        let approval = match self.approve(name, &subject, context, started, None).await {
            Ok(decision) => decision,
            Err(result) => return result,
        };
        match self.database.execute(statement).await {
            Ok(Outcome::Read(read)) => {
                self.record(
                    Some(name),
                    started,
                    Status::Ok,
                    None,
                    None,
                    Details {
                        rows: Some(read.rows.len()),
                        truncated: Some(read.truncated),
                        approval: Some(approval),
                        ..Details::default()
                    },
                );
                CallToolResult::structured(json!({
                    "columns": read.columns,
                    "rows": read.rows,
                    "truncated": read.truncated,
                }))
            }
            Ok(Outcome::Executed) => {
                self.record(
                    Some(name),
                    started,
                    Status::Ok,
                    None,
                    None,
                    Details {
                        approval: Some(approval),
                        ..Details::default()
                    },
                );
                CallToolResult::structured(json!({"executed": true}))
            }
            Ok(Outcome::Mutation(_)) => self.failed(
                name,
                started,
                None,
                ToolError::from_database(DbError::UnexpectedResult, true),
                Some(approval),
            ),
            Err(error) => self.failed(
                name,
                started,
                None,
                ToolError::from_database(error, true),
                Some(approval),
            ),
        }
    }

    /// Read-only catalog tree: no approval and no arbitrary SQL, available in
    /// every mode. The scope is recorded only after the schema validates.
    async fn explore_schema(&self, arguments: JsonObject, started: Instant) -> CallToolResult {
        let name = Known::ExploreSchema.name();
        let input = match serde_json::from_value::<ExploreSchema>(Value::Object(arguments)) {
            Ok(input) => input,
            Err(_) => {
                return self.failed(
                    name,
                    started,
                    None,
                    ToolError::invalid_arguments(Known::ExploreSchema),
                    None,
                );
            }
        };
        let plan = match input.plan() {
            Ok(plan) => plan,
            Err(message) => {
                return self.failed(
                    name,
                    started,
                    None,
                    ToolError::invalid_input(Known::ExploreSchema, message),
                    None,
                );
            }
        };
        let scope = match input.schema.as_deref() {
            Some(schema) => Scope::schema(schema),
            None => Scope::catalog(),
        };
        let mut catalog = Catalog::default();
        for (part, statement) in plan {
            match self.database.execute(statement).await {
                Ok(Outcome::Read(read)) => catalog.absorb(part, read),
                Ok(_) => {
                    return self.failed(
                        name,
                        started,
                        Some(scope),
                        ToolError::from_database(DbError::UnexpectedResult, false),
                        None,
                    );
                }
                Err(error) => {
                    return self.failed(
                        name,
                        started,
                        Some(scope),
                        ToolError::from_database(error, false),
                        None,
                    );
                }
            }
        }
        let (tree, truncated) = catalog.render(input.schema.as_deref());
        self.record(
            Some(name),
            started,
            Status::Ok,
            None,
            Some(scope),
            Details {
                truncated: Some(truncated),
                ..Details::default()
            },
        );
        CallToolResult::structured(json!({"tree": tree, "truncated": truncated}))
    }

    fn failed(
        &self,
        tool: &'static str,
        started: Instant,
        scope: Option<Scope<'_>>,
        error: ToolError,
        approval: Option<&'static str>,
    ) -> CallToolResult {
        self.record(
            Some(tool),
            started,
            Status::Error,
            Some(error.code),
            scope,
            Details {
                approval,
                ..Details::default()
            },
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
        details: Details,
    ) {
        audit::call(audit::CallRecord {
            tool,
            mode: self.mode,
            status,
            code,
            duration: started.elapsed(),
            scope,
            rows: details.rows,
            affected_rows: details.affected_rows,
            truncated: details.truncated,
            approval: details.approval,
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
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, rmcp::ErrorData> {
        let started = Instant::now();
        let Some(tool) = Known::from_name(request.name.as_ref()) else {
            self.record(
                None,
                started,
                Status::Error,
                Some(UNKNOWN_TOOL),
                None,
                Details::default(),
            );
            return Err(rmcp::ErrorData::new(
                ErrorCode::METHOD_NOT_FOUND,
                CallToolRequestMethod::VALUE,
                Some(json!({"code": UNKNOWN_TOOL})),
            ));
        };
        if !tool.allowed(self.mode) {
            let error = ToolError::mode_unavailable(self.mode, tool.is_mutation());
            return Ok(self.failed(tool.name(), started, None, error, None).into());
        }

        let arguments = request.arguments.unwrap_or_default();
        let result = match tool {
            Known::ListTables => {
                self.invoke(tool, arguments, &context, started, |input: &ListTables| {
                    input.prepare()
                })
                .await
            }
            Known::DescribeTable => {
                self.invoke(
                    tool,
                    arguments,
                    &context,
                    started,
                    |input: &DescribeTable| input.prepare(),
                )
                .await
            }
            Known::SelectRows => {
                self.invoke(tool, arguments, &context, started, |input: &SelectRows| {
                    input.prepare()
                })
                .await
            }
            Known::InsertRow => {
                self.invoke(tool, arguments, &context, started, |input: &InsertRow| {
                    input.prepare()
                })
                .await
            }
            Known::UpdateRows => {
                self.invoke(tool, arguments, &context, started, |input: &UpdateRows| {
                    input.prepare()
                })
                .await
            }
            Known::DeleteRows => {
                self.invoke(tool, arguments, &context, started, |input: &DeleteRows| {
                    input.prepare()
                })
                .await
            }
            Known::Query => self.query(arguments, &context, started).await,
            Known::ExploreSchema => self.explore_schema(arguments, started).await,
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
    let read = || {
        ToolAnnotations::new()
            .read_only(true)
            .destructive(false)
            .idempotent(true)
            .open_world(false)
    };
    let insert = || {
        ToolAnnotations::new()
            .read_only(false)
            .destructive(false)
            .idempotent(false)
            .open_world(false)
    };
    let destructive = || {
        ToolAnnotations::new()
            .read_only(false)
            .destructive(true)
            .idempotent(false)
            .open_world(false)
    };
    Ok(vec![
        Tool::new(
            Known::ListTables.name(),
            "List tables and views visible to the configured SQL Server login.",
            schema_for_input::<ListTables>().map_err(|_| "tool schema generation failed")?,
        )
        .with_annotations(read()),
        Tool::new(
            Known::DescribeTable.name(),
            "Describe the columns of a table or view.",
            schema_for_input::<DescribeTable>().map_err(|_| "tool schema generation failed")?,
        )
        .with_annotations(read()),
        Tool::new(
            Known::SelectRows.name(),
            "Read rows from a table or view with equality filters, ordering, and a bounded limit.",
            schema_for_input::<SelectRows>().map_err(|_| "tool schema generation failed")?,
        )
        .with_annotations(read()),
        Tool::new(
            Known::InsertRow.name(),
            "Insert one row and report the number of affected rows.",
            schema_for_input::<InsertRow>().map_err(|_| "tool schema generation failed")?,
        )
        .with_annotations(insert()),
        Tool::new(
            Known::UpdateRows.name(),
            "Update at most 100 rows matching equality filters.",
            schema_for_input::<UpdateRows>().map_err(|_| "tool schema generation failed")?,
        )
        .with_annotations(destructive()),
        Tool::new(
            Known::DeleteRows.name(),
            "Delete at most 100 rows matching equality filters.",
            schema_for_input::<DeleteRows>().map_err(|_| "tool schema generation failed")?,
        )
        .with_annotations(destructive()),
        Tool::new(
            Known::Query.name(),
            "Run one arbitrary T-SQL batch under the configured login after approval; at most one result set is returned, bounded to 100 rows.",
            schema_for_input::<Query>().map_err(|_| "tool schema generation failed")?,
        )
        .with_annotations(destructive()),
        Tool::new(
            Known::ExploreSchema.name(),
            "Show a bounded indented tree of schemas, tables, columns, relations, views, procedures and functions.",
            schema_for_input::<ExploreSchema>().map_err(|_| "tool schema generation failed")?,
        )
        .with_annotations(read()),
    ])
}
