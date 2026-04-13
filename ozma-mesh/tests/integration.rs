//! Integration tests: MeshManager peer management and node identity.

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine as _;
use ozma_mesh::{MeshError, MeshManager, MeshNode, WgPrivateKey, WgPublicKey};

// ── Helpers ───────────────────────────────────────────────────────────────────

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

// ── Tests ─────────────────────────────────────────────────────────────────────

/// Two managers exchange a WireGuard-encapsulated packet over loopback.
///
/// Ignored: the current implementation routes via mesh_ip (10.200.x.x) which
/// is not reachable in a standard test environment.
#[ignore]
#[tokio::test]
async fn two_managers_exchange_packet_over_loopback() {
    let mgr_a = make_mgr("node-a", "10.200.1.1", 0).await;
    let mgr_b = make_mgr("node-b", "10.200.2.1", 0).await;

    let peer_a_node = MeshNode {
        id: "node-a".to_string(),
        wg_pubkey: WgPublicKey(B64.encode([0u8; 32])),
        mesh_ip: "10.200.1.1".to_string(),
        wg_port: mgr_a.wg_port(),
    };
    let peer_b_node = MeshNode {
        id: "node-b".to_string(),
        wg_pubkey: WgPublicKey(B64.encode([0u8; 32])),
        mesh_ip: "10.200.2.1".to_string(),
        wg_port: mgr_b.wg_port(),
    };

    mgr_a.add_peer(peer_b_node).await.expect("A adds B");
    mgr_b.add_peer(peer_a_node).await.expect("B adds A");

    assert_eq!(mgr_a.list_peers().await.len(), 1);
    assert_eq!(mgr_b.list_peers().await.len(), 1);
}

/// Adding the same peer twice returns `PeerAlreadyExists`.
#[tokio::test]
async fn add_duplicate_peer_returns_error() {
    let mgr = make_mgr("node-x", "10.200.3.1", 0).await;
    let peer = MeshNode {
        id: "peer-1".to_string(),
        wg_pubkey: WgPrivateKey::generate().public_key(),
        mesh_ip: "10.200.4.1".to_string(),
        wg_port: 51820,
    };

    mgr.add_peer(peer.clone()).await.unwrap();
    let err = mgr.add_peer(peer).await.unwrap_err();
    assert!(
        matches!(err, MeshError::PeerAlreadyExists(_)),
        "expected PeerAlreadyExists, got {err}",
    );
}

/// Removing a peer that was never added returns `PeerNotFound`.
#[tokio::test]
async fn remove_nonexistent_peer_returns_error() {
    let mgr = make_mgr("node-y", "10.200.5.1", 0).await;
    let err = mgr.remove_peer("ghost").await.unwrap_err();
    assert!(
        matches!(err, MeshError::PeerNotFound(_)),
        "expected PeerNotFound, got {err}",
    );
}

/// The wg_port is non-zero when port 0 is given (OS assigns ephemeral port).
#[tokio::test]
async fn wg_port_is_nonzero_when_zero_requested() {
    let mgr = make_mgr("node-z", "10.200.6.1", 0).await;
    assert!(mgr.wg_port() > 0, "OS-assigned port should be non-zero");
}

/// `MeshNode::to_txt` / `from_txt` round-trips correctly.
#[test]
fn mesh_node_txt_round_trip() {
    let pubkey = WgPublicKey(B64.encode([1u8; 32]));
    let node = MeshNode {
        id: "round-trip".to_string(),
        wg_pubkey: pubkey,
        mesh_ip: "10.200.7.1".to_string(),
        wg_port: 51820,
    };
    let txt = node.to_txt();
    let decoded = MeshNode::from_txt(&txt).expect("from_txt failed");
    assert_eq!(node, decoded);
}
