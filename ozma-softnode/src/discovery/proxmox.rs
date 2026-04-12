//! Discover VMs on a Proxmox VE host via QMP sockets and config files.

use std::path::Path;

use anyhow::Result;
use tracing::{debug, info};

use crate::vm_info::VMInfo;

/// Returns `true` when running on a Proxmox VE host.
pub fn is_proxmox() -> bool {
    Path::new("/var/run/qemu-server").exists()
}

/// Discover all running VMs by scanning `/var/run/qemu-server/*.qmp`.
pub fn discover() -> Result<Vec<VMInfo>> {
    let qmp_dir = Path::new("/var/run/qemu-server");
    if !qmp_dir.exists() {
        return Ok(Vec::new());
    }

    let mut entries: Vec<_> = std::fs::read_dir(qmp_dir)?
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.path()
                .extension()
                .map(|x| x == "qmp")
                .unwrap_or(false)
        })
        .collect();
    entries.sort_by_key(|e| e.path());

    let mut vms = Vec::new();

    for entry in entries {
        let path = entry.path();
        let vmid = match path.file_stem().and_then(|s| s.to_str()) {
            Some(s) => s.to_owned(),
            None => continue,
        };

        let conf_path = format!("/etc/pve/qemu-server/{vmid}.conf");
        let mut name = vmid.clone();
        let mut vnc_port: u16 = 0;
        let mut guest_os = String::new();
        let mut has_gpu_passthrough = false;

        if let Ok(conf) = std::fs::read_to_string(&conf_path) {
            for line in conf.lines() {
                if let Some(rest) = line.strip_prefix("name:") {
                    name = rest.trim().to_owned();
                }
                if let Some(rest) = line.strip_prefix("ostype:") {
                    let ostype = rest.trim();
                    if ostype.starts_with("win") {
                        guest_os = "windows".into();
                    } else if ostype.starts_with('l') {
                        guest_os = "linux".into();
                    }
                }
                if line.starts_with("hostpci") && line.contains("x-vga=1") {
                    has_gpu_passthrough = true;
                }
            }
            if let Ok(id) = vmid.parse::<u16>() {
                vnc_port = 5900 + id;
            }
        }

        let mut vm = VMInfo::new(&name);
        vm.vm_id = vmid;
        vm.qmp_path = path.to_string_lossy().into_owned();
        vm.vnc_port = vnc_port;
        vm.has_guest_agent = true; // Proxmox always installs qemu-ga
        vm.guest_os = guest_os;
        vm.has_gpu_passthrough = has_gpu_passthrough;

        debug!("Proxmox: found VM {} (qmp={})", vm.name, vm.qmp_path);
        vms.push(vm);
    }

    info!("Proxmox: discovered {} VMs", vms.len());
    Ok(vms)
}
