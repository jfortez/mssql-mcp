# mssql-mcp

Native MCP server for Microsoft SQL Server. Read-only by default, with authenticated Streamable HTTP for sharing one process across clients.

## 1. Install

| Source | Command |
| --- | --- |
| This checkout (Rust 1.88+) | `cargo install --path . --locked` |
| crates.io, after publication | `cargo install mssql-mcp --locked` |
| Docker | skip Cargo; see [Docker](#docker) |

## 2. Configure

Every setting is a command-line flag and an environment variable. Use the environment for secrets.

| Variable | Flag | Values | Default |
| --- | --- | --- | --- |
| `MSSQL_HOST` | `--host` | SQL Server hostname or IP | required |
| `MSSQL_DATABASE` | `--database` | database name | required |
| `MSSQL_AUTH` | `--auth` | `sql`, `windows`, `ntlm` | detected from `MSSQL_USER` |
| `MSSQL_USER` | — | login, or `DOMAIN\user` for `ntlm`; not used by `windows` | required for `sql` and `ntlm` |
| `MSSQL_PASSWORD` | — | login password; environment only | required for `sql` and `ntlm` |
| `MSSQL_PORT` | `--port` | 1–65535; the instance's TCP port | `1433` |
| `MSSQL_MODE` | `--mode` | `read`, `write`, `both` | `read` |
| `MSSQL_MCP_YOLO` | `--yolo` | `true` skips the mutation approval prompt (dangerous) | `false` |
| `MSSQL_MCP_TRANSPORT` | `--transport` | `stdio`, `http` | `stdio` |
| `MSSQL_MCP_HTTP_LISTEN` | `--http-listen` | `IP:port` | `127.0.0.1:8765` |
| `MSSQL_MCP_TOKEN` | — | 32–128 of `A–Z a–z 0–9 - _`; HTTP only | required for `http` |
| `MSSQL_TIMEOUT_SECONDS` | `--timeout-seconds` | 1–300, per operation | `15` |
| `MSSQL_TRUST_SERVER_CERTIFICATE` | `--trust-server-certificate` | `true` skips certificate checks (development only) | `false` |

Authentication modes: `sql` uses the unqualified `MSSQL_USER` login and `MSSQL_PASSWORD`; `ntlm` uses a `DOMAIN\user` `MSSQL_USER` and `MSSQL_PASSWORD`; `windows` uses the current Windows process identity, so it is unavailable on Linux and in the Docker image and does not use the two SQL credentials. Without `--auth`, a backslash in `MSSQL_USER` selects `ntlm` and anything else selects `sql`; an explicit `--auth` always wins, and a combination that contradicts it, such as `--auth sql` with a qualified user, is rejected instead of guessed. No mode falls back to another identity.

TLS is required and certificates are validated against the system store: use the hostname in the server certificate. `MSSQL_MODE=read` exposes the read tools in the table below; `write` exposes `insert_row`, `update_rows`, `delete_rows`, `query` and `explore_schema`; `both` exposes all 15. Updates and deletes need at least one filter and roll back when more than 100 rows would change. Nothing reaches SQL Server until the human has answered, through one of two channels: a client that advertises MCP elicitation is asked directly before the connection opens, and any other client gets a non-error `approval_required` result with a one-shot token, relays that question to the human and repeats the call with the answer. `--yolo` (or `MSSQL_MCP_YOLO=true`) skips that question and the audit trail records the bypass. Give the login only the SQL Server permissions it needs.

Validate without connecting: `mssql-mcp --check-config`. All flags: `mssql-mcp --help`.

## 3. Start the server

### Native, shared over HTTP

Start one daemon and point every client at it:

```sh
export MSSQL_HOST=sql.example.com MSSQL_DATABASE=Inventory MSSQL_USER=mcp_reader
export MSSQL_PASSWORD="$(cat /run/secrets/sql-password)"
export MSSQL_MCP_TOKEN="$(openssl rand -hex 32)"
mssql-mcp --transport http
```

The endpoint is `http://127.0.0.1:8765/mcp`. Every request needs `Authorization: Bearer $MSSQL_MCP_TOKEN`, including from loopback. The server rejects foreign `Host` and `Origin` headers, so keep it on loopback and do not add CORS or a reverse proxy.

A second process on the same address fails at bind, so clients keep addressing exactly one endpoint. All clients share the daemon's SQL identity, mode, `read`/`write` surface and its four concurrent database operations; each tool call still opens its own SQL Server connection. Need different identities or modes? Run another daemon on another port. The HTTP transport cannot show an elicitation prompt, so a mutation or `query` returns `approval_required` instead of running: relay that question to the human and repeat the same request with `approval_token` and `approval_decision`. The token is returned only to the client that asked, it stays redeemable in the daemon for 120 seconds, and `--yolo` remains the audited bypass.

### Native, one client over stdio

Default transport. Each client entry starts its own process, so stdio is not shared:

```json
{
  "mcpServers": {
    "sql-server": {
      "command": "mssql-mcp",
      "env": {
        "MSSQL_HOST": "sql.example.com",
        "MSSQL_DATABASE": "Inventory",
        "MSSQL_USER": "mcp_reader",
        "MSSQL_MODE": "read"
      }
    }
  }
}
```

### Docker

`compose.yaml` runs one `mssql-mcp` service built from `Dockerfile`: Rust 1.88 builder, `debian:bookworm-slim` runtime, numeric user `10001:10001`, read-only root filesystem, `cap_drop: [ALL]`, `no-new-privileges`, and one loopback-published port. Compose reads these variables from your shell or from `./.env`:

```sh
export MSSQL_HOST=sql.example.com MSSQL_DATABASE=Inventory MSSQL_USER=mcp_reader
export MSSQL_PASSWORD="$(cat /run/secrets/sql-password)"
export MSSQL_MCP_TOKEN="$(openssl rand -hex 32)"

docker compose config --quiet     # validate interpolation, build nothing
docker compose up -d --build      # build and start
docker compose logs -f mssql-mcp  # follow JSON audit lines on stderr
docker compose down               # stop and remove
```

Inside the container the service listens on `0.0.0.0:8765`; only `127.0.0.1:8765` is published to the host, so clients use the same URL and token as the native daemon. Only the variables in the table above reach the container, and `MSSQL_HOST` is resolved inside it: `localhost` there means the container, so use a hostname or IP reachable from the Compose network. Docker Compose may recreate the container on a later `up`, but the same project never runs two.

Rebuild after pulling changes with `docker compose up -d --build`. To rebuild without cache: `docker compose build --no-cache`.

Secrets: `MSSQL_PASSWORD` and `MSSQL_MCP_TOKEN` are runtime environment variables, visible to anyone with Docker access (`docker inspect`, `docker compose config`, the host process list). They are not in the image, the build context (`.dockerignore` excludes `.env*`) or Git. Keep `./.env` untracked. The bearer token is a static access gate, not OAuth, and does not encrypt traffic: this image provides no TLS termination, so do not publish the port beyond loopback.

## 4. Connect clients

Launch every client with the **same `MSSQL_MCP_TOKEN`** and replace any existing stdio entry for this server.

### Claude Code

Add to the project's `.mcp.json`, then approve the server when Claude Code prompts:

```json
{
  "mcpServers": {
    "sql-server": {
      "type": "http",
      "url": "http://127.0.0.1:8765/mcp",
      "headers": { "Authorization": "Bearer ${MSSQL_MCP_TOKEN}" }
    }
  }
}
```

Check it with `claude mcp get sql-server`.

### Codex

Add to `~/.codex/config.toml`, then run `/mcp` in Codex:

```toml
[mcp_servers.sql-server]
url = "http://127.0.0.1:8765/mcp"
bearer_token_env_var = "MSSQL_MCP_TOKEN"
```

### Pi

```sh
pi mcp add sql-server --url http://127.0.0.1:8765/mcp --bearer-token-env-var MSSQL_MCP_TOKEN
```

Pi reads the token from its own environment and expands `${VAR}` in `mcp.json`, so keep the secret there rather than in the entry.

### OpenCode

Under `mcp.servers` in `opencode.jsonc` (V2):

```jsonc
{
  "$schema": "https://opencode.ai/config.json",
  "mcp": {
    "servers": {
      "sql-server": {
        "type": "remote",
        "url": "http://127.0.0.1:8765/mcp",
        "oauth": false,
        "headers": { "Authorization": "Bearer {env:MSSQL_MCP_TOKEN}" }
      }
    }
  }
}
```

Older releases put the entry directly under `mcp`. `oauth: false` is required because the endpoint uses a static bearer header.

### Other ACP hosts

The Agent Client Protocol does not carry an MCP transport by itself. An ACP host can reach this daemon only when its agent or adapter supports remote Streamable HTTP MCP servers; otherwise it spawns its own stdio process per client.

## Tools

| Tool | Arguments | Returns |
| --- | --- | --- |
| `list_tables` | `schema?`, `offset?` | `schema`, `name`, `type`, `object_id` per table or view, 100 per page |
| `describe_table` | `table`, `offset?` | Column name, declared and base types, maximum length, precision, scale, nullability and ordinal |
| `select_rows` | `table`, `columns`, `filters?`, `order_by?`, `limit?`, `offset?` | `columns`, `rows`, `truncated` |
| `insert_row` | `table`, `values`, `approval_token?`, `approval_decision?` | `affected_rows`, or `approval_required` |
| `update_rows` | `table`, `values`, `filters`, `approval_token?`, `approval_decision?` | `affected_rows`, or `approval_required` |
| `delete_rows` | `table`, `filters`, `approval_token?`, `approval_decision?` | `affected_rows`, or `approval_required` |
| `query` | `sql`, `limit?`, `approval_token?`, `approval_decision?` | `columns`, `rows`, `truncated` for one result set, `executed`, or `approval_required` |
| `explore_schema` | `schema?`, `include?`, `definition?` | `tree`, `truncated` |
| `schema_map` | `schema?`, `include?`, `table?`, `offset?`, `limit?` | Paged `entries`: schemas, objects, columns, keys, indexes and routine parameters |
| `object_dependencies` | `object`, `offset?`, `limit?` | Paged `edges` from SQL expressions and foreign keys, incoming and outgoing |
| `routine_definition` | `object`, `offset?`, `limit?` | One `sys.sql_modules` definition, paged by character, with its total length |
| `dependency_columns` | `object`, `column_hint?`, `offset?`, `limit?` | Paged referenced objects and their columns, optionally filtered by a literal column-name fragment |
| `search_objects` | `query`, `schema?`, `include?`, `offset?`, `limit?` | Paged `hits` by literal name fragment; `include` accepts `objects` or `columns` |
| `table_stats` | `table`, `offset?`, `limit?` | Approximate row count and base storage, plus index sizes paged by `next_index_offset` |
| `sample_rows` | `table`, `limit?` | Up to 25 rows, default 5, using the first 64 columns in catalog order |

`table` and `object` are `{"schema": "dbo", "name": "Products"}`; the schema defaults to `dbo`. `describe_table` reports `object_not_found` for missing or invisible objects and gives character lengths for character types (`-1` means `max`). Filters are equality checks combined with `AND`, with `null` matching SQL `NULL`.

`select_rows` accepts 1–64 column names and returns 25 rows by default (maximum 100). Values are text, including uppercase hex for binary and serialized XML; each cell is capped at 1,024 UTF-16 units. `truncated` means more rows exist or a returned value was clipped. Use a unique `order_by` for stable pagination. `sample_rows` is unordered; a table wider than 64 columns sets `columns_omitted` and `truncated`.

`insert_row` binds numeric JSON values as numbers. SQL Server can implicitly convert them into text columns; pass a JSON string when the exact text matters. A mutation that hits the 100-row rollback guard returns `affected_rows: -1` with `mutation_outcome: rolled_back`.

`query` runs one arbitrary T-SQL batch under the configured principal, exactly as sent. It can read or change any data or server state that principal can reach, including other databases, and it has no affected-row cap. Every batch requires approval unless `--yolo`, and the question carries the exact SQL so the human consents to the real bytes. At most one result set is returned, bounded to 100 rows by default (`limit` can lower it), 16 columns and 1,024 UTF-16 units per cell; a `truncated` flag reports a cut. A batch with no result set returns `executed: true`; more than one result set returns `unrecognized_result` without a mutation outcome. If SQL Server rejects a statement in a raw batch, check the database before retrying: earlier statements may have committed. SQL Server permissions remain the real boundary: the approval question is a guardrail, not a sandbox.

## Approving a mutation without elicitation

Elicitation is the first channel: when the client advertises the capability, the server asks it directly and the tool call finishes with the mutation result or with `approval_denied`. Every other client, including a transport that carries no client capabilities, uses the relay instead of being refused.

1. The mutation call returns a structured, non-error result with `code: approval_required`. Nothing reached SQL Server. It carries `approval_token` (one-shot, unguessable), `approval_token_id` (its short label), `approval_message` (the exact question: the target and column names for a table mutation, or the exact batch for `query`), `row_cap` (`100`, or `null` for `query`), `expires_in_seconds` (`120`) and `mutation_outcome: not_attempted`. `content[0].text` is the same question plus how to answer, so a text-only client can relay it too.
2. Show `approval_message` to the human, then repeat the same call with the same arguments plus `approval_token` and `approval_decision`. `grant` runs the statement once; `deny` refuses it. Both answers spend the token.
3. The retry executes only when the token is known, unexpired, unused, and the statement the server rebuilds from the retry's own arguments is byte-identical to the one the token was issued for. A token for one batch can never run a different batch, a different tool or the same call twice.

Refusals are named: `approval_token_unknown`, `approval_token_expired`, `approval_token_reused`, `approval_token_mismatch` (different tool, target, values or batch text), and `approval_token_invalid` (`approval_token` and `approval_decision` must be supplied together). None of them reaches SQL Server, and none of them consumes a pending question except expiry and reuse. A question that is never answered stops being redeemable after its 120 seconds, so an abandoned question cannot be approved later in the session.

`explore_schema` is read-only and available in every mode. It returns one indented `tree` string: schemas; under each, tables with column count and primary key, columns with type and nullability, outgoing and incoming foreign-key relations, views, procedures with parameter signatures and definition size, and functions with return type and parameter signatures. `schema` narrows the tree, `include` selects `tables`, `views`, `procedures` and `functions` (all four by default), and `definition: true` inlines procedure and function bodies from `sys.sql_modules`. Foreign-key edges identify the constraint and pair the child and referenced columns in constraint order. Catalog queries and the tree itself are bounded: per-table column cuts and per-routine definition cuts say how much was omitted. Any cut sets `truncated` and ends the tree with a visible `… truncated` line.

`schema_map` returns separate records for each object and its children with stable IDs. Follow `next_offset` until it is `null`; it is independent of `truncated`, which flags clipped default, computed, or index-filter expressions. Foreign keys include both named endpoints and separate ordered column pairs. The `include` filter accepts `tables`, `views`, `procedures`, and `functions`. Table counts are approximate partition metadata, not `COUNT(*)` scans. `object_dependencies` also reports SQL-expression references that SQL Server cannot resolve locally.

`routine_definition` reads one procedure, function, view or trigger from `sys.sql_modules` and returns its full definition paged by character. `offset` and `limit` count UTF-16 units, the same unit as the per-cell cap used elsewhere; `limit` defaults to 8,000 and is capped at 20,000. `definition_length` is the total body length, `next_offset` is the next character to read, and `truncated` means more of the body remains. Follow `next_offset` until it is `null` and concatenate the pages to reproduce the stored definition exactly. A NULL definition is reported as `definition_unavailable`, not as an empty body. These reads depend on `VIEW DEFINITION`: without it, `sys.sql_modules` returns no definition and `sys.sql_expression_dependencies` hides the rows, so the same object can look empty to that login.

`dependency_columns` resolves the objects one procedure, function, view or trigger references through `sys.sql_expression_dependencies` and lists their columns. `column_hint` is a literal name fragment matched with `CHARINDEX`; an empty or omitted hint returns every column. Every referenced object appears as an `object` entry, even when the hint matches none of its columns: its `matching_column_count` is `0` and it contributes no `column` entries. An object that does not appear in the feed at all was not referenced. Unresolved or cross-database references still appear as object entries with a null `object_id`, but have no readable columns. Follow `next_offset` until it is `null` to see every referenced object and column.

## Export a schema map without MCP

```sh
mssql-mcp map --format html --out /tmp/sales-schema.html --schema Sales
mssql-mcp map --format json --out /tmp/sales-schema.json --schema Sales
```

Use the same SQL connection flags or `MSSQL_*` connection settings as the server. The command connects directly to SQL Server, reads every catalog page, writes one new file, and exits. It does not start an MCP transport, need a bearer token, or modify the database. Omit `--schema` to export all visible schemas. `mssql-mcp map --help` lists the flags.

Open the HTML file offline to see tables grouped by schema, with arrows from child tables to the tables they reference. Dashed boxes identify tables outside the selected schema. JSON contains the catalog entries and identifies any clipped expressions. The command refuses to overwrite an existing path.

## Audit and errors

JSON Lines on stderr, stdout reserved for MCP. One `tool_call` record per handled call, plus `startup` and `shutdown`:

```json
{"ts":1795707647283,"event":"startup","mode":"read","auth":"sql","tls_verify":true}
{"ts":1795707647289,"event":"tool_call","tool":"select_rows","mode":"read","status":"error","code":"connection_failed","duration_ms":1,"scope":{"schema":"dbo","table":"Items"}}
{"ts":1795707647289,"event":"shutdown","status":"ok"}
```

Records carry the known tool name, effective mode, status, error code, duration, the validated `scope` and bounded counters (`rows`, `affected_rows`, `truncated`). Mutation records and every `query` batch that reaches the approval gate add `approval` as `yolo`, `granted`, `denied`, `required` or `unavailable`; `required` is the relay question, and the retry that answers it records `granted` or `denied`. A call that involves a relay token also carries `approval_token_id`, the short label of that token, so a question and its answer can be correlated; the token itself is never recorded. `explore_schema` and the other read tools never carry either field. A `query` record has no `scope`, because arbitrary SQL has no validated target, and it never contains the batch text. Records never contain SQL text, values, credentials, host names, result rows or raw driver messages; validated schema and table names do appear. The trail is operational evidence, not a signed or tamper-proof audit log: capture it with your client or supervisor.

Tool errors keep `isError`, a human-readable `content[0].text` and a `structuredContent` object with `code`, `message`, `action`, `retryable`, and a `mutation_outcome` for writes when the server can describe it. A SQL Server rejection also includes `sql_error: {number, severity, message}`. Login and transport diagnostics, custom error text, connection details, and known value-bearing error details are not relayed.

| `code` | Cause | `mutation_outcome` |
| --- | --- | --- |
| `mode_unavailable` | tool disabled by `MSSQL_MODE` | `not_attempted` |
| `invalid_arguments` | arguments do not match the tool schema | `not_attempted` |
| `invalid_input` | a validated field was rejected | `not_attempted` |
| `object_not_found` | a requested catalog object is missing or not visible | omitted |
| `definition_unavailable` | `sys.sql_modules.definition` is NULL, or the login lacks `VIEW DEFINITION` | omitted |
| `approval_denied` | the human declined or cancelled, or the approval prompt timed out | `not_attempted` |
| `approval_unavailable` | the client advertised elicitation but the prompt could not be delivered | `not_attempted` |
| `approval_token_unknown` | `approval_token` does not name a pending question | `not_attempted` |
| `approval_token_expired` | the question was answered after its 120 seconds | `not_attempted` |
| `approval_token_reused` | the token was already answered, granted or denied | `not_attempted` |
| `approval_token_mismatch` | the retry rebuilds a different tool or statement | `not_attempted` |
| `approval_token_invalid` | `approval_token` and `approval_decision` were not supplied together | `not_attempted` |
| `database_busy` | four operations already active | `not_attempted` |
| `connection_failed` | TCP, TLS or login failed before any statement | `not_attempted` |
| `authentication_unavailable` | mode missing from this build or platform | `not_attempted` |
| `query_failed` | SQL Server rejected the statement | `rolled_back` after confirming a generated mutation did not commit; omitted otherwise |
| `query_failed` | connection or driver failed during execution | `unknown` |
| `timeout` | the operation deadline elapsed | `unknown` |
| `unrecognized_result` | unexpected result shape from the driver | omitted |
| `mutation_rolled_back` | more than 100 rows would change | `rolled_back` |

`retryable` is `true` only for a busy server that ran nothing. `mutation_outcome` is present only when the server can describe it: `unknown` means a connection loss or timeout may have reached SQL Server, so read the database state before retrying. When SQL Server itself rejects the statement, the outcome is omitted for reads and raw batches and is `rolled_back` once a generated mutation's transaction is confirmed closed. An unknown tool name stays a JSON-RPC `-32601` error with `data.code: "unknown_tool"`.

## License

Apache-2.0. See [LICENSE](LICENSE).

## Working with an agent

Start with `explore_schema` before any query: it returns the schemas, tables, columns, keys, relations and routine signatures in one indented tree, so an agent can see where data lives without guessing. Narrow it with `schema`, or add `definition: true` to read procedure and function bodies.

A reliable order for a new database is `explore_schema` → `describe_table` → `sample_rows` or a sorted `select_rows`. Use `search_objects` if you know part of a name, `object_dependencies` for impact analysis, and `schema_map` when you need to page through the whole catalog.

To audit one routine, read its body with `routine_definition` and follow `next_offset` until it is `null`. Use `object_dependencies` for the edge list, then `dependency_columns` with a hint such as `CreadoEn` to find which referenced table carries that column without calling another tool. Once the table is known, `describe_table` or `select_rows` confirms the data. Remember that a login without `VIEW DEFINITION` sees none of these definitions or dependency rows.

Reach for `query` only when the structured tools cannot express the operation and the human has approved it or deliberately enabled `--yolo`.
