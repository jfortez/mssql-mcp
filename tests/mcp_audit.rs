use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

mod support;

const RESPONSE_TIMEOUT: Duration = Duration::from_secs(15);
const AUDIT_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_AUDIT_LINES: usize = 64;
const USER: &str = "audit-user";
const PASSWORD: &str = "audit-marker-password-9f2c";
const DOMAIN: &str = "AUDITDOMAIN";

fn closed_port() -> u16 {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind loopback");
    let port = listener.local_addr().expect("loopback address").port();
    drop(listener);
    port
}

#[derive(Debug)]
struct Audit {
    raw: String,
    json: Value,
}

impl Audit {
    fn parse(raw: String) -> Self {
        let json = serde_json::from_str::<Value>(&raw)
            .unwrap_or_else(|error| panic!("audit line is not JSON: {error}: {raw}"));
        assert!(
            json["ts"].is_u64(),
            "audit record without an epoch timestamp: {raw}"
        );
        assert!(
            json["event"].is_string(),
            "audit record without an event: {raw}"
        );
        Self { raw, json }
    }
}

struct Server {
    child: Child,
    stdin: Option<ChildStdin>,
    replies: Receiver<Result<Value, String>>,
    audit: Receiver<String>,
    next_id: u64,
}

impl Server {
    fn spawn(mode: &str, port: u16, auth: &str, trust: bool) -> Self {
        let port = port.to_string();
        let mut command = Command::new(env!("CARGO_BIN_EXE_mssql-mcp"));
        support::clear_env(&mut command)
            .env("MSSQL_USER", USER)
            .env("MSSQL_PASSWORD", PASSWORD)
            .env("MSSQL_DOMAIN", DOMAIN)
            .args([
                "--host",
                "127.0.0.1",
                "--port",
                port.as_str(),
                "--database",
                "audit-db",
                "--timeout-seconds",
                "1",
                "--mode",
                mode,
                "--auth",
                auth,
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if trust {
            command.arg("--trust-server-certificate");
        }
        let mut child = command.spawn().expect("spawn MCP server");
        let stdin = child.stdin.take().expect("server stdin");
        let stdout = child.stdout.take().expect("server stdout");
        let stderr = child.stderr.take().expect("server stderr");

        let (reply_sender, replies) = mpsc::channel();
        thread::spawn(move || {
            let reader = BufReader::new(stdout);
            for line in reader.lines() {
                let parsed = line
                    .map_err(|error| format!("failed to read server stdout: {error}"))
                    .and_then(|line| {
                        if line.trim().is_empty() {
                            return Ok(None);
                        }
                        let message = serde_json::from_str::<Value>(&line)
                            .map_err(|error| format!("non-JSON server output: {error}: {line}"))?;
                        if message.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
                            return Err(format!("non JSON-RPC server output: {line}"));
                        }
                        Ok(Some(message))
                    });
                match parsed {
                    Ok(None) => {}
                    Ok(Some(message)) => {
                        if reply_sender.send(Ok(message)).is_err() {
                            break;
                        }
                    }
                    Err(error) => {
                        let _ = reply_sender.send(Err(error));
                        break;
                    }
                }
            }
        });

        let (audit_sender, audit) = mpsc::channel();
        thread::spawn(move || {
            let reader = BufReader::new(stderr);
            let mut kept = 0usize;
            for line in reader.lines() {
                let Ok(line) = line else { break };
                if line.trim().is_empty() {
                    continue;
                }
                if kept == MAX_AUDIT_LINES {
                    continue;
                }
                kept += 1;
                if audit_sender.send(line).is_err() {
                    break;
                }
            }
        });

        Self {
            child,
            stdin: Some(stdin),
            replies,
            audit,
            next_id: 1,
        }
    }

    fn start(mode: &str, port: u16) -> Self {
        let mut server = Self::spawn(mode, port, "sql", false);
        let startup = server.next_audit();
        assert_eq!(startup.json["event"], "startup", "{}", startup.raw);
        server.initialize();
        server
    }

    fn initialize(&mut self) {
        let response = self.request(
            "initialize",
            json!({
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "mcp-audit-test", "version": "0.1.0"}
            }),
        );
        assert!(
            response.get("error").is_none(),
            "initialize failed: {response}"
        );
        self.notify("notifications/initialized", json!({}));
    }

    fn notify(&mut self, method: &str, params: Value) {
        self.send(json!({"jsonrpc": "2.0", "method": method, "params": params}));
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        self.response(id)
    }

    fn send(&mut self, message: Value) {
        let stdin = self.stdin.as_mut().expect("server stdin is open");
        serde_json::to_writer(&mut *stdin, &message).expect("encode client message");
        stdin.write_all(b"\n").expect("write client message");
        stdin.flush().expect("flush client message");
    }

    fn response(&mut self, id: u64) -> Value {
        let deadline = Instant::now() + RESPONSE_TIMEOUT;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            match self.replies.recv_timeout(remaining) {
                Ok(Ok(message)) if message.get("id").and_then(Value::as_u64) == Some(id) => {
                    return message;
                }
                Ok(Ok(_)) => {}
                Ok(Err(error)) => panic!("{error}"),
                Err(RecvTimeoutError::Timeout) => {
                    panic!("timed out after {RESPONSE_TIMEOUT:?} waiting for response {id}")
                }
                Err(RecvTimeoutError::Disconnected) => {
                    panic!("server closed stdout before responding to request {id}")
                }
            }
        }
    }

    fn call_tool(&mut self, name: &str, arguments: Value) -> Value {
        let response = self.request("tools/call", json!({"name": name, "arguments": arguments}));
        response
            .get("result")
            .cloned()
            .unwrap_or_else(|| panic!("tools/call failed: {response}"))
    }

    fn next_audit(&mut self) -> Audit {
        match self.audit.recv_timeout(AUDIT_TIMEOUT) {
            Ok(raw) => Audit::parse(raw),
            Err(RecvTimeoutError::Timeout) => {
                panic!("timed out after {AUDIT_TIMEOUT:?} waiting for an audit record")
            }
            Err(RecvTimeoutError::Disconnected) => panic!("server closed stderr without a record"),
        }
    }

    fn shutdown(mut self) -> Vec<Audit> {
        drop(self.stdin.take());
        let deadline = Instant::now() + Duration::from_secs(10);
        let code = loop {
            match self.child.try_wait().expect("poll server") {
                Some(status) => break status.code(),
                None if Instant::now() < deadline => thread::sleep(Duration::from_millis(20)),
                None => {
                    let _ = self.child.kill();
                    let _ = self.child.wait();
                    panic!("server did not exit after client EOF");
                }
            }
        };
        assert_eq!(code, Some(0), "server exited with {code:?}");
        let mut records = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            match self.audit.recv_timeout(remaining) {
                Ok(raw) => records.push(Audit::parse(raw)),
                Err(RecvTimeoutError::Disconnected) => return records,
                Err(RecvTimeoutError::Timeout) => {
                    panic!("stderr stayed open after the server exited")
                }
            }
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        drop(self.stdin.take());
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match self.child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(20)),
                _ => {
                    let _ = self.child.kill();
                    let _ = self.child.wait();
                    break;
                }
            }
        }
    }
}

fn error_of(result: &Value) -> &Value {
    assert_eq!(result["isError"], true, "expected a tool error: {result}");
    result
        .get("structuredContent")
        .filter(|error| error.is_object())
        .unwrap_or_else(|| panic!("tool error without structured content: {result}"))
}

#[test]
fn startup_and_shutdown_records_describe_static_config() {
    let port = closed_port();
    let mut server = Server::spawn("read", port, "sql", false);
    let startup = server.next_audit();
    assert_eq!(startup.json["mode"], "read", "{}", startup.raw);
    assert_eq!(startup.json["auth"], "sql", "{}", startup.raw);
    assert_eq!(startup.json["tls_verify"], true, "{}", startup.raw);
    for secret in [USER, PASSWORD, DOMAIN, "127.0.0.1", "audit-db"] {
        assert!(
            !startup.raw.contains(secret),
            "startup record leaked {secret:?}: {}",
            startup.raw
        );
    }
    server.initialize();
    let remaining = server.shutdown();
    assert_eq!(
        remaining.len(),
        1,
        "unexpected audit records after startup: {remaining:?}"
    );
    assert_eq!(remaining[0].json["event"], "shutdown");
    assert_eq!(remaining[0].json["status"], "ok");

    let mut server = Server::spawn("both", port, "ntlm", true);
    let startup = server.next_audit();
    assert_eq!(startup.json["mode"], "both", "{}", startup.raw);
    assert_eq!(startup.json["auth"], "ntlm", "{}", startup.raw);
    assert_eq!(startup.json["tls_verify"], false, "{}", startup.raw);
    assert!(
        !startup.raw.contains(PASSWORD) && !startup.raw.contains(DOMAIN),
        "startup record leaked credentials: {}",
        startup.raw
    );
    let remaining = server.shutdown();
    assert_eq!(remaining.len(), 1, "{remaining:?}");
    assert_eq!(remaining[0].json["status"], "ok");
}

#[test]
fn write_connection_failure_is_audited_as_not_attempted() {
    let mut server = Server::start("write", closed_port());
    let response = server.call_tool(
        "delete_rows",
        json!({"table": {"name": "Items"}, "filters": {"id": 1}}),
    );
    let error = error_of(&response);
    assert_eq!(error["code"], "connection_failed");
    assert_eq!(error["mutation_outcome"], "not_attempted");
    assert_eq!(response["content"][0]["text"], "database operation failed");
    let record = server.next_audit();
    assert_eq!(record.json["code"], "connection_failed", "{}", record.raw);
    assert_eq!(
        record.json["scope"],
        json!({"schema": "dbo", "table": "Items"})
    );
    assert!(!record.raw.contains("SELECT") && !record.raw.contains("@P1"));
    let remaining = server.shutdown();
    assert_eq!(remaining.len(), 1, "{remaining:?}");
    assert_eq!(remaining[0].json["event"], "shutdown");
}

#[test]
fn validated_scope_is_audited_without_queries_or_values() {
    let mut server = Server::start("both", closed_port());
    let response = server.call_tool(
        "select_rows",
        json!({
            "table": {"name": "Items"},
            "columns": ["id"],
            "filters": {"id": 1000},
            "limit": 5,
        }),
    );
    let error = error_of(&response);
    assert_eq!(error["code"], "connection_failed");
    let record = server.next_audit();
    assert_eq!(record.json["event"], "tool_call", "{}", record.raw);
    assert_eq!(record.json["tool"], "select_rows", "{}", record.raw);
    assert_eq!(record.json["mode"], "both", "{}", record.raw);
    assert_eq!(record.json["status"], "error", "{}", record.raw);
    assert_eq!(record.json["code"], "connection_failed", "{}", record.raw);
    assert!(
        record.json["duration_ms"].is_u64(),
        "audit record without a duration: {}",
        record.raw
    );
    assert_eq!(
        record.json["scope"],
        json!({"schema": "dbo", "table": "Items"}),
        "{}",
        record.raw
    );
    for absent in ["rows", "affected_rows", "truncated", "jsonrpc"] {
        assert!(
            record.json.get(absent).is_none(),
            "unexpected {absent} in {}",
            record.raw
        );
    }
    for leaked in ["SELECT", "1000", "@P1", PASSWORD, USER] {
        assert!(
            !record.raw.contains(leaked),
            "audit record leaked {leaked:?}: {}",
            record.raw
        );
    }

    let _ = server.call_tool("list_tables", json!({"schema": "sys"}));
    let record = server.next_audit();
    assert_eq!(
        record.json["scope"],
        json!({"schema": "sys"}),
        "{}",
        record.raw
    );

    let _ = server.call_tool("list_tables", json!({}));
    let record = server.next_audit();
    assert_eq!(
        record.json["scope"],
        json!({"catalog": true}),
        "{}",
        record.raw
    );

    let remaining = server.shutdown();
    assert_eq!(remaining.len(), 1, "{remaining:?}");
    assert_eq!(remaining[0].json["event"], "shutdown");
}

#[test]
fn filter_values_and_credentials_stay_out_of_audit_and_errors() {
    const MARKER: &str = "sensitive-marker-3f7a";
    let mut server = Server::start("read", closed_port());
    let response = server.call_tool(
        "select_rows",
        json!({
            "table": {"name": "Items"},
            "columns": ["id"],
            "filters": {"label": MARKER},
            "limit": 5,
        }),
    );
    let record = server.next_audit();
    for leaked in [MARKER, "SELECT", "@P1", PASSWORD, USER] {
        assert!(
            !record.raw.contains(leaked),
            "audit record leaked {leaked:?}: {}",
            record.raw
        );
    }
    let reply = response.to_string();
    for leaked in [MARKER, "SELECT", "@P1", PASSWORD, USER] {
        assert!(
            !reply.contains(leaked),
            "tool error leaked {leaked:?}: {reply}"
        );
    }
    server.shutdown();
}

#[test]
fn unvalidated_targets_are_not_audited() {
    let mut server = Server::start("read", closed_port());
    let response = server.call_tool(
        "select_rows",
        json!({"table": {"name": "Bad\u{7}Name"}, "columns": ["id"]}),
    );
    let error = error_of(&response);
    assert_eq!(error["code"], "invalid_input");
    assert_eq!(
        response["content"][0]["text"],
        "identifier must contain 1–128 UTF-16 units and no control characters"
    );
    let record = server.next_audit();
    assert_eq!(record.json["code"], "invalid_input", "{}", record.raw);
    assert_eq!(record.json["status"], "error", "{}", record.raw);
    assert!(record.json.get("scope").is_none(), "{}", record.raw);
    assert!(
        !record.raw.contains("Bad"),
        "unvalidated identifier leaked: {}",
        record.raw
    );

    let response = server.call_tool("select_rows", json!({"table": {"name": "Items"}}));
    let error = error_of(&response);
    assert_eq!(error["code"], "invalid_arguments");
    assert_eq!(
        response["content"][0]["text"],
        "invalid arguments for select_rows"
    );
    let record = server.next_audit();
    assert_eq!(record.json["code"], "invalid_arguments", "{}", record.raw);
    assert!(record.json.get("scope").is_none(), "{}", record.raw);
    assert!(
        !record.raw.contains("Items"),
        "unvalidated identifier leaked: {}",
        record.raw
    );
    server.shutdown();
}

#[test]
fn denied_mutations_are_audited_as_not_attempted() {
    let mut server = Server::start("read", closed_port());
    let response = server.call_tool(
        "insert_row",
        json!({"table": {"name": "Items"}, "values": {"id": 1}}),
    );
    let error = error_of(&response);
    assert_eq!(error["code"], "mode_unavailable");
    assert_eq!(error["retryable"], false);
    assert_eq!(error["mutation_outcome"], "not_attempted");
    assert_eq!(
        response["content"][0]["text"],
        "tool is not available in read mode"
    );
    let record = server.next_audit();
    assert_eq!(record.json["tool"], "insert_row", "{}", record.raw);
    assert_eq!(record.json["code"], "mode_unavailable", "{}", record.raw);
    assert!(record.json.get("scope").is_none(), "{}", record.raw);
    server.shutdown();
}

#[test]
fn unknown_tools_keep_json_rpc_errors_and_are_not_named_in_audit() {
    const NAME: &str = "audit-unknown-tool-xyz";
    let mut server = Server::start("read", closed_port());
    let response = server.request("tools/call", json!({"name": NAME, "arguments": {}}));
    assert_eq!(response["error"]["code"], -32601, "{response}");
    assert_eq!(response["error"]["message"], "tools/call", "{response}");
    assert_eq!(
        response["error"]["data"]["code"], "unknown_tool",
        "{response}"
    );
    let record = server.next_audit();
    assert_eq!(record.json["event"], "tool_call", "{}", record.raw);
    assert_eq!(record.json["status"], "error", "{}", record.raw);
    assert_eq!(record.json["code"], "unknown_tool", "{}", record.raw);
    assert!(record.json.get("tool").is_none(), "{}", record.raw);
    assert!(
        !record.raw.contains(NAME),
        "raw tool name leaked: {}",
        record.raw
    );
    server.shutdown();
}
