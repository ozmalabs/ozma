//! Discover VMs via `virsh` subprocess (no native libvirt binding required).
//!
//! Uses `virsh list --name` to enumerate running domains, then
//! `virsh dumpxml <name>` to extract QMP socket, VNC port, guest-agent
//! channel, GPU passthrough, and OS hints.

use std::path::Path;

use anyhow::{Context, Result};
use tracing::{debug, info, warn};

use crate::vm_info::VMInfo;

/// Run `virsh <args>` and return stdout as a String.
async fn virsh(args: &[&str]) -> Result<String> {
    let out = tokio::process::Command::new("virsh")
        .args(args)
        .output()
        .await
        .context("virsh not found")?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        anyhow::bail!(
            "virsh {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        )
    }
}

/// Discover all active VMs via `virsh list --name` + `virsh dumpxml`.
pub async fn discover() -> Result<Vec<VMInfo>> {
    let names_raw = virsh(&["list", "--name"]).await?;
    let names: Vec<&str> = names_raw
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();

    if names.is_empty() {
        return Ok(Vec::new());
    }

    let mut vms = Vec::new();
    for name in names {
        match parse_domain(name).await {
            Ok(vm) => {
                debug!("libvirt: found VM {}", vm.name);
                vms.push(vm);
            }
            Err(e) => {
                warn!("libvirt: failed to parse domain {}: {}", name, e);
            }
        }
    }

    info!("libvirt: discovered {} VMs", vms.len());
    Ok(vms)
}

async fn parse_domain(name: &str) -> Result<VMInfo> {
    let xml = virsh(&["dumpxml", name]).await?;
    let mut vm = VMInfo::new(name);

    // ── QMP socket ────────────────────────────────────────────────────────
    // Look for: <qemu:arg value='socket,id=ozma-mon,path=...'/>
    for line in xml.lines() {
        let line = line.trim();
        if line.contains("qemu:arg") && line.contains("socket") && line.contains("qmp") {
            if let Some(value) = extract_attr(line, "value") {
                if let Some(p) = value.split("path=").nth(1) {
                    let sock = p.split(',').next().unwrap_or("").trim_matches('\'');
                    if !sock.is_empty() {
                        vm.qmp_path = sock.to_owned();
                    }
                }
            }
        }
    }

    // Fallback: common libvirt monitor socket paths
    if vm.qmp_path.is_empty() {
        for candidate in [
            format!("/var/lib/libvirt/qemu/domain-{name}/monitor.sock"),
            format!("/tmp/qemu-{name}.qmp"),
            format!("/var/run/libvirt/qemu/{name}.monitor"),
        ] {
            if Path::new(&candidate).exists() {
                vm.qmp_path = candidate;
                break;
            }
        }
    }

    // ── VNC port ──────────────────────────────────────────────────────────
    // <graphics type='vnc' port='5910' .../>
    for line in xml.lines() {
        let line = line.trim();
        if line.contains("type='vnc'") || line.contains("type=\"vnc\"") {
            if let Some(port_str) = extract_attr(line, "port") {
                if let Ok(p) = port_str.parse::<i32>() {
                    if p > 0 {
                        vm.vnc_port = p as u16;
                    }
                }
            }
        }
    }

    // ── Guest agent channel ───────────────────────────────────────────────
    // <target name='org.qemu.guest_agent.0'/>
    if xml.contains("org.qemu.guest_agent.0") {
        vm.has_guest_agent = true;
    }

    // ── GPU passthrough ───────────────────────────────────────────────────
    // <hostdev type='pci'> with <driver name='vfio'/>
    if xml.contains("name='vfio'") || xml.contains("name=\"vfio\"") {
        vm.has_gpu_passthrough = true;
    }
    if !vm.has_gpu_passthrough {
        vm.has_gpu_passthrough = check_vfio_sysfs(&xml);
    }

    // ── Guest OS ──────────────────────────────────────────────────────────
    // <libosinfo:os id='http://microsoft.com/win/10'/>
    for line in xml.lines() {
        let line = line.trim();
        if line.contains("libosinfo") && line.contains("id=") {
            if let Some(id) = extract_attr(line, "id") {
                let id_lc = id.to_lowercase();
                if id_lc.contains("win") {
                    vm.guest_os = "windows".into();
                } else if id_lc.contains("linux")
                    || id_lc.contains("ubuntu")
                    || id_lc.contains("fedora")
                    || id_lc.contains("debian")
                    || id_lc.contains("rhel")
                {
                    vm.guest_os = "linux".into();
                }
            }
        }
    }

    Ok(vm)
}

/// Extract `key='value'` or `key="value"` from an XML line.
fn extract_attr<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let sq = format!("{key}='");
    let dq = format!("{key}=\"");
    if let Some(start) = line.find(&sq) {
        let rest = &line[start + sq.len()..];
        rest.find('\'').map(|end| &rest[..end])
    } else if let Some(start) = line.find(&dq) {
        let rest = &line[start + dq.len()..];
        rest.find('"').map(|end| &rest[..end])
    } else {
        None
    }
}

/// Check `/sys/bus/pci/devices/<addr>/driver` symlinks for vfio-pci.
fn check_vfio_sysfs(xml: &str) -> bool {
    for line in xml.lines() {
        let line = line.trim();
        if !line.starts_with("<address") {
            continue;
        }
        let domain = extract_attr(line, "domain")
            .unwrap_or("0x0000")
            .trim_start_matches("0x");
        let bus = extract_attr(line, "bus")
            .unwrap_or("0x00")
            .trim_start_matches("0x");
        let slot = extract_attr(line, "slot")
            .unwrap_or("0x00")
            .trim_start_matches("0x");
        let func = extract_attr(line, "function")
            .unwrap_or("0x0")
            .trim_start_matches("0x");

        let pci = format!(
            "{:04}:{:02}:{:02}.{}",
            u32::from_str_radix(domain, 16).unwrap_or(0),
            u32::from_str_radix(bus, 16).unwrap_or(0),
            u32::from_str_radix(slot, 16).unwrap_or(0),
            u32::from_str_radix(func, 16).unwrap_or(0),
        );

        let driver_link = format!("/sys/bus/pci/devices/{pci}/driver");
        if let Ok(target) = std::fs::read_link(&driver_link) {
            if target.to_string_lossy().contains("vfio") {
                return true;
            }
        }
    }
    false
}
