use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

const RESPONSE_TIMEOUT: Duration = Duration::from_secs(15);

const READ_TOOLS: &[&str] = &["list_tables", "describe_table", "select_rows"];
const WRITE_TOOLS: &[&str] = &["insert_row", "update_rows", "delete_rows"];
const ALL_TOOLS: &[&str] = &[
    "list_tables",
    "describe_table",
    "select_rows",
    "insert_row",
    "update_rows",
    "delete_rows",
];

fn closed_port() -> u16 {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind loopback");
    let port = listener.local_addr().expect("loopback address").port();
    drop(listener);
    port
}

struct McpServer {
    child: Child,
    stdin: Option<ChildStdin>,
    messages: Receiver<Result<Value, String>>,
    next_id: u64,
}

impl McpServer {
    fn start(mode: &str, port: u16) -> Self {
        let mut command = Command::new(env!("CARGO_BIN_EXE_mssql-mcp"));
        command
            .env_clear()
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
            .stderr(Stdio::null());
        let mut child = command.spawn().expect("spawn MCP server");
        let stdin = child.stdin.take().expect("server stdin");
        let stdout = child.stdout.take().expect("server stdout");
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
                        serde_json::from_str::<Value>(&line)
                            .map(Some)
                            .map_err(|error| format!("non-JSON server output: {error}: {line}"))
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
            next_id: 1,
        };
        server.initialize();
        server
    }

    fn initialize(&mut self) {
        let response = self.request(
            "initialize",
            json!({
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "mcp-stdio-test", "version": "0.1.0"}
            }),
        );
        assert!(
            response.get("error").is_none(),
            "initialize failed: {response}"
        );
        assert!(
            response["result"]["protocolVersion"].is_string(),
            "initialize returned no protocol version: {response}"
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
            match self.messages.recv_timeout(remaining) {
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

    fn call_tool(&mut self, name: &str, arguments: Value) -> Value {
        let response = self.request("tools/call", json!({"name": name, "arguments": arguments}));
        response
            .get("result")
            .cloned()
            .unwrap_or_else(|| panic!("tools/call failed: {response}"))
    }

    fn close(self) {
        drop(self);
    }

    fn wait_for_exit(mut self) -> Option<i32> {
        drop(self.stdin.take());
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match self.child.try_wait().expect("poll server") {
                Some(status) => return status.code(),
                None if Instant::now() < deadline => thread::sleep(Duration::from_millis(20)),
                None => {
                    let _ = self.child.kill();
                    let _ = self.child.wait();
                    panic!("server did not exit after client EOF");
                }
            }
        }
    }
}

impl Drop for McpServer {
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

fn tool_error_text(result: &Value) -> &str {
    assert_eq!(
        result["isError"], true,
        "expected a tool error, got {result}"
    );
    result["content"][0]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("tool error without text: {result}"))
}

fn structured_error(result: &Value) -> &Value {
    assert_eq!(
        result["isError"], true,
        "expected a tool error, got {result}"
    );
    result
        .get("structuredContent")
        .filter(|error| error.is_object())
        .unwrap_or_else(|| panic!("tool error without structured content: {result}"))
}

#[test]
fn mode_lists_are_fixed() {
    for (mode, expected) in [
        ("read", READ_TOOLS),
        ("write", WRITE_TOOLS),
        ("both", ALL_TOOLS),
    ] {
        let mut server = McpServer::start(mode, 1);
        let actual = server.tool_names();
        assert_eq!(
            actual.iter().map(String::as_str).collect::<Vec<_>>(),
            expected,
            "unexpected tool list for {mode} mode"
        );
        server.close();
    }
}

#[test]
fn tools_expose_object_input_schemas() {
    let mut server = McpServer::start("both", 1);
    let response = server.request("tools/list", json!({}));
    let tools = response["result"]["tools"]
        .as_array()
        .expect("tools/list result");
    assert_eq!(tools.len(), ALL_TOOLS.len());
    for tool in tools {
        let name = tool["name"].as_str().expect("tool name");
        assert_eq!(
            tool["inputSchema"]["type"], "object",
            "{name} input schema must be an object"
        );
        assert!(
            tool["inputSchema"]["properties"].is_object(),
            "{name} input schema must describe properties"
        );
    }
    let select = tools
        .iter()
        .find(|tool| tool["name"] == "select_rows")
        .expect("select_rows present");
    let required = select["inputSchema"]["required"]
        .as_array()
        .expect("required list");
    assert!(required.contains(&json!("table")));
    assert!(required.contains(&json!("columns")));
    server.close();
}

#[test]
fn disallowed_tools_are_rejected_without_connecting() {
    let port = closed_port();
    let mut server = McpServer::start("read", port);
    let result = server.call_tool(
        "insert_row",
        json!({"table": {"name": "Items"}, "values": {"id": 1}}),
    );
    assert_eq!(
        tool_error_text(&result),
        "tool is not available in read mode"
    );
    let error = structured_error(&result);
    assert_eq!(error["code"], "mode_unavailable");
    assert_eq!(error["retryable"], false);
    assert_eq!(error["mutation_outcome"], "not_attempted");
    assert!(
        error["action"]
            .as_str()
            .is_some_and(|action| !action.is_empty()),
        "mode rejection without guidance: {result}"
    );
    server.close();

    let mut server = McpServer::start("write", port);
    let result = server.call_tool("list_tables", json!({}));
    assert_eq!(
        tool_error_text(&result),
        "tool is not available in write mode"
    );
    let error = structured_error(&result);
    assert_eq!(error["code"], "mode_unavailable");
    assert!(error.get("mutation_outcome").is_none());
    server.close();
}

#[test]
fn valid_read_against_closed_loopback_returns_a_sanitized_error() {
    let port = closed_port();
    let mut server = McpServer::start("read", port);
    let result = server.call_tool("list_tables", json!({"schema": "dbo"}));
    let text = tool_error_text(&result);
    assert!(
        matches!(
            text,
            "database operation failed" | "database operation timed out"
        ),
        "unexpected database error text: {text}"
    );
    let error = structured_error(&result);
    assert_eq!(error["message"], text);
    assert_eq!(error["code"], "connection_failed");
    assert_eq!(error["retryable"], false);
    assert!(error.get("mutation_outcome").is_none());
    assert!(
        error["action"]
            .as_str()
            .is_some_and(|action| !action.is_empty()),
        "database error without guidance: {result}"
    );
    let reply = result.to_string();
    for leaked in [
        "127.0.0.1",
        "Connection refused",
        "connection refused",
        "test-password",
        "MSSQL_",
        "tiberius",
        "TDS",
        "SELECT",
    ] {
        assert!(
            !reply.contains(leaked),
            "database error leaked {leaked:?}: {reply}"
        );
    }
    server.close();
}

#[test]
fn malformed_arguments_are_rejected_before_database_access() {
    let port = closed_port();
    let mut server = McpServer::start("read", port);
    let result = server.call_tool(
        "select_rows",
        json!({"table": {"name": "Items"}, "columns": []}),
    );
    assert_eq!(
        tool_error_text(&result),
        "columns must contain between 1 and 16 names"
    );
    let error = structured_error(&result);
    assert_eq!(error["code"], "invalid_input");
    assert_eq!(error["retryable"], false);

    let result = server.call_tool(
        "select_rows",
        json!({"table": {"name": "Items"}, "columns": ["id"], "limit": 0}),
    );
    assert_eq!(tool_error_text(&result), "limit must be between 1 and 100");
    assert_eq!(structured_error(&result)["code"], "invalid_input");

    let result = server.call_tool(
        "select_rows",
        json!({"table": {"name": "Items"}, "columns": ["id"], "unknown": true}),
    );
    assert_eq!(
        tool_error_text(&result),
        "invalid arguments for select_rows"
    );
    let error = structured_error(&result);
    assert_eq!(error["code"], "invalid_arguments");
    assert_eq!(error["retryable"], false);
    server.close();
}

#[test]
fn write_validation_rejection_is_not_attempted() {
    let port = closed_port();
    let mut server = McpServer::start("write", port);
    let result = server.call_tool(
        "insert_row",
        json!({"table": {"name": "Items"}, "values": {}}),
    );
    assert_eq!(
        tool_error_text(&result),
        "values must contain between 1 and 16 entries"
    );
    let error = structured_error(&result);
    assert_eq!(error["code"], "invalid_input");
    assert_eq!(error["mutation_outcome"], "not_attempted");

    let result = server.call_tool(
        "delete_rows",
        json!({"table": {"name": "Items"}, "filters": "bad"}),
    );
    assert_eq!(
        tool_error_text(&result),
        "invalid arguments for delete_rows"
    );
    let error = structured_error(&result);
    assert_eq!(error["code"], "invalid_arguments");
    assert_eq!(error["mutation_outcome"], "not_attempted");
    server.close();
}

#[test]
fn client_eof_exits_normally() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_mssql-mcp"))
        .env_clear()
        .env("MSSQL_USER", "sa")
        .env("MSSQL_PASSWORD", "test-password")
        .args([
            "--host",
            "127.0.0.1",
            "--port",
            "1",
            "--database",
            "test",
            "--mode",
            "read",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn server");
    drop(child.stdin.take());
    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        match child.try_wait().expect("poll server") {
            Some(status) => break status,
            None if Instant::now() < deadline => thread::sleep(Duration::from_millis(20)),
            None => {
                let _ = child.kill();
                let _ = child.wait();
                panic!("server did not exit after client EOF before initialize");
            }
        }
    };
    assert_eq!(status.code(), Some(0));

    let server = McpServer::start("read", 1);
    assert_eq!(server.wait_for_exit(), Some(0));
}
