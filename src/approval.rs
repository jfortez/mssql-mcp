//! Human approval gate for the mutation tools and the raw `query` batch.
//!
//! Approval is server-side and fail-closed, and it has two channels that end in
//! one decision.
//!
//! * Elicitation, tried first: when the client advertises the MCP elicitation
//!   capability the gate asks it directly. The prompt for the table tools names
//!   only the tool, the target object, the column names and the row cap: SQL
//!   text, values, filter contents, credentials and the host stay out of it. The
//!   raw `query` prompt is different by design: it carries the exact batch so
//!   the human consents to the real bytes, and that text never reaches the audit
//!   trail or an error message.
//! * The relay, for every other client: the first call runs nothing and returns
//!   a structured, non-error `approval_required` result with a one-shot token
//!   and the exact question. The client relays that question to the human and
//!   repeats the same call with `approval_token` and `approval_decision`; only a
//!   known, unexpired and unused token whose statement is byte-identical to the
//!   one it was issued for runs anything, exactly once. A denial is an answer
//!   too: it is recorded, dropped and never replayed.
//!
//! `--yolo` (or `MSSQL_MCP_YOLO=true`) skips both channels and is audited as
//! such.

use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hash, Hasher};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rmcp::Peer;
use rmcp::RoleServer;
use rmcp::ServiceError;
use rmcp::service::ElicitationError;
use schemars::JsonSchema;
use serde::Deserialize;

use crate::query::Query;
use crate::statement::{
    DeleteRows, DescribeTable, InsertRow, ListTables, SelectRows, Statement, Table, UpdateRows,
};

/// How long a human has to answer the approval prompt.
pub const TIMEOUT: Duration = Duration::from_secs(120);
/// How long a relayed question stays answerable. It matches the prompt timeout:
/// an abandoned question is refused long before a later call could redeem it.
pub const RELAY_TTL: Duration = Duration::from_secs(120);
/// Mutation row cap, mirrored from the statement builder so the prompt can name
/// it without exposing SQL.
const ROW_CAP: u16 = 100;
/// Questions kept at once. Beyond this the oldest pending question is dropped,
/// which only makes its retry an unknown token.
const MAX_PENDING: usize = 16;
/// Answered tokens remembered so a replay is named as reuse instead of as an
/// unknown token.
const MAX_ANSWERED: usize = 16;

/// Typed answer requested from the human: only an explicit `true` runs the
/// mutation.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct Confirmation {
    /// Set to true to run this mutation; false refuses it.
    pub confirm: bool,
}

rmcp::elicit_safe!(Confirmation);

/// Answer a client relays back to the server on the human's behalf.
#[derive(Clone, Copy, Debug, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ApprovalDecision {
    /// The human accepted the question; the statement may run once.
    Grant,
    /// The human refused the question; nothing runs and the token is spent.
    Deny,
}

/// The relay half of a mutation call: the optional token and the optional
/// answer. Both are absent on a first call, which is what issues the question.
#[derive(Clone, Copy, Debug)]
pub struct Retry<'a> {
    pub token: Option<&'a str>,
    pub decision: Option<ApprovalDecision>,
}

impl Retry<'_> {
    pub const fn none() -> Self {
        Self {
            token: None,
            decision: None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    /// The operator opted out with `--yolo` or `MSSQL_MCP_YOLO`.
    Yolo,
    /// The human accepted the prompt.
    Granted,
    /// The human declined or cancelled, the prompt timed out, or the answer was
    /// not an explicit confirmation.
    Denied,
    /// The client cannot be asked, or asking it failed at the transport level.
    Unavailable,
}

impl Decision {
    /// Value recorded in the audit trail for a mutation call.
    pub const fn audit_name(self) -> &'static str {
        match self {
            Self::Yolo => "yolo",
            Self::Granted => "granted",
            Self::Denied => "denied",
            Self::Unavailable => "unavailable",
        }
    }

    /// Only a granted or bypassed decision may reach the database.
    pub const fn permitted(self) -> bool {
        matches!(self, Self::Yolo | Self::Granted)
    }
}

/// Facts a mutation discloses to the human, all of them validated identifiers,
/// or the exact raw batch for `query`.
pub struct Subject<'a> {
    prompt: Prompt<'a>,
}

enum Prompt<'a> {
    Object {
        tool: &'static str,
        schema: &'a str,
        table: &'a str,
        value_columns: Vec<&'a str>,
        filter_columns: Vec<&'a str>,
    },
    Batch {
        tool: &'static str,
        sql: &'a str,
    },
}

impl<'a> Subject<'a> {
    fn new(
        tool: &'static str,
        table: &'a Table,
        value_columns: Vec<&'a str>,
        filter_columns: Vec<&'a str>,
    ) -> Self {
        Self {
            prompt: Prompt::Object {
                tool,
                schema: table.schema.as_deref().unwrap_or("dbo"),
                table: table.name.as_str(),
                value_columns,
                filter_columns,
            },
        }
    }

    /// Prompt for the raw `query` tool: the exact batch is shown so the human
    /// consents to the real bytes. Nothing else about `query` is redacted here;
    /// the audit trail and error messages still never receive SQL.
    pub const fn batch(tool: &'static str, sql: &'a str) -> Self {
        Self {
            prompt: Prompt::Batch { tool, sql },
        }
    }

    /// Prompt shown to the human before the statement runs. The relay returns
    /// this same text, so both channels ask the same question.
    pub fn message(&self) -> String {
        match &self.prompt {
            Prompt::Object {
                tool,
                schema,
                table,
                value_columns,
                filter_columns,
            } => format!(
                "Approve {tool} on {schema}.{table}. Value columns: {values}. Filter columns: {filters}. \
                 At most {ROW_CAP} rows are targeted; a larger change is rolled back.",
                values = columns(value_columns),
                filters = columns(filter_columns),
            ),
            Prompt::Batch { tool, sql } => format!(
                "Approve {tool}: this runs the following T-SQL batch under the configured principal. \
                 It may affect any data or server state that principal can reach, has no row limit, \
                 and its outcome is unknown if the call fails or times out. The exact batch is:\n{sql}"
            ),
        }
    }
}

fn columns(names: &[&str]) -> String {
    if names.is_empty() {
        "none".to_owned()
    } else {
        names.join(", ")
    }
}

/// Input types that can describe a mutation approval.
pub trait Approves {
    /// `None` for reads, which never prompt.
    fn approval(&self) -> Option<Subject<'_>>;

    /// Relay arguments of this call. Every tool whose `approval` is `None` keeps
    /// the default, and so does a first call of a mutation.
    fn relay(&self) -> Retry<'_> {
        Retry::none()
    }
}

impl Approves for InsertRow {
    fn approval(&self) -> Option<Subject<'_>> {
        Some(Subject::new(
            "insert_row",
            &self.table,
            self.values.keys().map(String::as_str).collect(),
            Vec::new(),
        ))
    }

    fn relay(&self) -> Retry<'_> {
        Retry {
            token: self.approval_token.as_deref(),
            decision: self.approval_decision,
        }
    }
}

impl Approves for UpdateRows {
    fn approval(&self) -> Option<Subject<'_>> {
        Some(Subject::new(
            "update_rows",
            &self.table,
            self.values.keys().map(String::as_str).collect(),
            self.filters.keys().map(String::as_str).collect(),
        ))
    }

    fn relay(&self) -> Retry<'_> {
        Retry {
            token: self.approval_token.as_deref(),
            decision: self.approval_decision,
        }
    }
}

impl Approves for DeleteRows {
    fn approval(&self) -> Option<Subject<'_>> {
        Some(Subject::new(
            "delete_rows",
            &self.table,
            Vec::new(),
            self.filters.keys().map(String::as_str).collect(),
        ))
    }

    fn relay(&self) -> Retry<'_> {
        Retry {
            token: self.approval_token.as_deref(),
            decision: self.approval_decision,
        }
    }
}

impl Approves for Query {
    fn approval(&self) -> Option<Subject<'_>> {
        Some(Subject::batch("query", &self.sql))
    }

    fn relay(&self) -> Retry<'_> {
        Retry {
            token: self.approval_token.as_deref(),
            decision: self.approval_decision,
        }
    }
}

impl Approves for ListTables {
    fn approval(&self) -> Option<Subject<'_>> {
        None
    }
}

impl Approves for DescribeTable {
    fn approval(&self) -> Option<Subject<'_>> {
        None
    }
}

impl Approves for SelectRows {
    fn approval(&self) -> Option<Subject<'_>> {
        None
    }
}

/// Ask the human to approve one mutation through MCP elicitation. The caller
/// handles the `--yolo` bypass and only reaches this when the client advertised
/// the capability.
pub async fn gate(peer: &Peer<RoleServer>, subject: &Subject<'_>) -> Decision {
    match peer
        .elicit_with_timeout::<Confirmation>(subject.message(), Some(TIMEOUT))
        .await
    {
        Ok(Some(answer)) if answer.confirm => Decision::Granted,
        Ok(_) => Decision::Denied,
        Err(ElicitationError::UserDeclined | ElicitationError::UserCancelled) => Decision::Denied,
        Err(ElicitationError::ParseError { .. } | ElicitationError::NoContent) => Decision::Denied,
        Err(ElicitationError::Service(ServiceError::Timeout { .. })) => Decision::Denied,
        Err(ElicitationError::CapabilityNotSupported) => Decision::Unavailable,
        Err(_) => Decision::Unavailable,
    }
}

/// One issued question: the exact statement it authorizes and when it stops
/// being answerable. Values never leave this struct: nothing here is recorded.
struct Pending {
    token: String,
    tool: &'static str,
    statement: Statement,
    expires: Instant,
}

/// An answered question, kept until it would have expired so a replay is named
/// as reuse rather than as an unknown token.
struct Answered {
    token: String,
    expires: Instant,
}

/// A question this process is waiting on.
pub struct Issued {
    pub token: String,
    /// Short label of the token, safe to record.
    pub token_id: String,
    pub expires_in: Duration,
    pub row_cap: Option<u16>,
}

/// Verdict for one relayed retry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Answer {
    /// The token matched this call, was fresh and is now spent.
    Granted { token_id: String },
    /// The human refused; the token is spent and nothing runs.
    Denied { token_id: String },
    /// No pending question carries this token.
    Unknown,
    /// The question expired before it was answered.
    Expired,
    /// The token was already answered, granted or denied.
    Reused,
    /// The token was issued for a different tool or statement.
    Mismatch,
}

/// Pending relay questions. One instance per server, so a token is only ever
/// redeemable in the process that issued it.
#[derive(Default)]
pub struct Relay {
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    pending: Vec<Pending>,
    answered: Vec<Answered>,
}

impl Relay {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register the question a retry has to answer. The oldest pending question
    /// is dropped when the bounded store is full.
    pub fn issue(
        &self,
        now: Instant,
        tool: &'static str,
        statement: Statement,
        row_cap: Option<u16>,
        ttl: Duration,
    ) -> Issued {
        let token = new_token();
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if state.pending.len() >= MAX_PENDING {
            state.pending.remove(0);
        }
        let expires = now + ttl;
        state.pending.push(Pending {
            token: token.clone(),
            tool,
            statement,
            expires,
        });
        Issued {
            token_id: token_id(&token).to_owned(),
            token,
            expires_in: ttl,
            row_cap,
        }
    }

    /// Resolve one relayed retry. The order is deliberate: a token that is
    /// expired, unknown or issued for different bytes never consumes the pending
    /// question and never runs anything.
    pub fn answer(
        &self,
        now: Instant,
        token: &str,
        decision: ApprovalDecision,
        tool: &'static str,
        statement: &Statement,
    ) -> Answer {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        state.answered.retain(|answered| answered.expires > now);
        let Some(index) = state
            .pending
            .iter()
            .position(|pending| pending.token == token)
        else {
            return if state
                .answered
                .iter()
                .any(|answered| answered.token == token)
            {
                Answer::Reused
            } else {
                Answer::Unknown
            };
        };
        if state.pending[index].expires <= now {
            state.pending.remove(index);
            return Answer::Expired;
        }
        if state.pending[index].tool != tool || state.pending[index].statement != *statement {
            return Answer::Mismatch;
        }
        let pending = state.pending.remove(index);
        let token_id = token_id(&pending.token).to_owned();
        state.answered.push(Answered {
            token: pending.token,
            expires: pending.expires,
        });
        while state.answered.len() > MAX_ANSWERED {
            state.answered.remove(0);
        }
        match decision {
            ApprovalDecision::Grant => Answer::Granted { token_id },
            ApprovalDecision::Deny => Answer::Denied { token_id },
        }
    }
}

/// Stable, non-secret label of a token: its first eight hex characters. Enough
/// to correlate a question with its answer in the audit trail and useless for
/// redemption.
pub fn token_id(token: &str) -> &str {
    token.get(..8).unwrap_or(token)
}

/// Counts tokens so two issued in the same nanosecond still differ.
static SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// 128 bits as 32 lowercase hex characters, taken from two fresh instances of
/// std's randomly seeded hasher. `RandomState` keys come from operating-system
/// entropy, so the output is unguessable without adding a dependency.
fn new_token() -> String {
    let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos());
    let mut token = String::with_capacity(32);
    for salt in 0u64..2 {
        let mut hasher = RandomState::new().build_hasher();
        salt.hash(&mut hasher);
        sequence.hash(&mut hasher);
        nanos.hash(&mut hasher);
        std::process::id().hash(&mut hasher);
        token.push_str(&format!("{:016x}", hasher.finish()));
    }
    token
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use serde_json::json;

    use super::*;

    fn insert(id: i64) -> Statement {
        let mut values = BTreeMap::new();
        values.insert("id".to_owned(), json!(id));
        InsertRow {
            table: Table {
                name: "Items".to_owned(),
                schema: None,
            },
            values,
            approval_token: None,
            approval_decision: None,
        }
        .prepare()
        .expect("a valid insert builds a statement")
    }

    fn delete() -> Statement {
        DeleteRows {
            table: Table {
                name: "Items".to_owned(),
                schema: None,
            },
            filters: BTreeMap::from([("id".to_owned(), json!(1))]),
            approval_token: None,
            approval_decision: None,
        }
        .prepare()
        .expect("a valid delete builds a statement")
    }

    #[test]
    fn a_token_authorizes_only_the_statement_it_was_issued_for() {
        let relay = Relay::new();
        let now = Instant::now();
        let issued = relay.issue(now, "insert_row", insert(1), Some(ROW_CAP), RELAY_TTL);
        assert_eq!(issued.row_cap, Some(ROW_CAP));
        assert_eq!(issued.expires_in, RELAY_TTL);
        assert_eq!(issued.token_id, token_id(&issued.token));

        // A different batch, and a different tool with the same batch, are both
        // refused and neither consumes the pending question.
        assert_eq!(
            relay.answer(
                now,
                &issued.token,
                ApprovalDecision::Grant,
                "insert_row",
                &insert(2)
            ),
            Answer::Mismatch
        );
        assert_eq!(
            relay.answer(
                now,
                &issued.token,
                ApprovalDecision::Grant,
                "query",
                &insert(1)
            ),
            Answer::Mismatch
        );
        assert_eq!(
            relay.answer(
                now,
                &issued.token,
                ApprovalDecision::Grant,
                "insert_row",
                &insert(1)
            ),
            Answer::Granted {
                token_id: issued.token_id.clone()
            }
        );
    }

    #[test]
    fn an_answered_token_can_never_run_a_second_time() {
        let relay = Relay::new();
        let now = Instant::now();
        let issued = relay.issue(now, "insert_row", insert(1), Some(ROW_CAP), RELAY_TTL);
        assert!(matches!(
            relay.answer(
                now,
                &issued.token,
                ApprovalDecision::Grant,
                "insert_row",
                &insert(1)
            ),
            Answer::Granted { .. }
        ));
        for decision in [ApprovalDecision::Grant, ApprovalDecision::Deny] {
            assert_eq!(
                relay.answer(now, &issued.token, decision, "insert_row", &insert(1)),
                Answer::Reused,
                "{decision:?}"
            );
        }
    }

    #[test]
    fn an_abandoned_question_expires_and_then_vanishes() {
        let relay = Relay::new();
        let now = Instant::now();
        let issued = relay.issue(
            now,
            "insert_row",
            insert(1),
            Some(ROW_CAP),
            Duration::from_secs(30),
        );
        let late = now + Duration::from_secs(30);
        assert_eq!(
            relay.answer(
                late,
                &issued.token,
                ApprovalDecision::Grant,
                "insert_row",
                &insert(1)
            ),
            Answer::Expired
        );
        assert_eq!(
            relay.answer(
                late,
                &issued.token,
                ApprovalDecision::Deny,
                "insert_row",
                &insert(1)
            ),
            Answer::Unknown
        );
    }

    #[test]
    fn a_denial_is_an_answer_and_never_runs_or_replays() {
        let relay = Relay::new();
        let now = Instant::now();
        let issued = relay.issue(now, "delete_rows", delete(), Some(ROW_CAP), RELAY_TTL);
        assert_eq!(
            relay.answer(
                now,
                &issued.token,
                ApprovalDecision::Deny,
                "delete_rows",
                &delete()
            ),
            Answer::Denied {
                token_id: issued.token_id.clone()
            }
        );
        assert_eq!(
            relay.answer(
                now,
                &issued.token,
                ApprovalDecision::Grant,
                "delete_rows",
                &delete()
            ),
            Answer::Reused
        );
    }

    #[test]
    fn a_denial_for_other_bytes_is_refused_without_spending_the_question() {
        let relay = Relay::new();
        let now = Instant::now();
        let issued = relay.issue(now, "insert_row", insert(1), Some(ROW_CAP), RELAY_TTL);
        assert_eq!(
            relay.answer(
                now,
                &issued.token,
                ApprovalDecision::Deny,
                "insert_row",
                &insert(2)
            ),
            Answer::Mismatch
        );
        assert!(matches!(
            relay.answer(
                now,
                &issued.token,
                ApprovalDecision::Grant,
                "insert_row",
                &insert(1)
            ),
            Answer::Granted { .. }
        ));
    }

    #[test]
    fn an_unknown_token_never_authorizes_anything() {
        let relay = Relay::new();
        let now = Instant::now();
        relay.issue(now, "insert_row", insert(1), Some(ROW_CAP), RELAY_TTL);
        for token in ["", "0", "0123456789abcdef0123456789abcdef"] {
            assert_eq!(
                relay.answer(
                    now,
                    token,
                    ApprovalDecision::Grant,
                    "insert_row",
                    &insert(1)
                ),
                Answer::Unknown,
                "{token:?}"
            );
        }
    }

    #[test]
    fn the_pending_store_is_bounded() {
        let relay = Relay::new();
        let now = Instant::now();
        let first = relay.issue(now, "insert_row", insert(1), Some(ROW_CAP), RELAY_TTL);
        for id in 2..=MAX_PENDING as i64 + 1 {
            relay.issue(now, "insert_row", insert(id), Some(ROW_CAP), RELAY_TTL);
        }
        assert_eq!(
            relay.answer(
                now,
                &first.token,
                ApprovalDecision::Grant,
                "insert_row",
                &insert(1)
            ),
            Answer::Unknown,
            "the oldest question is dropped instead of growing the store"
        );
    }

    #[test]
    fn tokens_are_unguessable_labels_and_trailing_questions_stay_answerable() {
        let first = new_token();
        let second = new_token();
        assert_ne!(first, second);
        assert_eq!(first.len(), 32);
        assert!(first.chars().all(|character| character.is_ascii_hexdigit()));
        assert_eq!(token_id(&first), &first[..8]);
        assert_eq!(token_id(&first), token_id(&first), "the label is stable");
        assert_ne!(token_id(&first), token_id(&second));
        assert_eq!(token_id("short"), "short");

        let relay = Relay::new();
        let now = Instant::now();
        relay.issue(now, "insert_row", insert(1), Some(ROW_CAP), RELAY_TTL);
        let last = relay.issue(now, "insert_row", insert(2), Some(ROW_CAP), RELAY_TTL);
        assert!(matches!(
            relay.answer(
                now,
                &last.token,
                ApprovalDecision::Grant,
                "insert_row",
                &insert(2)
            ),
            Answer::Granted { .. }
        ));
    }

    #[test]
    fn both_channels_ask_the_same_question_for_a_table_mutation() {
        let input = InsertRow {
            table: Table {
                schema: Some("sales".to_owned()),
                name: "Items".to_owned(),
            },
            values: BTreeMap::from([("label".to_owned(), json!("secret-value"))]),
            approval_token: None,
            approval_decision: None,
        };
        let subject = input.approval().expect("a mutation has a subject");
        let message = subject.message();
        for expected in ["insert_row", "sales.Items", "label", "100"] {
            assert!(message.contains(expected), "{message}");
        }
        assert!(
            !message.contains("secret-value") && !message.contains("INSERT"),
            "{message}"
        );
        assert_eq!(input.relay().token, None);
        assert_eq!(input.relay().decision, None);
    }

    #[test]
    fn a_retry_exposes_its_token_and_decision() {
        let input = InsertRow {
            table: Table {
                name: "Items".to_owned(),
                schema: None,
            },
            values: BTreeMap::from([("id".to_owned(), json!(1))]),
            approval_token: Some("0123456789abcdef0123456789abcdef".to_owned()),
            approval_decision: Some(ApprovalDecision::Deny),
        };
        let retry = input.relay();
        assert_eq!(retry.token, Some("0123456789abcdef0123456789abcdef"));
        assert_eq!(retry.decision, Some(ApprovalDecision::Deny));
    }
}
