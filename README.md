# mssql-mcp

A native Rust MCP server for Microsoft SQL Server. Configure it with command-line options or environment variables; passwords are accepted only via the environment.

## Configuration

Set `MSSQL_HOST` and `MSSQL_DATABASE`, then select an authentication method:

| `MSSQL_AUTH` | Identity | Additional settings |
|---|---|---|
| `sql` (default) | SQL Server login | `MSSQL_USER`, `MSSQL_PASSWORD` |
| `windows` | Current Windows process identity | None. Windows only. |
| `ntlm` | Explicit Windows domain credentials | `MSSQL_DOMAIN`, `MSSQL_USER`, `MSSQL_PASSWORD`; pass the domain and unqualified username separately. |

`MSSQL_PORT` defaults to `1433`; use the instance's TCP port for a named instance. `MSSQL_MODE` defaults to `read`; use `write` for mutation or `both` for both sets of tools. `MSSQL_TIMEOUT_SECONDS` defaults to `15` (range: 1–300). TLS certificate verification is on by default; `--trust-server-certificate` is for development only.

Run `mssql-mcp --help` for options, or `mssql-mcp --check-config` to validate settings without a database connection.