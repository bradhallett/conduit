//! `--stdio-adapter`: the stdio face of the host daemon (one-gateway-per-host P2.2c).
//!
//! The adapter owns no registry, router, or downstream connections. It renders the
//! host daemon's Streamable HTTP MCP endpoint as a stdio MCP server: every JSON-RPC
//! message the client writes to stdin is POSTed to the daemon's `/mcp`, and every
//! message the daemon sends back (a response body, an SSE frame on the POST reply,
//! or a frame on the long-lived `GET /mcp` listen stream) is written to stdout.
//!
//! It never falls back to an in-process gateway: this role has no gateway to fall
//! back to. A transport failure becomes a JSON-RPC error to the client, and the
//! default stdio role stays the existing in-process gateway (the rollback is the
//! flag, not a code path).

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::daemon::{DaemonDescriptor, Rendezvous};
use crate::registry;
use crate::topology::CompatKey;

/// The flag that selects the adapter role instead of the in-process gateway.
pub const STDIO_ADAPTER_FLAG: &str = "--stdio-adapter";
/// Same per-frame bound the in-process stdio gateway applies to one client frame.
pub const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;
/// A single request may legitimately run long (a slow downstream call), so the
/// HTTP budget is generous; the listen stream is separate and reconnects.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(600);
/// How long to wait for the daemon to publish a session id before opening the
/// server-initiated listen stream.
const LISTEN_POLL: Duration = Duration::from_millis(100);
/// Backoff between listen-stream reconnects.
const LISTEN_RECONNECT: Duration = Duration::from_millis(500);

/// Whether the command line asked for the adapter role. Kept beside the flag so
/// the help text and the parser cannot disagree.
pub fn adapter_requested(args: &[String]) -> bool {
    args.iter().any(|arg| arg == STDIO_ADAPTER_FLAG)
}

/// Run the adapter: rendezvous with (or start) the host daemon, then proxy stdio
/// to it until the client closes stdin. Diverges: the process exit code is the
/// adapter's result.
pub fn run_stdio_adapter() -> ! {
    let Some(dir) = registry::conduit_dir() else {
        eprintln!("toolport-gateway {STDIO_ADAPTER_FLAG}: no data directory could be resolved");
        std::process::exit(1);
    };
    let compat = CompatKey::new(env!("CARGO_PKG_VERSION"), dir.display().to_string());
    let descriptor = match Rendezvous::new(&dir, compat).ensure(spawn_daemon) {
        Ok(descriptor) => descriptor,
        Err(error) => {
            eprintln!("toolport-gateway {STDIO_ADAPTER_FLAG}: {error}");
            std::process::exit(1);
        }
    };
    match proxy_stdio(&descriptor) {
        Ok(()) => std::process::exit(0),
        Err(error) => {
            eprintln!("toolport-gateway {STDIO_ADAPTER_FLAG}: {error}");
            std::process::exit(1);
        }
    }
}

/// Start the host daemon as a detached sibling. A new process group keeps the
/// daemon alive when the client tears down the adapter's group, so the next
/// adapter finds it through the rendezvous instead of paying a cold start.
fn spawn_daemon() -> Result<(), String> {
    let exe = std::env::current_exe()
        .map_err(|error| format!("could not locate this executable: {error}"))?;
    let mut command = Command::new(exe);
    command
        .arg("--daemon")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    command
        .spawn()
        .map(|_child| ())
        .map_err(|error| format!("could not start the host daemon: {error}"))
}

/// Shared adapter state: the daemon it talks to, the negotiated session id, and
/// the one stdout both the request loop and the listen stream may write to.
struct Session {
    descriptor: DaemonDescriptor,
    session_id: Mutex<Option<String>>,
    stdout: Mutex<std::io::Stdout>,
}

impl Session {
    fn new(descriptor: &DaemonDescriptor) -> Self {
        Self {
            descriptor: descriptor.clone(),
            session_id: Mutex::new(None),
            stdout: Mutex::new(std::io::stdout()),
        }
    }

    fn session_id(&self) -> Option<String> {
        self.session_id.lock().ok().and_then(|guard| guard.clone())
    }

    /// Write one already-serialized JSON-RPC message to stdout. The newline is the
    /// frame boundary a stdio MCP client reads.
    fn write_message(&self, message: &str) -> Result<(), String> {
        let mut out = self
            .stdout
            .lock()
            .map_err(|_| "stdout lock poisoned".to_string())?;
        writeln!(out, "{message}").map_err(|error| error.to_string())?;
        out.flush().map_err(|error| error.to_string())
    }

    /// POST one message to `/mcp` and forward every JSON-RPC frame the daemon
    /// answers with. `202 Accepted` (a notification) has no body to forward.
    fn exchange(&self, body: &str) -> Result<(), String> {
        let url = format!("http://{}/mcp", self.descriptor.endpoint);
        let mut request = ureq::post(&url)
            .set(
                "Authorization",
                &format!("Bearer {}", self.descriptor.token),
            )
            .set("Content-Type", "application/json")
            .set("Accept", "application/json, text/event-stream")
            .timeout(REQUEST_TIMEOUT);
        if let Some(session) = self.session_id() {
            request = request.set("Mcp-Session-Id", &session);
        }
        let response = request
            .send_string(body)
            .map_err(|error| error.to_string())?;
        if let Some(session) = response.header("Mcp-Session-Id") {
            if let Ok(mut guard) = self.session_id.lock() {
                *guard = Some(session.to_string());
            }
        }
        for message in response_frames(response)? {
            self.write_message(&message)?;
        }
        Ok(())
    }

    /// Close the daemon-side session on client EOF so its per-session state is
    /// released immediately rather than waiting for a lease TTL.
    fn close(&self) {
        let Some(session) = self.session_id() else {
            return;
        };
        let url = format!("http://{}/mcp", self.descriptor.endpoint);
        let _ = ureq::delete(&url)
            .set(
                "Authorization",
                &format!("Bearer {}", self.descriptor.token),
            )
            .set("Mcp-Session-Id", &session)
            .timeout(Duration::from_secs(5))
            .call();
    }
}

/// Read a whole request response into the JSON-RPC frames to forward. A JSON body
/// is one frame; an SSE body may carry several (progress, then the result), each
/// of which is written on its own line.
fn response_frames(response: ureq::Response) -> Result<Vec<String>, String> {
    let is_sse = response
        .header("Content-Type")
        .unwrap_or_default()
        .to_ascii_lowercase()
        .contains("text/event-stream");
    let body = response.into_string().map_err(|error| error.to_string())?;
    if !is_sse {
        let trimmed = body.trim();
        return Ok(if trimmed.is_empty() {
            Vec::new()
        } else {
            vec![trimmed.to_string()]
        });
    }
    Ok(body
        .lines()
        .filter_map(|line| line.strip_prefix("data:"))
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect())
}

/// Read from stdin and proxy to the daemon until EOF. Each message is sent
/// synchronously; a failed send becomes a JSON-RPC error for that request rather
/// than a silent drop or a fallback.
fn proxy_stdio(descriptor: &DaemonDescriptor) -> Result<(), String> {
    let session = Arc::new(Session::new(descriptor));
    spawn_listen_stream(Arc::clone(&session));

    let stdin = std::io::stdin();
    let mut reader = BufReader::new(stdin.lock());
    while let Some(frame) = read_bounded_line(&mut reader, MAX_FRAME_BYTES)? {
        let line = match frame {
            ClientFrame::Oversized => {
                write_error(
                    &session,
                    serde_json::Value::Null,
                    -32600,
                    "request frame exceeds the 16 MiB limit",
                );
                continue;
            }
            ClientFrame::Line(line) => line,
        };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let request: serde_json::Value = match serde_json::from_str(trimmed) {
            Ok(value) => value,
            Err(_) => {
                write_error(&session, serde_json::Value::Null, -32700, "parse error");
                continue;
            }
        };
        if let Err(error) = session.exchange(trimmed) {
            report_request_error(&session, &request, &error);
        }
    }
    session.close();
    Ok(())
}

/// Surface a failed daemon exchange to the client. A request with an id gets a
/// JSON-RPC error carrying it; a notification has nowhere to answer, so it is
/// logged. Either way the transport problem is stated, never masked by a local
/// retry.
fn report_request_error(session: &Session, request: &serde_json::Value, error: &str) {
    let message = format!("host daemon request failed: {error}");
    match request.get("id") {
        Some(id) if !id.is_null() => write_error(session, id.clone(), -32603, &message),
        _ => {
            eprintln!("toolport-gateway {STDIO_ADAPTER_FLAG}: notification failed: {error}");
        }
    }
}

/// Open the daemon's long-lived `GET /mcp` SSE stream and forward server-initiated
/// messages to the client, reconnecting if it drops. Frames are POSTed back by the
/// client through the normal stdin path, so no correlation table is needed here:
/// the daemon matches the response to its own outstanding request id.
fn spawn_listen_stream(session: Arc<Session>) {
    std::thread::spawn(move || loop {
        let Some(session_id) = session.session_id() else {
            std::thread::sleep(LISTEN_POLL);
            continue;
        };
        let url = format!("http://{}/mcp", session.descriptor.endpoint);
        let response = ureq::get(&url)
            .set(
                "Authorization",
                &format!("Bearer {}", session.descriptor.token),
            )
            .set("Accept", "text/event-stream")
            .set("Mcp-Session-Id", &session_id)
            .timeout(Duration::from_secs(3600))
            .call();
        match response {
            Ok(response) => {
                let mut reader = BufReader::new(response.into_reader());
                while let Ok(Some(ClientFrame::Line(line))) =
                    read_bounded_line(&mut reader, MAX_FRAME_BYTES)
                {
                    if let Some(data) = line.strip_prefix("data:") {
                        let data = data.trim();
                        if !data.is_empty() {
                            let _ = session.write_message(data);
                        }
                    }
                }
            }
            Err(_) => std::thread::sleep(LISTEN_RECONNECT),
        }
    });
}

/// One frame read from the client.
#[derive(Debug, PartialEq, Eq)]
enum ClientFrame {
    /// A complete newline-delimited frame.
    Line(String),
    /// A frame that exceeded the cap; its bytes were drained and dropped.
    Oversized,
}

/// Write a JSON-RPC error to the client.
fn write_error(session: &Session, id: serde_json::Value, code: i64, message: &str) {
    let response = serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message }
    });
    let _ = session.write_message(&response.to_string());
}

/// Read one newline-delimited frame, bounded so a client cannot make the adapter
/// allocate without limit. `None` is EOF.
fn read_bounded_line<R: BufRead>(
    reader: &mut R,
    max_bytes: usize,
) -> Result<Option<ClientFrame>, String> {
    let mut buf = Vec::new();
    loop {
        let mut byte = [0u8; 1];
        match reader.read(&mut byte) {
            Ok(0) => {
                return Ok(if buf.is_empty() {
                    None
                } else {
                    Some(ClientFrame::Line(
                        String::from_utf8_lossy(&buf).into_owned(),
                    ))
                });
            }
            Ok(_) => {
                if byte[0] == b'\n' {
                    return Ok(Some(ClientFrame::Line(
                        String::from_utf8_lossy(&buf).into_owned(),
                    )));
                }
                if buf.len() >= max_bytes {
                    // Drain the rest of this oversized frame so the next read starts
                    // at the next newline, then report it rather than dropping it as
                    // if it were an empty line.
                    loop {
                        let mut discard = [0u8; 1];
                        match reader.read(&mut discard) {
                            Ok(0) | Err(_) => break,
                            Ok(_) if discard[0] == b'\n' => break,
                            Ok(_) => {}
                        }
                    }
                    return Ok(Some(ClientFrame::Oversized));
                }
                buf.push(byte[0]);
            }
            Err(error) => return Err(error.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_flag_selects_the_adapter_role() {
        assert!(adapter_requested(&["--stdio-adapter".to_string()]));
        assert!(adapter_requested(&[
            "--http".to_string(),
            "--stdio-adapter".to_string()
        ]));
        assert!(!adapter_requested(&["--daemon".to_string()]));
        assert!(!adapter_requested(&[]));
    }

    #[test]
    fn a_bounded_reader_splits_lines_and_reports_eof() {
        let mut reader = std::io::BufReader::new(&b"one\ntwo\n"[..]);
        assert_eq!(
            read_bounded_line(&mut reader, 16).unwrap(),
            Some(ClientFrame::Line("one".to_string()))
        );
        assert_eq!(
            read_bounded_line(&mut reader, 16).unwrap(),
            Some(ClientFrame::Line("two".to_string()))
        );
        assert_eq!(read_bounded_line(&mut reader, 16).unwrap(), None);
    }

    #[test]
    fn an_oversized_frame_is_reported_and_drained() {
        let mut reader = std::io::BufReader::new(&b"aaaaaaaaaaaa\nok\n"[..]);
        assert_eq!(
            read_bounded_line(&mut reader, 4).unwrap(),
            Some(ClientFrame::Oversized)
        );
        assert_eq!(
            read_bounded_line(&mut reader, 4).unwrap(),
            Some(ClientFrame::Line("ok".to_string()))
        );
    }
}
