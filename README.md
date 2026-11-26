# mssql-mcp

Native stdio MCP server for Microsoft SQL Server. The Rust binary connects over TDS without ODBC or `sqlcmd`. It starts in read mode and connects to SQL Server only when a tool needs it.

## Install locally

```sh
cargo install --path . --locked
mssql-mcp --help
```

The crate is not yet published on crates.io.

## Connect an MCP client

In your MCP client's configuration, add this entry and replace the hostname, database and username. Supply `MSSQL_PASSWORD` to the child process through your client's secret store or inherited environment. If the client does not inherit your `PATH`, use the full path to the executable.

```json
{
  "mcpServers": {
    "sql-server": {
      "command": "mssql-mcp",
      "env": {
        "MSSQL_HOST": "sql.example.com",
        "MSSQL_DATABASE": "Inventory",
        "MSSQL_AUTH": "sql",
        "MSSQL_USER": "mcp_reader",
        "MSSQL_MODE": "read"
      }
    }
  }
}
```

### Authentication

| `MSSQL_AUTH` | Identity | Additional environment variables |
|---|---|---|
| `sql` (default) | SQL Server login | `MSSQL_USER`, `MSSQL_PASSWORD` |
| `windows` | Current Windows process identity via SSPI/NTLM | None. Windows only. Does not support Kerberos-only servers. |
| `ntlm` | Explicit Windows domain credentials | `MSSQL_DOMAIN`, `MSSQL_USER`, `MSSQL_PASSWORD`. Use an unqualified username and set the domain separately. Available on Windows and Unix. |

Windows/NTLM require SQL Server to recognize the Windows principal and accept NTLM. `windows` does not use `MSSQL_USER` or `MSSQL_PASSWORD`. No authentication mode falls back to another identity.

`MSSQL_PORT` defaults to `1433`; use the instance's configured TCP port for a named instance. `MSSQL_TIMEOUT_SECONDS` defaults to `15` (range: 1–300) and covers the SQL Server connection and operation. The client requires TLS and validates certificates against the system trust store. Use the DNS name in the server certificate. `--trust-server-certificate` (or `MSSQL_TRUST_SERVER_CERTIFICATE=true`) keeps TLS encryption but skips identity verification; use it only for disposable local development.

Run `mssql-mcp --check-config` in the same environment to validate settings without connecting. Do not add that flag to the MCP client's server entry.

## Tools and permissions

| `MSSQL_MODE` | Tools |
|---|---|
| `read` (default) | `list_tables`, `describe_table`, `select_rows` |
| `write` | `insert_row`, `update_rows`, `delete_rows` |
| `both` | All six tools |

Tools accept structured arguments, not arbitrary T-SQL. For example, pass this to `select_rows`:

```json
{
  "table": {"schema": "dbo", "name": "Products"},
  "columns": ["ProductId", "Name"],
  "filters": {"Active": true},
  "order_by": ["ProductId"],
  "limit": 25
}
```

- Names are scoped to the configured database; `schema` defaults to `dbo`. Filters combine equality checks with AND. A null filter matches SQL `NULL`.
- Selects return 25 rows by default and at most 100. Provide `order_by` with a unique key for stable offset pagination. Results contain `columns`, ordered `rows`, and a `truncated` flag. Values are SQL Server text conversions or JSON `null`; types SQL Server cannot convert to text are not supported. Text cells are capped at 1,024 UTF-16 code units. Catalog pages contain at most 100 rows.
- Updates and deletes require at least one filter. If the statement affects more than 100 target rows, the server rolls back the transaction. This cap does not count additional effects of triggers or cascading foreign keys. If a write fails after it was sent or times out, verify the database state before retrying. A lost response is not proof of rollback.
- Give each login only the SQL Server permissions it needs on the intended tables or views. Use a dedicated read-only principal for `read` and do not grant administrative roles. The MCP mode gates tools; it does not replace database authorization.
- Each tool call opens a fresh connection; the server allows at most four active database operations. A busy response does not run the tool.

## Audit and error contract

The server writes JSON Lines to stderr and keeps stdout for MCP protocol messages only. Redirect or capture stderr to retain the trail; the server writes no audit file and sends no audit messages over MCP.

```json
{"ts":1795707647283,"event":"startup","mode":"read","auth":"sql","tls_verify":true}
{"ts":1795707647289,"event":"tool_call","tool":"select_rows","mode":"read","status":"error","code":"connection_failed","duration_ms":1,"scope":{"schema":"dbo","table":"Items"}}
{"ts":1795707647289,"event":"shutdown","status":"ok"}
```

| Field | Meaning |
|---|---|
| `ts` | Unix epoch in milliseconds |
| `event` | `startup`, `tool_call`, or `shutdown` |
| `tool` | Known tool name; absent for an unknown tool |
| `mode` | Effective `MSSQL_MODE` |
| `status` | `ok` or `error` |
| `code` | Error code when `status` is `error` |
| `duration_ms` | Wall time from request to result |
| `scope` | Validated target: `{"schema":"dbo","table":"Items"}`, `{"schema":"dbo"}`, or `{"catalog":true}` |
| `rows`, `affected_rows`, `truncated` | Bounded result counters, never result data; `affected_rows: -1` marks a confirmed rollback |
| `auth`, `tls_verify` | Startup only: authentication kind and whether TLS certificate verification is enabled (on by default) |

The server records one `tool_call` per completed call handled by the server, including tool-level rejections and unknown tool names. Invalid MCP messages rejected by the transport are not audited. `scope` appears only after the arguments pass validation. Records never contain SQL text, filter or column values, credentials, host names, connection strings, result rows, raw driver messages, or tool names other than the six known ones. Validated schema and table names do appear, so treat stderr as sensitive when object names are.

The stderr trail is operational evidence, not a tamper-proof or compliance audit log: the server does not sign, rotate, or persist it, and anything able to write to the process streams can alter it. Capture it with your MCP client or supervisor and apply your own retention and access controls.

### Tool errors

Failures include `isError: true`, human-readable `content[0].text`, and a `structuredContent` object with a stable code. A write connection failure does not say the outcome is unknown: no statement was sent.

```json
{"code":"connection_failed","message":"database operation failed","action":"check that the server is reachable and that TLS and the credentials are accepted","retryable":false}
```

| Field | Meaning |
|---|---|
| `code` | Stable machine-readable error code |
| `message` | Human-readable description. On rollback, `content[0].text` retains the original JSON payload |
| `action` | What to check or do next |
| `retryable` | `true` only when an automatic retry is safe (a busy server that started no work) |
| `mutation_outcome` | Write tools only: `not_attempted`, `rolled_back`, or `unknown` when the statement may have taken effect |

| `code` | Cause | `mutation_outcome` |
|---|---|---|
| `mode_unavailable` | Tool disabled by `MSSQL_MODE` | `not_attempted` for writes |
| `invalid_arguments` | Arguments do not match the tool input schema | `not_attempted` for writes |
| `invalid_input` | A validated field was rejected (identifier, limit, filter, value) | `not_attempted` for writes |
| `database_busy` | Four operations were already active; nothing ran | `not_attempted` for writes |
| `connection_failed` | TCP, TLS, or login failed before any statement | `not_attempted` for writes |
| `authentication_unavailable` | Authentication mode missing from this build or platform | `not_attempted` for writes |
| `query_failed` | SQL Server failed the statement | `unknown` for writes |
| `timeout` | The operation deadline elapsed | `unknown` for writes |
| `unrecognized_result` | The driver returned an unexpected result shape | `unknown` for writes |
| `mutation_rolled_back` | More than 100 rows would change, so the transaction rolled back | `rolled_back` |

`mutation_outcome: unknown` means the request may have reached SQL Server: read the current database state before retrying a write. A rollback result keeps its `affected_rows: -1` and `error` fields alongside the fields above. An unknown tool name stays a JSON-RPC `-32601` error with `data.code: "unknown_tool"`.

## Verify

```sh
cargo fmt --all --check
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
cargo package --locked
```

To exercise SQL authentication, verified TLS rejection, metadata reads and mutations against SQL Server 2022 locally, run:

```sh
cargo build --locked
docker pull mcr.microsoft.com/mssql/server:2022-latest
python3 tests/sql_server_live.py
```

The test accepts the SQL Server Developer container EULA, publishes a random port on loopback, and removes its disposable container. It does not use an existing database. To validate `windows` and `ntlm` end to end, use a Windows/AD-enabled SQL Server that accepts NTLM; the container test does not cover domain authentication.
