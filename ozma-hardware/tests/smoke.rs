// SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
//! Hardware collector smoke tests.

#[cfg(target_os = "linux")]
#[test]
fn linux_snapshot_returns_core_cpu_and_memory_inventory() {
    use ozma_hardware::HardwareInfoCollector;

    let mut collector = HardwareInfoCollector::new().expect("linux collector");
    let snapshot = collector.snapshot().expect("hardware snapshot");

    assert!(snapshot.timestamp > 0.0);
    assert!(!snapshot.hostname.is_empty());
    assert!(!snapshot.cpu.brand.is_empty());
    assert!(snapshot.cpu.logical_cores > 0);
    assert!(snapshot.total_memory_gb > 0.0);
    // Serialization is what the node API serves.
    let json = serde_json::to_value(&snapshot).expect("snapshot JSON");
    assert_eq!(json["cpu"]["brand"], snapshot.cpu.brand);
}
