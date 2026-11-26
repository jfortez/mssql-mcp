use std::process::{Command, Output};

mod support;

fn run(args: &[&str], env: &[(&str, &str)]) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_mssql-mcp"));
    support::clear_env(&mut cmd)
        .envs(env.iter().copied())
        .args(args);
    cmd.output().unwrap()
}

const CONNECTION: &[&str] = &["--host", "localhost", "--database", "test"];
const TOKEN: &str = "mssql-mcp-http-test-token_0123456789";
const TOKEN_ERROR: &[u8] =
    b"Configuration error: MSSQL_MCP_TOKEN must be 32 to 128 characters of A-Z, a-z, 0-9, '_' or '-'\n";

fn sql_env(token: Option<&str>) -> Vec<(&str, &str)> {
    let mut env = vec![("MSSQL_USER", "sa"), ("MSSQL_PASSWORD", "never-print-this")];
    if let Some(token) = token {
        env.push(("MSSQL_MCP_TOKEN", token));
    }
    env
}

#[test]
fn sql_auth_and_read_mode_are_the_defaults() {
    let env = [("MSSQL_USER", "sa"), ("MSSQL_PASSWORD", "never-print-this")];
    let out = run(&[CONNECTION, &["--check-config"]].concat(), &env);
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(
        out.stdout,
        b"Configuration valid (auth=sql, mode=read)
"
    );
    assert!(out.stderr.is_empty());
}

#[test]
fn explicit_ntlm_requires_its_own_domain() {
    let env = [
        ("MSSQL_USER", "user"),
        ("MSSQL_PASSWORD", "never-print-this"),
    ];
    let out = run(
        &[CONNECTION, &["--auth", "ntlm", "--check-config"]].concat(),
        &env,
    );
    assert_eq!(out.status.code(), Some(2));
    assert!(out.stdout.is_empty());
    assert_eq!(
        out.stderr,
        b"Configuration error: MSSQL_DOMAIN is required for NTLM
"
    );
}

#[test]
fn explicit_ntlm_and_write_mode_can_be_configured_via_env() {
    let env = [
        ("MSSQL_HOST", "localhost"),
        ("MSSQL_DATABASE", "test"),
        ("MSSQL_AUTH", "ntlm"),
        ("MSSQL_MODE", "write"),
        ("MSSQL_DOMAIN", "EXAMPLE"),
        ("MSSQL_USER", "user"),
        ("MSSQL_PASSWORD", "never-print-this"),
    ];
    let out = run(&["--check-config"], &env);
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(
        out.stdout,
        b"Configuration valid (auth=ntlm, mode=write)
"
    );
    assert!(out.stderr.is_empty());
}

#[test]
fn windows_auth_is_not_silently_replaced_with_sql_or_ntlm() {
    let out = run(
        &[
            CONNECTION,
            &["--auth", "windows", "--mode", "both", "--check-config"],
        ]
        .concat(),
        &[],
    );
    if cfg!(windows) {
        assert_eq!(out.status.code(), Some(0));
        assert_eq!(
            out.stdout,
            b"Configuration valid (auth=windows, mode=both)
"
        );
        assert!(out.stderr.is_empty());
    } else {
        assert_eq!(out.status.code(), Some(2));
        assert!(out.stdout.is_empty());
        assert_eq!(
            out.stderr,
            b"Configuration error: Windows integrated authentication requires Windows
"
        );
    }
}

#[test]
fn http_transport_requires_a_bearer_token() {
    let out = run(
        &[CONNECTION, &["--transport", "http", "--check-config"]].concat(),
        &sql_env(None),
    );
    assert_eq!(out.status.code(), Some(2));
    assert!(out.stdout.is_empty());
    assert_eq!(
        out.stderr,
        b"Configuration error: MSSQL_MCP_TOKEN is required for the HTTP transport\n"
    );
}

#[test]
fn http_transport_rejects_tokens_that_are_not_url_safe_and_long_enough() {
    let boundary = "mssql-mcp-http-test-token_012345";
    assert_eq!(boundary.len(), 32);
    let invalid = [
        "short".to_owned(),
        "mssql-mcp-http-test-token_01234".to_owned(),
        "mssql-mcp-http-test-token+0123456789".to_owned(),
        "mssql-mcp-http-test-token 0123456789".to_owned(),
        "mssql-mcp-http-test-token/0123456789".to_owned(),
        "a".repeat(129),
    ];
    for token in &invalid {
        let out = run(
            &[CONNECTION, &["--transport", "http", "--check-config"]].concat(),
            &sql_env(Some(token)),
        );
        assert_eq!(out.status.code(), Some(2), "token {token:?} was accepted");
        assert!(out.stdout.is_empty(), "token {token:?} was accepted");
        assert_eq!(out.stderr, TOKEN_ERROR, "token {token:?}");
    }

    for token in [boundary.to_owned(), "a".repeat(128)] {
        let out = run(
            &[CONNECTION, &["--transport", "http", "--check-config"]].concat(),
            &sql_env(Some(&token)),
        );
        assert_eq!(
            out.status.code(),
            Some(0),
            "a {}-character token is valid",
            token.len()
        );
        assert_eq!(
            out.stdout, b"Configuration valid (auth=sql, mode=read)\n",
            "the check output must not describe the transport"
        );
        assert!(out.stderr.is_empty());
    }
}

#[test]
fn http_transport_can_be_configured_from_the_environment() {
    let env = [
        ("MSSQL_HOST", "localhost"),
        ("MSSQL_DATABASE", "test"),
        ("MSSQL_USER", "sa"),
        ("MSSQL_PASSWORD", "never-print-this"),
        ("MSSQL_MCP_TRANSPORT", "http"),
        ("MSSQL_MCP_HTTP_LISTEN", "127.0.0.1:8765"),
        ("MSSQL_MCP_TOKEN", TOKEN),
    ];
    let out = run(&["--check-config"], &env);
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(out.stdout, b"Configuration valid (auth=sql, mode=read)\n");
    assert!(out.stderr.is_empty());

    // Without the token the same environment must be rejected, which proves the
    // transport really came from the environment instead of falling back to stdio.
    let out = run(&["--check-config"], &env[..env.len() - 1]);
    assert_eq!(out.status.code(), Some(2));
    assert!(out.stdout.is_empty());
    assert_eq!(
        out.stderr,
        b"Configuration error: MSSQL_MCP_TOKEN is required for the HTTP transport\n"
    );
}

#[test]
fn stdio_transport_ignores_an_http_token() {
    let out = run(
        &[CONNECTION, &["--check-config"]].concat(),
        &sql_env(Some("short")),
    );
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(out.stdout, b"Configuration valid (auth=sql, mode=read)\n");
    assert!(out.stderr.is_empty());
}

#[test]
fn help_documents_the_loopback_default() {
    let out = run(&["--help"], &[]);
    assert_eq!(out.status.code(), Some(0));
    let help = String::from_utf8(out.stdout).expect("help output is UTF-8");
    assert!(
        help.contains("--transport") && help.contains("[default: stdio]"),
        "help must document that stdio remains the default: {help}"
    );
    assert!(
        help.contains("[default: 127.0.0.1:8765]"),
        "help must document the loopback default address: {help}"
    );
    assert!(
        help.contains("MSSQL_MCP_TOKEN") && help.contains("MSSQL_MCP_HTTP_LISTEN"),
        "help must document the HTTP transport environment: {help}"
    );
    assert!(
        !help.contains("--token"),
        "the bearer token must stay environment-only: {help}"
    );
}
