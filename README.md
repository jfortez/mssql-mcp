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
| `MSSQL_AUTH` | `--auth` | `sql`, `windows`, `ntlm` | `sql` |
| `MSSQL_USER` | — | login; not used by `windows` | required for `sql` and `ntlm` |
| `MSSQL_PASSWORD` | — | login password; environment only | required for `sql` and `ntlm` |
| `MSSQL_DOMAIN` | — | Windows domain, unqualified | required for `ntlm` |
| `MSSQL_PORT` | `--port` | 1–65535; the instance's TCP port | `1433` |
| `MSSQL_MODE` | `--mode` | `read`, `write`, `both` | `read` |
| `MSSQL_MCP_TRANSPORT` | `--transport` | `stdio`, `http` | `stdio` |
| `MSSQL_MCP_HTTP_LISTEN` | `--http-listen` | `IP:port` | `127.0.0.1:8765` |
| `MSSQL_MCP_TOKEN` | — | 32–128 of `A–Z a–z 0–9 - _`; HTTP only | required for `http` |
| `MSSQL_TIMEOUT_SECONDS` | `--timeout-seconds` | 1–300, per operation | `15` |
| `MSSQL_TRUST_SERVER_CERTIFICATE` | `--trust-server-certificate` | `true` skips certificate checks (development only) | `false` |

Authentication modes: `sql` uses `MSSQL_USER`/`MSSQL_PASSWORD`; `ntlm` uses those plus `MSSQL_DOMAIN`; `windows` uses the current Windows process identity, so it is unavailable on Linux and in the Docker image and does not use the two SQL credentials. No mode falls back to another identity.

TLS is required and certificates are validated against the system store: use the hostname in the server certificate. `MSSQL_MODE=read` exposes only `list_tables`, `describe_table` and `select_rows`; `write` exposes `insert_row`, `update_rows` and `delete_rows`; `both` exposes all six. Updates and deletes need at least one filter and roll back when more than 100 rows would change. Give the login only the SQL Server permissions it needs.

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

A second process on the same address fails at bind, so clients keep addressing exactly one endpoint. All clients share the daemon's SQL identity, mode, `read`/`write` surface and its four concurrent database operations; each tool call still opens its own SQL Server connection. Need different identities or modes? Run another daemon on another port.

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
| `describe_table` | `table`, `offset?` | `name`, `type`, `max_length`, `precision`, `scale`, `is_nullable`, `ordinal` |
| `select_rows` | `table`, `columns`, `filters?`, `order_by?`, `limit?`, `offset?` | `columns`, `rows`, `truncated` |
| `insert_row` | `table`, `values` | `affected_rows` |
| `update_rows` | `table`, `values`, `filters` | `affected_rows` |
| `delete_rows` | `table`, `filters` | `affected_rows` |

`table` is `{"schema": "dbo", "name": "Products"}`; functions such as `schema` default to `dbo`. Filters are equality checks combined with `AND`, `null` matching `NULL`. Selects return 25 rows by default (maximum 100) and projects every column as text, capped at 1,024 UTF-16 units; a `truncated` flag reports a cut cell or page. Use a unique `order_by` for stable pagination. Mutations return `affected_rows: -1` with `mutation_outcome: rolled_back` when the 100-row limit rolled back. There is no raw SQL endpoint.

## Audit and errors

JSON Lines on stderr, stdout reserved for MCP. One `tool_call` record per handled call, plus `startup` and `shutdown`:

```json
{"ts":1795707647283,"event":"startup","mode":"read","auth":"sql","tls_verify":true}
{"ts":1795707647289,"event":"tool_call","tool":"select_rows","mode":"read","status":"error","code":"connection_failed","duration_ms":1,"scope":{"schema":"dbo","table":"Items"}}
{"ts":1795707647289,"event":"shutdown","status":"ok"}
```

Records carry the known tool name, effective mode, status, error code, duration, the validated `scope` and bounded counters (`rows`, `affected_rows`, `truncated`). They never contain SQL text, values, credentials, host names, result rows or raw driver messages; validated schema and table names do appear. The trail is operational evidence, not a signed or tamper-proof audit log: capture it with your client or supervisor.

Tool errors keep `isError`, a human-readable `content[0].text` and a `structuredContent` object with `code`, `message`, `action`, `retryable`, and for writes `mutation_outcome`:

| `code` | Cause | `mutation_outcome` |
| --- | --- | --- |
| `mode_unavailable` | tool disabled by `MSSQL_MODE` | `not_attempted` |
| `invalid_arguments` | arguments do not match the tool schema | `not_attempted` |
| `invalid_input` | a validated field was rejected | `not_attempted` |
| `database_busy` | four operations already active | `not_attempted` |
| `connection_failed` | TCP, TLS or login failed before any statement | `not_attempted` |
| `authentication_unavailable` | mode missing from this build or platform | `not_attempted` |
| `query_failed` | SQL Server failed the statement | `unknown` |
| `timeout` | the operation deadline elapsed | `unknown` |
| `unrecognized_result` | unexpected result shape from the driver | `unknown` |
| `mutation_rolled_back` | more than 100 rows would change | `rolled_back` |

`retryable` is `true` only for a busy server that ran nothing. `mutation_outcome: unknown` means the statement may have reached SQL Server: read the database state before retrying. An unknown tool name stays a JSON-RPC `-32601` error with `data.code: "unknown_tool"`.

## License

Apache-2.0. See [LICENSE](LICENSE).
