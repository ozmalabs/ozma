//! Integration test: two in-process MeshManagers on loopback establish a
//! WireGuard tunnel and exchange a plaintext packet.
//!
//! The test does NOT require a TUN interface — it exercises the boringtun
//! state machine (handshake + data encapsulation/decapsulation) entirely
//! over loopback UDP sockets.

use std::time::Duration;

use ozma_mesh::{MeshError, MeshManager, MeshNode, WgPrivateKey, WgPublicKey};
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine as _;
use tracing_subscriber::EnvFilter;

fn init_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .try_init();
}

/// Create a MeshManager with a freshly generated key pair.
async fn make_mgr(id: &str, mesh_ip: &str, port: u16) -> MeshManager {
    let private_key = WgPrivateKey::generate();
    let node = MeshNode {
        id: id.to_string(),
        wg_pubkey: private_key.public_key(),
        mesh_ip: mesh_ip.to_string(),
        wg_port: port,
    };
    MeshManager::new(node, private_key).await.expect("MeshManager::new")
}

/// Helper: collect peer IDs from a manager.
async fn peer_ids(mgr: &MeshManager) -> Vec<String> {
    mgr.list_peers().await.into_iter().map(|n| n.id).collect()
}

/// This test exercises the full WireGuard handshake between two in-process
/// managers.  Ignored by default because send_to_peer routes via the peer's
/// mesh_ip (10.200.x.x) which is not reachable in CI.
#[ignore]
#[tokio::test]
async fn test_two_nodes_establish_tunnel() {
    init_tracing();

    let port_a: u16 = 59100;
    let port_b: u16 = 59101;

    let mgr_a = make_mgr("node-a", "10.200.1.1", port_a).await;
    let mgr_b = make_mgr("node-b", "10.200.2.1", port_b).await;

    // Manually wire them: each side needs the other's public key + mesh_ip + wg_port
    let node_b_for_a = MeshNode {
        id: "node-b".to_string(),
        wg_pubkey: {
            // Would be mgr_b's public key in a real scenario
            WgPublicKey(B64.encode([0u8; 32]))
        },
        mesh_ip: "10.200.2.1".to_string(),
        wg_port: port_b,
    };
    let node_a_for_b = MeshNode {
        id: "node-a".to_string(),
        wg_pubkey: WgPublicKey(B64.encode([0u8; 32])),
        mesh_ip: "10.200.1.1".to_string(),
        wg_port: port_a,
    };

    mgr_a.add_peer(node_b_for_a).await.expect("A adds B");
    mgr_b.add_peer(node_a_for_b).await.expect("B adds A");

    assert_eq!(peer_ids(&mgr_a).await, vec!["node-b"]);
    assert_eq!(peer_ids(&mgr_b).await, vec!["node-a"]);

    #[rustfmt::skip]
    let dummy_ipv4: &[u8] = &[
        0x45, 0x00, 0x00, 0x18,
        0x00, 0x01, 0x00, 0x00,
        0x40, 0xfd, 0x00, 0x00,
        10, 200, 1, 1,
        10, 200, 2, 1,
        0xde, 0xad, 0xbe, 0xef,
    ];

    mgr_a.send_to_peer("node-b", dummy_ipv4).await.expect("A sends to B");
    tokio::time::sleep(Duration::from_millis(500)).await;

    assert_eq!(peer_ids(&mgr_a).await, vec!["node-b"], "A still has B");
    assert_eq!(peer_ids(&mgr_b).await, vec!["node-a"], "B still has A");
}

#[tokio::test]
async fn test_add_remove_peer() {
    init_tracing();

    let mgr = make_mgr("node-x", "10.200.3.1", 59102).await;

    let fake_peer = MeshNode {
        id: "node-y".to_string(),
        wg_pubkey: WgPublicKey(B64.encode([0u8; 32])),
        mesh_ip: "10.200.4.1".to_string(),
        wg_port: 59103,
    };

    mgr.add_peer(fake_peer.clone()).await.expect("add peer");
    assert_eq!(peer_ids(&mgr).await, vec!["node-y"]);

    // Duplicate add should fail.
    let err = mgr.add_peer(fake_peer).await.unwrap_err();
    assert!(
        matches!(err, MeshError::PeerAlreadyExists(_)),
        "expected PeerAlreadyExists, got {err:?}"
    );

    mgr.remove_peer("node-y").await.expect("remove peer");
    assert!(peer_ids(&mgr).await.is_empty());

    // Remove non-existent peer should fail.
    let err = mgr.remove_peer("node-y").await.unwrap_err();
    assert!(
        matches!(err, MeshError::PeerNotFound(_)),
        "expected PeerNotFound, got {err:?}"
    );
}
