//! Behaviour of the `query` tool over stdio.
//!
//! The tool runs an arbitrary T-SQL batch under the configured principal, so
//! every batch asks the human first and the prompt carries the exact SQL. No
//! live SQL Server is used: a peer that counts and closes every accepted
//! connection proves whether the database layer was reached, and a loopback
//! peer that speaks nothing keeps every reached call failing at the connection
//! with a sanitized code.

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

mod support;

const RESPONSE_TIMEOUT: Duration = Duration::from_secs(15);
/// Grace period that lets an unexpected background connection become visible.
const CONNECTION_GRACE: Duration = Duration::from_millis(100);
const SQL_MARKER: &str = "query-batch-marker-7d3e";
const PASSWORD: &str = "query-test-password-b41c";
const HOST: &str = "127.0.0.1";

/// Loopback peer that counts every accepted TCP connection and closes it, so an
/// attempted database connection is both visible and harmless.
struct CountingPeer {
    port: u16,
    accepted: Arc<AtomicUsize>,
}

impl CountingPeer {
    fn start() -> Self {
        let listener = TcpListener::bind((HOST, 0)).expect("bind the counting peer");
        let port = listener.local_addr().expect("counting peer address").port();
        let accepted = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&accepted);
        thread::spawn(move || {
            for incoming in listener.incoming() {
                match incoming {
                    Ok(stream) => {
                        counter.fetch_add(1, Ordering::SeqCst);
                        drop(stream);
                    }
                    Err(_) => break,
                }
            }
        });
        Self { port, accepted }
    }

    fn attempts(&self) -> usize {
        self.accepted.load(Ordering::SeqCst)
    }
}

struct Server {
    child: Child,
    stdin: Option<ChildStdin>,
    messages: Receiver<Result<Value, String>>,
    audit: Arc<Mutex<Vec<String>>>,
    next_id: u64,
}

impl Server {
    fn start(mode: &str, port: u16, yolo: bool, elicitation: bool) -> Self {
        let mut command = Command::new(env!("CARGO_BIN_EXE_mssql-mcp"));
        support::clear_env(&mut command)
            .env("MSSQL_USER", "sa")
            .env("MSSQL_PASSWORD", PASSWORD)
            .args([
                "--host",
                HOST,
                "--port",
                &port.to_string(),
                "--database",
                "test",
                "--timeout-seconds",
                "1",
                "--mode",
                mode,
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if yolo {
            command.arg("--yolo");
        }
        let mut child = command.spawn().expect("spawn MCP server");
        let stdin = child.stdin.take().expect("server stdin");
        let stdout = child.stdout.take().expect("server stdout");
        let stderr = child.stderr.take().expect("server stderr");
        let audit = Arc::new(Mutex::new(Vec::new()));
        let lines = Arc::clone(&audit);
        thread::spawn(move || {
            let reader = BufReader::new(stderr);
            for line in reader.lines() {
                let Ok(line) = line else { break };
                if !line.trim().is_empty() {
                    lines.lock().expect("audit lines").push(line);
                }
            }
        });
        let (sender, messages) = mpsc::channel();
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
                        if sender.send(Ok(message)).is_err() {
                            break;
                        }
                    }
                    Err(error) => {
                        let _ = sender.send(Err(error));
                        break;
                    }
                }
            }
        });
        let mut server = Self {
            child,
            stdin: Some(stdin),
            messages,
            audit,
            next_id: 1,
        };
        server.initialize(elicitation);
        server
    }

    fn initialize(&mut self, elicitation: bool) {
        let capabilities = if elicitation {
            json!({"elicitation": {}})
        } else {
            json!({})
        };
        let response = self.request(
            "initialize",
            json!({
                "protocolVersion": "2025-06-18",
                "capabilities": capabilities,
                "clientInfo": {"name": "mcp-query-test", "version": "0.1.0"}
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
        let id = self.send_request(method, params);
        self.response(id)
    }

    fn send_request(&mut self, method: &str, params: Value) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        id
    }

    fn send(&mut self, message: Value) {
        let stdin = self.stdin.as_mut().expect("server stdin is open");
        serde_json::to_writer(&mut *stdin, &message).expect("encode client message");
        stdin.write_all(b"\n").expect("write client message");
        stdin.flush().expect("flush client message");
    }

    fn next_message(&mut self) -> Value {
        match self.messages.recv_timeout(RESPONSE_TIMEOUT) {
            Ok(Ok(message)) => message,
            Ok(Err(error)) => panic!("{error}"),
            Err(RecvTimeoutError::Timeout) => {
                panic!("timed out after {RESPONSE_TIMEOUT:?} waiting for a server message")
            }
            Err(RecvTimeoutError::Disconnected) => {
                panic!("server closed stdout before sending the expected message")
            }
        }
    }

    /// Wait for the response with `id`; an unsolicited server request fails the
    /// test because only an approval prompt may ask the client for something.
    fn response(&mut self, id: u64) -> Value {
        loop {
            let message = self.next_message();
            if message.get("method").is_some() {
                panic!("unexpected server request while waiting for response {id}: {message}");
            }
            if message.get("id").and_then(Value::as_u64) == Some(id) {
                return message;
            }
        }
    }

    fn await_elicitation(&mut self) -> Value {
        let message = self.next_message();
        assert_eq!(
            message.get("method").and_then(Value::as_str),
            Some("elicitation/create"),
            "expected an elicitation request, got: {message}"
        );
        message
    }

    fn answer(&mut self, request: &Value, result: Value) {
        let id = request
            .get("id")
            .cloned()
            .unwrap_or_else(|| panic!("server request without an id: {request}"));
        self.send(json!({"jsonrpc": "2.0", "id": id, "result": result}));
    }

    fn begin_tool_call(&mut self, name: &str, arguments: Value) -> u64 {
        self.send_request("tools/call", json!({"name": name, "arguments": arguments}))
    }

    fn call_tool(&mut self, name: &str, arguments: Value) -> Value {
        let id = self.begin_tool_call(name, arguments);
        let response = self.response(id);
        response
            .get("result")
            .cloned()
            .unwrap_or_else(|| panic!("tools/call failed: {response}"))
    }

    fn tool_names(&mut self) -> Vec<String> {
        let response = self.request("tools/list", json!({}));
        response["result"]["tools"]
            .as_array()
            .unwrap_or_else(|| panic!("no tool list in response: {response}"))
            .iter()
            .map(|tool| {
                tool["name"]
                    .as_str()
                    .unwrap_or_else(|| panic!("tool without name: {tool}"))
                    .to_owned()
            })
            .collect()
    }

    /// Audit line for `tool`, once the server has written it.
    fn audit_record(&self, tool: &str) -> Value {
        let needle = format!("\"tool\":\"{tool}\"");
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            {
                let lines = self.audit.lock().expect("audit lines");
                if let Some(line) = lines
                    .iter()
                    .rev()
                    .find(|line| line.contains("\"event\":\"tool_call\"") && line.contains(&needle))
                {
                    return serde_json::from_str(line).expect("audit record is JSON");
                }
            }
            assert!(
                Instant::now() < deadline,
                "no tool_call audit record for {tool}: {:?}",
                self.audit.lock().expect("audit lines")
            );
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn audit_text(&self) -> String {
        self.audit.lock().expect("audit lines").join("\n")
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

fn tool_error(result: &Value) -> &Value {
    assert_eq!(
        result["isError"], true,
        "expected a structured tool error, got {result}"
    );
    result
        .get("structuredContent")
        .filter(|error| error.is_object())
        .unwrap_or_else(|| panic!("tool error without structured content: {result}"))
}

fn error_text(result: &Value) -> &str {
    result["content"][0]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("tool error without text: {result}"))
}

/// The relay question: a structured result, never an error.
fn approval_question(result: &Value) -> &Value {
    assert_eq!(
        result["isError"], false,
        "a question is not an error: {result}"
    );
    let payload = result
        .get("structuredContent")
        .filter(|payload| payload.is_object())
        .unwrap_or_else(|| panic!("question without structured content: {result}"));
    assert_eq!(payload["code"], "approval_required", "{result}");
    payload
}

#[test]
fn query_is_absent_from_read_mode_and_never_connects() {
    let peer = CountingPeer::start();
    let mut server = Server::start("read", peer.port, false, false);
    assert!(
        !server.tool_names().iter().any(|name| name == "query"),
        "read mode must not list query"
    );
    let result = server.call_tool("query", json!({"sql": "SELECT 1"}));
    let error = tool_error(&result);
    assert_eq!(error["code"], "mode_unavailable", "{result}");
    assert_eq!(error["mutation_outcome"], "not_attempted", "{result}");
    assert_eq!(error["retryable"], false, "{result}");
    thread::sleep(CONNECTION_GRACE);
    assert_eq!(peer.attempts(), 0, "a refused query must not connect");
    assert!(
        server.audit_record("query").get("approval").is_none(),
        "a mode refusal must not claim an approval outcome"
    );
}

#[test]
fn query_without_approval_asks_before_connecting_in_write_and_both() {
    for mode in ["write", "both"] {
        let peer = CountingPeer::start();
        let mut server = Server::start(mode, peer.port, false, false);
        let sql = format!("SELECT '{SQL_MARKER}' AS [marker]");
        let result = server.call_tool("query", json!({"sql": sql}));
        let question = approval_question(&result);
        assert_eq!(question["mutation_outcome"], "not_attempted", "{mode}");
        assert_eq!(question["row_cap"], Value::Null, "{mode}: {result}");
        assert_eq!(question["expires_in_seconds"], 120, "{mode}: {result}");
        let token = question["approval_token"].as_str().expect("token");
        assert_eq!(token.len(), 32, "{mode}: {token}");
        assert_eq!(question["approval_token_id"], json!(&token[..8]), "{mode}");
        let message = question["approval_message"]
            .as_str()
            .unwrap_or_else(|| panic!("{mode}: no human message: {result}"));
        assert!(
            message.contains(&sql),
            "{mode}: the question must carry the exact batch: {message}"
        );
        thread::sleep(CONNECTION_GRACE);
        assert_eq!(
            peer.attempts(),
            0,
            "{mode}: a query without an answer must not connect"
        );
        let record = server.audit_record("query");
        assert_eq!(record["approval"], "required", "{mode}: {record}");
        assert_eq!(record["approval_token_id"], json!(&token[..8]), "{mode}");
        assert_eq!(record["status"], "ok", "{mode}: {record}");
        let audit = server.audit_text();
        assert!(
            !audit.contains(SQL_MARKER) && !audit.contains("SELECT"),
            "{mode}: SQL text leaked into the audit trail: {audit}"
        );
    }
}

#[test]
fn the_relay_binds_the_exact_batch_and_runs_it_once() {
    let peer = CountingPeer::start();
    let mut server = Server::start("write", peer.port, false, false);
    let sql = format!("DELETE FROM dbo.Items WHERE [id] = 42; -- {SQL_MARKER}");
    let result = server.call_tool("query", json!({"sql": sql}));
    let token = approval_question(&result)["approval_token"]
        .as_str()
        .expect("token")
        .to_owned();

    // A different batch, down to one character, is never authorized.
    for swapped in [
        format!("{sql} "),
        sql.replace("42", "43"),
        "SELECT 1".to_owned(),
    ] {
        let refusal = server.call_tool(
            "query",
            json!({"sql": swapped, "approval_token": token, "approval_decision": "grant"}),
        );
        assert_eq!(
            tool_error(&refusal)["code"],
            "approval_token_mismatch",
            "{swapped:?}: {refusal}"
        );
    }
    assert_eq!(peer.attempts(), 0, "a swapped batch must not connect");

    let mut retry = json!({"sql": sql});
    retry["approval_token"] = json!(token);
    retry["approval_decision"] = json!("grant");
    let granted = server.call_tool("query", retry.clone());
    assert_eq!(
        tool_error(&granted)["code"],
        "connection_failed",
        "a granted retry must reach the database layer: {granted}"
    );
    assert_eq!(server.audit_record("query")["approval"], "granted");

    let replay = server.call_tool("query", retry);
    assert_eq!(
        tool_error(&replay)["code"],
        "approval_token_reused",
        "{replay}"
    );
    thread::sleep(CONNECTION_GRACE);
    assert_eq!(
        peer.attempts(),
        1,
        "the replay must not open a second connection"
    );
}

#[test]
fn denying_a_query_question_never_runs_the_batch() {
    let peer = CountingPeer::start();
    let mut server = Server::start("both", peer.port, false, false);
    let sql = format!("DROP TABLE dbo.Items; -- {SQL_MARKER}");
    let question = server.call_tool("query", json!({"sql": sql}));
    let token = approval_question(&question)["approval_token"]
        .as_str()
        .expect("token");
    let denied = server.call_tool(
        "query",
        json!({"sql": sql, "approval_token": token, "approval_decision": "deny"}),
    );
    assert_eq!(tool_error(&denied)["code"], "approval_denied", "{denied}");
    assert_eq!(tool_error(&denied)["mutation_outcome"], "not_attempted");
    thread::sleep(CONNECTION_GRACE);
    assert_eq!(peer.attempts(), 0, "a denial must never connect");
    assert_eq!(server.audit_record("query")["approval"], "denied");
    let audit = server.audit_text();
    assert!(
        !audit.contains(SQL_MARKER) && !audit.contains("DROP"),
        "the denial must not record the batch: {audit}"
    );
}

#[test]
fn declining_the_query_prompt_refuses_before_connecting() {
    let peer = CountingPeer::start();
    let mut server = Server::start("write", peer.port, false, true);
    let id = server.begin_tool_call("query", json!({"sql": "SELECT 1"}));
    let request = server.await_elicitation();
    server.answer(&request, json!({"action": "decline"}));
    let response = server.response(id);
    let result = &response["result"];
    let error = tool_error(result);
    assert_eq!(error["code"], "approval_denied", "{response}");
    assert_eq!(error["mutation_outcome"], "not_attempted", "{response}");
    thread::sleep(CONNECTION_GRACE);
    assert_eq!(peer.attempts(), 0, "a declined query must not connect");
    assert_eq!(
        server.audit_record("query")["approval"],
        "denied",
        "the audit trail must record the denial"
    );
}

#[test]
fn yolo_forwards_the_batch_to_the_database_layer() {
    for mode in ["write", "both"] {
        let (port, _peer) = support::dropping_peer();
        let mut server = Server::start(mode, port, true, false);
        let sql = format!("SELECT '{SQL_MARKER}' AS [marker]");
        let result = server.call_tool("query", json!({"sql": sql}));
        let error = tool_error(&result);
        assert_eq!(
            error["code"], "connection_failed",
            "{mode}: --yolo must reach the database layer: {result}"
        );
        assert_eq!(error["mutation_outcome"], "not_attempted", "{mode}");
        let record = server.audit_record("query");
        assert_eq!(record["approval"], "yolo", "{mode}: {record}");
        let audit = server.audit_text();
        assert!(
            !audit.contains(SQL_MARKER) && !audit.contains("SELECT"),
            "{mode}: SQL text leaked into the audit trail: {audit}"
        );
        let reply = result.to_string();
        assert!(
            !reply.contains(SQL_MARKER) && !reply.contains("SELECT"),
            "{mode}: SQL text leaked into the tool result: {reply}"
        );
    }
}

#[test]
fn the_prompt_carries_the_exact_batch_and_reaches_the_database_after_approval() {
    let (port, _peer) = support::dropping_peer();
    let mut server = Server::start("write", port, false, true);
    let sql = format!(
        "UPDATE dbo.Items SET [label] = '{SQL_MARKER}' WHERE [id] = 42;\nSELECT 1; -- trailing"
    );
    let id = server.begin_tool_call("query", json!({"sql": sql}));
    let request = server.await_elicitation();
    let message = request["params"]["message"]
        .as_str()
        .unwrap_or_else(|| panic!("approval prompt without a message: {request}"));
    assert!(
        message.contains(&sql),
        "the prompt must carry the exact batch text: {message}"
    );
    for leaked in [PASSWORD, HOST, "test-password"] {
        assert!(
            !message.contains(leaked),
            "the prompt leaked {leaked:?}: {message}"
        );
    }
    server.answer(
        &request,
        json!({"action": "accept", "content": {"confirm": true}}),
    );
    let response = server.response(id);
    let result = &response["result"];
    let error = tool_error(result);
    assert_eq!(
        error["code"], "connection_failed",
        "approval must precede the connection: {response}"
    );
    assert_eq!(error["mutation_outcome"], "not_attempted", "{response}");
    assert_eq!(
        server.audit_record("query")["approval"],
        "granted",
        "the audit trail must record the granted approval"
    );
    let reply = response.to_string();
    assert!(
        !reply.contains(SQL_MARKER) && !reply.contains("UPDATE"),
        "SQL text leaked into the result: {reply}"
    );
    let audit = server.audit_text();
    assert!(
        !audit.contains(SQL_MARKER) && !audit.contains("UPDATE"),
        "SQL text leaked into the audit trail: {audit}"
    );
}

#[test]
fn invalid_arguments_are_rejected_before_the_gate() {
    let peer = CountingPeer::start();
    let mut server = Server::start("write", peer.port, false, true);
    for (arguments, message, code) in [
        (
            json!({"sql": "   "}),
            "sql must not be blank",
            "invalid_input",
        ),
        (
            json!({"sql": "SELECT 1", "limit": 0}),
            "limit must be between 1 and 100",
            "invalid_input",
        ),
        (
            json!({"sql": "SELECT 1", "limit": 101}),
            "limit must be between 1 and 100",
            "invalid_input",
        ),
        (
            json!({"sql": "SELECT 1", "limit": "many"}),
            "invalid arguments for query",
            "invalid_arguments",
        ),
        (
            json!({"sql": "SELECT 1", "unknown": true}),
            "invalid arguments for query",
            "invalid_arguments",
        ),
        (
            json!({}),
            "invalid arguments for query",
            "invalid_arguments",
        ),
    ] {
        let result = server.call_tool("query", arguments);
        assert_eq!(error_text(&result), message, "{result}");
        assert_eq!(tool_error(&result)["code"], code, "{result}");
    }
    let oversized = format!("SELECT '{}'", "x".repeat(32_768));
    let result = server.call_tool("query", json!({"sql": oversized}));
    assert_eq!(
        error_text(&result),
        "sql must be at most 32768 UTF-16 units"
    );
    assert_eq!(tool_error(&result)["code"], "invalid_input");
    thread::sleep(CONNECTION_GRACE);
    assert_eq!(
        peer.attempts(),
        0,
        "rejected arguments must not reach the database"
    );
}
