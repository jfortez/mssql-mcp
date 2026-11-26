# mssql-mcp

Native MCP server for Microsoft SQL Server. Read-only by default, with authenticated Streamable HTTP for sharing one process across clients.

## 1. Install with Cargo

From this checkout (Rust 1.88+): `cargo install --path . --locked`.

From crates.io **after publication**: `cargo install mssql-mcp --locked`. For Docker, skip Cargo.

## 2. Start one server

Export `MSSQL_PASSWORD` from your secret store or shell. Set the host, database and user, then generate **one** token for the server and all clients:

```sh
export MSSQL_HOST=sql.example.com MSSQL_DATABASE=Inventory MSSQL_USER=mcp_reader
export MSSQL_MCP_TOKEN="$(openssl rand -hex 32)"
mssql-mcp --transport http
```

**Docker instead:** with those same variables set, run `docker compose up -d --build`. It binds only to host loopback and runs as non-root with a read-only filesystem. `MSSQL_HOST` must be reachable from the container. Stop with `docker compose down`. Keep secrets out of Git and the image; Docker administrators can inspect container environment variables.

Both options serve `http://127.0.0.1:8765/mcp`. Start **one** of them. Repeated `docker compose up` in the same project keeps exactly one container (Compose may recreate it, never run two); a second listener cannot take the same port. Stdio is still available, but starts a process per client. Do not expose the local bearer endpoint to the network.

## 3. Connect your clients

Launch each client with the **same `MSSQL_MCP_TOKEN`** in its environment. Replace any existing stdio entry for this server.

### Claude Code

Add to the project’s `.mcp.json`, then approve the server when Claude Code prompts:

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

Check the connection with `claude mcp get sql-server`.

### Codex

Add to `~/.codex/config.toml`, then run `/mcp` in Codex:

```toml
[mcp_servers.sql-server]
url = "http://127.0.0.1:8765/mcp"
bearer_token_env_var = "MSSQL_MCP_TOKEN"
```

### Pi, OpenCode and other clients

- **Pi:** `pi mcp add sql-server --url http://127.0.0.1:8765/mcp --bearer-token-env-var MSSQL_MCP_TOKEN`.
- **OpenCode:** under [`mcp.servers.sql-server`](https://opencode.ai/docs/mcp-servers/) (V2), set `type: "remote"`, the same `url`, `oauth: false`, and `headers: { "Authorization": "Bearer {env:MSSQL_MCP_TOKEN}" }`. Older versions put the entry directly under `mcp`.
- **Other ACP hosts:** the agent or adapter must support remote Streamable HTTP MCP. Point it at the same URL and send `Authorization: Bearer <token>`.

## Settings and checks

| Option | Values |
| --- | --- |
| `MSSQL_AUTH` | `sql` (default): user/password. `ntlm`: domain, unqualified user and password. `windows`: current Windows identity via NTLM, unavailable in Linux Docker and on Kerberos-only servers. |
| `MSSQL_MODE` | `read` (default), `write`, or `both`. All clients share the daemon’s SQL identity and mode. |
| SQL connection | `MSSQL_PORT` defaults to `1433`; `MSSQL_TIMEOUT_SECONDS` defaults to `15`. Use your SQL Server DNS name and keep TLS verification on. |

Use a least-privilege login. `mssql-mcp --check-config` validates settings without connecting; `--help` lists all options. Audit records are JSON Lines on stderr and omit credentials, SQL text, and values. Errors expose `code`, `action`, `retryable`, and write outcomes; verify database state before retrying a write with `mutation_outcome: unknown`.

Run `cargo test --locked` locally. [CI](.github/workflows/ci.yml) also verifies the Cargo package and tests one Dockerized MCP with two clients against SQL Server 2022.
