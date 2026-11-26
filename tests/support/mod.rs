use std::net::TcpListener;
use std::process::Command;
use std::sync::Mutex;
use std::thread::{self, JoinHandle};

/// Loopback port that a test binary never hands out twice.
///
/// Binding port zero and dropping the listener lets the operating system hand
/// the same port to a later test while an earlier client still dials it, which
/// shows up as an unrelated connection reset or as a client reaching the wrong
/// server. A monotonic counter outside the ephemeral range, verified with a
/// bind, removes that race.
#[allow(dead_code)]
pub fn unique_loopback_port() -> u16 {
    static NEXT: Mutex<Option<u16>> = Mutex::new(None);
    let mut next = NEXT.lock().expect("port allocator");
    let base = *next.get_or_insert_with(|| {
        let offset = std::process::id() % 8_000;
        u16::try_from(20_000 + offset).expect("port base fits in u16")
    });
    let mut candidate = base.max(20_000);
    loop {
        if TcpListener::bind(("127.0.0.1", candidate)).is_ok() {
            *next = Some(candidate.saturating_add(1));
            return candidate;
        }
        candidate = candidate.saturating_add(1);
    }
}

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
