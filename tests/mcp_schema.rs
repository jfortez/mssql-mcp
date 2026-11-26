//! Behaviour of the `explore_schema` tool over stdio.
//!
//! The tool is read-only and available in every mode. No live SQL Server is
//! used: a loopback peer that speaks nothing makes an attempted catalog query
//! fail deterministically, and an elicitation-capable client proves that the
//! tool never asks for approval.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

mod support;

const RESPONSE_TIMEOUT: Duration = Duration::from_secs(15);
const PASSWORD: &str = "schema-test-password-c92a";
const HOST: &str = "127.0.0.1";

struct Server {
    child: Child,
    stdin: Option<ChildStdin>,
    messages: Receiver<Result<Value, String>>,
    audit: Arc<Mutex<Vec<String>>>,
    next_id: u64,
}

impl Server {
    fn start(mode: &str, port: u16, elicitation: bool) -> Self {
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
                "clientInfo": {"name": "mcp-schema-test", "version": "0.1.0"}
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

    fn response(&mut self, id: u64) -> Value {
        let deadline = Instant::now() + RESPONSE_TIMEOUT;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            match self.messages.recv_timeout(remaining) {
                Ok(Ok(message)) if message.get("id").and_then(Value::as_u64) == Some(id) => {
                    if let Some(method) = message.get("method") {
                        panic!("unexpected server request before response {id}: {method}");
                    }
                    return message;
                }
                Ok(Ok(message)) if message.get("method").is_some() => {
                    panic!("the schema tool must not prompt the client: {message}");
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

#[test]
fn explore_schema_is_listed_in_every_mode() {
    for mode in ["read", "write", "both"] {
        let mut server = Server::start(mode, support::unique_loopback_port(), false);
        assert!(
            server
                .tool_names()
                .iter()
                .any(|name| name == "explore_schema"),
            "{mode} mode must list explore_schema"
        );
    }
}

#[test]
fn closed_database_returns_a_sanitized_error_without_approval() {
    let (port, _peer) = support::dropping_peer();
    let mut server = Server::start("read", port, true);
    for arguments in [
        json!({}),
        json!({"schema": "dbo", "include": ["tables"]}),
        json!({"definition": true}),
    ] {
        let result = server.call_tool("explore_schema", arguments.clone());
        let text = error_text(&result);
        assert_eq!(text, "database operation failed", "{arguments}: {result}");
        let error = tool_error(&result);
        assert_eq!(error["code"], "connection_failed", "{arguments}: {result}");
        assert_eq!(error["retryable"], false, "{arguments}");
        assert!(
            error.get("mutation_outcome").is_none(),
            "a read must not report a mutation outcome: {result}"
        );
        let reply = result.to_string();
        for leaked in [
            HOST, PASSWORD, "sys.", "SELECT", "@P1", "tiberius", "TDS", "MSSQL_",
        ] {
            assert!(
                !reply.contains(leaked),
                "{arguments}: the error leaked {leaked:?}: {reply}"
            );
        }
        let record = server.audit_record("explore_schema");
        assert!(
            record.get("approval").is_none(),
            "explore_schema must not carry an approval outcome: {record}"
        );
        assert_eq!(
            record["scope"],
            match arguments.get("schema").and_then(Value::as_str) {
                Some(schema) => json!({"schema": schema}),
                None => json!({"catalog": true}),
            },
            "{arguments}: {record}"
        );
    }
}

#[test]
fn definition_is_accepted_in_read_mode_without_approval() {
    let (port, _peer) = support::dropping_peer();
    let mut server = Server::start("read", port, true);
    let result = server.call_tool(
        "explore_schema",
        json!({"include": ["procedures", "functions"], "definition": true}),
    );
    let error = tool_error(&result);
    assert_eq!(
        error["code"], "connection_failed",
        "definition must stay available in read mode: {result}"
    );
    assert!(
        error.get("mutation_outcome").is_none(),
        "definition must not turn the read tool into a mutation: {result}"
    );
    assert!(
        server
            .audit_record("explore_schema")
            .get("approval")
            .is_none(),
        "definition must not trigger the approval gate"
    );
}

#[test]
fn invalid_arguments_are_rejected_before_connecting() {
    let mut server = Server::start("read", support::unique_loopback_port(), false);
    for (arguments, message, code) in [
        (
            json!({"schema": "bad\u{7}name"}),
            "identifier must contain 1–128 UTF-16 units and no control characters",
            "invalid_input",
        ),
        (
            json!({"include": []}),
            "include must contain at least one of tables, views, procedures, functions",
            "invalid_input",
        ),
        (
            json!({"include": ["triggers"]}),
            "invalid arguments for explore_schema",
            "invalid_arguments",
        ),
        (
            json!({"definition": "yes"}),
            "invalid arguments for explore_schema",
            "invalid_arguments",
        ),
        (
            json!({"unknown": true}),
            "invalid arguments for explore_schema",
            "invalid_arguments",
        ),
    ] {
        let result = server.call_tool("explore_schema", arguments);
        assert_eq!(error_text(&result), message, "{result}");
        assert_eq!(tool_error(&result)["code"], code, "{result}");
        assert!(
            !result.to_string().contains("\"mutation_outcome\""),
            "a schema validation error is not a mutation: {result}"
        );
    }
}
