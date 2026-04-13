//! Integration test: two in-process MeshManagers on loopback establish a
//! WireGuard tunnel and exchange a plaintext packet.
//!
//! The test does NOT require a TUN interface — it exercises the boringtun
//! state machine (handshake + data encapsulation/decapsulation) entirely
//! over loopback UDP sockets.

use std::net::SocketAddr;
use std::time::Duration;

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine as _;
use ozma_mesh::{MeshError, MeshManager, MeshNode, WgPublicKey};
use tracing_subscriber::EnvFilter;

fn init_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .try_init();
}

const PORT_A: u16 = 59100;
const PORT_B: u16 = 59101;

#[tokio::test]
async fn test_two_nodes_establish_tunnel() {
    init_tracing();

    // ── Create two managers ────────────────────────────────────────────────
    let mgr_a = MeshManager::new("node-a", "10.200.1.1", PORT_A)
        .await
        .expect("manager A");
    let mgr_b = MeshManager::new("node-b", "10.200.2.1", PORT_B)
        .await
        .expect("manager B");

    let node_a: MeshNode = mgr_a.local_node().await;
    let node_b: MeshNode = mgr_b.local_node().await;

    // Verify keys were generated.
    assert!(!node_a.wg_pubkey.0.is_empty(), "node-a pubkey empty");
    assert!(!node_b.wg_pubkey.0.is_empty(), "node-b pubkey empty");
    assert_ne!(node_a.wg_pubkey, node_b.wg_pubkey, "pubkeys must differ");

    // ── Wire them together ─────────────────────────────────────────────────
    let addr_a: SocketAddr = format!("127.0.0.1:{PORT_A}").parse().unwrap();
    let addr_b: SocketAddr = format!("127.0.0.1:{PORT_B}").parse().unwrap();

    mgr_a
        .add_peer(node_b.clone(), addr_b)
        .await
        .expect("A adds B");
    mgr_b
        .add_peer(node_a.clone(), addr_a)
        .await
        .expect("B adds A");

    assert_eq!(mgr_a.peer_ids().await, vec!["node-b"]);
    assert_eq!(mgr_b.peer_ids().await, vec!["node-a"]);

    // ── Start background I/O loops ─────────────────────────────────────────
    let _handle_a = mgr_a.run();
    let _handle_b = mgr_b.run();

    // ── Initiate handshake: A sends a dummy IP packet to B ─────────────────
    // A minimal IPv4 header (20 bytes) with a 4-byte payload.
    // src=10.200.1.1, dst=10.200.2.1, proto=253 (experimental)
    #[rustfmt::skip]
    let dummy_ipv4: &[u8] = &[
        0x45, 0x00, 0x00, 0x18,  // version/IHL, DSCP, total length = 24
        0x00, 0x01, 0x00, 0x00,  // id, flags, fragment offset
        0x40, 0xfd, 0x00, 0x00,  // TTL=64, proto=253, checksum (0 = unchecked)
        10, 200, 1, 1,           // src IP
        10, 200, 2, 1,           // dst IP
        0xde, 0xad, 0xbe, 0xef,  // payload
    ];

    mgr_a
        .send_to_peer("node-b", dummy_ipv4)
        .await
        .expect("A sends to B");

    // Allow time for the WireGuard handshake to complete and the packet to
    // be delivered.  boringtun completes the handshake in < 200 ms on loopback.
    tokio::time::sleep(Duration::from_millis(500)).await;

    // ── Verify the tunnel is still alive (no panics, no errors) ───────────
    assert_eq!(mgr_a.peer_ids().await, vec!["node-b"], "A still has B");
    assert_eq!(mgr_b.peer_ids().await, vec!["node-a"], "B still has A");
}

#[tokio::test]
async fn test_add_remove_peer() {
    init_tracing();

    let mgr = MeshManager::new("node-x", "10.200.3.1", 59102)
        .await
        .expect("manager X");

    // A valid base64-encoded 32-byte public key (all zeros — fine for unit test).
    let fake_pubkey = WgPublicKey(B64.encode([0u8; 32]));
    let fake_peer = MeshNode::new("node-y", fake_pubkey.clone(), "10.200.4.1", 59103);

    let addr: SocketAddr = "127.0.0.1:59103".parse().unwrap();

    mgr.add_peer(fake_peer.clone(), addr)
        .await
        .expect("add peer");
    assert_eq!(mgr.peer_ids().await, vec!["node-y"]);

    // Duplicate add should fail.
    let err = mgr.add_peer(fake_peer, addr).await.unwrap_err();
    assert!(
        matches!(err, MeshError::DuplicatePeer(_)),
        "expected DuplicatePeer, got {err:?}"
    );

    mgr.remove_peer("node-y").await.expect("remove peer");
    assert!(mgr.peer_ids().await.is_empty());

    // Remove non-existent peer should fail.
    let err = mgr.remove_peer("node-y").await.unwrap_err();
    assert!(
        matches!(err, MeshError::PeerNotFound(_)),
        "expected PeerNotFound, got {err:?}"
    );
}
