//! VM metadata types shared across discovery backends.

/// A single display output on a VM.
#[derive(Debug, Clone)]
pub struct VMDisplayOutput {
    pub index: usize,
    /// "dbus", "vnc", "ivshmem", "agent"
    pub source_type: String,
    pub vnc_port: u16,
    pub dbus_console: usize,
    pub ivshmem_path: String,
    pub resolution: (u32, u32),
    pub capture_source_id: String,
}

impl VMDisplayOutput {
    pub fn new(index: usize) -> Self {
        Self {
            index,
            source_type: "dbus".into(),
            vnc_port: 0,
            dbus_console: 0,
            ivshmem_path: String::new(),
            resolution: (1920, 1080),
            capture_source_id: String::new(),
        }
    }
}

/// Discovered VM metadata.
#[derive(Debug, Clone)]
pub struct VMInfo {
    pub name: String,
    pub vm_id: String,
    pub qmp_path: String,
    pub vnc_port: u16,
    pub vnc_host: String,
    pub state: String,
    pub pid: u32,
    pub has_guest_agent: bool,
    /// "windows", "linux", "" (unknown)
    pub guest_os: String,
    pub has_gpu_passthrough: bool,
    pub displays: Vec<VMDisplayOutput>,
}

impl VMInfo {
    pub fn new(name: impl Into<String>) -> Self {
        let name = name.into();
        let vm_id = name.clone();
        Self {
            name,
            vm_id,
            qmp_path: String::new(),
            vnc_port: 0,
            vnc_host: "127.0.0.1".into(),
            state: "running".into(),
            pid: 0,
            has_guest_agent: false,
            guest_os: String::new(),
            has_gpu_passthrough: false,
            displays: Vec::new(),
        }
    }
}
