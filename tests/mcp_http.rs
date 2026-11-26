//! Behaviour of the authenticated shared Streamable HTTP transport.
//!
//! One daemon binds one address and every compatible client addresses the same
//! `/mcp` endpoint. These tests drive the released executable over loopback TCP
//! with hand-written HTTP/1.1 so that hostile headers, duplicate credentials and
//! oversized chunked bodies can be exercised. No live SQL Server is used: the
//! configured database port is closed, so dispatched calls fail deterministically.

use std::io::{self, BufRead, BufReader, Read, Write};

mod support;
use std::net::{SocketAddr, TcpStream};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

/// Synthetic credential; never a real token.
const TOKEN: &str = "mssql-mcp-http-test-token_0123456789";
/// Same length as [`TOKEN`] so the failure is the value, not the length.
const WRONG_TOKEN: &str = "mssql-mcp-http-test-token_9876543210";

const BODY_LIMIT: usize = 1024 * 1024;
const IO_TIMEOUT: Duration = Duration::from_secs(20);
const STARTUP_TIMEOUT: Duration = Duration::from_secs(20);
const EXIT_TIMEOUT: Duration = Duration::from_secs(15);

const READ_TOOLS: [&str; 11] = [
    "list_tables",
    "describe_table",
    "select_rows",
    "explore_schema",
    "schema_map",
    "object_dependencies",
    "routine_definition",
    "dependency_columns",
    "search_objects",
    "table_stats",
    "sample_rows",
];
const WRITE_TOOLS: [&str; 5] = [
    "insert_row",
    "update_rows",
    "delete_rows",
    "query",
    "explore_schema",
];

#[test]
fn one_daemon_serves_independent_clients() {
    let mut daemon = Daemon::start("read");

    let first = send(daemon.addr, authorized(initialize(1, "client-a")));
    assert_eq!(first.status, 200, "initialize failed: {}", first.text());
    assert!(
        first.header("mcp-session-id").is_none(),
        "the shared transport must not hand out per-client sessions: {:?}",
        first.headers
    );
    let first = first.json();
    assert_eq!(first["result"]["serverInfo"]["name"], "mssql-mcp");
    assert!(
        first["result"]["protocolVersion"].is_string(),
        "initialize returned no protocol version: {first}"
    );

    let second = send(daemon.addr, authorized(initialize(1, "client-b")));
    assert_eq!(
        second.status,
        200,
        "the second client failed to initialize: {}",
        second.text()
    );
    assert_eq!(
        second.json()["result"]["serverInfo"],
        first["result"]["serverInfo"],
        "both clients must reach the same server identity"
    );

    assert_eq!(tool_names(&daemon, 2), READ_TOOLS);
    assert_eq!(tool_names(&daemon, 3), READ_TOOLS);
    let allowed = send(daemon.addr, authorized(tools_list(6)));
    assert!(
        allowed.header("access-control-allow-origin").is_none(),
        "the transport must not grant CORS access: {:?}",
        allowed.headers
    );

    for (client, id) in [("client-a", 4), ("client-b", 5)] {
        let response = send(daemon.addr, authorized(tools_call(id, "list_tables")));
        assert_eq!(
            response.status,
            200,
            "tools/call failed for {client}: {}",
            response.text()
        );
        let result = response.json()["result"].clone();
        assert_eq!(
            result["isError"], true,
            "{client} did not reach the tool handler: {result}"
        );
        assert_eq!(
            result["structuredContent"]["code"], "connection_failed",
            "{client} did not reach the database layer: {result}"
        );
    }

    assert!(
        daemon.is_running(),
        "the shared daemon must stay alive between clients"
    );
    wait_for_tool_calls(&daemon, 2);
    let stderr = daemon.stderr_text();
    assert!(
        stderr.contains("\"event\":\"startup\""),
        "missing audit startup record: {stderr}"
    );
    assert_eq!(
        stderr.matches("\"event\":\"tool_call\"").count(),
        2,
        "both clients' calls must reach the one process: {stderr}"
    );
    assert!(
        daemon.stdout_text().is_empty(),
        "HTTP mode must keep stdout free of MCP frames: {}",
        daemon.stdout_text()
    );
    assert!(
        !stderr.contains(TOKEN),
        "the token must never be logged: {stderr}"
    );
}

#[test]
fn a_second_daemon_cannot_bind_the_same_address() {
    let mut daemon = Daemon::start("read");

    let mut command = daemon_command(daemon.bind, "read", &[]);
    let mut second = command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn the second daemon");
    let stderr = capture(second.stderr.take().expect("second daemon stderr"));
    let status = wait_for_exit(&mut second, EXIT_TIMEOUT);
    let Some(status) = status else {
        let _ = second.kill();
        let _ = second.wait();
        panic!("the second daemon kept running instead of failing to bind");
    };
    assert!(
        !status.success(),
        "the second daemon must fail instead of sharing the address"
    );
    let stderr = stderr.lock().expect("second daemon stderr").join("\n");
    assert!(
        stderr.contains(&daemon.bind.to_string()),
        "the bind error must name the address: {stderr}"
    );
    assert!(
        stderr.contains("already in use"),
        "the bind error must be actionable: {stderr}"
    );
    assert!(
        !stderr.contains(TOKEN),
        "the bind error must not leak the token: {stderr}"
    );

    let response = send(
        daemon.addr,
        authorized(initialize(1, "client-after-bind-conflict")),
    );
    assert_eq!(
        response.status,
        200,
        "the first daemon stopped serving: {}",
        response.text()
    );
    assert!(daemon.is_running(), "the first daemon exited");
}

#[test]
fn mcp_requests_require_exactly_one_valid_bearer_token() {
    let daemon = Daemon::start("read");
    let call = tools_call(1, "list_tables");

    assert_unauthorized(&send(daemon.addr, rpc(call.clone())));
    assert_unauthorized(&send(
        daemon.addr,
        rpc(call.clone()).with_header("authorization", &format!("Bearer {WRONG_TOKEN}")),
    ));
    assert_unauthorized(&send(
        daemon.addr,
        rpc(call.clone()).with_header("authorization", &format!("Basic {TOKEN}")),
    ));
    assert_unauthorized(&send(
        daemon.addr,
        rpc(call.clone()).with_header("authorization", &format!("Bearer {}", &TOKEN[..16])),
    ));
    assert_unauthorized(&send(
        daemon.addr,
        rpc(call.clone())
            .with_header("authorization", &format!("Bearer {TOKEN}"))
            .with_header("authorization", &format!("Bearer {WRONG_TOKEN}")),
    ));
    assert_unauthorized(&send(
        daemon.addr,
        rpc(call.clone())
            .with_header("authorization", &format!("Bearer {WRONG_TOKEN}"))
            .with_header("authorization", &format!("Bearer {TOKEN}")),
    ));

    let stderr = daemon.stderr_text();
    assert!(
        !stderr.contains("\"event\":\"tool_call\""),
        "rejected requests must not reach tool dispatch: {stderr}"
    );

    let accepted = send(daemon.addr, authorized(tools_list(2)));
    assert_eq!(
        accepted.status,
        200,
        "the daemon must keep serving after rejected requests: {}",
        accepted.text()
    );
}

#[test]
fn foreign_origin_and_host_are_rejected_before_dispatch() {
    let daemon = Daemon::start("read");
    let call = tools_call(1, "list_tables");

    let foreign_origin = send(
        daemon.addr,
        authorized(call.clone()).with_header("origin", "https://evil.example"),
    );
    assert_eq!(
        foreign_origin.status,
        403,
        "a supplied foreign Origin must be rejected: {}",
        foreign_origin.text()
    );
    assert!(
        !foreign_origin.text().contains("list_tables"),
        "a foreign Origin was dispatched: {}",
        foreign_origin.text()
    );

    let loopback_origin = send(
        daemon.addr,
        authorized(call.clone()).with_header("origin", &format!("http://{}", daemon.addr)),
    );
    assert_eq!(
        loopback_origin.status,
        403,
        "the HTTP transport must not be a permissive CORS endpoint: {}",
        loopback_origin.text()
    );

    let foreign_host = send(
        daemon.addr,
        authorized(call.clone()).with_header("host", "evil.example"),
    );
    assert_eq!(
        foreign_host.status,
        403,
        "a DNS-rebinding Host must be rejected: {}",
        foreign_host.text()
    );

    let accepted = send(daemon.addr, authorized(call));
    assert_eq!(
        accepted.status,
        200,
        "a client without Origin must be allowed: {}",
        accepted.text()
    );
    wait_for_tool_calls(&daemon, 1);
    let stderr = daemon.stderr_text();
    assert_eq!(
        stderr.matches("\"event\":\"tool_call\"").count(),
        1,
        "rejected requests must not reach dispatch: {stderr}"
    );
}

#[test]
fn mode_gates_apply_to_the_http_transport() {
    let read = Daemon::start("read");
    assert_eq!(tool_names(&read, 1), READ_TOOLS);
    let result = tool_result(&read, 2, "insert_row");
    assert_eq!(
        result["isError"], true,
        "a mutation must be refused in read mode: {result}"
    );
    assert_eq!(result["structuredContent"]["code"], "mode_unavailable");
    assert_eq!(
        result["structuredContent"]["mutation_outcome"],
        "not_attempted"
    );

    let write = Daemon::start("write");
    assert_eq!(tool_names(&write, 1), WRITE_TOOLS);
    let result = tool_result(&write, 2, "list_tables");
    assert_eq!(
        result["isError"], true,
        "a read must be refused in write mode: {result}"
    );
    assert_eq!(result["structuredContent"]["code"], "mode_unavailable");
    assert!(
        result["structuredContent"]
            .get("mutation_outcome")
            .is_none(),
        "a refused read was not a mutation: {result}"
    );
}

#[test]
fn mutations_need_a_relayed_answer_over_http_and_never_run_without_one() {
    let daemon = Daemon::start("write");
    let arguments = json!({"table": {"name": "Items"}, "values": {"id": 1}});
    let started = Instant::now();
    let response = send(
        daemon.addr,
        authorized(tools_call_with(2, "insert_row", arguments.clone())),
    );
    assert_eq!(response.status, 200, "{}", response.text());
    let result = response.json()["result"].clone();
    assert_eq!(
        result["isError"], false,
        "the question is not an error: {result}"
    );
    let question = &result["structuredContent"];
    assert_eq!(question["code"], "approval_required", "{result}");
    assert_eq!(question["mutation_outcome"], "not_attempted", "{result}");
    assert_eq!(question["row_cap"], 100, "{result}");
    assert_eq!(question["expires_in_seconds"], 120, "{result}");
    let token = question["approval_token"]
        .as_str()
        .unwrap_or_else(|| panic!("question without a token: {result}"))
        .to_owned();
    let token_id = question["approval_token_id"]
        .as_str()
        .unwrap_or_else(|| panic!("question without a token label: {result}"));
    assert_eq!(token_id, &token[..8]);
    let action = question["action"]
        .as_str()
        .unwrap_or_else(|| panic!("question without an action: {result}"));
    assert!(
        action.contains("approval_token") && action.contains("approval_decision"),
        "the action must tell the client how to answer: {action}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "a transported client cannot elicit, so the gate must ask instead of waiting for the 120 s prompt timeout"
    );
    wait_for_tool_calls(&daemon, 1);
    let stderr = daemon.stderr_text();
    assert!(
        stderr.contains("\"approval\":\"required\""),
        "the question must be audited: {stderr}"
    );
    assert!(
        stderr.contains(&format!("\"approval_token_id\":\"{token_id}\"")),
        "the audit record must carry the token label: {stderr}"
    );

    // The retry is the only call that may reach SQL Server. The daemon's
    // database port is closed, so an executed statement fails at the connection.
    let mut granted = arguments;
    granted["approval_token"] = json!(token);
    granted["approval_decision"] = json!("grant");
    let response = send(
        daemon.addr,
        authorized(tools_call_with(3, "insert_row", granted.clone())),
    );
    assert_eq!(response.status, 200, "{}", response.text());
    let result = response.json()["result"].clone();
    assert_eq!(result["isError"], true, "{result}");
    assert_eq!(
        result["structuredContent"]["code"], "connection_failed",
        "a granted retry must reach the database layer: {result}"
    );
    assert_eq!(
        result["structuredContent"]["mutation_outcome"],
        "not_attempted"
    );

    // One token, one execution: the replay is refused as reuse.
    let response = send(
        daemon.addr,
        authorized(tools_call_with(4, "insert_row", granted)),
    );
    let result = response.json()["result"].clone();
    assert_eq!(result["isError"], true, "{result}");
    assert_eq!(
        result["structuredContent"]["code"], "approval_token_reused",
        "{result}"
    );

    wait_for_tool_calls(&daemon, 3);
    let stderr = daemon.stderr_text();
    assert_eq!(
        stderr.matches("\"event\":\"tool_call\"").count(),
        3,
        "every relayed call must be audited: {stderr}"
    );
    assert!(
        stderr.contains("\"approval\":\"granted\""),
        "the answer must be audited as granted: {stderr}"
    );
    assert!(
        stderr.contains(&format!("\"approval_token_id\":\"{token_id}\"")),
        "the granted record must carry the same token label: {stderr}"
    );
}

#[test]
fn request_bodies_are_bounded_to_one_mebibyte() {
    let daemon = Daemon::start("read");

    let at_limit = send_body(daemon.addr, vec![b'x'; BODY_LIMIT], false);
    assert_ne!(
        at_limit.status,
        413,
        "a body of exactly {BODY_LIMIT} bytes must not be rejected as oversized: {}",
        at_limit.text()
    );

    let over_limit = send_body(daemon.addr, vec![b'x'; BODY_LIMIT + 1], false);
    assert_eq!(
        over_limit.status,
        413,
        "a body over {BODY_LIMIT} bytes must be rejected: {}",
        over_limit.text()
    );

    let chunked = send_body(daemon.addr, vec![b'x'; BODY_LIMIT + 1], true);
    assert_eq!(
        chunked.status,
        413,
        "a chunked body over {BODY_LIMIT} bytes must be rejected: {}",
        chunked.text()
    );
}

#[test]
fn concurrent_clients_share_the_same_daemon() {
    let mut daemon = Daemon::start("read");
    let addr = daemon.addr;
    let clients: Vec<_> = (0..4)
        .map(|_| {
            thread::spawn(move || {
                let initialized = send(addr, authorized(initialize(1, "concurrent-client")));
                assert_eq!(
                    initialized.status,
                    200,
                    "a concurrent client failed to initialize: {}",
                    initialized.text()
                );
                let called = send(addr, authorized(tools_call(2, "list_tables")));
                assert_eq!(
                    called.status,
                    200,
                    "a concurrent client failed to call: {}",
                    called.text()
                );
                called.json()["result"]["structuredContent"]["code"].clone()
            })
        })
        .collect();
    for client in clients {
        assert_eq!(
            client.join().expect("concurrent client thread"),
            json!("connection_failed")
        );
    }

    assert!(
        daemon.is_running(),
        "the shared daemon must stay alive for concurrent clients"
    );
    let stderr = daemon.stderr_text();
    assert_eq!(
        stderr.matches("\"event\":\"tool_call\"").count(),
        4,
        "every concurrent client must reach the one process: {stderr}"
    );
}

#[test]
fn an_explicit_non_loopback_bind_is_served_with_authentication() {
    let bind = SocketAddr::from(([0, 0, 0, 0], support::unique_loopback_port()));
    let mut daemon = Daemon::start_on(bind, "read");
    assert!(
        daemon.is_running(),
        "the daemon exited before serving: {}",
        daemon.stderr_text()
    );

    let response = send(daemon.addr, authorized(initialize(1, "docker-client")));
    assert_eq!(
        response.status,
        200,
        "an explicit non-loopback bind must serve clients: {}",
        response.text()
    );
    assert_unauthorized(&send(daemon.addr, rpc(tools_list(2))));
}

fn assert_unauthorized(response: &Response) {
    assert_eq!(
        response.status,
        401,
        "expected an authentication challenge, got {}: {}",
        response.status,
        response.text()
    );
    assert_eq!(
        response.header("www-authenticate"),
        Some("Bearer"),
        "missing bearer challenge: {:?}",
        response.headers
    );
    let body = response.text();
    assert!(
        !body.contains(TOKEN),
        "the response echoed the token: {body}"
    );
    assert!(
        !body.contains(WRONG_TOKEN),
        "the response echoed the credential: {body}"
    );
    assert!(
        !body.contains("list_tables") && !body.contains("\"result\""),
        "an unauthorized request was dispatched: {body}"
    );
    assert!(
        response.header("access-control-allow-origin").is_none(),
        "the transport must not grant CORS access: {:?}",
        response.headers
    );
}

fn tool_names(daemon: &Daemon, id: u64) -> Vec<String> {
    let response = send(daemon.addr, authorized(tools_list(id)));
    assert_eq!(
        response.status,
        200,
        "tools/list failed: {}",
        response.text()
    );
    response.json()["result"]["tools"]
        .as_array()
        .unwrap_or_else(|| panic!("no tool list in response: {}", response.text()))
        .iter()
        .map(|tool| {
            tool["name"]
                .as_str()
                .unwrap_or_else(|| panic!("tool without a name: {tool}"))
                .to_owned()
        })
        .collect()
}

fn tool_result(daemon: &Daemon, id: u64, tool: &str) -> Value {
    let response = send(daemon.addr, authorized(tools_call(id, tool)));
    assert_eq!(
        response.status,
        200,
        "tools/call failed for {tool}: {}",
        response.text()
    );
    response.json()["result"].clone()
}

struct Daemon {
    child: Child,
    /// Address passed to `--http-listen`.
    bind: SocketAddr,
    /// Address a client dials to reach that listener.
    addr: SocketAddr,
    stdout: Arc<Mutex<Vec<String>>>,
    stderr: Arc<Mutex<Vec<String>>>,
}

impl Daemon {
    fn start(mode: &str) -> Self {
        let bind = SocketAddr::from(([127, 0, 0, 1], support::unique_loopback_port()));
        Self::start_on(bind, mode)
    }

    fn start_on(bind: SocketAddr, mode: &str) -> Self {
        let addr = if bind.ip().is_unspecified() || bind.ip().is_loopback() {
            SocketAddr::from(([127, 0, 0, 1], bind.port()))
        } else {
            bind
        };
        let mut command = daemon_command(bind, mode, &[]);
        let mut child = command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn the HTTP daemon");
        let stdout = capture(child.stdout.take().expect("daemon stdout"));
        let stderr = capture(child.stderr.take().expect("daemon stderr"));
        let mut daemon = Self {
            child,
            bind,
            addr,
            stdout,
            stderr,
        };
        daemon.wait_until_listening();
        daemon
    }

    fn wait_until_listening(&mut self) {
        let deadline = Instant::now() + STARTUP_TIMEOUT;
        loop {
            if TcpStream::connect_timeout(&self.addr, Duration::from_millis(250)).is_ok() {
                return;
            }
            if let Ok(Some(status)) = self.child.try_wait() {
                panic!(
                    "daemon exited with {status} before listening: {}",
                    self.stderr_text()
                );
            }
            assert!(
                Instant::now() < deadline,
                "daemon did not listen on {} within {STARTUP_TIMEOUT:?}: {}",
                self.addr,
                self.stderr_text()
            );
            thread::sleep(Duration::from_millis(20));
        }
    }

    fn is_running(&mut self) -> bool {
        self.child.try_wait().expect("poll the daemon").is_none()
    }

    fn stdout_text(&self) -> String {
        self.stdout.lock().expect("daemon stdout").join("\n")
    }

    fn stderr_text(&self) -> String {
        self.stderr.lock().expect("daemon stderr").join("\n")
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn daemon_command(addr: SocketAddr, mode: &str, extra: &[String]) -> Command {
    let (database_port, _peer) = support::dropping_peer();
    let mut command = Command::new(env!("CARGO_BIN_EXE_mssql-mcp"));
    support::clear_env(&mut command)
        .env("MSSQL_USER", "sa")
        .env("MSSQL_PASSWORD", "test-password")
        .env("MSSQL_MCP_TOKEN", TOKEN)
        .args([
            "--host",
            "127.0.0.1",
            "--port",
            &database_port.to_string(),
            "--database",
            "test",
            "--timeout-seconds",
            "1",
            "--mode",
            mode,
            "--transport",
            "http",
            "--http-listen",
            &addr.to_string(),
        ])
        .args(extra);
    command
}

fn wait_for_exit(child: &mut Child, timeout: Duration) -> Option<ExitStatus> {
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait().expect("poll the child") {
            Some(status) => return Some(status),
            None if Instant::now() < deadline => thread::sleep(Duration::from_millis(20)),
            None => return None,
        }
    }
}

fn wait_for_tool_calls(daemon: &Daemon, expected: usize) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if daemon
            .stderr_text()
            .matches("\"event\":\"tool_call\"")
            .count()
            >= expected
        {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
}

fn capture<R: Read + Send + 'static>(reader: R) -> Arc<Mutex<Vec<String>>> {
    let lines = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&lines);
    thread::spawn(move || {
        let mut reader = BufReader::new(reader);
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) => break,
                Ok(_) => sink
                    .lock()
                    .expect("captured output")
                    .push(line.trim_end().to_owned()),
                Err(_) => break,
            }
        }
    });
    lines
}

fn initialize(id: u64, client: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": {"name": client, "version": "0.1.0"}
        }
    })
}

fn tools_list(id: u64) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "method": "tools/list", "params": {}})
}

fn tools_call(id: u64, tool: &str) -> Value {
    tools_call_with(id, tool, json!({}))
}

fn tools_call_with(id: u64, tool: &str, arguments: Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "tools/call",
        "params": {"name": tool, "arguments": arguments}
    })
}

struct Outgoing {
    method: &'static str,
    path: &'static str,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
    chunked: bool,
}

impl Outgoing {
    fn with_header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.to_owned(), value.to_owned()));
        self
    }
}

fn rpc(body: Value) -> Outgoing {
    Outgoing {
        method: "POST",
        path: "/mcp",
        headers: vec![
            ("content-type".to_owned(), "application/json".to_owned()),
            (
                "accept".to_owned(),
                "application/json, text/event-stream".to_owned(),
            ),
            ("mcp-protocol-version".to_owned(), "2025-06-18".to_owned()),
        ],
        body: body.to_string().into_bytes(),
        chunked: false,
    }
}

fn authorized(body: Value) -> Outgoing {
    rpc(body).with_header("authorization", &format!("Bearer {TOKEN}"))
}

fn send_body(addr: SocketAddr, body: Vec<u8>, chunked: bool) -> Response {
    let mut request = authorized(Value::Null);
    request.body = body;
    request.chunked = chunked;
    send(addr, request)
}

struct Response {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Response {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(header, _)| header == name)
            .map(|(_, value)| value.as_str())
    }

    fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    fn json(&self) -> Value {
        serde_json::from_slice(&self.body)
            .unwrap_or_else(|error| panic!("response is not JSON ({error}): {}", self.text()))
    }
}

/// Send one request over a fresh connection.
///
fn send(addr: SocketAddr, outgoing: Outgoing) -> Response {
    let mut last = None;
    for _ in 0..3 {
        match attempt(addr, &outgoing) {
            Ok(response) => return response,
            Err(error) if is_transient(&error) => last = Some(error),
            Err(error) => panic!("no HTTP response from {addr}: {error}"),
        }
    }
    panic!(
        "no HTTP response from {addr}: {}",
        last.expect("a transient error")
    );
}

/// A fresh connection that the daemon closed while starting or accepting is a
/// transient harness condition; a real HTTP failure still returns a response.
fn is_transient(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::ConnectionReset
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::BrokenPipe
    )
}

fn attempt(addr: SocketAddr, outgoing: &Outgoing) -> io::Result<Response> {
    let mut head = format!("{} {} HTTP/1.1\r\n", outgoing.method, outgoing.path);
    if !outgoing
        .headers
        .iter()
        .any(|(name, _)| name.eq_ignore_ascii_case("host"))
    {
        head.push_str(&format!("Host: {addr}\r\n"));
    }
    head.push_str("Connection: close\r\n");
    if outgoing.chunked {
        head.push_str("Transfer-Encoding: chunked\r\n");
    } else {
        head.push_str(&format!("Content-Length: {}\r\n", outgoing.body.len()));
    }
    for (name, value) in &outgoing.headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("\r\n");

    let mut stream = TcpStream::connect_timeout(&addr, IO_TIMEOUT)?;
    stream.set_read_timeout(Some(IO_TIMEOUT))?;
    stream.set_write_timeout(Some(IO_TIMEOUT))?;
    let mut reader = stream.try_clone()?;
    if outgoing.chunked {
        stream.write_all(head.as_bytes())?;
        stream.flush()?;
    }
    let pending = thread::spawn(move || read_response(&mut reader));
    let written = if outgoing.chunked {
        write_body(&mut stream, &outgoing.body, true)
    } else {
        let mut request = head.into_bytes();
        request.extend_from_slice(&outgoing.body);
        stream.write_all(&request)?;
        stream.flush()
    };
    match pending.join().expect("response reader") {
        Ok(response) => Ok(response),
        Err(error) => {
            if let Err(write_error) = &written {
                return Err(io::Error::new(write_error.kind(), write_error.to_string()));
            }
            Err(io::Error::other(format!(
                "no response after writing {written:?}: {error}"
            )))
        }
    }
}

fn write_body(stream: &mut TcpStream, body: &[u8], chunked: bool) -> io::Result<()> {
    if !chunked {
        stream.write_all(body)?;
        return stream.flush();
    }
    for chunk in body.chunks(16 * 1024) {
        write!(stream, "{:x}\r\n", chunk.len())?;
        stream.write_all(chunk)?;
        stream.write_all(b"\r\n")?;
    }
    stream.write_all(b"0\r\n\r\n")?;
    stream.flush()
}

fn read_response(stream: &mut TcpStream) -> io::Result<Response> {
    let mut reader = BufReader::new(stream);
    let mut status_line = String::new();
    reader.read_line(&mut status_line)?;
    if status_line.trim().is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "connection closed without a response",
        ));
    }
    let mut parts = status_line.split_whitespace();
    let _version = parts.next();
    let status = parts
        .next()
        .and_then(|code| code.parse::<u16>().ok())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("invalid status line: {status_line:?}"),
            )
        })?;
    let mut headers = Vec::new();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            break;
        }
        let line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            headers.push((name.trim().to_ascii_lowercase(), value.trim().to_owned()));
        }
    }
    let content_length = headers
        .iter()
        .find(|(name, _)| name == "content-length")
        .and_then(|(_, value)| value.parse::<usize>().ok());
    let chunked = headers
        .iter()
        .any(|(name, value)| name == "transfer-encoding" && value.eq_ignore_ascii_case("chunked"));
    let body = if chunked {
        read_chunked(&mut reader)?
    } else if let Some(length) = content_length {
        let mut body = vec![0; length];
        reader.read_exact(&mut body)?;
        body
    } else {
        let mut body = Vec::new();
        reader.read_to_end(&mut body)?;
        body
    };
    Ok(Response {
        status,
        headers,
        body,
    })
}

fn read_chunked(reader: &mut impl BufRead) -> io::Result<Vec<u8>> {
    let mut body = Vec::new();
    loop {
        let mut size_line = String::new();
        if reader.read_line(&mut size_line)? == 0 {
            break;
        }
        let size = usize::from_str_radix(size_line.trim(), 16).map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("invalid chunk size {size_line:?}: {error}"),
            )
        })?;
        if size == 0 {
            let mut trailer = String::new();
            while reader.read_line(&mut trailer)? > 0 {
                if trailer.trim().is_empty() {
                    break;
                }
                trailer.clear();
            }
            break;
        }
        let mut chunk = vec![0; size];
        reader.read_exact(&mut chunk)?;
        body.extend_from_slice(&chunk);
        let mut terminator = String::new();
        reader.read_line(&mut terminator)?;
    }
    Ok(body)
}
