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
        procedures = """
            EXEC('CREATE PROCEDURE dbo.usp_items_count AS SELECT COUNT(*) AS total FROM dbo.Items;');
            EXEC('CREATE SCHEMA probe AUTHORIZATION dbo');
            ALTER AUTHORIZATION ON SCHEMA::probe TO mcp_writer;
            GRANT EXECUTE ON dbo.usp_items_count TO mcp_writer;
            GRANT VIEW DEFINITION ON dbo.usp_items_count TO mcp_writer;
            GRANT CREATE TABLE TO mcp_writer;
            GRANT CREATE PROCEDURE TO mcp_writer;
            GRANT ALTER ON SCHEMA::probe TO mcp_writer;
        """
        wide = ("CREATE TABLE dbo.Wide (" + ", ".join(f"c{i} int NULL" for i in range(120))
                + "); GRANT SELECT ON dbo.Wide TO mcp_writer;"
                + " CREATE TABLE probe.Doomed (id int NOT NULL PRIMARY KEY);"
                + " INSERT probe.Doomed VALUES (1), (2), (3);"
                + " GRANT SELECT, DELETE ON probe.Doomed TO mcp_writer;")
        types_and_relations = """
            CREATE TABLE dbo.Addresses (id int NOT NULL PRIMARY KEY);
            INSERT dbo.Addresses VALUES (1), (2);
            CREATE TABLE dbo.TypedReads (
                id int NOT NULL PRIMARY KEY, fixed dbo.Nickname NULL,
                bytes varbinary(8) NULL, document xml NULL,
                ship_to int NOT NULL, bill_to int NOT NULL,
                CONSTRAINT FK_TypedReads_Ship FOREIGN KEY (ship_to) REFERENCES dbo.Addresses (id),
                CONSTRAINT FK_TypedReads_Bill FOREIGN KEY (bill_to) REFERENCES dbo.Addresses (id)
            );
            INSERT dbo.TypedReads VALUES (1, N'James', 0xFE010A, N'<value/>', 1, 2);
            GRANT SELECT ON dbo.TypedReads TO mcp_reader, mcp_writer;
            GRANT VIEW DEFINITION ON TYPE::dbo.Nickname TO mcp_writer;
            GRANT SELECT ON dbo.Addresses TO mcp_reader, mcp_writer;
        """
        long_definition = (
            "CREATE PROCEDURE dbo.usp_long_definition AS /* " + "x" * 1600
            + " */ SELECT 42 AS result;"
        )
        batches = [("create", create, "master"), ("schema", schema, "McpFixture"),
                   ("trigger", trigger, "McpFixture"), ("procedures", procedures, "McpFixture"),
                   ("wide", wide, "McpFixture"),
                   ("alias type", "CREATE TYPE dbo.Nickname FROM nchar(6) NULL;", "McpFixture"),
                   ("types and relations", types_and_relations, "McpFixture"),
                   ("long definition", long_definition, "McpFixture"),
                   ("definition grant", "GRANT VIEW DEFINITION ON dbo.usp_long_definition TO mcp_writer;", "McpFixture")]
        for label, batch, db in batches:
            seeded = sql(name, sa, batch, db)
            if seeded.returncode:
                detail = (seeded.stderr or seeded.stdout).strip()[:600]
                raise RuntimeError(f"Could not seed {label} in {db}: {detail}")
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
def server(binary, host, port, user, password, mode, *, trust, yolo=False, database="McpFixture"):
    args = [str(binary), "--host", host, "--port", port, "--database", database,
            "--auth", "sql", "--mode", mode, "--timeout-seconds", "10"]
    if trust:
        args.append("--trust-server-certificate")
    if yolo:
        args.append("--yolo")
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
            client.initialize = initialize["result"]
            assert "tools" in client.initialize["capabilities"], initialize
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
        typed = {"name": "TypedReads"}
        entry = next((entry for entry in client.tool("schema_map", {"table": typed})["entries"]
                      if entry["kind"] == "column" and entry["name"] == "fixed"), None)
        assert entry and entry["data_type"] == "nchar(6)", entry
        hits = client.tool("search_objects", {"query": "fixed", "include": ["columns"]})["hits"]
        assert any(hit["kind"] == "column" and hit["data_type"] == "nchar(6)" for hit in hits), hits
        result = client.tool("select_rows", query)
        assert result["columns"] == ["id", "label"], result
        assert result["rows"] == [["1000", "x" * 1024]], result
        assert result["truncated"] is True, result
        client.tool("delete_rows", {"table": table, "filters": {"id": 1}}, error=True)
    select_audit = next(record for record in audit_calls(client) if record["tool"] == "select_rows")
    assert select_audit["scope"] == {"schema": "dbo", "table": "Items"}, select_audit
    assert (select_audit["status"], select_audit["rows"], select_audit["truncated"]) == ("ok", 1, True), select_audit
    assert "approval" not in select_audit, select_audit
    assert b"x" * 100 not in client.stderr
    print("PASS: SQL login, read-mode tools, catalog, typed bindings, bounded text and read audit")

    with server(binary, host, port, "mcp_reader", reader, "both", trust=True, yolo=True) as client:
        unprivileged = {"table": table, "values": {"id": 1001, "label": "blocked", "group_id": 2}}
        client.tool("insert_row", unprivileged, error=True)
        assert client.tool("select_rows", {**query, "filters": {"id": 1001}})["rows"] == []
    unprivileged_audit = next(record for record in audit_calls(client) if record["tool"] == "insert_row")
    assert unprivileged_audit["approval"] == "yolo", unprivileged_audit
    print("PASS: SQL Server grants still forbid mutations by a read-only login")

    with server(binary, host, port, "mcp_writer", writer, "write", trust=True) as client:
        instructions = client.initialize.get("instructions", "")
        assert "elicitation" in instructions and "--yolo" in instructions, instructions
        refused = client.tool(
            "insert_row",
            {"table": table, "values": {"id": 1002, "label": "refused", "group_id": 2}},
            error=True,
        )
        detail = refused["result"]["structuredContent"]
        assert detail["code"] == "approval_unavailable", detail
        assert detail["mutation_outcome"] == "not_attempted", detail
        assert detail["retryable"] is False, detail
        assert "elicitation" in detail["message"] and "--yolo" in detail["message"], detail
    refusal = next(record for record in audit_calls(client) if record["tool"] == "insert_row")
    assert (refusal["code"], refusal["approval"]) == ("approval_unavailable", "unavailable"), refusal
    print("PASS: a mutation without approval and without --yolo is refused before connecting")

    with server(binary, host, port, "mcp_writer", writer, "write", trust=True, yolo=True) as client:
        client.tool("select_rows", query, error=True)
        inserted = {"table": table, "values": {"id": 1001, "label": "O'Brien", "group_id": 2}}
        assert client.tool("insert_row", inserted) == {"affected_rows": 1}
    insert_audit = next(record for record in audit_calls(client) if record["tool"] == "insert_row")
    assert insert_audit["affected_rows"] == 1 and insert_audit["status"] == "ok", insert_audit
    assert insert_audit["approval"] == "yolo", insert_audit
    for sensitive in ("O'Brien", writer, "trigger result must not escape into MCP"):
        assert sensitive.encode() not in client.stderr
    with server(binary, host, port, "mcp_writer", writer, "both", trust=True, yolo=True) as client:
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
        rollback_client = client

        tree = client.tool("explore_schema", {"definition": True})
        payload, text = tree, tree["tree"]
        assert "dbo" in text and "Items" in text and "usp_items_count" in text, text
        assert "id" in text and "group_id" in text, text
        assert "CREATE PROCEDURE dbo.usp_items_count" in text, text
        assert "Wide (120 columns)" in text and "columns omitted" in text, text
        assert all(any(column in line and "Addresses" in line for line in text.splitlines())
                   for column in ("ship_to", "bill_to")), text
        long_tree = client.tool("explore_schema", {"include": ["procedures"], "definition": True})
        assert "usp_long_definition" in long_tree["tree"], long_tree
        assert (long_tree["truncated"] and "definition" in long_tree["tree"]
                and long_tree["tree"].count("…") >= 2), long_tree
        assert payload["truncated"] is True and text.rstrip().endswith("… truncated"), text[-200:]
        print("PASS: explore_schema renders schemas, columns, relations and procedure bodies, and marks a catalog cut")

        client.tool("query", {"sql": "DROP PROCEDURE dbo.usp_items_count"}, error=True)
        survived = client.tool("query", {"sql": "SELECT OBJECT_ID('dbo.usp_items_count') AS id"})
        assert survived["rows"] != [[None]], survived

    with server(binary, host, port, "mcp_writer", writer, "write", trust=True, yolo=True) as client:
        create = "CREATE PROCEDURE probe.usp_probe_sum @a int, @b int AS SELECT @a + @b AS total"
        assert client.tool("query", {"sql": create}) == {"executed": True}
        body = client.tool("query", {"sql": "SELECT OBJECT_DEFINITION(OBJECT_ID('probe.usp_probe_sum')) AS ddl"})
        assert "@a + @b" in body["rows"][0][0], body
        alter = "ALTER PROCEDURE probe.usp_probe_sum @a int, @b int AS SELECT @a * @b AS total"
        assert client.tool("query", {"sql": alter}) == {"executed": True}
        body = client.tool("query", {"sql": "SELECT OBJECT_DEFINITION(OBJECT_ID('probe.usp_probe_sum')) AS ddl"})
        assert "@a * @b" in body["rows"][0][0], body
        executed = client.tool("query", {"sql": "EXEC probe.usp_probe_sum @a = 6, @b = 7"})
        assert executed["rows"] == [["42"]], executed
        assert client.tool("query", {"sql": "DROP PROCEDURE probe.usp_probe_sum"}) == {"executed": True}
        gone = client.tool("query", {"sql": "SELECT OBJECT_ID('probe.usp_probe_sum') AS id"})
        assert gone["rows"] == [[None]], gone
    print("PASS: query creates, alters and drops a stored procedure under approval bypass")

    with server(binary, host, port, "mcp_writer", writer, "both", trust=True) as client:
        for statement in ("DROP TABLE probe.Doomed", "DELETE FROM probe.Doomed", "TRUNCATE TABLE probe.Doomed"):
            refused = client.tool("query", {"sql": statement}, error=True)["result"]["structuredContent"]
            assert refused["code"] == "approval_unavailable", (statement, refused)
            assert refused["mutation_outcome"] == "not_attempted", (statement, refused)
        assert client.tool("select_rows", {"table": {"schema": "probe", "name": "Doomed"}, "columns": ["id"], "order_by": ["id"]})["rows"] == [["1"], ["2"], ["3"]]
    print("PASS: destructive SQL without approval is refused and leaves the table untouched")

    with server(binary, host, port, "mcp_reader", reader, "read", trust=True) as client:
        for statement in ("DROP TABLE dbo.Doomed", "DELETE FROM dbo.Doomed"):
            refused = client.tool("query", {"sql": statement}, error=True)["result"]["structuredContent"]
            assert refused["code"] == "mode_unavailable", (statement, refused)
    print("PASS: read mode has no query tool at all")

    with server(binary, host, port, "mcp_writer", writer, "both", trust=True, yolo=True) as client:
        assert client.tool("query", {"sql": "DELETE FROM probe.Doomed"}) == {"executed": True}
        assert client.tool("select_rows", {"table": {"schema": "probe", "name": "Doomed"}, "columns": ["id"]})["rows"] == []
        assert client.tool("query", {"sql": "TRUNCATE TABLE probe.Doomed"}) == {"executed": True}
        assert client.tool("query", {"sql": "DROP TABLE probe.Doomed"}) == {"executed": True}
        assert all(row[1] != "Doomed" for row in client.tool("list_tables", {})["rows"])
    print("PASS: an approved bypass really deletes every row, truncates and drops")
    reverted = next(record for record in audit_calls(rollback_client) if record.get("code") == "mutation_rolled_back")
    assert (reverted["tool"], reverted["status"], reverted["affected_rows"]) == ("delete_rows", "error", -1), reverted
    assert reverted["approval"] == "yolo", reverted
    for audit in audit_calls(client):
        if audit.get("tool") == "query":
            assert "sql" not in audit and "DROP" not in str(audit).upper(), audit

    with server(binary, host, port, "mcp_writer", writer, "write", trust=True) as client:
        refused_query = client.tool("query", {"sql": "SELECT 1 AS n"}, error=True)
        detail = refused_query["result"]["structuredContent"]
        assert detail["code"] == "approval_unavailable" and detail["mutation_outcome"] == "not_attempted", detail
    query_audit = next(record for record in audit_calls(client) if record["tool"] == "query")
    assert "sql" not in query_audit and "SELECT 1" not in str(query_audit), query_audit
    print("PASS: query is refused without approval and its text stays out of the audit record")
    print("PASS: write/both, INSERT/UPDATE/DELETE, NULL, sorted paging, audited rollback and value redaction")

    with server(binary, host, port, "mcp_writer", writer, "both", trust=True, yolo=True) as client:
        verify_read_regressions(client, table)
        procedure_as_table = client.tool("schema_map", {"table": {"name": "usp_items_count"}})
        assert procedure_as_table["total"] == 0, procedure_as_table
        named = client.tool("schema_map", {"table": table})
        assert any(entry["kind"] == "object" and entry["name"] == "Items"
                   for entry in named["entries"]), named
        typed = {"name": "TypedReads"}
        columns = ["id", "bytes", "document"]
        result = client.tool("select_rows", {"table": typed, "columns": columns, "limit": 1})
        assert result == {"columns": columns, "rows": [["1", "FE010A", "<value/>"]],
                          "truncated": False}, result
        result = client.tool("describe_table", {"table": typed})
        description = dict(zip(result["columns"], next(row for row in result["rows"] if row[0] == "fixed")))
        assert (description["type"], description["base_type"], description["max_length"]) == (
            "Nickname", "nchar", "6"), description
        wide = client.tool("sample_rows", {"table": {"name": "Wide"}})
        assert len(wide["columns"]) == 64 and wide["columns_omitted"] and wide["truncated"], wide
        inserted = client.tool("insert_row", {"table": table, "values": {"id": 1004, "label": 123,
                                                                         "group_id": 2}})
        assert inserted == {"affected_rows": 1}, inserted
        coercion = client.tool("select_rows", {"table": table, "columns": ["label"],
                                              "filters": {"id": 1004}})
        assert coercion["rows"] == [["123"]], coercion
        rejected = client.tool("insert_row", {"table": table, "values": {"id": 1, "group_id": 1}},
                               error=True)["result"]["structuredContent"]
        assert rejected["code"] == "query_failed" and rejected["mutation_outcome"] == "rolled_back", rejected
        assert rejected["sql_error"]["number"] in (2601, 2627), rejected
        original = client.tool("select_rows", {"table": table, "columns": ["id", "label"],
                                               "filters": {"id": 1}})
        assert original["rows"] == [["1", "item 1"]], original
    print("PASS: typed projection, alias metadata, SQL diagnostics, row truncation and confirmed rollback")


def verify_read_regressions(client, table):
    tools = {tool["name"]: tool for tool in client.send("tools/list", {})["result"]["tools"]}
    for name in ("select_rows", "query"):
        limit = tools[name]["inputSchema"]["properties"]["limit"]
        assert (limit["minimum"], limit["maximum"]) == (1, 100), limit
    key = "id" if table["name"] == "Items" else "OrderID"
    for order in ({}, {"order_by": [key]}):
        rows = client.tool("select_rows", {"table": table, "columns": [key], "limit": 1, **order})
        assert len(rows["rows"]) == 1 and rows["truncated"] is True, rows
    absent = client.tool("describe_table", {"table": {**table, "name": "McpRegressionTableAbsent"}},
                         error=True)["result"]["structuredContent"]
    assert "not found" in absent["message"].lower(), absent
    for arguments, field in [({"columns": []}, "columns"), ({"columns": [key], "limit": 0}, "limit"),
                             ({"columns": [key], "offset": 1}, "offset")]:
        invalid = client.tool("select_rows", {"table": table, **arguments},
                              error=True)["result"]["structuredContent"]
        assert "invalid" in invalid["code"] and field in invalid["message"], invalid
    invalid = client.tool("select_rows", {"table": table}, error=True)["result"]["structuredContent"]
    assert "columns" in invalid["message"], invalid
    rejected = client.tool("query", {"sql": "SELECT McpRegressionColumnAbsent"},
                           error=True)["result"]["structuredContent"]
    diag = rejected["sql_error"]
    assert (rejected["code"], diag["number"], diag["severity"]) == ("query_failed", 207, 16), rejected
    assert "invalid column" in diag["message"].lower(), diag
    multiple = client.tool("query", {"sql": "SELECT 1 AS n; SELECT 2 AS n;"},
                           error=True)["result"]["structuredContent"]
    assert multiple["code"] == "unrecognized_result" and "mutation_outcome" not in multiple, multiple


def verify_wwi(binary):
    address = command("docker", "port", "wwi", "1433/tcp").stdout.strip()
    host, port = address.rsplit(":", 1)
    if host != "127.0.0.1":
        raise RuntimeError("WideWorldImporters must be exposed only on loopback")
    user, password = os.environ.get("MSSQL_USER", "sa"), os.environ.get("MSSQL_PASSWORD")
    if not password:
        raise RuntimeError("Set MSSQL_PASSWORD for the existing WideWorldImporters fixture")
    with server(binary, host, port, user, password, "both", trust=True, yolo=True,
                database="WideWorldImporters") as client:
        orders = {"schema": "Sales", "name": "Orders"}
        verify_read_regressions(client, orders)
        stock = {"schema": "Warehouse", "name": "StockItems"}
        metadata = client.tool("describe_table", {"table": stock})
        columns = [row[0] for row in metadata["rows"]]
        assert len(columns) > 22 and "Photo" in columns, metadata
        rows = client.tool("select_rows", {"table": stock, "columns": columns,
                                          "order_by": ["StockItemID"], "limit": 1})
        assert rows["columns"] == columns and len(rows["rows"][0]) == len(columns), rows.keys()
        tree = client.tool("explore_schema", {"definition": True})
        assert "columns omitted" in tree["tree"] and tree["truncated"], tree["tree"][-500:]
        order = client.tool("select_rows", {"table": orders, "columns": ["OrderID", "CustomerID"],
                                            "order_by": ["OrderID"], "limit": 1})["rows"][0]
        filters = {"OrderID": int(order[0])}
        rejected = client.tool("update_rows", {"table": orders, "filters": filters,
                                                "values": {"CustomerID": None}},
                               error=True)["result"]["structuredContent"]
        assert rejected["mutation_outcome"] == "rolled_back", rejected
        assert rejected["sql_error"]["number"] == 515, rejected
        unchanged = client.tool("select_rows", {"table": orders, "columns": ["OrderID", "CustomerID"],
                                                "filters": filters})
        assert unchanged["rows"] == [order], unchanged
        verify_wwi_catalog(client, orders, stock)
    verify_wwi_export(binary, host, port, user, password)
    print("PASS: WideWorldImporters live regressions against the rebuilt binary")


def verify_wwi_catalog(client, orders, stock):
    entries, ids, offset = [], set(), 0
    while True:
        page = client.tool("schema_map", {"table": orders, "offset": offset, "limit": 7})
        assert page["offset"] == offset and len(page["entries"]) <= 7, page
        for entry in page["entries"]:
            assert entry["id"] not in ids, entry
            ids.add(entry["id"])
            entries.append(entry)
        if page["next_offset"] is None:
            assert len(ids) == page["total"], page
            break
        assert page["next_offset"] == len(ids) and page["next_offset"] > offset, page
        offset = page["next_offset"]
    kinds = {entry["kind"] for entry in entries}
    assert {"schema", "object", "column", "key", "key_column", "foreign_key",
            "foreign_key_column", "index", "index_column"} <= kinds, kinds
    assert any(entry["kind"] == "foreign_key_column" and entry["parent_column"] == "CustomerID"
               and entry["referenced_column"] == "CustomerID" for entry in entries)
    assert any(entry["kind"] == "column" and entry["name"] == "OrderID"
               and entry["data_type"] == "int" and entry["nullable"] is False for entry in entries)
    assert any(entry["kind"] == "object" and entry["name"] == "Orders" and entry["row_count"] > 1
               for entry in entries)

    dependencies = client.tool("object_dependencies", {"object": orders})
    directions = {edge["direction"] for edge in dependencies["edges"]}
    assert {"depends_on", "depended_on_by"} <= directions, dependencies
    hits = client.tool("search_objects", {"query": "StockItem", "schema": "Warehouse", "limit": 3})
    assert len(hits["hits"]) == 3 and hits["next_offset"] == 3, hits
    literal = client.tool("search_objects", {"query": "StockItem%", "schema": "Warehouse"})
    assert literal["hits"] == [], literal
    stats = client.tool("table_stats", {"table": orders})
    assert stats["row_count"] > 1 and stats["row_count_approximate"], stats
    assert stats["reserved_bytes"] >= stats["used_bytes"] > 0 and stats["indexes"], stats
    rows = client.tool("sample_rows", {"table": stock, "limit": 2})
    assert len(rows["rows"]) == 2 and "StockItemID" in rows["columns"], rows
    assert "Photo" in rows["columns"] and rows["truncated"], rows
    print("PASS: catalog paging, stable IDs, foreign keys, literal search, stats and sampling")


def verify_wwi_export(binary, host, port, user, password):
    env = {name: value for name, value in os.environ.items() if not name.startswith("MSSQL_")}
    env.update(MSSQL_USER=user, MSSQL_PASSWORD=password, MSSQL_MCP_TRANSPORT="http")
    args = [str(binary), "map", "--host", host, "--port", port, "--database", "WideWorldImporters",
            "--trust-server-certificate", "--schema", "Sales"]
    with tempfile.TemporaryDirectory(prefix="mssql-mcp-map-") as directory:
        html = Path(directory) / "schema.html"
        data = Path(directory) / "schema.json"
        for kind, path in (("html", html), ("json", data)):
            result = command(*args, "--format", kind, "--out", str(path), env=env, timeout=90,
                             check=False)
            assert result.returncode == 0, (result.returncode, result.stderr)
        svg = html.read_text()
        assert "<svg" in svg and 'class="edge"' in svg and "Orders" in svg
        assert "External table" in svg and "<script" not in svg and "<link" not in svg
        export = json.loads(data.read_text())
        assert export["schema"] == "Sales" and len(export["entries"]) > 100, len(export["entries"])
        refused = command(*args, "--out", str(html), env=env, timeout=90, check=False)
        assert refused.returncode != 0 and html.read_text() == svg, refused.returncode
    print("PASS: standalone HTML and JSON exports use the same SQL configuration and never overwrite")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, default=ROOT / "target/debug/mssql-mcp")
    parser.add_argument("--image", default="mcr.microsoft.com/mssql/server:2022-latest")
    parser.add_argument("--wwi", action="store_true", help="Probe the preloaded loopback wwi container")
    args = parser.parse_args()
    if not args.binary.is_file():
        parser.error("MCP binary not found; run cargo build --locked first")
    if args.wwi:
        verify_wwi(args.binary.resolve())
    else:
        with fixture(args.image) as connection:
            verify(args.binary.resolve(), *connection)
