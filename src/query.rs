//! `query` tool: one arbitrary T-SQL batch executed under the configured
//! principal.
//!
//! There is no statement classifier and no affected-row guard: every batch asks
//! a trusted human for approval with the exact SQL in the question, and SQL
//! Server permissions decide what it can do. Only the result set is bounded.
//! Without a trusted approval channel the call fails closed before connecting.

use schemars::JsonSchema;
use serde::Deserialize;

use crate::statement::Statement;

/// Longest accepted batch in UTF-16 units. The approval prompt carries the
/// exact batch, so this keeps the prompt inside the transport frame budget.
const MAX_SQL_UNITS: usize = 32_768;
const DEFAULT_LIMIT: u32 = 100;
const MAX_ROWS: u32 = 100;

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Query {
    /// One T-SQL batch; every statement in it runs exactly as written.
    pub sql: String,
    /// Maximum rows to return from the result set, 1–100; defaults to 100.
    #[schemars(range(min = 1, max = 100))]
    pub limit: Option<u32>,
}

impl Query {
    pub fn prepare(&self) -> Result<Statement, &'static str> {
        if self.sql.trim().is_empty() {
            return Err("sql must not be blank");
        }
        if self.sql.encode_utf16().count() > MAX_SQL_UNITS {
            return Err("sql must be at most 32768 UTF-16 units");
        }
        let limit = self.limit.unwrap_or(DEFAULT_LIMIT);
        if !(1..=MAX_ROWS).contains(&limit) {
            return Err("limit must be between 1 and 100");
        }
        Ok(Statement::batch(
            self.sql.clone(),
            usize::try_from(limit).unwrap_or_default(),
        ))
    }
}
