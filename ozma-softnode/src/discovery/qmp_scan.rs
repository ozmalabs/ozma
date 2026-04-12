//! Discover VMs by scanning a directory for QMP / monitor Unix sockets.

use std::path::Path;

use anyhow::Result;
use tracing::info;

use crate::vm_info::VMInfo;

/// Scan `dir` for `*qmp*` and `*.monitor` Unix sockets.
pub fn discover(dir: &str) -> Result<Vec<VMInfo>> {
    let patterns = [
        format!("{dir}/*qmp*"),
        format!("{dir}/*.monitor"),
    ];

    let mut paths: Vec<std::path::PathBuf> = Vec::new();
    for pat in &patterns {
        for entry in glob::glob(pat).unwrap_or_else(|_| glob::glob("").unwrap()) {
            if let Ok(p) = entry {
                paths.push(p);
            }
        }
    }
    paths.sort();
    paths.dedup();

    let mut vms = Vec::new();
    for path in paths {
        if !is_unix_socket(&path) {
            continue;
        }
        let path_str = path.to_string_lossy();
        // Skip ozma's own sockets
        if path_str.contains("/run/ozma/")
            || path_str.contains("ozma-mon")
            || path_str.contains("ozma-stream")
        {
            continue;
        }

        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .replace("ozma-", "")
            .replace(".qmp", "")
            .replace(".monitor", "");

        let mut vm = VMInfo::new(&stem);
        vm.vm_id = stem.clone();
        vm.qmp_path = path.to_string_lossy().into_owned();
        vms.push(vm);
    }

    if !vms.is_empty() {
        info!("QMP scan: found {} sockets in {}", vms.len(), dir);
    }
    Ok(vms)
}

fn is_unix_socket(path: &Path) -> bool {
    use std::os::unix::fs::FileTypeExt;
    path.metadata()
        .map(|m| m.file_type().is_socket())
        .unwrap_or(false)
}
