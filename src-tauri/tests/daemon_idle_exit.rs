//! End-to-end guard for the host daemon idle exit (one-gateway-per-host P2.3).
//!
//! Starts `toolport-gateway --daemon` with a short idle grace against a scratch
//! data directory. A daemon with nothing connected exits on its own and clears its
//! descriptor; a daemon holding a live MCP session does not until that session is
//! deleted. The unit tests cover the rendezvous; this is the only test that watches
//! the process actually leave.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn scratch_dir() -> PathBuf {
    std::env::temp_dir().join(format!(
        "toolport-daemon-idle-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ))
}

/// Read the rendezvous descriptor the daemon publishes into the data directory.
fn read_descriptor(dir: &Path) -> Option<serde_json::Value> {
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with("daemon-") && name.ends_with(".json") {
            let raw = std::fs::read_to_string(entry.path()).ok()?;
            if let Ok(value) = serde_json::from_str(&raw) {
                return Some(value);
            }
        }
    }
    None
}

fn spawn_daemon(dir: &Path, grace_ms: u64) -> ChildGuard {
    let child = Command::new(env!("CARGO_BIN_EXE_toolport-gateway"))
        .arg("--daemon")
        .env("TOOLPORT_DATA_DIR", dir)
        .env("TOOLPORT_REGISTRY", dir.join("registry.json"))
        .env("TOOLPORT_DAEMON_IDLE_GRACE_MS", grace_ms.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn the daemon");
    ChildGuard(child)
}

fn wait_for_descriptor(child: &mut ChildGuard, dir: &Path) -> serde_json::Value {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if let Some(value) = read_descriptor(dir) {
            return value;
        }
        assert!(
            child.0.try_wait().expect("daemon status").is_none(),
            "daemon exited before publishing a descriptor"
        );
        assert!(
            Instant::now() < deadline,
            "no descriptor within the deadline"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn wait_for_exit(child: &mut ChildGuard, within: Duration) -> ExitStatus {
    let deadline = Instant::now() + within;
    loop {
        if let Some(status) = child.0.try_wait().expect("daemon status") {
            return status;
        }
        assert!(
            Instant::now() < deadline,
            "the daemon did not exit within the deadline"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn post_mcp(
    endpoint: &str,
    token: &str,
    session: Option<&str>,
    body: &serde_json::Value,
) -> ureq::Response {
    let mut request = ureq::post(&format!("http://{endpoint}/mcp"))
        .set("Authorization", &format!("Bearer {token}"))
        .set("Content-Type", "application/json")
        .set("Accept", "application/json, text/event-stream")
        .timeout(Duration::from_secs(20));
    if let Some(session) = session {
        request = request.set("Mcp-Session-Id", session);
    }
    request.send_json(body.clone()).expect("MCP request")
}

#[test]
fn an_idle_daemon_exits_and_clears_its_descriptor() {
    let dir = scratch_dir();
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create data dir");
    let mut child = spawn_daemon(&dir, 300);

    wait_for_descriptor(&mut child, &dir);
    let status = wait_for_exit(&mut child, Duration::from_secs(30));
    assert!(status.success(), "unexpected daemon exit: {status:?}");
    assert!(
        read_descriptor(&dir).is_none(),
        "the descriptor outlived the daemon"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_daemon_with_a_live_session_does_not_exit_until_it_is_deleted() {
    let dir = scratch_dir();
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create data dir");
    let mut child = spawn_daemon(&dir, 300);

    let descriptor = wait_for_descriptor(&mut child, &dir);
    let endpoint = descriptor["endpoint"]
        .as_str()
        .expect("endpoint")
        .to_string();
    let token = descriptor["token"].as_str().expect("token").to_string();

    let initialize = post_mcp(
        &endpoint,
        &token,
        None,
        &serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": { "name": "daemon-idle-test", "version": "1" }
            }
        }),
    );
    let session = initialize
        .header("Mcp-Session-Id")
        .map(str::to_string)
        .expect("a session id");

    // Well past the grace: the live session holds the daemon open.
    std::thread::sleep(Duration::from_millis(1200));
    assert!(
        child.0.try_wait().expect("daemon status").is_none(),
        "the daemon exited while a session was live"
    );

    // Deleting the session leaves nothing to keep it alive.
    ureq::delete(&format!("http://{endpoint}/mcp"))
        .set("Authorization", &format!("Bearer {token}"))
        .set("Mcp-Session-Id", &session)
        .timeout(Duration::from_secs(10))
        .call()
        .expect("delete the session");

    let status = wait_for_exit(&mut child, Duration::from_secs(30));
    assert!(status.success(), "unexpected daemon exit: {status:?}");
    let _ = std::fs::remove_dir_all(&dir);
}
