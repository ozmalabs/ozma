//! VM discovery — tries backends in priority order:
//!   1. Proxmox  (most specific — checks for /var/run/qemu-server)
//!   2. libvirt  (via `virsh` subprocess)
//!   3. QMP socket scan (fallback)

pub mod libvirt;
pub mod proxmox;
pub mod qmp_scan;

use anyhow::Result;
use crate::vm_info::VMInfo;

/// Discover all running VMs using the best available backend.
pub async fn discover_vms(qmp_dir: &str) -> Result<Vec<VMInfo>> {
    // 1. Proxmox
    if proxmox::is_proxmox() {
        let vms = proxmox::discover()?;
        if !vms.is_empty() {
            return Ok(vms);
        }
    }

    // 2. libvirt (virsh)
    match libvirt::discover().await {
        Ok(vms) if !vms.is_empty() => return Ok(vms),
        Ok(_) => {}
        Err(e) => tracing::debug!("libvirt discovery failed: {}", e),
    }

    // 3. QMP socket scan
    let dir = if qmp_dir.is_empty() { "/tmp" } else { qmp_dir };
    qmp_scan::discover(dir)
}
