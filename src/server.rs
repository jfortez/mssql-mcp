use std::sync::Arc;
use std::time::Instant;

use rmcp::Peer;
use rmcp::ServerHandler;
use rmcp::handler::server::common::schema_for_input;
use rmcp::model::{
    CallToolRequestMethod, CallToolRequestParams, CallToolResponse, CallToolResult, ConstString,
    ContentBlock, ErrorCode, Implementation, JsonObject, ListToolsResult, PaginatedRequestParams,
    ServerCapabilities, ServerConfig, Tool, ToolAnnotations,
};
use rmcp::service::{ElicitationMode, RequestContext, RoleServer, ServerInitializeError};
use rmcp::transport::async_rw::AsyncRwTransport;
use serde_json::{Value, json};

use crate::approval::{self, Approves, Decision, Subject};
use crate::audit::{self, Scope, Status};
use crate::catalog::{
    DependencyColumns, ObjectDependencies, RoutineDefinition, SchemaMap, SearchObjects, TableStats,
};
use crate::config::{Mode, Settings, Transport};
use crate::database::{Database, DbError, Outcome, SqlError};
use crate::query::Query;
use crate::schema::{Catalog, ExploreSchema};
use crate::statement::{
    DeleteRows, DescribeTable, InsertRow, ListTables, SampleRows, SelectRows, Statement, Table,
    UpdateRows,
};

#[path = "server_reads.rs"]
mod reads;

const MAX_FRAME_BYTES: usize = 1024 * 1024;
const MUTATION_ROLLED_BACK: &str = "mutation exceeded the 100-row limit and was rolled back";

const MODE_UNAVAILABLE: &str = "mode_unavailable";
const INVALID_ARGUMENTS: &str = "invalid_arguments";
const INVALID_INPUT: &str = "invalid_input";
const OBJECT_NOT_FOUND: &str = "object_not_found";
const DEFINITION_UNAVAILABLE: &str = "definition_unavailable";
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
const CORRECT_ACTION: &str = "correct the statement and retry";
const VERIFY_STATE_ACTION: &str = "verify the database state before retrying the operation";
const ROLLBACK_ACTION: &str = "narrow the filters so at most 100 rows change, then retry";
const APPROVAL_ACTION: &str =
    "accept the approval prompt for the mutation, or start the server with --yolo";
const APPROVAL_UNAVAILABLE_ACTION: &str = concat!(
    "start the server with --trust-elicitation on an installed host that reserves the MCP ",
    "form prompt for a human, then retry; or start the server with --yolo"
);
const APPROVAL_UNAVAILABLE_MESSAGE: &str =
    "no trusted human approval channel is available for this mutation; nothing was executed";
/// Discovery text advertised in `initialize` so a client learns why a mutation
/// needs an answer and how it is asked for before it tries one. It only
/// describes the gate: it never changes which tools are listed or whether
/// approval is required.
const INSTRUCTIONS: &str = concat!(
    "This server runs T-SQL against one configured SQL Server login. ",
    "insert_row, update_rows, delete_rows and query never run without a human approval that ",
    "the server itself obtains: by default no approval channel is trusted, so they refuse with ",
    "approval_unavailable and mutation_outcome not_attempted before any connection is opened. ",
    "On stdio, start the server with --trust-elicitation only when the installed host reserves ",
    "the native MCP form prompt for a human and isolates that confirmation from the model; the ",
    "host must advertise form elicitation and explicitly confirm each prepared operation. The ",
    "HTTP transport is stateless and never offers that prompt. The model cannot supply an ",
    "approval as a tool argument. Start the server with --yolo (or MSSQL_MCP_YOLO=true) only ",
    "when the operator has chosen to bypass that question. Read mode exposes only read tools: ",
    "list_tables, describe_table, select_rows, explore_schema, schema_map, object_dependencies, ",
    "routine_definition, dependency_columns, search_objects, table_stats and sample_rows."
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
    SchemaMap,
    ObjectDependencies,
    RoutineDefinition,
    DependencyColumns,
    SearchObjects,
    TableStats,
    SampleRows,
}

const KNOWN: [Known; 15] = [
    Known::ListTables,
    Known::DescribeTable,
    Known::SelectRows,
    Known::InsertRow,
    Known::UpdateRows,
    Known::DeleteRows,
    Known::Query,
    Known::ExploreSchema,
    Known::SchemaMap,
    Known::ObjectDependencies,
    Known::RoutineDefinition,
    Known::DependencyColumns,
    Known::SearchObjects,
    Known::TableStats,
    Known::SampleRows,
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
            Self::SchemaMap => "schema_map",
            Self::ObjectDependencies => "object_dependencies",
            Self::RoutineDefinition => "routine_definition",
            Self::DependencyColumns => "dependency_columns",
            Self::SearchObjects => "search_objects",
            Self::TableStats => "table_stats",
            Self::SampleRows => "sample_rows",
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
                Self::ListTables
                    | Self::DescribeTable
                    | Self::SelectRows
                    | Self::ExploreSchema
                    | Self::SchemaMap
                    | Self::ObjectDependencies
                    | Self::RoutineDefinition
                    | Self::DependencyColumns
                    | Self::SearchObjects
                    | Self::TableStats
                    | Self::SampleRows
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
    /// Sanitized SQL Server diagnostics; absent for every failure class whose
    /// text could carry login or connection details.
    sql_error: Option<SqlError>,
}

impl ToolError {
    fn mode_unavailable(mode: Mode, mutation: bool) -> Self {
        Self {
            code: MODE_UNAVAILABLE,
            message: format!("tool is not available in {} mode", mode.name()),
            action: MODE_ACTION,
            retryable: false,
            mutation_outcome: mutation.then_some(NOT_ATTEMPTED),
            sql_error: None,
        }
    }

    fn invalid_arguments(tool: Known) -> Self {
        Self {
            code: INVALID_ARGUMENTS,
            message: format!("invalid arguments for {}", tool.name()),
            action: ARGUMENTS_ACTION,
            retryable: false,
            mutation_outcome: tool.is_mutation().then_some(NOT_ATTEMPTED),
            sql_error: None,
        }
    }

    fn invalid_input(tool: Known, message: &'static str) -> Self {
        Self {
            code: INVALID_INPUT,
            message: message.to_owned(),
            action: INPUT_ACTION,
            retryable: false,
            mutation_outcome: tool.is_mutation().then_some(NOT_ATTEMPTED),
            sql_error: None,
        }
    }

    fn not_found() -> Self {
        Self {
            code: OBJECT_NOT_FOUND,
            message: "database object not found or not visible to the configured login".to_owned(),
            action: "check the object name, schema, and login permissions",
            retryable: false,
            mutation_outcome: None,
            sql_error: None,
        }
    }

    fn object_not_found(message: String) -> Self {
        Self {
            code: OBJECT_NOT_FOUND,
            message,
            action: "check the object name, schema, and login permissions",
            retryable: false,
            mutation_outcome: None,
            sql_error: None,
        }
    }

    fn definition_unavailable(message: String) -> Self {
        Self {
            code: DEFINITION_UNAVAILABLE,
            message,
            action: "grant VIEW DEFINITION on the database, or check whether the module is encrypted",
            retryable: false,
            mutation_outcome: None,
            sql_error: None,
        }
    }

    fn approval_denied() -> Self {
        Self {
            code: APPROVAL_DENIED,
            message: "the mutation was not approved".to_owned(),
            action: APPROVAL_ACTION,
            retryable: false,
            mutation_outcome: Some(NOT_ATTEMPTED),
            sql_error: None,
        }
    }

    fn approval_unavailable() -> Self {
        Self {
            code: APPROVAL_UNAVAILABLE,
            message: APPROVAL_UNAVAILABLE_MESSAGE.to_owned(),
            action: APPROVAL_UNAVAILABLE_ACTION,
            retryable: false,
            mutation_outcome: Some(NOT_ATTEMPTED),
            sql_error: None,
        }
    }

    fn from_database(error: DbError, mutation: bool) -> Self {
        let (code, action, retryable) = match &error {
            DbError::Busy => (DATABASE_BUSY, BUSY_ACTION, true),
            DbError::Connect => (CONNECTION_FAILED, CONNECTION_ACTION, false),
            DbError::UnsupportedAuth => (AUTH_UNAVAILABLE, AUTH_ACTION, false),
            DbError::Failed => (QUERY_FAILED, retry_action(mutation), false),
            DbError::Server(_) => (QUERY_FAILED, retry_action(mutation), false),
            // The server already finished the generated transaction without
            // committing, so the operator has nothing to verify; only the
            // statement itself needs correcting.
            DbError::RolledBack(_) => (QUERY_FAILED, CORRECT_ACTION, false),
            DbError::Timeout => (TIMEOUT, retry_action(mutation), false),
            DbError::UnexpectedResult => (
                UNRECOGNIZED_RESULT,
                "inspect the statement and returned result sets",
                false,
            ),
        };
        let mutation_outcome = if !mutation {
            None
        } else {
            match &error {
                DbError::Busy | DbError::Connect | DbError::UnsupportedAuth => Some(NOT_ATTEMPTED),
                DbError::RolledBack(_) => Some(ROLLED_BACK),
                // A result-shape failure only says the response was not what we
                // can decode: it must never imply that data changed, and for an
                // arbitrary batch it cannot tell us what earlier statements
                // did, so no outcome is claimed.
                DbError::UnexpectedResult | DbError::Server(_) => None,
                DbError::Failed | DbError::Timeout => Some(UNKNOWN_OUTCOME),
            }
        };
        let sql_error = match &error {
            DbError::Server(diagnostics) | DbError::RolledBack(diagnostics) => {
                Some(diagnostics.clone())
            }
            _ => None,
        };
        Self {
            code,
            message: error.message(mutation),
            action,
            retryable,
            mutation_outcome,
            sql_error,
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
        if let Some(diagnostics) = self.sql_error {
            structured["sql_error"] = json!({
                "number": diagnostics.number,
                "severity": diagnostics.severity,
                "message": diagnostics.message,
            });
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

/// One mutation call as the gate sees it: the question to ask and the facts the
/// audit record needs. Keeping them in one value keeps every refusal on the
/// same decision path.
struct GateCall<'a> {
    tool: &'static str,
    subject: &'a Subject<'a>,
    started: Instant,
    scope: Option<Scope<'a>>,
}

/// Audit fact of one approval decision.
#[derive(Clone, Copy, Debug)]
struct Approval {
    outcome: &'static str,
}

impl Approval {
    const fn audited(outcome: &'static str) -> Self {
        Self { outcome }
    }
}

/// Bounded result details of one call; `approval` is set only for mutations
/// that reached the gate.
#[derive(Default)]
struct Details {
    rows: Option<usize>,
    affected_rows: Option<i64>,
    truncated: Option<bool>,
    approval: Option<Approval>,
}

pub struct McpServer {
    database: Database,
    mode: Mode,
    yolo: bool,
    /// Set only when the operator trusted a stdio host and the transport can
    /// carry a per-client prompt. The HTTP transport never sets it.
    trust_elicitation: bool,
    tools: Vec<Tool>,
}

impl McpServer {
    pub fn new(settings: Settings, trust_elicitation: bool) -> Result<Self, &'static str> {
        let mode = settings.mode;
        let yolo = settings.yolo;
        // Configuration rejects the flag for HTTP; this second check keeps a
        // stateless server from ever treating a request as promptable.
        let trust_elicitation = trust_elicitation && settings.transport == Transport::Stdio;
        let database = Database::new(Arc::new(settings));
        let tools = build_tools()?;
        Ok(Self {
            database,
            mode,
            yolo,
            trust_elicitation,
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
            match input.approval() {
                Some(subject) => {
                    let call = GateCall {
                        tool: name,
                        subject: &subject,
                        started,
                        scope: Some(scope),
                    };
                    match self.approve(call, context).await {
                        Ok(approval) => Some(approval),
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
                        Some(Approval::audited(Decision::Unavailable.audit_name())),
                    );
                }
            }
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

    /// The one gate that decides whether a mutation may run. `--yolo` bypasses
    /// it. Every other call needs the operator's trusted stdio host and the
    /// client's native form elicitation; the human's explicit confirmation for
    /// this prepared operation is the only answer that runs anything. Every
    /// other combination is refused before connecting.
    async fn approve(
        &self,
        call: GateCall<'_>,
        context: &RequestContext<RoleServer>,
    ) -> Result<Approval, CallToolResult> {
        if self.yolo {
            return Ok(Approval::audited(Decision::Yolo.audit_name()));
        }
        if !self.trust_elicitation || !elicitation_supported(&context.peer) {
            return Err(self.unavailable(call));
        }
        let decision = tokio::select! {
            decision = approval::gate(&context.peer, call.subject) => decision,
            _ = context.ct.cancelled() => {
                return Err(self.failed(
                    call.tool,
                    call.started,
                    call.scope,
                    ToolError::approval_denied(),
                    Some(Approval::audited(Decision::Denied.audit_name())),
                ));
            }
        };
        if context.ct.is_cancelled() {
            return Err(self.failed(
                call.tool,
                call.started,
                call.scope,
                ToolError::approval_denied(),
                Some(Approval::audited(decision.audit_name())),
            ));
        }
        if decision.permitted() {
            return Ok(Approval::audited(decision.audit_name()));
        }
        let error = if decision == Decision::Denied {
            ToolError::approval_denied()
        } else {
            ToolError::approval_unavailable()
        };
        Err(self.failed(
            call.tool,
            call.started,
            call.scope,
            error,
            Some(Approval::audited(decision.audit_name())),
        ))
    }

    /// Refusal of a mutation that has no trusted human channel. Nothing has
    /// reached SQL Server and no model-callable answer exists.
    fn unavailable(&self, call: GateCall<'_>) -> CallToolResult {
        self.failed(
            call.tool,
            call.started,
            call.scope,
            ToolError::approval_unavailable(),
            Some(Approval::audited(Decision::Unavailable.audit_name())),
        )
    }

    /// One raw batch: approval always precedes the connection and the question
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
        let Some(subject) = input.approval() else {
            let error = ToolError::approval_unavailable();
            return self.failed(
                name,
                started,
                None,
                error,
                Some(Approval::audited(Decision::Unavailable.audit_name())),
            );
        };
        let call = GateCall {
            tool: name,
            subject: &subject,
            started,
            scope: None,
        };
        let approval = match self.approve(call, context).await {
            Ok(approval) => approval,
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
        approval: Option<Approval>,
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
            approval: details.approval.as_ref().map(|approval| approval.outcome),
        });
    }
}

/// Native form elicitation is necessary but not sufficient: the operator also
/// has to trust this host, because the protocol cannot attest that a host hands
/// its prompt to a human instead of answering it itself.
fn elicitation_supported(peer: &Peer<RoleServer>) -> bool {
    peer.supported_elicitation_modes()
        .contains(&ElicitationMode::Form)
}

impl ServerHandler for McpServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(
                env!("CARGO_PKG_NAME"),
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions(INSTRUCTIONS)
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
            Known::DescribeTable => self.describe_table(arguments, started).await,
            Known::SelectRows => self.select_rows(arguments, started).await,
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
            Known::SampleRows => self.sample_rows(arguments, started).await,
            Known::SchemaMap => {
                self.catalog_call(
                    tool,
                    arguments,
                    started,
                    SchemaMap::plan,
                    |plan| async move { plan.execute(&self.database).await },
                )
                .await
            }
            Known::ObjectDependencies => {
                self.catalog_call(
                    tool,
                    arguments,
                    started,
                    ObjectDependencies::plan,
                    |plan| async move { plan.execute(&self.database).await },
                )
                .await
            }
            Known::RoutineDefinition => self.routine_definition_call(arguments, started).await,
            Known::DependencyColumns => {
                self.catalog_call(
                    tool,
                    arguments,
                    started,
                    DependencyColumns::plan,
                    |plan| async move { plan.execute(&self.database).await },
                )
                .await
            }
            Known::SearchObjects => {
                self.catalog_call(
                    tool,
                    arguments,
                    started,
                    SearchObjects::plan,
                    |plan| async move { plan.execute(&self.database).await },
                )
                .await
            }
            Known::TableStats => {
                self.catalog_call(
                    tool,
                    arguments,
                    started,
                    TableStats::plan,
                    |plan| async move { plan.execute(&self.database).await },
                )
                .await
            }
        };
        Ok(result.into())
    }
}

pub async fn serve(settings: Settings, trust_elicitation: bool) -> Result<(), String> {
    // The trusted prompt needs a per-client stdio connection. The HTTP
    // transport is stateless, so configuration rejects the combination and
    // this mask keeps a daemon from ever trusting a request.
    let trust_elicitation = trust_elicitation && settings.transport == Transport::Stdio;
    audit::startup(
        settings.mode,
        settings.auth.name(),
        !settings.trust_server_certificate,
    );
    let result = match settings.transport {
        Transport::Stdio => run_stdio(settings, trust_elicitation).await,
        Transport::Http => crate::http::run(settings, trust_elicitation).await,
    };
    audit::shutdown(if result.is_ok() {
        Status::Ok
    } else {
        Status::Error
    });
    result
}

async fn run_stdio(settings: Settings, trust_elicitation: bool) -> Result<(), String> {
    let server = McpServer::new(settings, trust_elicitation).map_err(str::to_owned)?;
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
            "Insert one row and report the number of affected rows; needs a trusted human approval before connecting.",
            schema_for_input::<InsertRow>().map_err(|_| "tool schema generation failed")?,
        )
        .with_annotations(insert()),
        Tool::new(
            Known::UpdateRows.name(),
            "Update at most 100 rows matching equality filters; needs a trusted human approval before connecting.",
            schema_for_input::<UpdateRows>().map_err(|_| "tool schema generation failed")?,
        )
        .with_annotations(destructive()),
        Tool::new(
            Known::DeleteRows.name(),
            "Delete at most 100 rows matching equality filters; needs a trusted human approval before connecting.",
            schema_for_input::<DeleteRows>().map_err(|_| "tool schema generation failed")?,
        )
        .with_annotations(destructive()),
        Tool::new(
            Known::Query.name(),
            "Run one arbitrary T-SQL batch under the configured login after a trusted human approval; at most one result set is returned, bounded to 100 rows.",
            schema_for_input::<Query>().map_err(|_| "tool schema generation failed")?,
        )
        .with_annotations(destructive()),
        Tool::new(
            Known::ExploreSchema.name(),
            "Show a bounded indented tree of schemas, tables, columns, relations, views, procedures and functions.",
            schema_for_input::<ExploreSchema>().map_err(|_| "tool schema generation failed")?,
        )
        .with_annotations(read()),
        Tool::new(Known::SchemaMap.name(), "Page through a machine-readable catalog of schemas, objects, columns, keys, indexes and routine signatures.", schema_for_input::<SchemaMap>().map_err(|_| "tool schema generation failed")?).with_annotations(read()),
        Tool::new(Known::ObjectDependencies.name(), "Page through incoming and outgoing SQL-expression and foreign-key dependencies.", schema_for_input::<ObjectDependencies>().map_err(|_| "tool schema generation failed")?).with_annotations(read()),
        Tool::new(Known::RoutineDefinition.name(), "Return one procedure, function, view or trigger definition from sys.sql_modules, paged by character with the total length.", schema_for_input::<RoutineDefinition>().map_err(|_| "tool schema generation failed")?).with_annotations(read()),
        Tool::new(Known::DependencyColumns.name(), "List the objects one module references and their columns. Every referenced object gets an object entry even when column_hint matches none of its columns, and matching_column_count is 0 for that object; an object missing from the feed was not referenced.", schema_for_input::<DependencyColumns>().map_err(|_| "tool schema generation failed")?).with_annotations(read()),
        Tool::new(Known::SearchObjects.name(), "Find objects and columns by a literal name fragment.", schema_for_input::<SearchObjects>().map_err(|_| "tool schema generation failed")?).with_annotations(read()),
        Tool::new(Known::TableStats.name(), "Return approximate partition row counts and paged index storage sizes without a table scan.", schema_for_input::<TableStats>().map_err(|_| "tool schema generation failed")?).with_annotations(read()),
        Tool::new(Known::SampleRows.name(), "Sample up to 25 rows using the first 64 columns in catalog order.", schema_for_input::<SampleRows>().map_err(|_| "tool schema generation failed")?).with_annotations(read()),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The exact wire shape of a rendered tool error.
    fn wire(error: ToolError) -> Value {
        serde_json::to_value(error.into_result()).expect("a tool error serializes to JSON")
    }

    fn sql_error() -> SqlError {
        SqlError {
            number: 207,
            severity: 16,
            message: "Invalid column name 'missing'.".to_owned(),
        }
    }

    #[test]
    fn a_sql_server_rejection_reports_number_severity_and_message() {
        let json = wire(ToolError::from_database(
            DbError::Server(sql_error()),
            false,
        ));
        assert_eq!(json["isError"], true, "{json}");
        assert_eq!(json["structuredContent"]["code"], QUERY_FAILED, "{json}");
        assert_eq!(json["structuredContent"]["retryable"], false, "{json}");
        assert_eq!(
            json["structuredContent"]["sql_error"],
            json!({
                "number": 207,
                "severity": 16,
                "message": "Invalid column name 'missing'.",
            }),
            "{json}"
        );
        assert_eq!(
            json["content"][0]["text"],
            json!("SQL Server error 207 (severity 16): Invalid column name 'missing'."),
            "{json}"
        );
        assert_eq!(
            json["content"][0]["text"], json["structuredContent"]["message"],
            "the text must match the structured message: {json}"
        );
        assert!(
            json["structuredContent"].get("mutation_outcome").is_none(),
            "a read must not report an outcome: {json}"
        );

        // SQL Server directly rejected a statement. For a raw batch the
        // caller must check preceding statements; neither outcome is claimed.
        let json = wire(ToolError::from_database(DbError::Server(sql_error()), true));
        assert!(
            json["structuredContent"].get("mutation_outcome").is_none(),
            "{json}"
        );
        assert_eq!(
            json["structuredContent"]["action"], VERIFY_STATE_ACTION,
            "{json}"
        );
        assert_eq!(json["structuredContent"]["code"], QUERY_FAILED, "{json}");
        assert_eq!(
            json["structuredContent"]["sql_error"]["number"], 207,
            "{json}"
        );
    }

    #[test]
    fn a_verified_rollback_reports_the_outcome_without_a_row_count() {
        let json = wire(ToolError::from_database(
            DbError::RolledBack(sql_error()),
            true,
        ));
        assert_eq!(json["isError"], true, "{json}");
        assert_eq!(json["structuredContent"]["code"], QUERY_FAILED, "{json}");
        assert_eq!(
            json["structuredContent"]["mutation_outcome"], ROLLED_BACK,
            "{json}"
        );
        assert_eq!(json["structuredContent"]["retryable"], false, "{json}");
        assert_eq!(
            json["structuredContent"]["sql_error"]["severity"], 16,
            "{json}"
        );
        assert!(
            json["structuredContent"].get("affected_rows").is_none(),
            "the server never reported a row count: {json}"
        );
        assert!(
            json["content"][0]["text"]
                .as_str()
                .is_some_and(|text| text.contains("no changes from this call were committed")),
            "{json}"
        );
    }

    #[test]
    fn failures_without_server_diagnostics_stay_generic() {
        for error in [
            DbError::Connect,
            DbError::Timeout,
            DbError::Failed,
            DbError::Busy,
        ] {
            let json = wire(ToolError::from_database(error, true));
            assert!(
                json["structuredContent"].get("sql_error").is_none(),
                "only a server rejection carries diagnostics: {json}"
            );
        }
        let json = wire(ToolError::from_database(DbError::Connect, true));
        assert_eq!(
            json["content"][0]["text"], "database operation failed",
            "{json}"
        );
        assert_eq!(
            json["structuredContent"]["mutation_outcome"], NOT_ATTEMPTED,
            "{json}"
        );
        let json = wire(ToolError::from_database(DbError::Timeout, true));
        assert_eq!(
            json["structuredContent"]["mutation_outcome"], UNKNOWN_OUTCOME,
            "{json}"
        );
    }

    #[test]
    fn a_result_shape_failure_never_claims_a_mutation_outcome() {
        for mutation in [true, false] {
            let json = wire(ToolError::from_database(
                DbError::UnexpectedResult,
                mutation,
            ));
            assert_eq!(json["isError"], true, "{json}");
            assert_eq!(
                json["structuredContent"]["code"], UNRECOGNIZED_RESULT,
                "{json}"
            );
            assert_eq!(json["structuredContent"]["retryable"], false, "{json}");
            assert_eq!(
                json["content"][0]["text"], "database returned an unexpected result",
                "{json}"
            );
            assert!(
                json["structuredContent"].get("mutation_outcome").is_none(),
                "a result-shape failure must not imply that data may have changed: {json}"
            );
            assert!(
                json["structuredContent"].get("sql_error").is_none(),
                "a result-shape failure carries no server diagnostics: {json}"
            );
        }
    }

    #[test]
    fn an_untrusted_mutation_is_refused_with_no_relay_to_answer() {
        let json = wire(ToolError::approval_unavailable());
        assert_eq!(json["isError"], true, "{json}");
        let payload = &json["structuredContent"];
        assert_eq!(payload["code"], APPROVAL_UNAVAILABLE, "{json}");
        assert_eq!(payload["mutation_outcome"], NOT_ATTEMPTED, "{json}");
        assert_eq!(payload["retryable"], false, "{json}");
        assert_eq!(json["content"][0]["text"], payload["message"], "{json}");
        assert!(
            payload["action"].as_str().is_some_and(|action| {
                action.contains("--trust-elicitation") && action.contains("--yolo")
            }),
            "the refusal must name both operator choices: {json}"
        );
        for relay in ["approval_token", "approval_decision", "approval_required"] {
            assert!(
                !json.to_string().contains(relay),
                "a refusal must not offer a model-callable answer ({relay}): {json}"
            );
        }
    }
}
