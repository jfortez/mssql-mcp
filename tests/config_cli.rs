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

#[test]
fn map_help_describes_the_one_shot_export_without_connecting() {
    let out = run(&["map", "--help"], &[]);
    assert_eq!(out.status.code(), Some(0));
    assert!(out.stderr.is_empty());
    let help = String::from_utf8(out.stdout).unwrap();
    for option in [
        "--format",
        "html",
        "json",
        "--out",
        "--schema",
        "--host",
        "--database",
    ] {
        assert!(help.contains(option), "missing {option} in {help}");
    }
}
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

const NTLM_DOMAIN_ERROR: &[u8] =
    b"Configuration error: NTLM requires the domain in MSSQL_USER as DOMAIN\\user, not only the user name\n";
const SQL_DOMAIN_ERROR: &[u8] =
    b"Configuration error: SQL authentication cannot use a qualified MSSQL_USER: the DOMAIN\\user principal needs --auth ntlm\n";
const EMPTY_PART_ERROR: &[u8] =
    b"Configuration error: MSSQL_USER must be DOMAIN\\user with a non-empty domain and user\n";

#[test]
fn explicit_ntlm_requires_the_domain_in_the_user_string() {
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
    assert_eq!(out.stderr, NTLM_DOMAIN_ERROR);
}

#[test]
fn explicit_sql_rejects_a_qualified_user_instead_of_guessing() {
    let env = [
        ("MSSQL_USER", "BSOFTSOLUTION\\jcaiza"),
        ("MSSQL_PASSWORD", "never-print-this"),
    ];
    let out = run(
        &[CONNECTION, &["--auth", "sql", "--check-config"]].concat(),
        &env,
    );
    assert_eq!(out.status.code(), Some(2));
    assert!(out.stdout.is_empty());
    assert_eq!(out.stderr, SQL_DOMAIN_ERROR);
}

#[test]
fn a_qualified_user_selects_ntlm_when_the_auth_flag_is_absent() {
    let env = [
        ("MSSQL_USER", "BSOFTSOLUTION\\jcaiza"),
        ("MSSQL_PASSWORD", "never-print-this"),
    ];
    let out = run(&[CONNECTION, &["--check-config"]].concat(), &env);
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(out.stdout, b"Configuration valid (auth=ntlm, mode=read)\n");
    assert!(out.stderr.is_empty());

    // Without a backslash the same omitted flag keeps the SQL login.
    let env = [
        ("MSSQL_USER", "jcaiza"),
        ("MSSQL_PASSWORD", "never-print-this"),
    ];
    let out = run(&[CONNECTION, &["--check-config"]].concat(), &env);
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(out.stdout, b"Configuration valid (auth=sql, mode=read)\n");
    assert!(out.stderr.is_empty());

    // Detection is not a bypass: an absent user string still fails like SQL.
    let env = [("MSSQL_PASSWORD", "never-print-this")];
    let out = run(&[CONNECTION, &["--check-config"]].concat(), &env);
    assert_eq!(out.status.code(), Some(2));
    assert!(out.stdout.is_empty());
    assert_eq!(
        out.stderr,
        b"Configuration error: MSSQL_USER is required for SQL authentication\n"
    );
}

#[test]
fn the_removed_domain_variable_has_no_effect() {
    // An unqualified user stays a SQL login even with MSSQL_DOMAIN set, because
    // the variable is no longer read.
    let env = [
        ("MSSQL_USER", "jcaiza"),
        ("MSSQL_PASSWORD", "never-print-this"),
        ("MSSQL_DOMAIN", "BSOFTSOLUTION"),
    ];
    let out = run(&[CONNECTION, &["--check-config"]].concat(), &env);
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(out.stdout, b"Configuration valid (auth=sql, mode=read)\n");
    assert!(out.stderr.is_empty());

    // Nor can it satisfy `--auth ntlm`: the domain belongs in MSSQL_USER.
    let out = run(
        &[CONNECTION, &["--auth", "ntlm", "--check-config"]].concat(),
        &env,
    );
    assert_eq!(out.status.code(), Some(2));
    assert!(out.stdout.is_empty());
    assert_eq!(out.stderr, NTLM_DOMAIN_ERROR);
}

#[test]
fn a_backslash_needs_a_domain_and_a_user_around_it() {
    for user in ["\\jcaiza", "BSOFTSOLUTION\\"] {
        let env = [("MSSQL_USER", user), ("MSSQL_PASSWORD", "never-print-this")];
        let out = run(&[CONNECTION, &["--check-config"]].concat(), &env);
        assert_eq!(out.status.code(), Some(2), "{user:?} was accepted");
        assert!(out.stdout.is_empty(), "{user:?} was accepted");
        assert_eq!(out.stderr, EMPTY_PART_ERROR, "{user:?}");
    }
}

#[test]
fn explicit_ntlm_and_write_mode_can_be_configured_via_env() {
    let env = [
        ("MSSQL_HOST", "localhost"),
        ("MSSQL_DATABASE", "test"),
        ("MSSQL_AUTH", "ntlm"),
        ("MSSQL_MODE", "write"),
        ("MSSQL_USER", "EXAMPLE\\user"),
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
fn yolo_is_accepted_in_every_mode_and_keeps_check_config_identical() {
    let read_output = b"Configuration valid (auth=sql, mode=read)\n";
    let baseline = run(&[CONNECTION, &["--check-config"]].concat(), &sql_env(None));
    assert_eq!(baseline.status.code(), Some(0));
    assert_eq!(baseline.stdout, read_output);
    assert!(baseline.stderr.is_empty());

    // The opt-out is accepted in every mode but never changes the validation
    // report: only the effective auth and mode are printed.
    for mode in ["read", "write", "both"] {
        let with_yolo = run(
            &[CONNECTION, &["--mode", mode, "--yolo", "--check-config"]].concat(),
            &sql_env(None),
        );
        let without_yolo = run(
            &[CONNECTION, &["--mode", mode, "--check-config"]].concat(),
            &sql_env(None),
        );
        assert_eq!(with_yolo.status.code(), Some(0), "{mode} mode");
        assert_eq!(with_yolo.stdout, without_yolo.stdout, "{mode} mode");
        assert_eq!(with_yolo.stderr, without_yolo.stderr, "{mode} mode");
        assert!(with_yolo.stderr.is_empty(), "{mode} mode");
    }

    // The environment variable is a boolean: true turns it on, false keeps the
    // default, and anything else is rejected instead of being guessed.
    for value in ["true", "false"] {
        let mut env = sql_env(None);
        env.push(("MSSQL_MCP_YOLO", value));
        let out = run(&[CONNECTION, &["--check-config"]].concat(), &env);
        assert_eq!(out.status.code(), Some(0), "MSSQL_MCP_YOLO={value}");
        assert_eq!(out.stdout, read_output, "MSSQL_MCP_YOLO={value}");
        assert!(out.stderr.is_empty(), "MSSQL_MCP_YOLO={value}");
    }

    let mut env = sql_env(None);
    env.push(("MSSQL_MCP_YOLO", "maybe"));
    let out = run(&[CONNECTION, &["--check-config"]].concat(), &env);
    assert_eq!(out.status.code(), Some(2));
    assert!(out.stdout.is_empty());
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(
        stderr.contains("invalid value 'maybe'") && stderr.contains("--yolo"),
        "the invalid boolean must be reported: {stderr}"
    );
}

#[test]
fn help_documents_the_yolo_opt_out() {
    let out = run(&["--help"], &[]);
    assert_eq!(out.status.code(), Some(0));
    let help = String::from_utf8(out.stdout).expect("help output is UTF-8");
    assert!(help.contains("--yolo"), "help must document --yolo: {help}");
    assert!(
        help.contains("MSSQL_MCP_YOLO"),
        "help must document the yolo environment variable: {help}"
    );
}

#[test]
fn help_documents_detection_instead_of_a_sql_default() {
    let out = run(&["--help"], &[]);
    assert_eq!(out.status.code(), Some(0));
    let help = String::from_utf8(out.stdout).expect("help output is UTF-8");
    assert!(
        !help.contains("[default: sql]"),
        "an omitted auth flag must not claim an implicit default: {help}"
    );
    assert!(
        help.contains("selects NTLM when this flag is absent"),
        "help must document the detection from MSSQL_USER: {help}"
    );
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
