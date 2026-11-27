//! Behaviour of the mutation approval gate over stdio.
//!
//! These tests drive the released executable and answer server-to-client
//! `elicitation/create` requests, so the gate is exercised exactly as a client
//! observes it. No live SQL Server is used: a loopback peer that closes every
//! accepted connection makes an attempted connection fail deterministically,
//! and a counting peer proves that a refused mutation never connected at all.

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
const VALUE_MARKER: &str = "approval-value-marker-1f4b";
const FILTER_MARKER: &str = "approval-filter-marker-8c2d";
/// Distinct, non-secret detail fixtures: one marker per JSON scalar type plus a
/// string whose value needs escaping. The negative number cannot match a
/// timestamp or a duration in the audit trail.
const DETAIL_VALUE_TEXT: &str = "approval-detail-value-5b7e";
const DETAIL_FILTER_TEXT: &str = "approval-detail-filter-2c9a";
const DETAIL_ESCAPE_TEXT: &str =
    "approval-detail-escape quote\" slash\\ newline\ntab\tbell\u{7} next-line\u{85} bidi\u{202e}";
const DETAIL_NUMBER: i64 = -867_530_912;
const DETAIL_PREFIX: &str = "approval-detail";

/// Loopback peer that counts every accepted TCP connection and closes it, so an
/// attempted database connection is both visible and harmless.
struct CountingPeer {
    port: u16,
    accepted: Arc<AtomicUsize>,
}

impl CountingPeer {
    fn start() -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind the counting peer");
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Yolo {
    Off,
    Flag,
    Env,
    EnvFalse,
}

struct Server {
    child: Child,
    stdin: Option<ChildStdin>,
    messages: Receiver<Result<Value, String>>,
    audit: Arc<Mutex<Vec<String>>>,
    /// The full `initialize` response, which carries the discovery instructions.
    initialize: Value,
    next_id: u64,
}

impl Server {
    /// Start the released binary and initialize with or without the elicitation
    /// client capability, which is what decides whether approval is possible.
    fn start(mode: &str, port: u16, yolo: Yolo, elicitation: bool, trust: bool) -> Self {
        let mut command = Command::new(env!("CARGO_BIN_EXE_mssql-mcp"));
        support::clear_env(&mut command)
            .env("MSSQL_USER", "sa")
            .env("MSSQL_PASSWORD", "test-password")
            .args([
                "--host",
                "127.0.0.1",
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
        match yolo {
            Yolo::Off => {}
            Yolo::Flag => {
                command.arg("--yolo");
            }
            Yolo::Env => {
                command.env("MSSQL_MCP_YOLO", "true");
            }
            Yolo::EnvFalse => {
                command.env("MSSQL_MCP_YOLO", "false");
            }
        }
        if trust {
            command.arg("--trust-elicitation");
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
                if line.trim().is_empty() {
                    continue;
                }
                lines.lock().expect("audit lines").push(line);
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
            initialize: Value::Null,
            next_id: 1,
        };
        let initialize = server.request_initialize(elicitation);
        server.initialize = initialize;
        server
    }

    fn request_initialize(&mut self, elicitation: bool) -> Value {
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
                "clientInfo": {"name": "mcp-approval-test", "version": "0.1.0"}
            }),
        );
        assert!(
            response.get("error").is_none(),
            "initialize failed: {response}"
        );
        self.notify("notifications/initialized", json!({}));
        response
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

    /// Wait for the response with `id`; an unsolicited server request is a test
    /// failure because only the gate may ask the client for something.
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
        assert!(
            message.get("id").is_some(),
            "an elicitation request needs an id: {message}"
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

fn approval_error(result: &Value) -> &Value {
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

/// The refusal of a mutation without a trusted human channel: an error that
/// offers no model-callable answer and never reaches SQL Server.
fn approval_unavailable(result: &Value) -> &Value {
    let error = approval_error(result);
    assert_eq!(error["code"], "approval_unavailable", "{result}");
    assert_eq!(error["mutation_outcome"], "not_attempted", "{result}");
    assert_eq!(error["retryable"], false, "{result}");
    let reply = result.to_string();
    for relay in ["approval_token", "approval_decision", "approval_required"] {
        assert!(
            !reply.contains(relay),
            "the model-facing relay leaked {relay:?}: {reply}"
        );
    }
    error
}

#[test]
fn initialize_instructions_explain_the_gate_without_changing_it() {
    for mode in ["write", "both"] {
        let peer = CountingPeer::start();
        let mut server = Server::start(mode, peer.port, Yolo::Off, false, false);
        let instructions = server.initialize["result"]["instructions"]
            .as_str()
            .unwrap_or_else(|| {
                panic!(
                    "{mode}: initialize carries no instructions: {}",
                    server.initialize
                )
            });
        for expected in ["approval", "elicitation", "--trust-elicitation", "--yolo"] {
            assert!(
                instructions.contains(expected),
                "{mode}: the instructions must name {expected:?}: {instructions}"
            );
        }
        // Discovery is not a bypass: the same client is still refused.
        let result = server.call_tool(
            "insert_row",
            json!({"table": {"name": "Items"}, "values": {"id": 1}}),
        );
        approval_unavailable(&result);
        thread::sleep(CONNECTION_GRACE);
        assert_eq!(
            peer.attempts(),
            0,
            "{mode}: an unapproved mutation must not connect"
        );
    }

    // Read mode advertises the same discovery text it can actually honour.
    let server = Server::start(
        "read",
        support::unique_loopback_port(),
        Yolo::Off,
        false,
        false,
    );
    let instructions = server.initialize["result"]["instructions"]
        .as_str()
        .unwrap_or_else(|| panic!("read: no instructions: {}", server.initialize));
    assert!(
        instructions.contains("--yolo") && instructions.contains("read"),
        "read: the instructions must describe the read-only contract: {instructions}"
    );
}

#[test]
fn a_client_without_a_trusted_channel_is_refused_without_connecting() {
    let peer = CountingPeer::start();
    let mut server = Server::start("write", peer.port, Yolo::Off, false, false);
    let result = server.call_tool(
        "insert_row",
        json!({"table": {"schema": "sales", "name": "Items"}, "values": {"id": 1}}),
    );
    let error = approval_unavailable(&result);
    assert!(
        error["action"].as_str().is_some_and(
            |action| action.contains("--trust-elicitation") && action.contains("--yolo")
        ),
        "the refusal must name the operator choices: {result}"
    );
    assert_eq!(error_text(&result), error["message"], "{result}");
    thread::sleep(CONNECTION_GRACE);
    assert_eq!(
        peer.attempts(),
        0,
        "a refused mutation must not reach SQL Server"
    );
    let audit = server.audit_record("insert_row");
    assert_eq!(audit["approval"], "unavailable", "{audit}");
    assert_eq!(audit["code"], "approval_unavailable", "{audit}");
    assert_eq!(audit["status"], "error", "{audit}");
    assert!(
        audit.get("approval_token_id").is_none(),
        "no relay label exists: {audit}"
    );
    assert!(
        audit.get("affected_rows").is_none(),
        "nothing ran, so no row count exists: {audit}"
    );
    assert_eq!(audit["scope"], json!({"schema": "sales", "table": "Items"}));
}

#[test]
fn an_advertised_capability_without_the_trust_flag_fails_closed() {
    let peer = CountingPeer::start();
    let mut server = Server::start("both", peer.port, Yolo::Off, true, false);
    // The response helper fails on any unsolicited server request, so this
    // also proves the server did not ask the client to approve anything.
    let result = server.call_tool(
        "delete_rows",
        json!({"table": {"name": "Items"}, "filters": {"id": 1}}),
    );
    approval_unavailable(&result);
    thread::sleep(CONNECTION_GRACE);
    assert_eq!(peer.attempts(), 0, "a capability is not an approval");
    assert_eq!(
        server.audit_record("delete_rows")["approval"],
        "unavailable"
    );
}

#[test]
fn a_trusted_host_still_needs_the_elicitation_capability() {
    let peer = CountingPeer::start();
    let mut server = Server::start("write", peer.port, Yolo::Off, false, true);
    let result = server.call_tool(
        "insert_row",
        json!({"table": {"name": "Items"}, "values": {"id": 1}}),
    );
    approval_unavailable(&result);
    thread::sleep(CONNECTION_GRACE);
    assert_eq!(
        peer.attempts(),
        0,
        "no native form support means no approval"
    );
    assert_eq!(server.audit_record("insert_row")["approval"], "unavailable");
}

#[test]
fn relay_arguments_are_rejected_before_the_gate() {
    let peer = CountingPeer::start();
    let mut server = Server::start("write", peer.port, Yolo::Off, false, false);
    let statement = json!({"table": {"name": "Items"}, "values": {"id": 1}});
    for arguments in [
        json!({"approval_token": "0123456789abcdef0123456789abcdef", "approval_decision": "grant"}),
        json!({"approval_token": "0123456789abcdef0123456789abcdef", "approval_decision": "deny"}),
        json!({"approval_token": "0123456789abcdef0123456789abcdef"}),
        json!({"approval_decision": "grant"}),
    ] {
        let mut retry = statement.clone();
        for (key, value) in arguments.as_object().expect("object arguments") {
            retry[key] = value.clone();
        }
        let result = server.call_tool("insert_row", retry);
        assert_eq!(
            approval_error(&result)["code"],
            "invalid_arguments",
            "{arguments}: {result}"
        );
        assert_eq!(
            approval_error(&result)["mutation_outcome"],
            "not_attempted",
            "{arguments}: {result}"
        );
    }
    thread::sleep(CONNECTION_GRACE);
    assert_eq!(peer.attempts(), 0, "no refusal may reach SQL Server");
}

#[test]
fn accepting_the_prompt_reaches_the_database_after_approval() {
    let (port, _peer) = support::dropping_peer();
    let mut server = Server::start("both", port, Yolo::Off, true, true);
    let id = server.begin_tool_call(
        "update_rows",
        json!({
            "table": {"schema": "sales", "name": "Items"},
            "values": {"label": VALUE_MARKER, "qty": 2},
            "filters": {"id": 7, "region": FILTER_MARKER},
        }),
    );
    let request = server.await_elicitation();
    let params = &request["params"];
    let message = params["message"]
        .as_str()
        .unwrap_or_else(|| panic!("approval prompt without a message: {request}"));
    for expected in [
        "update_rows",
        r#"schema "sales", table "Items""#,
        "label",
        "qty",
        "id",
        "region",
        r#""label":"approval-value-marker-1f4b""#,
        r#""qty":2"#,
        r#""id":7"#,
        r#""region":"approval-filter-marker-8c2d""#,
        "100",
    ] {
        assert!(
            message.contains(expected),
            "the prompt must name {expected:?}: {message}"
        );
    }
    for leaked in ["UPDATE", "SET ", "@P1", "127.0.0.1", "test-password"] {
        assert!(
            !message.contains(leaked),
            "the prompt leaked {leaked:?}: {message}"
        );
    }
    assert_eq!(
        params["requestedSchema"]["type"], "object",
        "elicitation requires an object schema: {request}"
    );
    assert_eq!(
        params["requestedSchema"]["properties"]["confirm"]["type"], "boolean",
        "the typed answer must ask for a boolean confirmation: {request}"
    );
    assert!(
        params["requestedSchema"]["required"]
            .as_array()
            .is_some_and(|required| required.contains(&json!("confirm"))),
        "the confirmation must be required: {request}"
    );

    server.answer(
        &request,
        json!({"action": "accept", "content": {"confirm": true}}),
    );
    let response = server.response(id);
    let result = &response["result"];
    let error = approval_error(result);
    assert_eq!(
        error["code"], "connection_failed",
        "approval must precede the connection: {response}"
    );
    assert_eq!(error["mutation_outcome"], "not_attempted", "{response}");
    assert_eq!(
        server.audit_record("update_rows")["approval"],
        "granted",
        "the audit trail must record the granted approval"
    );
}

#[test]
fn structured_mutation_approval_shows_exact_details_without_logging_them() {
    let peer = CountingPeer::start();
    let mut server = Server::start("both", peer.port, Yolo::Off, true, true);

    // Deterministic JSON: keys follow the validated BTreeMap order, string
    // values keep their escapes, numbers/bools/null keep their JSON types.
    let insert_values = format!(
        r#"{{"cleared":null,"enabled":true,"note":"approval-detail-escape quote\" slash\\ newline\ntab\tbell\u0007 next-line\u0085 bidi\u202e","qty":{DETAIL_NUMBER},"sku":"{DETAIL_VALUE_TEXT}"}}"#
    );
    let update_values = format!(r#"{{"balance":0,"cleared":null,"sku":"{DETAIL_VALUE_TEXT}"}}"#);
    let update_filters = format!(
        r#"{{"enabled":false,"id":{DETAIL_NUMBER},"missing":null,"region":"{DETAIL_FILTER_TEXT}"}}"#
    );
    let delete_filters =
        format!(r#"{{"id":{DETAIL_NUMBER},"missing":null,"region":"{DETAIL_FILTER_TEXT}"}}"#);

    let cases = [
        (
            "insert_row",
            json!({
                "table": {"schema": "sales", "name": "Items"},
                "values": {
                    "sku": DETAIL_VALUE_TEXT,
                    "qty": DETAIL_NUMBER,
                    "enabled": true,
                    "cleared": null,
                    "note": DETAIL_ESCAPE_TEXT,
                },
            }),
            vec![
                insert_values,
                "none (no existing rows are matched)".to_owned(),
            ],
        ),
        (
            "update_rows",
            json!({
                "table": {"schema": "sales", "name": "Items"},
                "values": {"sku": DETAIL_VALUE_TEXT, "balance": 0, "cleared": null},
                "filters": {
                    "id": DETAIL_NUMBER,
                    "region": DETAIL_FILTER_TEXT,
                    "missing": null,
                    "enabled": false,
                },
            }),
            vec![update_values, update_filters],
        ),
        (
            "delete_rows",
            json!({
                "table": {"schema": "sales", "name": "Items"},
                "filters": {
                    "id": DETAIL_NUMBER,
                    "region": DETAIL_FILTER_TEXT,
                    "missing": null,
                },
            }),
            vec![
                delete_filters,
                "none (no column values are written)".to_owned(),
            ],
        ),
    ];

    for (tool, arguments, expected) in cases {
        let id = server.begin_tool_call(tool, arguments);
        let request = server.await_elicitation();
        let message = request["params"]["message"]
            .as_str()
            .unwrap_or_else(|| panic!("{tool}: the approval form has no message: {request}"));
        for fragment in &expected {
            assert!(
                message.contains(fragment),
                "{tool}: the prompt must show {fragment:?}: {message}"
            );
        }
        for expected in [
            "Values to write (JSON; null is SQL NULL, \"null\" is text)",
            "Filters (equality predicates joined with AND; JSON null matches SQL NULL)",
            "At most 100 rows are targeted; a larger change is rolled back.",
            tool,
            r#"schema "sales", table "Items""#,
        ] {
            assert!(
                message.contains(expected),
                "{tool}: the prompt must state {expected:?}: {message}"
            );
        }
        assert!(
            message.chars().all(|character| !character.is_control()),
            "{tool}: a raw control character can misrepresent the request: {message}"
        );
        for raw_sql in ["INSERT INTO", "UPDATE TOP", "DELETE TOP", "@P1"] {
            assert!(
                !message.contains(raw_sql),
                "{tool}: the structured form must not carry SQL: {message}"
            );
        }

        // Decline after reading the form: nothing may reach the database and
        // the details may not travel back through the tool result or the log.
        server.answer(&request, json!({"action": "decline"}));
        let response = server.response(id);
        let result = &response["result"];
        let error = approval_error(result);
        assert_eq!(error["code"], "approval_denied", "{tool}: {response}");
        assert_eq!(
            error["mutation_outcome"], "not_attempted",
            "{tool}: {response}"
        );
        assert_eq!(error["retryable"], false, "{tool}: {response}");
        assert_eq!(error_text(result), error["message"], "{tool}: {response}");
        let reply = response.to_string();
        assert!(
            !reply.contains(DETAIL_PREFIX),
            "{tool}: the tool result echoed the approval values: {reply}"
        );
        assert!(
            !reply.contains(&DETAIL_NUMBER.to_string()),
            "{tool}: the tool result echoed the approval number: {reply}"
        );

        let audit = server.audit_record(tool);
        assert_eq!(audit["approval"], "denied", "{tool}: {audit}");
        let audit_text = server.audit.lock().expect("audit lines").join("\n");
        assert!(
            !audit_text.contains(DETAIL_PREFIX),
            "{tool}: the audit trail echoed the approval values: {audit_text}"
        );
        assert!(
            !audit_text.contains(&DETAIL_NUMBER.to_string()),
            "{tool}: the audit trail echoed the approval number: {audit_text}"
        );

        thread::sleep(CONNECTION_GRACE);
        assert_eq!(
            peer.attempts(),
            0,
            "{tool}: a declined mutation must not connect"
        );
    }
}

#[test]
fn declining_cancelling_or_rejecting_the_prompt_refuses_without_connecting() {
    for answer in [
        json!({"action": "decline"}),
        json!({"action": "cancel"}),
        json!({"action": "accept", "content": {"confirm": false}}),
    ] {
        let peer = CountingPeer::start();
        let mut server = Server::start("write", peer.port, Yolo::Off, true, true);
        let id = server.begin_tool_call(
            "delete_rows",
            json!({"table": {"name": "Items"}, "filters": {"id": 1}}),
        );
        let request = server.await_elicitation();
        server.answer(&request, answer.clone());
        let response = server.response(id);
        let result = &response["result"];
        let error = approval_error(result);
        assert_eq!(error["code"], "approval_denied", "{answer}: {response}");
        assert_eq!(error["mutation_outcome"], "not_attempted", "{answer}");
        assert_eq!(error["retryable"], false, "{answer}");
        assert!(
            error["action"]
                .as_str()
                .is_some_and(|action| !action.is_empty()),
            "{answer}: denial without guidance: {response}"
        );
        assert_eq!(error_text(result), error["message"], "{answer}: {response}");
        thread::sleep(CONNECTION_GRACE);
        assert_eq!(
            peer.attempts(),
            0,
            "{answer}: a refused mutation must not connect"
        );
        assert_eq!(
            server.audit_record("delete_rows")["approval"],
            "denied",
            "{answer}: the audit trail must record the refusal"
        );
    }
}

#[test]
fn cancelling_a_tool_call_with_pending_approval_does_not_connect() {
    let peer = CountingPeer::start();
    let mut server = Server::start("write", peer.port, Yolo::Off, true, true);
    let id = server.begin_tool_call(
        "delete_rows",
        json!({"table": {"name": "Items"}, "filters": {"id": 1}}),
    );
    server.await_elicitation();
    server.notify(
        "notifications/cancelled",
        json!({"requestId": id, "reason": "test"}),
    );

    // A cancelled request may not get a response. The audit record confirms
    // the handler observed cancellation before the unanswered prompt timed out.
    let audit = server.audit_record("delete_rows");
    assert_eq!(audit["approval"], "denied", "{audit}");
    assert_eq!(audit["code"], "approval_denied", "{audit}");

    let response = server.request("tools/list", json!({}));
    assert!(
        response["result"]["tools"]
            .as_array()
            .is_some_and(|tools| tools.iter().any(|tool| tool["name"] == "delete_rows")),
        "the server must still respond to tools/list: {response}"
    );
    assert_eq!(peer.attempts(), 0, "a cancelled mutation must not connect");
}

#[test]
fn yolo_skips_the_prompt_and_reaches_the_database() {
    for yolo in [Yolo::Flag, Yolo::Env] {
        let (port, _peer) = support::dropping_peer();
        // The client cannot elicit, so the call must ask instead of reaching the
        // database layer.
        let mut server = Server::start("write", port, yolo, false, false);
        let result = server.call_tool(
            "insert_row",
            json!({"table": {"name": "Items"}, "values": {"id": 1}}),
        );
        let error = approval_error(&result);
        assert_eq!(
            error["code"], "connection_failed",
            "{yolo:?} must reach the database layer: {result}"
        );
        assert_eq!(error["mutation_outcome"], "not_attempted", "{yolo:?}");
        assert_eq!(
            server.audit_record("insert_row")["approval"],
            "yolo",
            "{yolo:?}: the audit trail must record the bypass"
        );
    }

    // The environment variable is a boolean: an explicit false keeps the gate
    // on instead of being treated as present.
    let peer = CountingPeer::start();
    let mut server = Server::start("write", peer.port, Yolo::EnvFalse, false, false);
    let result = server.call_tool(
        "insert_row",
        json!({"table": {"name": "Items"}, "values": {"id": 1}}),
    );
    approval_unavailable(&result);
    thread::sleep(CONNECTION_GRACE);
    assert_eq!(
        peer.attempts(),
        0,
        "MSSQL_MCP_YOLO=false must not let the mutation connect"
    );
}

#[test]
fn validation_rejections_are_reported_before_the_gate() {
    let mut server = Server::start(
        "write",
        support::unique_loopback_port(),
        Yolo::Off,
        true,
        true,
    );
    let result = server.call_tool(
        "insert_row",
        json!({"table": {"name": "Items"}, "values": {}}),
    );
    let error = approval_error(&result);
    assert_eq!(error["code"], "invalid_input", "{result}");
    assert_eq!(error["mutation_outcome"], "not_attempted", "{result}");
}

#[test]
fn read_tools_never_ask_for_approval() {
    let (port, _peer) = support::dropping_peer();
    let mut server = Server::start("read", port, Yolo::Off, true, true);
    let result = server.call_tool(
        "select_rows",
        json!({"table": {"name": "Items"}, "columns": ["id"], "limit": 5}),
    );
    let error = approval_error(&result);
    assert_eq!(error["code"], "connection_failed", "{result}");
    assert!(
        error.get("mutation_outcome").is_none(),
        "a read must not report a mutation outcome: {result}"
    );
    assert!(
        server.audit_record("select_rows").get("approval").is_none(),
        "a read record must not carry an approval outcome"
    );
}
