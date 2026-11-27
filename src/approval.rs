//! Human approval gate for the mutation tools and the raw `query` batch.
//!
//! Approval is server-side and fail-closed, and there is exactly one channel:
//! the native MCP form elicitation prompt the server sends to the client. A
//! mutation runs only when the operator trusted this stdio host with
//! `--trust-elicitation`, the client advertised form elicitation, and the human
//! answered the prompt for this prepared operation with an explicit
//! confirmation. Every other call is refused with `approval_unavailable` before
//! a connection is opened, and no approval can be supplied as a tool argument.
//!
//! The prompt for the table tools names the tool, the unambiguous target and
//! the exact values to write and equality-filter values to match, so the human
//! consents to the actual change: SQL text, credentials and the host stay out of
//! it. The raw `query` prompt is different by design: it carries the exact batch
//! so the human consents to the real bytes. Neither form reaches the audit trail,
//! an error message or a tool result; the installed host must show it to the
//! human and keep it out of logs and model-visible output.
//!
//! Trust cannot be established by the protocol: a host can advertise form
//! elicitation and answer its own prompt. `--trust-elicitation` is therefore an
//! operator promise about the installed host, and `--yolo` (or
//! `MSSQL_MCP_YOLO=true`) remains the explicit, documented bypass.

use std::fmt::Write as _;
use std::time::Duration;

use rmcp::Peer;
use rmcp::RoleServer;
use rmcp::ServiceError;
use rmcp::service::ElicitationError;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

use crate::query::Query;
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
    /// No trusted prompt is available, or asking it failed at the transport
    /// level.
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

/// Facts a mutation discloses to the human: validated identifiers, the exact
/// validated scalar values and filters to match, or the exact raw batch for
/// `query`.
pub struct Subject<'a> {
    prompt: Prompt<'a>,
}

enum Prompt<'a> {
    Object {
        tool: &'static str,
        schema: &'a str,
        table: &'a str,
        values: Vec<(&'a str, &'a Value)>,
        filters: Vec<(&'a str, &'a Value)>,
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
        values: Vec<(&'a str, &'a Value)>,
        filters: Vec<(&'a str, &'a Value)>,
    ) -> Self {
        Self {
            prompt: Prompt::Object {
                tool,
                schema: table.schema.as_deref().unwrap_or("dbo"),
                table: table.name.as_str(),
                values,
                filters,
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

    /// Question shown to the human before the statement runs.
    pub fn message(&self) -> String {
        match &self.prompt {
            Prompt::Object {
                tool,
                schema,
                table,
                values,
                filters,
            } => format!(
                "Approve {tool} on schema {}, table {}. \
                 Values to write (JSON; null is SQL NULL, \"null\" is text): {values}. \
                 Filters (equality predicates joined with AND; JSON null matches SQL NULL): {filters}. \
                 At most {ROW_CAP} rows are targeted; a larger change is rolled back.",
                json_string(schema),
                json_string(table),
                values = describe(values, "no column values are written"),
                filters = describe(filters, "no existing rows are matched"),
            ),
            Prompt::Batch { tool, sql } => format!(
                "Approve {tool}: this runs the following T-SQL batch under the configured principal. \
                 It may affect any data or server state that principal can reach, has no row limit, \
                 and its outcome is unknown if the call fails or times out. The exact batch is:\n{sql}"
            ),
        }
    }
}

/// Deterministic escaped JSON object for already validated scalar pairs: keys
/// keep their validated spelling, values keep their JSON type, and nothing is
/// truncated. A null stays JSON null so the human sees SQL NULL rather than a
/// text value.
fn describe(pairs: &[(&str, &Value)], empty: &'static str) -> String {
    if pairs.is_empty() {
        return format!("none ({empty})");
    }
    let entries = pairs
        .iter()
        .map(|(column, value)| {
            let value = match value {
                Value::String(text) => json_string(text),
                scalar => scalar.to_string(),
            };
            format!("{}:{value}", json_string(column))
        })
        .collect::<Vec<_>>()
        .join(",");
    format!("{{{entries}}}")
}

/// Escape not just JSON delimiters but also controls that can reorder or
/// split the displayed text. Every escape decodes to the original character.
fn json_string(text: &str) -> String {
    let json = Value::String(text.to_owned()).to_string();
    let mut display = String::with_capacity(json.len());
    for ch in json.chars() {
        if ch.is_control()
            || matches!(
                ch,
                '\u{061c}' | '\u{200b}'..='\u{200f}' | '\u{2028}'..='\u{202e}'
                    | '\u{2066}'..='\u{206f}' | '\u{feff}'
            )
        {
            write!(display, "\\u{:04x}", u32::from(ch)).expect("writing to String cannot fail");
        } else {
            display.push(ch);
        }
    }
    display
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
            self.values
                .iter()
                .map(|(column, value)| (column.as_str(), value))
                .collect(),
            Vec::new(),
        ))
    }
}

impl Approves for UpdateRows {
    fn approval(&self) -> Option<Subject<'_>> {
        Some(Subject::new(
            "update_rows",
            &self.table,
            self.values
                .iter()
                .map(|(column, value)| (column.as_str(), value))
                .collect(),
            self.filters
                .iter()
                .map(|(column, value)| (column.as_str(), value))
                .collect(),
        ))
    }
}

impl Approves for DeleteRows {
    fn approval(&self) -> Option<Subject<'_>> {
        Some(Subject::new(
            "delete_rows",
            &self.table,
            Vec::new(),
            self.filters
                .iter()
                .map(|(column, value)| (column.as_str(), value))
                .collect(),
        ))
    }
}

impl Approves for Query {
    fn approval(&self) -> Option<Subject<'_>> {
        Some(Subject::batch("query", &self.sql))
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

/// Ask the human to approve one mutation through native MCP form elicitation.
/// The caller handles the `--yolo` bypass and the trusted operator opt-in, and
/// only reaches this when both allow the prompt.
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

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use serde_json::json;

    use super::*;

    #[test]
    fn a_table_mutation_prompt_carries_exact_values_and_filters_without_sql() {
        let input = UpdateRows {
            table: Table {
                schema: Some("sales".to_owned()),
                name: "Items".to_owned(),
            },
            values: BTreeMap::from([
                ("label".to_owned(), json!("text \"quoted\"\nline")),
                ("qty".to_owned(), json!(2)),
                ("active".to_owned(), json!(true)),
                ("cleared".to_owned(), json!(null)),
            ]),
            filters: BTreeMap::from([
                ("id".to_owned(), json!(7)),
                ("region".to_owned(), json!("null")),
                ("missing".to_owned(), json!(null)),
            ]),
        };
        let subject = input.approval().expect("a mutation has a subject");
        let message = subject.message();
        for expected in [
            "update_rows",
            r#"schema "sales", table "Items""#,
            r#"{"active":true,"cleared":null,"label":"text \"quoted\"\nline","qty":2}"#,
            r#"{"id":7,"missing":null,"region":"null"}"#,
            "equality predicates joined with AND",
            "JSON null matches SQL NULL",
            "At most 100 rows are targeted; a larger change is rolled back.",
        ] {
            assert!(message.contains(expected), "{message}");
        }
        for unexpected in ["UPDATE", "SET ", "@P1"] {
            assert!(!message.contains(unexpected), "{message}");
        }
        assert!(
            message.chars().all(|character| !character.is_control()),
            "{message}"
        );
    }

    #[test]
    fn non_applicable_details_are_marked_explicitly() {
        let insert = InsertRow {
            table: Table {
                schema: None,
                name: "Items".to_owned(),
            },
            values: BTreeMap::from([("label".to_owned(), json!("x"))]),
        };
        let message = insert
            .approval()
            .expect("a mutation has a subject")
            .message();
        assert!(
            message.contains(r#"insert_row on schema "dbo", table "Items""#),
            "{message}"
        );
        assert!(message.contains(r#"{"label":"x"}"#), "{message}");
        assert!(
            message.contains(
                "Filters (equality predicates joined with AND; JSON null matches SQL NULL): \
                 none (no existing rows are matched)"
            ),
            "{message}"
        );

        let delete = DeleteRows {
            table: Table {
                schema: None,
                name: "Items".to_owned(),
            },
            filters: BTreeMap::from([("id".to_owned(), json!(1))]),
        };
        let message = delete
            .approval()
            .expect("a mutation has a subject")
            .message();
        assert!(
            message.contains(r#"delete_rows on schema "dbo", table "Items""#),
            "{message}"
        );
        assert!(message.contains(r#"{"id":1}"#), "{message}");
        assert!(
            message.contains(
                "Values to write (JSON; null is SQL NULL, \"null\" is text): \
                 none (no column values are written)"
            ),
            "{message}"
        );
    }

    #[test]
    fn target_schema_and_table_stay_unambiguous() {
        let input = DeleteRows {
            table: Table {
                schema: Some("a.b".to_owned()),
                name: "c.d".to_owned(),
            },
            filters: BTreeMap::from([("id".to_owned(), json!(1))]),
        };
        let message = input
            .approval()
            .expect("a mutation has a subject")
            .message();
        assert!(
            message.contains(r#"schema "a.b", table "c.d""#),
            "{message}"
        );
    }

    #[test]
    fn a_batch_prompt_carries_the_exact_sql_and_no_approval_arguments() {
        let sql = "CREATE PROCEDURE dbo.usp_sum @a int, @b int AS SELECT @a + @b AS total";
        let input = Query {
            sql: sql.to_owned(),
            limit: None,
        };
        let subject = input.approval().expect("query always needs approval");
        let message = subject.message();
        assert!(message.contains("query"), "{message}");
        assert!(
            message.contains(sql),
            "the prompt must carry the exact batch"
        );
        assert!(message.contains("no row limit"), "{message}");
    }

    #[test]
    fn reads_have_no_approval_subject() {
        assert!(
            ListTables {
                schema: None,
                offset: 0,
            }
            .approval()
            .is_none()
        );
        assert!(
            DescribeTable {
                table: Table {
                    name: "Items".to_owned(),
                    schema: None,
                },
                offset: 0,
            }
            .approval()
            .is_none()
        );
        assert!(
            SelectRows {
                table: Table {
                    name: "Items".to_owned(),
                    schema: None,
                },
                columns: vec!["id".to_owned()],
                filters: BTreeMap::new(),
                order_by: Vec::new(),
                offset: 0,
                limit: None,
            }
            .approval()
            .is_none()
        );
    }
}
