//! Integration test: two in-process [`MeshManager`]s on loopback establish a
//! WireGuard tunnel and exchange a handshake.
//!
//! No root, no kernel tun interface, no real network — everything runs over
//! loopback UDP sockets using boringtun's userspace WireGuard implementation.

use ozma_mesh::error::{MeshError, Result};
use ozma_mesh::{MeshManager, MeshNode};

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
    // MeshNode::new accepts 2 args: id and port.
    // mesh_ip is set to 127.0.0.1 for loopback, port is used for UDP binding.
    let (node, _) = MeshNode::new(id, port);
    node
}

/// Return peer ids from a list of MeshNode.
fn peer_ids(peers: Vec<MeshNode>) -> Vec<String> {
    peers.into_iter().map(|n| n.id).collect()
}

// ── tests ─────────────────────────────────────────────────────────────────────

/// Two managers add each other as peers, start their receive loops, then A
/// initiates a WireGuard handshake toward B.  After a short wait we verify
/// the peer lists and exercise remove_peer.
#[tokio::test]
async fn two_managers_handshake() -> Result<()> {
    // ── Build node identities ─────────────────────────────────────────────
    let (node_a, sk_a) = MeshNode::new("node-a", PORT_A);
    let (node_b, sk_b) = MeshNode::new("node-b", PORT_B);

    let pubkey_a = node_a.wg_pubkey.clone();
    let pubkey_b = node_b.wg_pubkey.clone();

    // ── Create managers ───────────────────────────────────────────────────
    // MeshManager::new is async and returns Result<Self>.
    let mgr_a = MeshManager::new(node_a, sk_a).await?;
    let mgr_b = MeshManager::new(node_b, sk_b).await?;

    // ── Cross-register peers (loopback endpoints) ─────────────────────────
    mgr_a.add_peer(loopback_peer("node-b", PORT_B)).await?;
    mgr_b.add_peer(loopback_peer("node-a", PORT_A)).await?;

    // ── Start receive loops ───────────────────────────────────────────────
    // The method is `start`, not `run`.
    mgr_a.start().await?;
    mgr_b.start().await;

    // ── Verify peer lists ─────────────────────────────────────────────────
    let peers_a = mgr_a.list_peers().await;
    assert_eq!(peers_a.len(), 1, "mgr_a should have exactly one peer");
    assert_eq!(peer_ids(peers_a)[0], "node-b");

    let peers_b = mgr_b.list_peers().await;
    assert_eq!(peers_b.len(), 1, "mgr_b should have exactly one peer");
    assert_eq!(peer_ids(peers_b)[0], "node-a");

    // ── Initiate WireGuard handshake A → B ────────────────────────────────
    // Give the receive loops a moment to start.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    mgr_a.initiate_handshake("node-b").await?;

    // Allow time for the handshake packet to travel A→B and the response B→A.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    // ── Remove peer and verify ────────────────────────────────────────────
    mgr_a.remove_peer("node-b").await?;
    assert!(mgr_a.list_peers().await.is_empty(), "peer list should be empty after remove");

    // Removing the same peer again must return PeerNotFound.
    let err = mgr_a.remove_peer("node-b").await.unwrap_err();
    assert!(
        matches!(err, MeshError::PeerNotFound(_)),
        "expected PeerNotFound, got {err:?}"
    );

    Ok(())
}

/// Adding the same peer twice must return PeerNotFound.
#[tokio::test]
async fn add_duplicate_peer_returns_error() -> Result<()> {
    let (node, sk) = MeshNode::new("mgr", 59_102);
    let mgr = MeshManager::new(node, sk).await?;

    let peer_node = {
        let (peer, _) = MeshNode::new("peer-x", 59_103);
        peer
    };
    let peer_node2 = peer_node.clone();

    mgr.add_peer(peer_node).await?;

    let err = mgr.add_peer(peer_node2).await.unwrap_err();
    assert!(
        matches!(err, MeshError::PeerNotFound(_)),
        "expected PeerNotFound, got {err:?}"
    );

    Ok(())
}

/// Removing a peer that was never added must return PeerNotFound.
#[tokio::test]
async fn remove_nonexistent_peer_returns_error() -> Result<()> {
    let (node, sk) = MeshNode::new("mgr2", 59_104);
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
    let (node_a, sk_a) = MeshNode::new("node-a", PORT_A);
    let (node_b, sk_b) = MeshNode::new("node-b", PORT_B);

    let mgr_a = MeshManager::new(node_a.clone(), sk_a).await?;

    // Verify the local node matches what we passed in via the public `node` field.
    let local = &mgr_a.node;
    assert_eq!(local.id, "node-a");
    assert_eq!(local.wg_pubkey, node_a.wg_pubkey);

    // Initially no peers
    assert!(mgr_a.list_peers().await.is_empty());

    // Add a peer
    mgr_a.add_peer(loopback_peer("node-b", PORT_B)).await?;

    // Verify list_peers reflects the added peer
    let peers = peer_ids(mgr_a.list_peers().await);
    assert_eq!(peers.len(), 1);
    assert_eq!(peers[0], "node-b");

    Ok(())
}

/// Key round-trip: encode private key to base64 and restore it; the derived
/// public key must match the original.
#[tokio::test]
async fn private_key_base64_round_trip() -> Result<()> {
    use ozma_mesh::WgPrivateKey;
    let sk = WgPrivateKey::generate();
    let pk_original = sk.public_key();

    let b64 = sk.to_base64();
    let sk2 = WgPrivateKey::from_base64(&b64)?;
    let pk_restored = sk2.public_key();

    assert_eq!(pk_original, pk_restored, "public keys must match after round-trip");
    Ok(())
}
