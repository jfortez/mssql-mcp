use std::process::{Command, Output};

fn run(args: &[&str], env: &[(&str, &str)]) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_mssql-mcp"));
    cmd.env_clear().envs(env.iter().copied()).args(args);
    cmd.output().unwrap()
}

const CONNECTION: &[&str] = &["--host", "localhost", "--database", "test"];

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
