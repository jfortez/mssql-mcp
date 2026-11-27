# mssql-mcp

MCP server for Microsoft SQL Server. Read-only by default; supports stdio and authenticated Streamable HTTP.

## 1. Install

| Source | Command |
| --- | --- |
| Source (Rust 1.88+) | `cargo install --path . --locked` |
| [crates.io](https://crates.io/crates/mssql-mcp) | `cargo install mssql-mcp --locked` |
| Docker | [Run with Compose](#docker) |

## 2. Configure

Use flags or environment variables. Secrets are environment-only.

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

**Authentication.** `sql` uses an unqualified login and password. `ntlm` uses `DOMAIN\user` and a password. `windows` uses the current Windows identity without SQL credentials and is unavailable on Linux or Docker. Without `--auth`, a backslash in `MSSQL_USER` selects `ntlm`; otherwise the default is `sql`. Explicit modes reject incompatible credentials without falling back.

**TLS.** Use the hostname in the server certificate. The server verifies certificates against the system trust store by default.

**Access.** `read` exposes the read tools; `write` exposes `insert_row`, `update_rows`, `delete_rows`, `query`, and `explore_schema`; `both` exposes all 15. Updates and deletes require filters and roll back above 100 rows. Mutations and `query` require human approval before connecting unless `--yolo` is set. The bypass is audited. Grant the login only the SQL Server permissions it needs.

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

Send `Authorization: Bearer $MSSQL_MCP_TOKEN` with every request to `http://127.0.0.1:8765/mcp`. Keep the endpoint on loopback: foreign `Host` and `Origin` headers are rejected. Do not add CORS or a reverse proxy.

Clients share one SQL identity, mode, and four concurrent database slots. Each tool call opens its own SQL connection. Use another port for a different identity or mode. HTTP uses the [approval relay](#approving-a-mutation-without-elicitation).

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

Compose runs one non-root service with a read-only filesystem, no Linux capabilities, and no new privileges. It reads settings from the shell or `./.env`:

```sh
export MSSQL_HOST=sql.example.com MSSQL_DATABASE=Inventory MSSQL_USER=mcp_reader
export MSSQL_PASSWORD="$(cat /run/secrets/sql-password)"
export MSSQL_MCP_TOKEN="$(openssl rand -hex 32)"

docker compose config --quiet     # validate interpolation, build nothing
docker compose up -d --build      # build and start
docker compose logs -f mssql-mcp  # follow JSON audit lines on stderr
docker compose down               # stop and remove
```

The container listens on `0.0.0.0:8765` and publishes only `127.0.0.1:8765`. Use an `MSSQL_HOST` reachable from the container; `localhost` means the container itself.

Rebuild with `docker compose up -d --build`. To bypass the build cache, run `docker compose build --no-cache` first.

Keep `.env` untracked. Docker users can inspect runtime secrets; the image and build context do not include them. The bearer token is not OAuth, and the image does not terminate TLS. Keep the published port on loopback.

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

Use the HTTP daemon if the ACP host's agent or adapter supports Streamable HTTP MCP. Otherwise, run one stdio process per client.

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

`query` sends one T-SQL batch unchanged. SQL Server permissions are the boundary: the batch can reach other databases and has no affected-row cap. Approval shows the exact SQL unless `--yolo` is set.

- **One result set:** Up to 100 rows (`limit` can lower it), 16 columns, and 1,024 UTF-16 units per cell. Cuts set `truncated`.
- **No result set:** Returns `executed: true`.
- **Multiple result sets:** Returns `unrecognized_result` without a mutation outcome.

Check the database before retrying a failed batch: earlier statements may have committed.

## Approving a mutation without elicitation

Clients that advertise elicitation get a direct prompt. Other clients, including HTTP clients, relay the question:

1. Read `approval_required`. It is not an error, and `mutation_outcome: not_attempted` means SQL Server was not contacted.
2. Show `approval_message` to the human: target and columns for a table mutation, or the exact SQL for `query`. Text-only clients can use `content[0].text`.
3. Repeat the same call with `approval_token` and `approval_decision`: `grant` runs it once; `deny` refuses it. Both spend the token.

The reply includes `approval_token_id` for correlation, `row_cap` (`100`, or `null` for `query`), and `expires_in_seconds` (`120`). A retry must use a known, unexpired, unused token for the same tool and byte-identical statement. Rejected tokens never reach SQL Server; see [error codes](#audit-and-errors).

## Reading the catalog

- **`explore_schema`:** Returns one indented `tree` in every mode. Filter by `schema` or `include` (`tables`, `views`, `procedures`, `functions`; all by default). Use `definition: true` to include procedure and function bodies. Cuts set `truncated` and end the tree with `… truncated`.
- **`schema_map`:** Returns records with stable IDs. Foreign keys name both endpoints and list column pairs in order. Follow `next_offset` until `null`; `truncated` flags clipped expressions, not missing pages. Table counts come from partition metadata, not `COUNT(*)`.
- **`routine_definition`:** Pages a procedure, function, view, or trigger body in UTF-16 units. `limit` defaults to 8,000 and caps at 20,000. `definition_length` is the total length. Follow `next_offset` until `null` and join the pages. A NULL body returns `definition_unavailable`.
- **`dependency_columns`:** Lists referenced objects and their columns. `column_hint` matches a literal fragment; omit it for all columns. Objects with no matches still appear with `matching_column_count: 0`. Unresolved or cross-database objects have a null `object_id` and no readable columns. Follow `next_offset` until `null`.

`object_dependencies` includes unresolved SQL references. Without `VIEW DEFINITION`, SQL Server can hide definitions and dependency rows.

## Export a schema map without MCP

```sh
mssql-mcp map --format html --out /tmp/sales-schema.html --schema Sales
mssql-mcp map --format json --out /tmp/sales-schema.json --schema Sales
```

Use the server's SQL flags or `MSSQL_*` settings. `map` reads the catalog directly without MCP, a bearer token, or database writes. Omit `--schema` for all visible schemas. See `mssql-mcp map --help`.

HTML shows relationships offline; dashed boxes mark tables outside the selected schema. JSON lists catalog entries and flags clipped expressions. Neither format overwrites an existing file.

## Audit and errors

JSON Lines on stderr, stdout reserved for MCP. One `tool_call` record per handled call, plus `startup` and `shutdown`:

```json
{"ts":1795707647283,"event":"startup","mode":"read","auth":"sql","tls_verify":true}
{"ts":1795707647289,"event":"tool_call","tool":"select_rows","mode":"read","status":"error","code":"connection_failed","duration_ms":1,"scope":{"schema":"dbo","table":"Items"}}
{"ts":1795707647289,"event":"shutdown","status":"ok"}
```

- **Tool calls:** Record tool, mode, status, error code, duration, validated `scope`, and bounded counters (`rows`, `affected_rows`, `truncated`). `query` has no validated `scope`.
- **Approval:** Mutations and `query` reaching the approval gate record `approval` as `yolo`, `granted`, `denied`, `required`, or `unavailable`. `approval_token_id` links a relay question to its answer without logging the token. Read tools omit both fields.
- **Privacy:** Logs omit SQL text, values, credentials, host names, result rows, and raw driver messages. Validated schema and table names can appear. Capture the log with your client or supervisor; it is not tamper-proof.

Tool errors return `isError`, readable `content[0].text`, and `structuredContent` with `code`, `message`, `action`, `retryable`, and `mutation_outcome` when known. SQL Server rejections include `sql_error: {number, severity, message}`. Sensitive diagnostics and error text are not relayed.

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

Only `database_busy` is `retryable`. If `mutation_outcome` is `unknown`, check the database before retrying. An unknown tool returns JSON-RPC `-32601` with `data.code: "unknown_tool"`.

## Maintainer releases

1. Bump the version in `Cargo.toml` and refresh `Cargo.lock` on the default branch.
2. Run **Actions → Release** with `dry_run` enabled. No token, upload, or tag.
3. Set the `CARGO_REGISTRY_TOKEN` secret and run again with `dry_run` disabled.

The workflow rejects an existing remote `v<version>` before upload. After publishing, it pushes an annotated `v<version>` on the published commit and creates a GitHub Release with generated notes.

If tagging or release creation fails after upload, use the run's commit SHA to restore the missing tag or release. Verify any existing tag without moving it. Don't republish the same version.

## License

Apache-2.0. See [LICENSE](LICENSE).

## Working with an agent

- **Explore data:** `explore_schema` → `describe_table` → `sample_rows` or a sorted `select_rows`. Use `search_objects` for partial names and `schema_map` for the full catalog.
- **Trace a routine:** `routine_definition` → `object_dependencies` → `dependency_columns`. Use a `column_hint` such as `CreadoEn` to find matching columns on referenced objects.
- **Run raw SQL:** Use `query` only when structured tools cannot express the operation. It requires human approval unless `--yolo` is set.
