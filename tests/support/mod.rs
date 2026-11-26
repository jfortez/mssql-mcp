use std::net::TcpListener;
use std::process::Command;
use std::thread::{self, JoinHandle};

pub fn clear_env(command: &mut Command) -> &mut Command {
    command.env_clear();
    restore_system_environment(command);
    command
}

#[cfg(windows)]
fn restore_system_environment(command: &mut Command) {
    for name in ["SystemRoot", "windir", "PATH", "TEMP", "TMP"] {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
}

#[cfg(not(windows))]
const fn restore_system_environment(_command: &mut Command) {}

#[allow(dead_code)]
pub fn dropping_peer() -> (u16, JoinHandle<()>) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind the dropping peer");
    let port = listener.local_addr().expect("dropping peer address").port();
    let handle = thread::spawn(move || {
        for incoming in listener.incoming() {
            if incoming.is_err() {
                break;
            }
        }
    });
    (port, handle)
}
