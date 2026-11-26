use std::process::Command;

pub fn clear_env(command: &mut Command) -> &mut Command {
    command.env_clear();
    #[cfg(windows)]
    for name in ["SystemRoot", "windir", "PATH", "TEMP", "TMP"] {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    command
}
