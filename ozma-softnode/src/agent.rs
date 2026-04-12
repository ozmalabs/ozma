//! Agent provisioner — installs and starts ozma-agent inside a VM via
//! the QEMU guest agent (`virsh qemu-agent-command`).

use anyhow::Result;
use serde_json::json;
use tracing::{debug, info};

use crate::vm_info::VMInfo;

pub struct AgentProvisioner {
    controller_url: String,
}

impl AgentProvisioner {
    pub fn new(controller_url: impl Into<String>) -> Self {
        Self {
            controller_url: controller_url.into(),
        }
    }

    /// Try to provision the agent inside a VM. Returns `true` if successful.
    pub async fn provision(&self, vm: &VMInfo) -> bool {
        if self.agent_alive(vm).await {
            info!("Agent already running in {}", vm.name);
            return true;
        }
        if vm.has_guest_agent {
            if self.provision_via_guest_agent(vm).await {
                return true;
            }
        }
        debug!(
            "Agent provisioning not available for {} (no guest agent channel)",
            vm.name
        );
        false
    }

    // ── Internal helpers ──────────────────────────────────────────────────

    async fn agent_alive(&self, vm: &VMInfo) -> bool {
        if !vm.has_guest_agent {
            return false;
        }
        let ping = json!({"execute": "guest-ping"}).to_string();
        match virsh_agent_command(&vm.name, &ping).await {
            Ok(_) => self.check_agent_process(vm).await,
            Err(_) => false,
        }
    }

    async fn check_agent_process(&self, vm: &VMInfo) -> bool {
        let cmd = if vm.guest_os == "windows" {
            json!({
                "execute": "guest-exec",
                "arguments": {
                    "path": "tasklist",
                    "arg": ["/FI", "IMAGENAME eq ozma-agent.exe"],
                    "capture-output": true
                }
            })
        } else {
            json!({
                "execute": "guest-exec",
                "arguments": {
                    "path": "pgrep",
                    "arg": ["-f", "ozma"],
                    "capture-output": true
                }
            })
        };

        let Ok(out) = virsh_agent_command(&vm.name, &cmd.to_string()).await else {
            return false;
        };

        let pid = serde_json::from_str::<serde_json::Value>(&out)
            .ok()
            .and_then(|v| v["return"]["pid"].as_u64())
            .unwrap_or(0);

        if pid == 0 {
            return false;
        }

        tokio::time::sleep(std::time::Duration::from_secs(1)).await;

        let status_cmd = json!({
            "execute": "guest-exec-status",
            "arguments": {"pid": pid}
        })
        .to_string();

        let Ok(out2) = virsh_agent_command(&vm.name, &status_cmd).await else {
            return false;
        };

        serde_json::from_str::<serde_json::Value>(&out2)
            .ok()
            .and_then(|v| v["return"]["exitcode"].as_i64())
            .map(|c| c == 0)
            .unwrap_or(false)
    }

    async fn provision_via_guest_agent(&self, vm: &VMInfo) -> bool {
        info!("Provisioning agent in {} via guest agent...", vm.name);
        let controller = if self.controller_url.is_empty() {
            "http://10.200.0.1:7380"
        } else {
            &self.controller_url
        };
        match vm.guest_os.as_str() {
            "windows" => self.provision_windows(vm, controller).await,
            "linux" => self.provision_linux(vm, controller).await,
            _ => {
                if self.provision_linux(vm, controller).await {
                    true
                } else {
                    self.provision_windows(vm, controller).await
                }
            }
        }
    }

    async fn provision_linux(&self, vm: &VMInfo, controller: &str) -> bool {
        let install_cmd = format!(
            "uv pip install ozma-agent 2>/dev/null || pip3 install ozma-agent 2>/dev/null; \
             ozma-agent --controller {controller} --daemon"
        );
        guest_exec(&vm.name, "/bin/sh", &["-c", &install_cmd]).await
    }

    async fn provision_windows(&self, vm: &VMInfo, controller: &str) -> bool {
        let start_cmd = format!(
            "net start ozma-agent 2>nul || \
             start /B C:\\ozma-agent\\ozma-agent.exe --controller {controller}"
        );
        guest_exec(&vm.name, "cmd.exe", &["/C", &start_cmd]).await
    }
}

// ── Low-level helpers ──────────────────────────────────────────────────────

async fn virsh_agent_command(vm_name: &str, cmd: &str) -> Result<String> {
    let out = tokio::process::Command::new("virsh")
        .args(["qemu-agent-command", vm_name, cmd])
        .output()
        .await?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        anyhow::bail!(
            "virsh qemu-agent-command failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )
    }
}

async fn guest_exec(vm_name: &str, path: &str, args: &[&str]) -> bool {
    let cmd = json!({
        "execute": "guest-exec",
        "arguments": {
            "path": path,
            "arg": args,
            "capture-output": true
        }
    });

    match tokio::time::timeout(
        std::time::Duration::from_secs(30),
        virsh_agent_command(vm_name, &cmd.to_string()),
    )
    .await
    {
        Ok(Ok(_)) => {
            info!("Agent provisioning command sent to {}", vm_name);
            true
        }
        Ok(Err(e)) => {
            debug!("guest-exec failed for {}: {}", vm_name, e);
            false
        }
        Err(_) => {
            debug!("guest-exec timed out for {}", vm_name);
            false
        }
    }
}
