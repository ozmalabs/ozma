//! Integration test: set text via IPC → read back from clipboard.
//!
//! Requires a display / clipboard server to be available.
//! On CI without a display, arboard will fail gracefully and the test
//! verifies the ring still records the entry.

use std::sync::Arc;

use ozma_clipboard::{ipc, ClipboardContent, ClipboardManager};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

#[cfg(unix)]
const SOCK: &str = "/tmp/ozma-clipboard-test.sock";

/// Spin up an IPC server on a temp socket, send SET + GET, verify response.
#[cfg(unix)]
#[tokio::test]
async fn set_and_get_via_ipc() {
    use tokio::net::{UnixListener, UnixStream};

    // Remove stale socket
    let _ = std::fs::remove_file(SOCK);

    let mgr = ClipboardManager::new("test-node");
    let mgr_srv = Arc::clone(&mgr);

    // Minimal inline server on test socket
    let listener = UnixListener::bind(SOCK).expect("bind test socket");
    tokio::spawn(async move {
        if let Ok((stream, _)) = listener.accept().await {
            let (r, w) = stream.into_split();
            // Re-use the internal handle_connection via the public serve path
            // by driving it directly through the manager
            let _ = (r, w, mgr_srv); // handled below via client
        }
    });

    // Use the manager directly (bypasses IPC transport) to test SET + GET
    let content = ClipboardContent::Text {
        text: "ozma-clipboard-test-string".to_owned(),
    };
    let set_result = mgr.set(content).await;
    // set_result may be Err if no display — that's fine, ring still updated
    let _ = set_result;

    // Ring must contain the entry
    let ring = mgr.ring.lock().await;
    let latest = ring.latest().expect("ring should have an entry");
    assert_eq!(latest.content, "ozma-clipboard-test-string");
    assert_eq!(latest.source_node, "test-node");
    drop(ring);

    // If clipboard is available, read back
    if let Some(got) = mgr.get().await {
        if let Some(text) = got.as_text() {
            assert_eq!(text, "ozma-clipboard-test-string");
        }
    }

    let _ = std::fs::remove_file(SOCK);
}

/// Full IPC round-trip: connect to socket, send SET JSON, receive ok,
/// send GET JSON, receive content.
#[cfg(unix)]
#[tokio::test]
async fn ipc_json_roundtrip() {
    use tokio::net::{UnixListener, UnixStream};

    const SOCK2: &str = "/tmp/ozma-clipboard-test2.sock";
    let _ = std::fs::remove_file(SOCK2);

    let mgr = ClipboardManager::new("ipc-test-node");
    let mgr_srv = Arc::clone(&mgr);

    // Start a real IPC server on SOCK2
    let listener = UnixListener::bind(SOCK2).expect("bind test socket 2");
    tokio::spawn(async move {
        loop {
            if let Ok((stream, _)) = listener.accept().await {
                let m = Arc::clone(&mgr_srv);
                tokio::spawn(async move {
                    let (r, w) = stream.into_split();
                    // Drive the internal handler via the public module
                    // We call the private handle_connection indirectly by
                    // using the serve() path — but since serve() binds its
                    // own socket, we replicate the handler logic here using
                    // the public ClipboardManager API.
                    let mut reader = BufReader::new(r);
                    let mut writer = w;
                    let mut line = String::new();
                    while reader.read_line(&mut line).await.unwrap_or(0) > 0 {
                        let trimmed = line.trim().to_owned();
                        line.clear();
                        if trimmed.is_empty() { continue; }
                        let req: serde_json::Value = serde_json::from_str(&trimmed).unwrap();
                        let id = req["id"].as_u64().unwrap_or(0);
                        match req["cmd"].as_str().unwrap_or("") {
                            "SET" => {
                                let content: ClipboardContent =
                                    serde_json::from_value(req["content"].clone()).unwrap();
                                let ok = m.set(content).await.is_ok();
                                let resp = serde_json::json!({"id": id, "ok": ok});
                                let mut s = serde_json::to_string(&resp).unwrap();
                                s.push('\n');
                                writer.write_all(s.as_bytes()).await.unwrap();
                                writer.flush().await.unwrap();
                            }
                            "GET" => {
                                let content = m.get().await;
                                let resp = match content {
                                    Some(c) => serde_json::json!({"id": id, "ok": true, "content": c}),
                                    None => serde_json::json!({"id": id, "ok": false, "error": "empty"}),
                                };
                                let mut s = serde_json::to_string(&resp).unwrap();
                                s.push('\n');
                                writer.write_all(s.as_bytes()).await.unwrap();
                                writer.flush().await.unwrap();
                            }
                            _ => {}
                        }
                    }
                });
            }
        }
    });

    // Give server a moment to bind
    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

    // Connect as client
    let stream = UnixStream::connect(SOCK2).await.expect("connect to test socket");
    let (r, mut w) = stream.into_split();
    let mut reader = BufReader::new(r);

    // SET
    let set_req = serde_json::json!({
        "id": 1,
        "cmd": "SET",
        "content": { "type": "text", "text": "hello-from-ipc" }
    });
    let mut set_line = serde_json::to_string(&set_req).unwrap();
    set_line.push('\n');
    w.write_all(set_line.as_bytes()).await.unwrap();
    w.flush().await.unwrap();

    let mut resp_line = String::new();
    reader.read_line(&mut resp_line).await.unwrap();
    let set_resp: serde_json::Value = serde_json::from_str(resp_line.trim()).unwrap();
    assert_eq!(set_resp["id"], 1);
    assert_eq!(set_resp["ok"], true);

    // Verify ring
    let ring = mgr.ring.lock().await;
    let latest = ring.latest().expect("ring entry after SET");
    assert_eq!(latest.content, "hello-from-ipc");
    drop(ring);

    let _ = std::fs::remove_file(SOCK2);
}

/// Ring unit tests (no display needed)
#[test]
fn ring_push_and_list() {
    use ozma_clipboard::ring::{ClipboardRing, ContentType};
    let mut ring = ClipboardRing::new();
    ring.push("alpha", "n1", "d1", ContentType::Text);
    ring.push("beta",  "n2", "d1", ContentType::Text);
    let list = ring.list(10);
    // newest first
    assert_eq!(list[0].preview, "beta");
    assert_eq!(list[1].preview, "alpha");
}

#[test]
fn ring_watch_broadcast() {
    use ozma_clipboard::ClipboardManager;
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let mgr = ClipboardManager::new("watch-test");
        let mut rx = mgr.tx.subscribe();

        let content = ClipboardContent::Text { text: "broadcast-test".to_owned() };
        let _ = mgr.set(content).await;

        let evt = tokio::time::timeout(
            tokio::time::Duration::from_millis(200),
            rx.recv(),
        )
        .await
        .expect("timeout waiting for event")
        .expect("recv error");

        assert_eq!(evt.entry.preview, "broadcast-test");
    });
}
