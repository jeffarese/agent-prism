//! Claude hooks → the running agent-prism (DESIGN §8).
//!
//! Client: `agp hook <Event>` forwards the hook payload over the socket in
//! `AGP_SOCK`. Server: a listener thread in the session turns each line
//! into a [`SessionEvent::Hook`].

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use crossbeam_channel::Sender;
use prism_term::runner::ExternalEvent;
use serde_json::Value;

use crate::host::SessionEvent;

const MAX_PAYLOAD: u64 = 1024 * 1024;
const TIMEOUT: Duration = Duration::from_millis(200);

/// `agp hook <Event>`: never prints, never fails; always exit code 0.
pub fn run_client(event: &str) -> i32 {
    let Some(sock) = prism_core::env::var_os("AGP_SOCK") else {
        return 0;
    };
    let mut input = Vec::new();
    let _ = std::io::stdin().take(MAX_PAYLOAD).read_to_end(&mut input);
    let payload: Value = serde_json::from_slice(&input).unwrap_or(Value::Null);
    let line = serde_json::json!({ "event": event, "payload": payload }).to_string();
    let _ = send(Path::new(&sock), &line);
    0
}

fn send(sock: &Path, line: &str) -> Result<()> {
    // UnixStream has no connect timeout; connect is local and fast, and the
    // write timeout bounds a stuck reader.
    let mut s = UnixStream::connect(sock)?;
    s.set_write_timeout(Some(TIMEOUT))?;
    s.write_all(line.as_bytes())?;
    s.write_all(b"\n")?;
    Ok(())
}

/// Parses one hook line into `(event, payload)`.
pub fn parse_line(line: &str) -> Option<(String, Value)> {
    let v: Value = serde_json::from_str(line).ok()?;
    let event = v.get("event")?.as_str()?.to_string();
    Some((event, v.get("payload").cloned().unwrap_or(Value::Null)))
}

/// Binds `sock` (replacing a stale file) and forwards hook lines to `tx`
/// from a background thread.
pub fn serve(sock: &Path, tx: Sender<ExternalEvent<SessionEvent>>) -> Result<()> {
    let _ = std::fs::remove_file(sock);
    let listener =
        UnixListener::bind(sock).with_context(|| format!("binding {}", sock.display()))?;
    std::thread::Builder::new()
        .name("agp-hooks".into())
        .spawn(move || {
            for conn in listener.incoming() {
                let Ok(conn) = conn else { continue };
                let _ = conn.set_read_timeout(Some(Duration::from_secs(2)));
                let mut r = BufReader::new(conn.take(MAX_PAYLOAD + 4096));
                let mut line = String::new();
                if r.read_line(&mut line).is_err() {
                    continue;
                }
                match parse_line(&line) {
                    Some((event, payload)) => {
                        log::debug!("hook {event}");
                        let ev = SessionEvent::Hook { event, payload };
                        if tx.send(ExternalEvent::Custom(ev)).is_err() {
                            return;
                        }
                    }
                    None => log::warn!("unreadable hook message"),
                }
            }
        })
        .context("spawning the hook listener")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_without_sock_is_silent() {
        // SAFETY: tests in this module do not read AGP_SOCK concurrently.
        unsafe { std::env::remove_var("AGP_SOCK") };
        assert_eq!(run_client("Stop"), 0);
    }

    #[test]
    fn round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("ctl.sock");
        std::fs::write(&sock, "stale").unwrap();
        let (tx, rx) = crossbeam_channel::unbounded();
        serve(&sock, tx).unwrap();
        send(
            &sock,
            r#"{"event":"SessionStart","payload":{"session_id":"s1"}}"#,
        )
        .unwrap();
        match rx.recv_timeout(Duration::from_secs(2)).unwrap() {
            ExternalEvent::Custom(SessionEvent::Hook { event, payload }) => {
                assert_eq!(event, "SessionStart");
                assert_eq!(payload["session_id"], "s1");
            }
            _ => panic!("unexpected event"),
        }
        assert!(parse_line("nope").is_none());
    }
}
