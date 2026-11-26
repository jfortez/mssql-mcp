//! Human approval gate for the mutation tools and the raw `query` batch.
//!
//! Approval is server-side and fail-closed. A mutation asks the client for a
//! typed confirmation before any connection is opened, so a connection failure
//! can never be confused with an approval failure. The prompt for the table
//! tools names only the tool, the target object, the column names and the row
//! cap: SQL text, values, filter contents, credentials and the host stay out of
//! it. The raw `query` prompt is different by design: it carries the exact
//! batch so the human consents to the real bytes, and that text never reaches
//! the audit trail or an error message. `--yolo` (or `MSSQL_MCP_YOLO=true`)
//! skips the prompt and is audited as such.

use std::time::Duration;

use rmcp::Peer;
use rmcp::RoleServer;
use rmcp::ServiceError;
use rmcp::service::ElicitationError;
use schemars::JsonSchema;
use serde::Deserialize;

use crate::statement::{
    DeleteRows, DescribeTable, InsertRow, ListTables, SelectRows, Table, UpdateRows,
};

/// How long a human has to answer the approval prompt.
pub const TIMEOUT: Duration = Duration::from_secs(120);
/// Mutation row cap, mirrored from the statement builder so the prompt can name
/// it without exposing SQL.
const ROW_CAP: u16 = 100;

/// Typed answer requested from the human: only an explicit `true` runs the
/// mutation.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct Confirmation {
    /// Set to true to run this mutation; false refuses it.
    pub confirm: bool,
}

rmcp::elicit_safe!(Confirmation);

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

    /// Prompt shown to the human before the statement runs.
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

/// Ask the human to approve one mutation.
pub async fn gate(peer: &Peer<RoleServer>, subject: &Subject<'_>, yolo: bool) -> Decision {
    if yolo {
        return Decision::Yolo;
    }
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
