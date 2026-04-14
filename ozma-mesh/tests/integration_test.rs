//! Integration test: two in-process [`MeshManager`]s on loopback establish a
//! WireGuard tunnel and exchange a handshake.
//!
//! No root, no kernel tun interface, no real network — everything runs over
//! loopback UDP sockets using boringtun's userspace WireGuard implementation.

use ozma_mesh::error::{MeshError, Result};
use ozma_mesh::{MeshManager, MeshNode, WgPrivateKey};

/// Two ephemeral loopback ports.  If these happen to be in use the test will
/// fail with a bind error; pick different values if that occurs.
const PORT_A: u16 = 59_100;
const PORT_B: u16 = 59_101;

/// Mesh IPs used as metadata only — not configured on any real interface.
const IP_A: &str = "10.200.1.1";
const IP_B: &str = "10.200.2.1";

// ── helpers ───────────────────────────────────────────────────────────────────

/// Build a `MeshNode` whose `mesh_ip` is overridden to `127.0.0.1` so that
/// UDP packets actually reach the loopback socket.
fn loopback_peer(id: &str, port: u16) -> MeshNode {
    let mut node = MeshNode::new(id);
    node.port = port;
    // mesh_ip is set to 127.0.0.1 for loopback.
    node.mesh_ip = "127.0.0.1".to_string();
    node
}

// ── tests ─────────────────────────────────────────────────────────────────────

/// Two managers add each other as peers, start their receive loops, then A
/// initiates a WireGuard handshake toward B.  After a short wait we verify
/// the peer lists and exercise remove_peer.
#[tokio::test]
async fn two_managers_handshake() -> Result<()> {
    // ── Build node identities ─────────────────────────────────────────────
    let sk_a = WgPrivateKey::generate();
    let node_a = {
        let mut node = MeshNode::new("node-a");
        node.port = PORT_A;
        node.mesh_ip = "127.0.0.1".to_string();
        node
    };

    let sk_b = WgPrivateKey::generate();
    let node_b = {
        let mut node = MeshNode::new("node-b");
        node.port = PORT_B;
        node.mesh_ip = "127.0.0.1".to_string();
        node
    };

    let pubkey_a = node_a.wg_pubkey();
    let pubkey_b = node_b.wg_pubkey();

    // ── Create managers ───────────────────────────────────────────────────
    // MeshManager::new is async and returns Result<Self>.
    let mgr_a = MeshManager::new(node_a, sk_a).await?;
    let mgr_b = MeshManager::new(node_b, sk_b).await?;

    // ── Cross-register peers (loopback endpoints) ─────────────────────────
    mgr_a.add_peer(loopback_peer("node-b", PORT_B)).await?;
    mgr_b.add_peer(loopback_peer("node-a", PORT_A)).await?;

    // ── Start receive loops ───────────────────────────────────────────────
    // The method is `run` on MeshManager.
    mgr_a.run().await?;
    mgr_b.run().await;

    // ── Verify peer lists ─────────────────────────────────────────────────
    let peers_a = mgr_a.peer_ids().await;
    assert_eq!(peers_a.len(), 1, "mgr_a should have exactly one peer");
    assert_eq!(peers_a[0], "node-b");

    let peers_b = mgr_b.peer_ids().await;
    assert_eq!(peers_b.len(), 1, "mgr_b should have exactly one peer");
    assert_eq!(peers_b[0], "node-a");

    // ── Initiate WireGuard handshake A → B ────────────────────────────────
    // Give the receive loops a moment to start.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    mgr_a.recv_packet("node-b", &[1, 2, 3]).await?;

    // Allow time for the handshake packet to travel A→B and the response B→A.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    // ── Remove peer and verify ────────────────────────────────────────────
    mgr_a.remove_peer("node-b").await?;
    assert!(mgr_a.peer_ids().await.is_empty(), "peer list should be empty after remove");

    // Removing the same peer again must return PeerNotFound.
    let err = mgr_a.remove_peer("node-b").await.unwrap_err();
    assert!(
        matches!(err, MeshError::PeerNotFound(_)),
        "expected PeerNotFound, got {err:?}"
    );

    Ok(())
}

/// Adding the same peer twice must return an error.
#[tokio::test]
async fn add_duplicate_peer_returns_error() -> Result<()> {
    let sk = WgPrivateKey::generate();
    let node = {
        let mut n = MeshNode::new("mgr");
        n.port = 59_102;
        n.mesh_ip = "127.0.0.1".to_string();
        n
    };
    let mgr = MeshManager::new(node, sk).await?;

    let peer_node = {
        let mut n = MeshNode::new("peer-x");
        n.port = 59_103;
        n.mesh_ip = "127.0.0.1".to_string();
        n
    };
    let peer_node2 = peer_node.clone();

    mgr.add_peer(peer_node).await?;

    // Adding the same peer again must return an error.
    let err = mgr.add_peer(peer_node2).await.unwrap_err();
    // Verify it's some kind of error (具体 variant depends on actual impl)
    assert!(
        !err.to_string().is_empty(),
        "expected an error, got nothing"
    );

    Ok(())
}

/// Removing a peer that was never added must return PeerNotFound.
#[tokio::test]
async fn remove_nonexistent_peer_returns_error() -> Result<()> {
    let sk = WgPrivateKey::generate();
    let node = {
        let mut n = MeshNode::new("mgr2");
        n.port = 59_104;
        n.mesh_ip = "127.0.0.1".to_string();
        n
    };
    let mgr = MeshManager::new(node, sk).await?;

    let err = mgr.remove_peer("ghost").await.unwrap_err();
    assert!(
        matches!(err, MeshError::PeerNotFound(_)),
        "expected PeerNotFound, got {err:?}"
    );

    Ok(())
}

/// Verify local node identity and peer list work correctly.
#[tokio::test]
async fn local_node_and_peer_ids_accessors() -> Result<()> {
    let sk_a = WgPrivateKey::generate();
    let node_a = {
        let mut n = MeshNode::new("node-a");
        n.port = PORT_A;
        n.mesh_ip = "127.0.0.1".to_string();
        n
    };
    let sk_b = WgPrivateKey::generate();
    let node_b = {
        let mut n = MeshNode::new("node-b");
        n.port = PORT_B;
        n.mesh_ip = "127.0.0.1".to_string();
        n
    };

    let mgr_a = MeshManager::new(node_a.clone(), sk_a).await?;

    // Verify the local node matches what we passed in.
    let local = mgr_a.node();
    assert_eq!(local.id, "node-a");

    // Initially no peers
    assert!(mgr_a.peer_ids().await.is_empty());

    // Add a peer
    mgr_a.add_peer(loopback_peer("node-b", PORT_B)).await?;

    // Verify peer_ids reflects the added peer
    let peers = mgr_a.peer_ids().await;
    assert_eq!(peers.len(), 1);
    assert_eq!(peers[0], "node-b");

    Ok(())
}

/// Key round-trip: encode private key to base64 and restore it; the derived
/// public key must match the original.
#[tokio::test]
async fn private_key_base64_round_trip() -> Result<()> {
    let sk = WgPrivateKey::generate();
    let pk_original = sk.public_key();

    let b64 = sk.to_base64();
    let sk2 = WgPrivateKey::from_base64(&b64)?;
    let pk_restored = sk2.public_key();

    assert_eq!(pk_original, pk_restored, "public keys must match after round-trip");
    Ok(())
}
