"""Exercise the MCP binary against a disposable SQL Server 2022 container."""

import argparse
import contextlib
import json
import os
from pathlib import Path
import secrets
import selectors
import subprocess
import tempfile
import time
import uuid


ROOT = Path(__file__).resolve().parent.parent
SQLCMD = "/opt/mssql-tools18/bin/sqlcmd"


def command(*args, env=None, timeout=30, check=True):
    return subprocess.run(args, env=env, text=True, capture_output=True, timeout=timeout, check=check)


def sql(container, password, batch, database="master"):
    return command(
        "docker", "exec", "-e", "SQLCMDPASSWORD", container, SQLCMD,
        "-S", "localhost", "-U", "sa", "-C", "-b", "-l", "2", "-d", database,
        "-Q", batch, env={**os.environ, "SQLCMDPASSWORD": password}, timeout=10,
        check=False,
    )


@contextlib.contextmanager
def fixture(image, *, network=None, reader_password=None):
    name = f"mssql-mcp-test-{uuid.uuid4().hex[:12]}"
    sa, reader, writer = (secrets.token_hex(22) + "Aa1!" for _ in range(3))
    if reader_password is not None:
        reader = reader_password
    network_args = ("--network", network, "--network-alias", "sql") if network else ()
    started = command(
        "docker", "run", "--detach", "--rm", "--pull=never", "--name", name,
        "--publish", "127.0.0.1::1433", "-e", "ACCEPT_EULA=Y", "-e", "MSSQL_PID=Developer",
        "-e", "MSSQL_SA_PASSWORD", *network_args, image,
        env={**os.environ, "MSSQL_SA_PASSWORD": sa}, check=False,
    )
    if started.returncode:
        raise RuntimeError(f"Could not launch the SQL Server fixture: {started.stderr.strip()}")
    try:
        expires = time.monotonic() + 120
        while sql(name, sa, "SELECT 1").returncode:
            running = command("docker", "inspect", "--format", "{{.State.Running}}", name, check=False)
            if running.stdout.strip() != "true" or time.monotonic() > expires:
                raise RuntimeError("SQL Server fixture did not accept SQL logins within 120 seconds")
            time.sleep(2)
        create = f"""
            CREATE DATABASE McpFixture;
            CREATE LOGIN mcp_reader WITH PASSWORD = '{reader}';
            CREATE LOGIN mcp_writer WITH PASSWORD = '{writer}';
        """
        schema = """
            CREATE USER mcp_reader FOR LOGIN mcp_reader;
            CREATE USER mcp_writer FOR LOGIN mcp_writer;
            CREATE TABLE dbo.Items (
                id int NOT NULL PRIMARY KEY, label nvarchar(max) NULL, group_id int NOT NULL
            );
            GRANT SELECT ON dbo.Items TO mcp_reader;
            GRANT SELECT, INSERT, UPDATE, DELETE ON dbo.Items TO mcp_writer;
            INSERT dbo.Items (id, label, group_id) SELECT
                value, CONCAT('item ', value), 1 FROM GENERATE_SERIES(1, 101);
            INSERT dbo.Items VALUES (1000, REPLICATE(CONVERT(nvarchar(max), N'x'), 1500), 2);
        """
        trigger = """
            CREATE TRIGGER dbo.Items_insert_message ON dbo.Items AFTER INSERT AS
            BEGIN
                SET NOCOUNT ON;
                SELECT N'trigger result must not escape into MCP' AS trigger_note;
            END;
        """
        for batch, db in [(create, "master"), (schema, "McpFixture"), (trigger, "McpFixture")]:
            if sql(name, sa, batch, db).returncode:
                raise RuntimeError(f"Could not seed isolated database {db}")
        address = command("docker", "port", name, "1433/tcp").stdout.strip()
        host, port = address.rsplit(":", 1)
        if host != "127.0.0.1":
            raise RuntimeError("Unexpected SQL Server fixture bind address")
        yield host, port, reader, writer
    finally:
        stopped = command("docker", "stop", "--time", "5", name, check=False)
        if stopped.returncode and "No such container" not in stopped.stderr:
            raise RuntimeError(f"Could not stop disposable container {name}")


class Mcp:
    def __init__(self, child):
        self.child = child
        self.next_id = 0
        self.pending = b""
        self.stderr = b""

    def send(self, method, params, request=True):
        message = {"jsonrpc": "2.0", "method": method, "params": params}
        if request:
            self.next_id += 1
            message["id"] = self.next_id
        self.child.stdin.write(json.dumps(message).encode() + b"\n")
        self.child.stdin.flush()
        if not request:
            return None
        expires = time.monotonic() + 25
        with selectors.DefaultSelector() as selector:
            selector.register(self.child.stdout, selectors.EVENT_READ)
            while b"\n" not in self.pending:
                if not selector.select(max(0, expires - time.monotonic())):
                    raise RuntimeError(f"MCP did not respond to {method} within 25 seconds")
                chunk = os.read(self.child.stdout.fileno(), 65536)
                if not chunk:
                    raise RuntimeError(f"MCP process exited before responding to {method}")
                self.pending += chunk
                if len(self.pending) > 8 * 1024 * 1024:
                    raise RuntimeError("Unexpected oversized MCP response")
        line, self.pending = self.pending.split(b"\n", 1)
        reply = json.loads(line)
        assert reply["id"] == self.next_id, reply
        return reply

    def tool(self, name, arguments, *, error=False):
        reply = self.send("tools/call", {"name": name, "arguments": arguments})
        if error:
            assert "error" in reply or reply["result"].get("isError") is True, reply
            return reply
        result = reply["result"]
        assert not result.get("isError", False), result
        return result["structuredContent"]


@contextlib.contextmanager
def server(binary, host, port, user, password, mode, *, trust):
    args = [str(binary), "--host", host, "--port", port, "--database", "McpFixture",
            "--auth", "sql", "--mode", mode, "--timeout-seconds", "10"]
    if trust:
        args.append("--trust-server-certificate")
    env = {name: value for name, value in os.environ.items() if not name.startswith("MSSQL_")}
    env.update(MSSQL_USER=user, MSSQL_PASSWORD=password)
    with (
        tempfile.TemporaryFile() as stderr,
        subprocess.Popen(args, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=stderr, env=env) as child,
    ):
        try:
            client = Mcp(child)
            initialize = client.send("initialize", {
                "protocolVersion": "2025-06-18", "capabilities": {},
                "clientInfo": {"name": "sql-server-live-test", "version": "1"},
            })
            assert "tools" in initialize["result"]["capabilities"], initialize
            client.send("notifications/initialized", {}, request=False)
            yield client
        finally:
            child.stdin.close()
            try:
                child.wait(timeout=3)
            except subprocess.TimeoutExpired:
                child.kill()
                child.wait()
                raise RuntimeError("MCP server did not exit after closing stdin") from None
            stderr.seek(0)
            client.stderr = stderr.read(65_537)
            assert len(client.stderr) <= 65_536, "unexpected oversized MCP stderr"
            assert child.returncode == 0, (child.returncode, client.stderr.decode(errors="replace"))
            startup = json.loads(client.stderr.splitlines()[0])
            assert (startup["event"], startup["tls_verify"]) == ("startup", not trust), startup


def audit_calls(client):
    records = [json.loads(line) for line in client.stderr.splitlines()]
    assert records[0]["event"] == "startup", records
    assert records[-1]["event"] == "shutdown", records
    return [record for record in records if record["event"] == "tool_call"]


def verify(binary, host, port, reader, writer):
    table = {"name": "Items"}
    query = {"table": table, "columns": ["id", "label"], "filters": {"id": 1000}}
    with server(binary, host, port, "mcp_reader", reader, "read", trust=False) as client:
        client.tool("select_rows", query, error=True)
    print("PASS: default TLS rejects the fixture's untrusted certificate")

    with server(binary, host, port, "mcp_reader", reader, "read", trust=True) as client:
        assert client.tool("list_tables", {})["rows"], "visible tables are empty"
        assert len(client.tool("describe_table", {"table": table})["rows"]) == 3
        result = client.tool("select_rows", query)
        assert result["columns"] == ["id", "label"], result
        assert result["rows"] == [["1000", "x" * 1024]], result
        assert result["truncated"] is True, result
        client.tool("delete_rows", {"table": table, "filters": {"id": 1}}, error=True)
    select_audit = next(record for record in audit_calls(client) if record["tool"] == "select_rows")
    assert select_audit["scope"] == {"schema": "dbo", "table": "Items"}, select_audit
    assert (select_audit["status"], select_audit["rows"], select_audit["truncated"]) == ("ok", 1, True), select_audit
    assert b"x" * 100 not in client.stderr
    print("PASS: SQL login, read-mode tools, catalog, typed bindings, bounded text and read audit")

    with server(binary, host, port, "mcp_reader", reader, "both", trust=True) as client:
        unprivileged = {"table": table, "values": {"id": 1001, "label": "blocked", "group_id": 2}}
        client.tool("insert_row", unprivileged, error=True)
        assert client.tool("select_rows", {**query, "filters": {"id": 1001}})["rows"] == []
    print("PASS: SQL Server grants still forbid mutations by a read-only login")

    with server(binary, host, port, "mcp_writer", writer, "write", trust=True) as client:
        client.tool("select_rows", query, error=True)
        inserted = {"table": table, "values": {"id": 1001, "label": "O'Brien", "group_id": 2}}
        assert client.tool("insert_row", inserted) == {"affected_rows": 1}
    insert_audit = next(record for record in audit_calls(client) if record["tool"] == "insert_row")
    assert insert_audit["affected_rows"] == 1 and insert_audit["status"] == "ok", insert_audit
    for sensitive in ("O'Brien", writer, "trigger result must not escape into MCP"):
        assert sensitive.encode() not in client.stderr
    with server(binary, host, port, "mcp_writer", writer, "both", trust=True) as client:
        inserted_query = {**query, "filters": {"id": 1001}}
        assert client.tool("select_rows", inserted_query)["rows"] == [["1001", "O'Brien"]]
        edit = {"table": table, "values": {"label": None}, "filters": {"id": 1001}}
        assert client.tool("update_rows", edit)["affected_rows"] == 1
        assert client.tool("select_rows", inserted_query)["rows"] == [["1001", None]]
        assert client.tool("delete_rows", {"table": table, "filters": {"id": 1001}})["affected_rows"] == 1
        assert client.tool("select_rows", inserted_query)["rows"] == []
        rollback = client.tool("delete_rows", {"table": table, "filters": {"group_id": 1}}, error=True)
        detail = rollback["result"]["structuredContent"]
        assert detail["code"] == "mutation_rolled_back", detail
        assert detail["mutation_outcome"] == "rolled_back" and detail["retryable"] is False, detail
        page = {"table": table, "columns": ["id"], "order_by": ["id"], "limit": 1,
                "offset": 100, "filters": {"group_id": 1}}
        assert client.tool("select_rows", page)["rows"] == [["101"]]
    reverted = next(record for record in audit_calls(client) if record.get("code") == "mutation_rolled_back")
    assert (reverted["tool"], reverted["status"], reverted["affected_rows"]) == ("delete_rows", "error", -1), reverted
    print("PASS: write/both, INSERT/UPDATE/DELETE, NULL, sorted paging, audited rollback and value redaction")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, default=ROOT / "target/debug/mssql-mcp")
    parser.add_argument("--image", default="mcr.microsoft.com/mssql/server:2022-latest")
    args = parser.parse_args()
    if not args.binary.is_file():
        parser.error("MCP binary not found; run cargo build --locked first")
    with fixture(args.image) as connection:
        verify(args.binary.resolve(), *connection)
