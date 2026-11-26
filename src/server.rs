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

use crate::approval::{self, Approves, Decision, Retry, Subject};
use crate::audit::{self, Scope, Status};
use crate::catalog::{
    DependencyColumns, ObjectDependencies, RoutineDefinition, SchemaMap, SearchObjects, TableStats,
};
use crate::config::{Mode, Settings, Transport};
use crate::database::{Database, DbError, Outcome, SqlError};
use crate::query::Query;
use crate::schema::{Catalog, ExploreSchema};
use crate::statement::{
    DeleteRows, DescribeTable, InsertRow, ListTables, MAX_ROWS, SampleRows, SelectRows, Statement,
    Table, UpdateRows,
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
const APPROVAL_REQUIRED: &str = "approval_required";
const APPROVAL_DENIED: &str = "approval_denied";
const APPROVAL_UNAVAILABLE: &str = "approval_unavailable";
const APPROVAL_TOKEN_UNKNOWN: &str = "approval_token_unknown";
const APPROVAL_TOKEN_EXPIRED: &str = "approval_token_expired";
const APPROVAL_TOKEN_REUSED: &str = "approval_token_reused";
const APPROVAL_TOKEN_MISMATCH: &str = "approval_token_mismatch";
const APPROVAL_TOKEN_INVALID: &str = "approval_token_invalid";

const NOT_ATTEMPTED: &str = "not_attempted";
const ROLLED_BACK: &str = "rolled_back";
const UNKNOWN_OUTCOME: &str = "unknown";
/// Audit value of the question a non-eliciting client has to answer; the
/// decision itself is recorded as `granted` or `denied` on the retry.
const REQUIRED: &str = "required";

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
const APPROVAL_UNAVAILABLE_ACTION: &str =
    "check that the client can answer elicitation, or start the server with --yolo";
const APPROVAL_UNAVAILABLE_MESSAGE: &str = concat!(
    "the approval prompt could not be delivered to the client; retry it, ",
    "or start the server with --yolo"
);
/// What the client has to do with an `approval_required` result.
const RELAY_ACTION: &str = concat!(
    "relay approval_message to the human, then repeat this call with approval_token ",
    "and approval_decision (\"grant\" or \"deny\")"
);
const RETRY_QUESTION_ACTION: &str =
    "repeat the call without approval_token to ask the question again";
const TOKEN_INVALID_ACTION: &str =
    "supply approval_token and approval_decision together, or neither";
const TOKEN_MISMATCH_ACTION: &str = concat!(
    "repeat the call with exactly the arguments the token was issued for, ",
    "or ask again without approval_token"
);
/// Discovery text advertised in `initialize` so a client learns why a mutation
/// needs an answer and how it is asked for before it tries one. It only
/// describes the gate: it never changes which tools are listed or whether
/// approval is required.
const INSTRUCTIONS: &str = concat!(
    "This server runs T-SQL against one configured SQL Server login. ",
    "insert_row, update_rows, delete_rows and query require explicit human approval before any ",
    "connection is opened: a client that advertises MCP elicitation is asked directly, and any ",
    "other client receives a non-error approval_required result with a one-shot approval_token, ",
    "relays that question to the human and repeats the call with approval_token and ",
    "approval_decision (grant or deny). Nothing reaches SQL Server until that answer arrives. ",
    "Start the server with --yolo (or MSSQL_MCP_YOLO=true) only when the operator has chosen to ",
    "bypass that question. Read mode exposes only read tools: list_tables, describe_table, ",
    "select_rows, explore_schema, schema_map, object_dependencies, routine_definition, ",
    "dependency_columns, search_objects, table_stats and sample_rows."
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

    /// Refusal of a relayed retry whose token does not authorize this call.
    fn token(code: &'static str, message: &'static str, action: &'static str) -> Self {
        Self {
            code,
            message: message.to_owned(),
            action,
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

/// The single refusal vocabulary of the relay: every token that cannot
/// authorize this call is named, and none of them reaches SQL Server.
fn token_refusal(answer: approval::Answer) -> ToolError {
    match answer {
        approval::Answer::Unknown => ToolError::token(
            APPROVAL_TOKEN_UNKNOWN,
            "the approval token is not known to this server",
            RETRY_QUESTION_ACTION,
        ),
        approval::Answer::Expired => ToolError::token(
            APPROVAL_TOKEN_EXPIRED,
            "the approval token expired before it was answered",
            RETRY_QUESTION_ACTION,
        ),
        approval::Answer::Reused => ToolError::token(
            APPROVAL_TOKEN_REUSED,
            "the approval token was already answered",
            RETRY_QUESTION_ACTION,
        ),
        approval::Answer::Mismatch => ToolError::token(
            APPROVAL_TOKEN_MISMATCH,
            "the approval token was issued for a different tool or statement",
            TOKEN_MISMATCH_ACTION,
        ),
        // The caller handles a token that did authorize this call; a spent one
        // reaching here is still a refusal, never an execution.
        approval::Answer::Granted { .. } | approval::Answer::Denied { .. } => ToolError::token(
            APPROVAL_TOKEN_INVALID,
            "the approval token cannot authorize this call",
            TOKEN_INVALID_ACTION,
        ),
    }
}

/// Structured, non-error question a non-eliciting client relays to the human.
/// It carries the exact bytes the retry has to repeat and the one-shot token
/// that authorizes them; nothing has reached SQL Server.
fn approval_required_result(message: &str, issued: &approval::Issued) -> CallToolResult {
    let payload = json!({
        "code": APPROVAL_REQUIRED,
        "approval_token": issued.token,
        "approval_token_id": issued.token_id,
        "approval_message": message,
        "row_cap": issued.row_cap,
        "expires_in_seconds": issued.expires_in.as_secs(),
        "mutation_outcome": NOT_ATTEMPTED,
        "action": RELAY_ACTION,
    });
    let mut result = CallToolResult::structured(payload);
    result.content = vec![ContentBlock::text(format!(
        "{message}\n\nNothing has reached SQL Server. Repeat this call with \"approval_token\": \"{token}\" \
         and \"approval_decision\": \"grant\" or \"deny\". The token expires in {seconds} seconds \
         and can be used once.",
        token = issued.token,
        seconds = issued.expires_in.as_secs(),
    ))];
    result
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

/// One mutation call as the gate sees it: the question to ask, the exact
/// statement a token has to match, and the facts the audit record needs. Keeping
/// them in one value keeps every channel on the same decision path.
struct GateCall<'a> {
    tool: &'static str,
    subject: &'a Subject<'a>,
    statement: &'a Statement,
    row_cap: Option<u16>,
    started: Instant,
    scope: Option<Scope<'a>>,
}

/// Audit facts of one approval: the recorded outcome and, when a relay token
/// took part, its short label. The token itself is never recorded.
#[derive(Clone, Debug)]
struct Approval {
    outcome: &'static str,
    token_id: Option<String>,
}

impl Approval {
    fn audited(outcome: &'static str) -> Self {
        Self {
            outcome,
            token_id: None,
        }
    }

    fn relayed(outcome: &'static str, token_id: &str) -> Self {
        Self {
            outcome,
            token_id: Some(token_id.to_owned()),
        }
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
    /// Questions waiting for a relayed answer; one store per process.
    relay: approval::Relay,
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
            relay: approval::Relay::new(),
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
            match input.approval() {
                Some(subject) => {
                    let call = GateCall {
                        tool: name,
                        subject: &subject,
                        statement: &statement,
                        row_cap: Some(MAX_ROWS),
                        started,
                        scope: Some(scope),
                    };
                    match self.approve(call, input.relay(), context).await {
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
    /// it; every other call is answered by the client's own elicitation prompt
    /// when it advertises one, and by the relay when it does not. Both channels
    /// end in the same decision and the same audit facts.
    async fn approve(
        &self,
        call: GateCall<'_>,
        retry: Retry<'_>,
        context: &RequestContext<RoleServer>,
    ) -> Result<Approval, CallToolResult> {
        if self.yolo {
            return Ok(Approval::audited(Decision::Yolo.audit_name()));
        }
        if !elicitation_supported(&context.peer) {
            return self.redeem(call, retry);
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

    /// The relay channel: the first call asks and runs nothing, a retry answers
    /// the question and runs only when the token still authorizes these exact
    /// bytes. Nothing here opens a connection by itself.
    fn redeem(&self, call: GateCall<'_>, retry: Retry<'_>) -> Result<Approval, CallToolResult> {
        let now = Instant::now();
        match (retry.token, retry.decision) {
            (None, None) => {
                let issued = self.relay.issue(
                    now,
                    call.tool,
                    call.statement.clone(),
                    call.row_cap,
                    approval::RELAY_TTL,
                );
                Err(self.question(call, &issued))
            }
            (Some(token), Some(decision)) => {
                match self
                    .relay
                    .answer(now, token, decision, call.tool, call.statement)
                {
                    approval::Answer::Granted { token_id } => {
                        Ok(Approval::relayed(Decision::Granted.audit_name(), &token_id))
                    }
                    approval::Answer::Denied { token_id } => Err(self.failed(
                        call.tool,
                        call.started,
                        call.scope,
                        ToolError::approval_denied(),
                        Some(Approval::relayed(Decision::Denied.audit_name(), &token_id)),
                    )),
                    refusal => Err(self.failed(
                        call.tool,
                        call.started,
                        call.scope,
                        token_refusal(refusal),
                        None,
                    )),
                }
            }
            _ => Err(self.failed(
                call.tool,
                call.started,
                call.scope,
                ToolError::token(
                    APPROVAL_TOKEN_INVALID,
                    "approval_token and approval_decision must be supplied together",
                    TOKEN_INVALID_ACTION,
                ),
                None,
            )),
        }
    }

    /// Record and return the relay question. It is not an error: the client has
    /// to show it to the human and repeat the call with the answer.
    fn question(&self, call: GateCall<'_>, issued: &approval::Issued) -> CallToolResult {
        let message = call.subject.message();
        let approval = Approval::relayed(REQUIRED, &issued.token_id);
        self.record(
            Some(call.tool),
            call.started,
            Status::Ok,
            None,
            call.scope,
            Details {
                approval: Some(approval),
                ..Details::default()
            },
        );
        approval_required_result(&message, issued)
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
            statement: &statement,
            row_cap: None,
            started,
            scope: None,
        };
        let approval = match self.approve(call, input.relay(), context).await {
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
            approval_token_id: details
                .approval
                .as_ref()
                .and_then(|approval| approval.token_id.as_deref()),
        });
    }
}

/// Elicitation is the first channel whenever the client advertised it; the
/// relay answers every other client, including one whose capabilities are
/// unknown because the transport carries no client context per request.
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
            "Insert one row and report the number of affected rows; needs human approval, answered through elicitation or an approval_token retry.",
            schema_for_input::<InsertRow>().map_err(|_| "tool schema generation failed")?,
        )
        .with_annotations(insert()),
        Tool::new(
            Known::UpdateRows.name(),
            "Update at most 100 rows matching equality filters; needs human approval, answered through elicitation or an approval_token retry.",
            schema_for_input::<UpdateRows>().map_err(|_| "tool schema generation failed")?,
        )
        .with_annotations(destructive()),
        Tool::new(
            Known::DeleteRows.name(),
            "Delete at most 100 rows matching equality filters; needs human approval, answered through elicitation or an approval_token retry.",
            schema_for_input::<DeleteRows>().map_err(|_| "tool schema generation failed")?,
        )
        .with_annotations(destructive()),
        Tool::new(
            Known::Query.name(),
            "Run one arbitrary T-SQL batch under the configured login after approval, answered through elicitation or an approval_token retry; at most one result set is returned, bounded to 100 rows.",
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

    fn question(row_cap: Option<u16>) -> Value {
        let issued = approval::Issued {
            token: "0123456789abcdef0123456789abcdef".to_owned(),
            token_id: "01234567".to_owned(),
            expires_in: approval::RELAY_TTL,
            row_cap,
        };
        let result = approval_required_result("Approve insert_row on dbo.Items.", &issued);
        serde_json::to_value(result).expect("a question serializes to JSON")
    }

    #[test]
    fn the_relay_question_is_not_an_error_and_carries_what_the_human_sees() {
        let json = question(Some(MAX_ROWS));
        assert_eq!(json["isError"], false, "{json}");
        let payload = &json["structuredContent"];
        assert_eq!(payload["code"], APPROVAL_REQUIRED, "{json}");
        assert_eq!(
            payload["approval_token"], "0123456789abcdef0123456789abcdef",
            "{json}"
        );
        assert_eq!(payload["approval_token_id"], "01234567", "{json}");
        assert_eq!(
            payload["approval_message"],
            "Approve insert_row on dbo.Items."
        );
        assert_eq!(payload["row_cap"], MAX_ROWS, "{json}");
        assert_eq!(payload["expires_in_seconds"], 120, "{json}");
        assert_eq!(payload["mutation_outcome"], NOT_ATTEMPTED, "{json}");
        assert!(
            payload["action"]
                .as_str()
                .is_some_and(|action| action.contains("approval_decision")),
            "{json}"
        );
        let text = json["content"][0]["text"]
            .as_str()
            .unwrap_or_else(|| panic!("a question carries text: {json}"));
        for expected in [
            "Approve insert_row on dbo.Items.",
            "0123456789abcdef0123456789abcdef",
            "grant",
            "deny",
            "Nothing has reached SQL Server",
        ] {
            assert!(text.contains(expected), "{text}");
        }

        // A raw batch has no affected-row cap, so the question says so instead
        // of inventing one.
        assert_eq!(question(None)["structuredContent"]["row_cap"], Value::Null);
    }

    #[test]
    fn every_unusable_token_is_a_named_refusal_that_claims_no_outcome() {
        for (answer, code) in [
            (approval::Answer::Unknown, APPROVAL_TOKEN_UNKNOWN),
            (approval::Answer::Expired, APPROVAL_TOKEN_EXPIRED),
            (approval::Answer::Reused, APPROVAL_TOKEN_REUSED),
            (approval::Answer::Mismatch, APPROVAL_TOKEN_MISMATCH),
            (
                approval::Answer::Granted {
                    token_id: "01234567".to_owned(),
                },
                APPROVAL_TOKEN_INVALID,
            ),
            (
                approval::Answer::Denied {
                    token_id: "01234567".to_owned(),
                },
                APPROVAL_TOKEN_INVALID,
            ),
        ] {
            let json = wire(token_refusal(answer.clone()));
            assert_eq!(json["isError"], true, "{answer:?}: {json}");
            assert_eq!(
                json["structuredContent"]["code"], code,
                "{answer:?}: {json}"
            );
            assert_eq!(
                json["structuredContent"]["mutation_outcome"], NOT_ATTEMPTED,
                "{answer:?}: {json}"
            );
            assert_eq!(json["structuredContent"]["retryable"], false, "{json}");
            assert!(
                json["structuredContent"]["action"]
                    .as_str()
                    .is_some_and(|action| !action.is_empty()),
                "{answer:?}: {json}"
            );
        }
    }
}
