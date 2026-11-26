//! Operational audit trail written as JSON Lines on stderr.
//!
//! Records never reach stdout, which carries only MCP protocol traffic. Each
//! record has an epoch-millisecond `ts` and an `event`: `startup` and
//! `shutdown` describe the process, and `tool_call` describes one completed
//! call with a known tool name, the effective mode, a status and code, the
//! duration, the validated scope and bounded result counts. Values, SQL text,
//! credentials and unvalidated identifiers are never recorded.

use std::io::Write;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Serialize;

use crate::config::Mode;

/// Outcome of a lifecycle step or tool call.
#[derive(Clone, Copy)]
pub enum Status {
    Ok,
    Error,
}

impl Status {
    const fn name(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Error => "error",
        }
    }
}

/// Validated object scope of a tool call; absent when validation did not run.
#[derive(Serialize)]
pub struct Scope<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    schema: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    table: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    catalog: Option<bool>,
}

impl<'a> Scope<'a> {
    pub const fn table(schema: &'a str, table: &'a str) -> Self {
        Self {
            schema: Some(schema),
            table: Some(table),
            catalog: None,
        }
    }

    pub const fn schema(schema: &'a str) -> Self {
        Self {
            schema: Some(schema),
            table: None,
            catalog: None,
        }
    }

    pub const fn catalog() -> Self {
        Self {
            schema: None,
            table: None,
            catalog: Some(true),
        }
    }
}

/// Completed tool call to record.
pub struct CallRecord<'a> {
    pub tool: Option<&'static str>,
    pub mode: Mode,
    pub status: Status,
    pub code: Option<&'static str>,
    pub duration: Duration,
    pub scope: Option<Scope<'a>>,
    pub rows: Option<usize>,
    pub affected_rows: Option<i64>,
    pub truncated: Option<bool>,
}

#[derive(Serialize)]
struct StartupRecord<'a> {
    ts: u64,
    event: &'static str,
    mode: &'a str,
    auth: &'a str,
    tls_verify: bool,
}

#[derive(Serialize)]
struct ShutdownRecord {
    ts: u64,
    event: &'static str,
    status: &'static str,
}

#[derive(Serialize)]
struct ToolCallRecord<'a> {
    ts: u64,
    event: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool: Option<&'static str>,
    mode: &'a str,
    status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    code: Option<&'static str>,
    duration_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    scope: Option<Scope<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    rows: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    affected_rows: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    truncated: Option<bool>,
}

pub fn startup(mode: Mode, auth: &str, tls_verify: bool) {
    emit(&StartupRecord {
        ts: now_ms(),
        event: "startup",
        mode: mode.name(),
        auth,
        tls_verify,
    });
}

pub fn shutdown(status: Status) {
    emit(&ShutdownRecord {
        ts: now_ms(),
        event: "shutdown",
        status: status.name(),
    });
}

pub fn call(record: CallRecord<'_>) {
    emit(&ToolCallRecord {
        ts: now_ms(),
        event: "tool_call",
        tool: record.tool,
        mode: record.mode.name(),
        status: record.status.name(),
        code: record.code,
        duration_ms: millis(record.duration),
        scope: record.scope,
        rows: record.rows,
        affected_rows: record.affected_rows,
        truncated: record.truncated,
    });
}

fn emit<T: Serialize>(record: &T) {
    let Ok(mut line) = serde_json::to_string(record) else {
        return;
    };
    line.push('\n');
    let stderr = std::io::stderr();
    let mut handle = stderr.lock();
    let _ = handle.write_all(line.as_bytes());
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(millis)
        .unwrap_or(0)
}

fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}
