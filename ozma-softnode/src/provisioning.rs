//! Opt-in libvirt VM provisioning for the per-VM `ozma-node` runtime.
//!
//! The only thing this module edits is libvirt's inactive domain XML. When the
//! operator passes `--provision`, a running libvirt domain that has no QMP
//! socket gets one extra `<qemu:commandline>` block:
//!
//! * QMP control socket (`ozma-mon`),
//! * QMP input socket (`ozma-input`),
//! * QMP display/D-Bus setup socket (`ozma-display`),
//! * `-display dbus,p2p=yes`,
//! * optional `input-linux` evdev objects, but only when the operator supplied
//!   **both** an evdev keyboard and mouse path,
//! * optional `PULSE_SERVER` / `XDG_RUNTIME_DIR` environment entries and an
//!   `<audio type='pulseaudio'/>` replacement when a Pulse server is given.
//!
//! The XML is fed to `virsh define /dev/stdin`; this module never writes it to
//! a user path. If a restart is explicitly requested it runs
//! `virsh destroy`/`virsh start` and waits, with a bound, for the control socket
//! to appear.
//!
//! It does **not** create evdev devices, PulseAudio sinks, USB/media devices, or
//! use the libvirt API. It only injects paths and environment that the operator
//! configured, and it refuses to touch Proxmox configuration.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use thiserror::Error;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tracing::{debug, info};

use crate::vm_info::VMInfo;

/// Default directory for the QMP sockets created by provisioning.
pub const DEFAULT_QMP_DIR: &str = "/run/ozma/qmp";
/// Default bounded wait for the control socket after a requested restart.
pub const DEFAULT_RESTART_WAIT: Duration = Duration::from_secs(10);
/// Proxmox marker directory. Provisioning never edits Proxmox configuration.
pub const PROXMOX_MARKER: &str = "/var/run/qemu-server";

const QEMU_NS: &str = "xmlns:qemu='http://libvirt.org/schemas/domain/qemu/1.0'";

/// Operator-selected provisioning behaviour.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProvisionConfig {
    /// Directory for the sockets this module asks QEMU to create.
    pub qmp_dir: PathBuf,
    /// Destroy and start the VM after defining the new XML. Off by default.
    pub restart_vm: bool,
    /// `PULSE_SERVER` value injected into QEMU's environment, when configured.
    pub pulse_server: Option<String>,
    /// `XDG_RUNTIME_DIR` value injected into QEMU's environment, when configured.
    pub xdg_runtime_dir: Option<String>,
    /// evdev keyboard path. Ignored unless `evdev_mouse` is also set.
    pub evdev_keyboard: Option<PathBuf>,
    /// evdev mouse path. Ignored unless `evdev_keyboard` is also set.
    pub evdev_mouse: Option<PathBuf>,
    /// How long to wait for the control socket after `virsh destroy`/`start`.
    pub restart_wait: Duration,
}

impl Default for ProvisionConfig {
    fn default() -> Self {
        Self {
            qmp_dir: PathBuf::from(DEFAULT_QMP_DIR),
            restart_vm: false,
            pulse_server: None,
            xdg_runtime_dir: None,
            evdev_keyboard: None,
            evdev_mouse: None,
            restart_wait: DEFAULT_RESTART_WAIT,
        }
    }
}

impl ProvisionConfig {
    /// Validate operator input before any VM is touched.
    pub fn validate(&self) -> Result<(), ProvisionError> {
        validate_path(&self.qmp_dir, "qmp_dir")?;
        if let Some(keyboard) = &self.evdev_keyboard {
            validate_path(keyboard, "evdev_keyboard")?;
        }
        if let Some(mouse) = &self.evdev_mouse {
            validate_path(mouse, "evdev_mouse")?;
        }
        if let Some(value) = &self.pulse_server {
            validate_text("pulse_server", value)?;
        }
        if let Some(value) = &self.xdg_runtime_dir {
            validate_text("xdg_runtime_dir", value)?;
        }
        Ok(())
    }
}

/// Result of ensuring the QMP/display plumbing for one VM.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProvisionedVm {
    /// Control QMP socket used for capture, input, and power.
    pub qmp_socket: PathBuf,
    /// Dedicated QMP input socket.
    pub qmp_input_socket: PathBuf,
    /// QMP socket used to attach the D-Bus display client.
    pub display_socket: PathBuf,
    /// True when this call defined new libvirt XML.
    pub changed: bool,
    /// True when XML changed but the VM was not restarted, or when provisioned
    /// XML exists but the control socket is still absent. Callers must not spawn
    /// a node until the socket actually exists.
    pub needs_restart: bool,
}

/// Typed provisioning failure. Messages are stable and never include XML or
/// stdin content.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ProvisionError {
    #[error("VM domain {domain:?} is not safe for use as a socket name")]
    UnsafeDomain { domain: String },
    #[error("path {path:?} is not safe for provisioning: {reason}")]
    UnsafePath { path: String, reason: &'static str },
    #[error("invalid provisioning configuration: {detail}")]
    InvalidConfig { detail: String },
    #[error(
        "Proxmox VM {domain:?} has no native QMP socket; refusing to edit Proxmox \
         configuration (use the native QMP socket)"
    )]
    ProxmoxNoQmp { domain: String },
    #[error("virsh dumpxml failed for {domain:?}: {detail}")]
    DumpXml { domain: String, detail: String },
    #[error("virsh define failed for {domain:?}: {detail}")]
    Define { domain: String, detail: String },
    #[error("libvirt XML for {domain:?} is not a single valid <domain> document: {detail}")]
    InvalidXml { domain: String, detail: String },
    #[error("failed to restart {domain:?}: {detail}")]
    Restart { domain: String, detail: String },
    #[error("QMP socket for {domain:?} did not appear within {timeout_ms} ms after restart")]
    Timeout { domain: String, timeout_ms: u128 },
    #[error("failed to run {program:?}: {detail}")]
    Command { program: String, detail: String },
    #[error("cannot prepare QMP directory {path:?}: {detail}")]
    PrepareDir { path: String, detail: String },
}

/// Output of one process invocation.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CommandOutput {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub success: bool,
    pub code: Option<i32>,
}

impl CommandOutput {
    fn stderr_detail(&self) -> String {
        sanitize_detail(&self.stderr)
    }
}

/// Process runner used by [`LibvirtProvisioner`]. Arguments are passed directly;
/// there is no shell interpolation.
#[async_trait]
pub trait CommandRunner: Send + Sync {
    async fn run(
        &self,
        program: &str,
        args: &[String],
        stdin: Option<&[u8]>,
    ) -> Result<CommandOutput, ProvisionError>;
}

/// Real process runner. Never invokes a shell.
#[derive(Debug, Default)]
pub struct TokioCommandRunner;

#[async_trait]
impl CommandRunner for TokioCommandRunner {
    async fn run(
        &self,
        program: &str,
        args: &[String],
        stdin: Option<&[u8]>,
    ) -> Result<CommandOutput, ProvisionError> {
        let mut command = Command::new(program);
        command
            .args(args)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .stdin(if stdin.is_some() {
                std::process::Stdio::piped()
            } else {
                std::process::Stdio::null()
            });

        let mut child = command.spawn().map_err(|error| ProvisionError::Command {
            program: program.to_owned(),
            detail: sanitize_detail(error.to_string().as_bytes()),
        })?;

        if let Some(input) = stdin {
            if let Some(mut pipe) = child.stdin.take() {
                pipe.write_all(input)
                    .await
                    .map_err(|error| ProvisionError::Command {
                        program: program.to_owned(),
                        detail: format!(
                            "cannot write stdin: {}",
                            sanitize_detail(error.to_string().as_bytes())
                        ),
                    })?;
                pipe.shutdown()
                    .await
                    .map_err(|error| ProvisionError::Command {
                        program: program.to_owned(),
                        detail: format!(
                            "cannot close stdin: {}",
                            sanitize_detail(error.to_string().as_bytes())
                        ),
                    })?;
            }
        }

        let output = child
            .wait_with_output()
            .await
            .map_err(|error| ProvisionError::Command {
                program: program.to_owned(),
                detail: sanitize_detail(error.to_string().as_bytes()),
            })?;

        Ok(CommandOutput {
            stdout: output.stdout,
            stderr: output.stderr,
            success: output.status.success(),
            code: output.status.code(),
        })
    }
}

/// Provisioning seam used by the supervisor. `VmLiber` is retained as the
/// historical name for code that already refers to it.
#[async_trait]
pub trait VmProvisioner: Send + Sync {
    async fn ensure(&self, vm: &VMInfo) -> Result<ProvisionedVm, ProvisionError>;
}

/// Alias kept for the plan's `VmLiber` naming.
#[allow(unused_imports)]
pub use VmProvisioner as VmLiber;

/// libvirt implementation: dump the inactive XML, inject one ozma command line,
/// define it through stdin, and optionally restart the VM.
pub struct LibvirtProvisioner {
    config: ProvisionConfig,
    runner: Arc<dyn CommandRunner>,
    proxmox_marker: PathBuf,
}

impl LibvirtProvisioner {
    pub fn new(config: ProvisionConfig, runner: Arc<dyn CommandRunner>) -> Self {
        Self {
            config,
            runner,
            proxmox_marker: PathBuf::from(PROXMOX_MARKER),
        }
    }

    /// Test/embedding hook for a non-default Proxmox marker.
    #[allow(dead_code)]
    pub fn with_proxmox_marker(
        config: ProvisionConfig,
        runner: Arc<dyn CommandRunner>,
        proxmox_marker: PathBuf,
    ) -> Self {
        Self {
            config,
            runner,
            proxmox_marker,
        }
    }

    fn is_proxmox(&self) -> bool {
        self.proxmox_marker.exists()
    }

    async fn restart(&self, domain: &str, qmp_socket: &Path) -> Result<(), ProvisionError> {
        let destroy = self
            .runner
            .run("virsh", &["destroy".to_owned(), domain.to_owned()], None)
            .await?;
        if !destroy.success && !is_already_stopped(&destroy.stderr_detail()) {
            return Err(ProvisionError::Restart {
                domain: domain.to_owned(),
                detail: format!("virsh destroy failed: {}", destroy.stderr_detail()),
            });
        }

        let start = self
            .runner
            .run("virsh", &["start".to_owned(), domain.to_owned()], None)
            .await?;
        if !start.success {
            return Err(ProvisionError::Restart {
                domain: domain.to_owned(),
                detail: format!("virsh start failed: {}", start.stderr_detail()),
            });
        }

        let deadline = tokio::time::Instant::now() + self.config.restart_wait;
        loop {
            if qmp_socket.exists() {
                return Ok(());
            }
            let now = tokio::time::Instant::now();
            if now >= deadline {
                return Err(ProvisionError::Timeout {
                    domain: domain.to_owned(),
                    timeout_ms: self.config.restart_wait.as_millis(),
                });
            }
            let remaining = deadline.saturating_duration_since(now);
            tokio::time::sleep(remaining.min(Duration::from_millis(20))).await;
        }
    }
}

#[async_trait]
impl VmProvisioner for LibvirtProvisioner {
    async fn ensure(&self, vm: &VMInfo) -> Result<ProvisionedVm, ProvisionError> {
        // A discovered socket is the strongest signal. Use it untouched.
        let discovered = vm.qmp_path.trim();
        if !discovered.is_empty() {
            let qmp = PathBuf::from(discovered);
            if qmp.exists() {
                let (input, display) = sibling_paths(&qmp);
                debug!(
                    "VM {}: using discovered QMP socket {} (no provisioning)",
                    vm.name,
                    qmp.display()
                );
                return Ok(ProvisionedVm {
                    qmp_socket: qmp,
                    qmp_input_socket: input,
                    display_socket: display,
                    changed: false,
                    needs_restart: false,
                });
            }
            debug!(
                "VM {}: discovered QMP path {} does not exist; checking libvirt XML",
                vm.name,
                qmp.display()
            );
        }

        // Proxmox owns its QEMU arguments. Never edit its configuration.
        if self.is_proxmox() {
            return Err(ProvisionError::ProxmoxNoQmp {
                domain: vm.name.clone(),
            });
        }

        validate_domain(&vm.name)?;
        self.config.validate()?;
        let (qmp_socket, input_socket, display_socket) = socket_paths(&self.config, &vm.name)?;

        tokio::fs::create_dir_all(&self.config.qmp_dir)
            .await
            .map_err(|error| ProvisionError::PrepareDir {
                path: self.config.qmp_dir.display().to_string(),
                detail: sanitize_detail(error.to_string().as_bytes()),
            })?;

        let dumped = self
            .runner
            .run(
                "virsh",
                &[
                    "dumpxml".to_owned(),
                    "--inactive".to_owned(),
                    vm.name.clone(),
                ],
                None,
            )
            .await?;
        if !dumped.success {
            return Err(ProvisionError::DumpXml {
                domain: vm.name.clone(),
                detail: dumped.stderr_detail(),
            });
        }
        let xml = String::from_utf8(dumped.stdout).map_err(|_| ProvisionError::InvalidXml {
            domain: vm.name.clone(),
            detail: "virsh dumpxml did not return UTF-8 XML".to_owned(),
        })?;

        if xml.contains("ozma-mon") {
            // Already provisioned. Report the paths without touching anything.
            // If the VM has not been restarted yet the socket is absent; keep
            // telling the caller that a restart is still required.
            let needs_restart = !qmp_socket.exists();
            return Ok(ProvisionedVm {
                qmp_socket,
                qmp_input_socket: input_socket,
                display_socket,
                changed: false,
                needs_restart,
            });
        }

        let new_xml = inject_ozma_commandline(&xml, &self.config, &vm.name)?;
        let defined = self
            .runner
            .run(
                "virsh",
                &["define".to_owned(), "/dev/stdin".to_owned()],
                Some(new_xml.as_bytes()),
            )
            .await?;
        if !defined.success {
            return Err(ProvisionError::Define {
                domain: vm.name.clone(),
                detail: defined.stderr_detail(),
            });
        }

        info!(
            "VM {}: defined ozma QMP ({}), input ({}), display ({}) via virsh define",
            vm.name,
            qmp_socket.display(),
            input_socket.display(),
            display_socket.display()
        );

        if !self.config.restart_vm {
            info!(
                "VM {}: XML defined; restart required before the QMP socket exists",
                vm.name
            );
            return Ok(ProvisionedVm {
                qmp_socket,
                qmp_input_socket: input_socket,
                display_socket,
                changed: true,
                needs_restart: true,
            });
        }

        self.restart(&vm.name, &qmp_socket).await?;
        info!("VM {}: restarted with ozma QMP sockets", vm.name);
        Ok(ProvisionedVm {
            qmp_socket,
            qmp_input_socket: input_socket,
            display_socket,
            changed: true,
            needs_restart: false,
        })
    }
}

/// Pure XML transformation. Adds the qemu namespace if absent, inserts exactly
/// one ozma command-line block, optionally adds evdev/environment entries, and
/// replaces `<audio id='1' type='none'/>` when a Pulse server is configured.
///
/// Calling this on already-provisioned XML is a no-op (apart from adding a
/// missing namespace), so repeated reconciles never duplicate the block.
pub fn inject_ozma_commandline(
    xml: &str,
    config: &ProvisionConfig,
    domain: &str,
) -> Result<String, ProvisionError> {
    validate_input_document(xml, domain)?;
    validate_domain(domain)?;
    let (qmp_socket, input_socket, display_socket) = socket_paths(config, domain)?;
    let args = render_qemu_args(config, &qmp_socket, &input_socket, &display_socket)?;

    let mut document = ensure_qemu_namespace(xml, domain)?;

    if document.contains("ozma-mon") {
        validate_document(&document, domain)?;
        return Ok(document);
    }

    if document.contains("<qemu:commandline>") {
        let closes = document.matches("</qemu:commandline>").count();
        if closes != 1 {
            return Err(invalid_xml(
                domain,
                "existing <qemu:commandline> block is not balanced",
            ));
        }
        let close = document
            .rfind("</qemu:commandline>")
            .expect("counted closing tag");
        let mut updated = String::with_capacity(document.len() + args.len());
        updated.push_str(&document[..close]);
        if !updated.ends_with('\n') {
            updated.push('\n');
        }
        updated.push_str(&args);
        updated.push_str("  </qemu:commandline>");
        updated.push_str(&document[close + "</qemu:commandline>".len()..]);
        document = updated;
    } else {
        let close = document
            .rfind("</domain>")
            .expect("input document validated");
        let mut block = String::with_capacity(args.len() + 64);
        block.push_str("  <qemu:commandline>\n");
        block.push_str(&args);
        block.push_str("  </qemu:commandline>\n");

        let mut updated = String::with_capacity(document.len() + block.len());
        updated.push_str(&document[..close]);
        if !updated.ends_with('\n') {
            updated.push('\n');
        }
        updated.push_str(&block);
        updated.push_str(&document[close..]);
        document = updated;
    }

    if has_pulse_server(config) {
        document = replace_audio_none_with_pulseaudio(&document);
    }

    validate_document(&document, domain)?;
    Ok(document)
}

fn validate_input_document(xml: &str, domain: &str) -> Result<(), ProvisionError> {
    if xml.trim().is_empty() {
        return Err(invalid_xml(domain, "XML is empty"));
    }
    let closes = xml.matches("</domain>").count();
    if closes != 1 {
        return Err(invalid_xml(
            domain,
            &format!("expected exactly one </domain>, found {closes}"),
        ));
    }
    let start = xml
        .find("<domain")
        .ok_or_else(|| invalid_xml(domain, "missing <domain> element"))?;
    find_tag_end(xml, start)
        .ok_or_else(|| invalid_xml(domain, "unterminated <domain> opening tag"))?;
    Ok(())
}

fn validate_document(xml: &str, domain: &str) -> Result<(), ProvisionError> {
    validate_input_document(xml, domain)?;
    let start = xml.find("<domain").expect("validated opening element");
    let end = find_tag_end(xml, start).expect("validated opening tag");
    if !xml[start..=end].contains("xmlns:qemu=") {
        return Err(invalid_xml(domain, "missing qemu namespace declaration"));
    }
    Ok(())
}

fn ensure_qemu_namespace(xml: &str, domain: &str) -> Result<String, ProvisionError> {
    let start = xml
        .find("<domain")
        .ok_or_else(|| invalid_xml(domain, "missing <domain> element"))?;
    let end = find_tag_end(xml, start)
        .ok_or_else(|| invalid_xml(domain, "unterminated <domain> opening tag"))?;
    if xml[start..=end].contains("xmlns:qemu=") {
        return Ok(xml.to_owned());
    }

    let mut updated = String::with_capacity(xml.len() + QEMU_NS.len() + 1);
    updated.push_str(&xml[..end]);
    updated.push(' ');
    updated.push_str(QEMU_NS);
    updated.push_str(&xml[end..]);
    Ok(updated)
}

fn render_qemu_args(
    config: &ProvisionConfig,
    qmp_socket: &Path,
    input_socket: &Path,
    display_socket: &Path,
) -> Result<String, ProvisionError> {
    let mut args = String::new();
    push_chardev(&mut args, "ozma-mon", qmp_socket);
    push_chardev(&mut args, "ozma-input", input_socket);
    push_chardev(&mut args, "ozma-display", display_socket);

    push_arg(&mut args, "-display");
    push_arg(&mut args, "dbus,p2p=yes");

    if let (Some(keyboard), Some(mouse)) = (&config.evdev_keyboard, &config.evdev_mouse) {
        let keyboard = validate_path(keyboard, "evdev_keyboard")?;
        let mouse = validate_path(mouse, "evdev_mouse")?;
        push_arg(&mut args, "-object");
        push_arg(
            &mut args,
            &format!("input-linux,id=ozma-kbd,evdev={keyboard}"),
        );
        push_arg(&mut args, "-object");
        push_arg(
            &mut args,
            &format!("input-linux,id=ozma-mouse,evdev={mouse},grab_all=on"),
        );
    }

    if let Some(value) = non_empty(config.pulse_server.as_deref()) {
        push_env(&mut args, "PULSE_SERVER", value);
    }
    if let Some(value) = non_empty(config.xdg_runtime_dir.as_deref()) {
        push_env(&mut args, "XDG_RUNTIME_DIR", value);
    }
    Ok(args)
}

fn push_chardev(args: &mut String, id: &str, path: &Path) {
    let value = format!(
        "socket,id={id},path={},server=on,wait=off",
        path.to_string_lossy()
    );
    push_arg(args, "-chardev");
    push_arg(args, &value);
    push_arg(args, "-mon");
    push_arg(args, &format!("chardev={id},mode=control"));
}

fn push_arg(args: &mut String, value: &str) {
    args.push_str("    <qemu:arg value='");
    args.push_str(&xml_escape(value));
    args.push_str("'/>\n");
}

fn push_env(args: &mut String, name: &str, value: &str) {
    args.push_str("    <qemu:env name='");
    args.push_str(&xml_escape(name));
    args.push_str("' value='");
    args.push_str(&xml_escape(value));
    args.push_str("'/>\n");
}

fn socket_paths(
    config: &ProvisionConfig,
    domain: &str,
) -> Result<(PathBuf, PathBuf, PathBuf), ProvisionError> {
    validate_domain(domain)?;
    validate_path(&config.qmp_dir, "qmp_dir")?;
    Ok((
        config.qmp_dir.join(format!("{domain}.sock")),
        config.qmp_dir.join(format!("{domain}-input.sock")),
        config.qmp_dir.join(format!("{domain}-display.sock")),
    ))
}

fn sibling_paths(qmp: &Path) -> (PathBuf, PathBuf) {
    let parent = qmp.parent().unwrap_or_else(|| Path::new(""));
    let stem = qmp
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .filter(|stem| !stem.is_empty())
        .unwrap_or_else(|| "qmp".to_owned());
    (
        parent.join(format!("{stem}-input.sock")),
        parent.join(format!("{stem}-display.sock")),
    )
}

fn replace_audio_none_with_pulseaudio(xml: &str) -> String {
    let mut updated = xml.replace(
        "<audio id='1' type='none'/>",
        "<audio id='1' type='pulseaudio'/>",
    );
    updated = updated.replace(
        "<audio id=\"1\" type=\"none\"/>",
        "<audio id=\"1\" type=\"pulseaudio\"/>",
    );
    updated
}

fn has_pulse_server(config: &ProvisionConfig) -> bool {
    non_empty(config.pulse_server.as_deref()).is_some()
}

fn non_empty(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

fn validate_domain(domain: &str) -> Result<(), ProvisionError> {
    let ok = !domain.is_empty()
        && domain.len() <= 200
        && domain != "."
        && domain != ".."
        && !domain.starts_with('-')
        && domain
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-'));

    if ok {
        Ok(())
    } else {
        Err(ProvisionError::UnsafeDomain {
            domain: domain.to_owned(),
        })
    }
}

fn validate_path(path: &Path, _field: &'static str) -> Result<String, ProvisionError> {
    let text = path.to_str().ok_or_else(|| ProvisionError::UnsafePath {
        path: path.display().to_string(),
        reason: "path is not valid UTF-8",
    })?;
    let reason = if text.is_empty() {
        Some("path is empty")
    } else if text.contains('\0') || text.contains('\n') || text.contains('\r') {
        Some("path contains a control character")
    } else if !path.is_absolute() {
        Some("path must be absolute")
    } else {
        None
    };
    match reason {
        Some(reason) => Err(ProvisionError::UnsafePath {
            path: text.to_owned(),
            reason,
        }),
        None => Ok(text.to_owned()),
    }
}

fn validate_text(field: &'static str, value: &str) -> Result<(), ProvisionError> {
    if value.trim().is_empty() {
        return Err(ProvisionError::InvalidConfig {
            detail: format!("{field} must not be empty"),
        });
    }
    if value.contains('\0') || value.contains('\n') || value.contains('\r') {
        return Err(ProvisionError::InvalidConfig {
            detail: format!("{field} contains a control character"),
        });
    }
    Ok(())
}

fn find_tag_end(xml: &str, start: usize) -> Option<usize> {
    let bytes = xml.as_bytes();
    let mut quote = None;
    for (index, byte) in bytes.iter().enumerate().skip(start) {
        match quote {
            Some(end) => {
                if *byte == end {
                    quote = None;
                }
            }
            None => match byte {
                b'\'' | b'"' => quote = Some(*byte),
                b'>' => return Some(index),
                _ => {}
            },
        }
    }
    None
}

fn xml_escape(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '\'' => escaped.push_str("&apos;"),
            '"' => escaped.push_str("&quot;"),
            _ => escaped.push(ch),
        }
    }
    escaped
}

fn invalid_xml(domain: &str, detail: &str) -> ProvisionError {
    ProvisionError::InvalidXml {
        domain: domain.to_owned(),
        detail: detail.to_owned(),
    }
}

fn is_already_stopped(detail: &str) -> bool {
    let detail = detail.to_ascii_lowercase();
    detail.contains("not running")
        || detail.contains("already inactive")
        || detail.contains("domain is not active")
        || detail.contains("is not active")
        || detail.contains("not active")
}

/// Bound error details and flatten newlines; stdin/XML is never included.
fn sanitize_detail(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let flattened = text.split_whitespace().collect::<Vec<_>>().join(" ");
    const MAX: usize = 400;
    if flattened.chars().count() > MAX {
        let mut truncated: String = flattened.chars().take(MAX).collect();
        truncated.push_str("...");
        truncated
    } else {
        flattened
    }
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::Mutex;

    const DOMAIN_KVM: &str = r#"<domain type='kvm'>
  <name>vm-a</name>
  <devices>
    <audio id='1' type='none'/>
  </devices>
</domain>"#;

    const DOMAIN_PLAIN: &str = r#"<domain>
  <name>vm-a</name>
  <devices/>
</domain>"#;

    fn config() -> ProvisionConfig {
        ProvisionConfig {
            qmp_dir: PathBuf::from("/run/ozma/qmp"),
            restart_wait: Duration::from_millis(50),
            ..ProvisionConfig::default()
        }
    }

    fn vm(name: &str, qmp: &str) -> VMInfo {
        let mut vm = VMInfo::new(name);
        vm.qmp_path = qmp.to_owned();
        vm
    }

    #[derive(Clone, Debug)]
    struct Call {
        program: String,
        args: Vec<String>,
        stdin: Option<Vec<u8>>,
    }

    #[derive(Clone, Debug)]
    struct FakeStep {
        stdout: Vec<u8>,
        stderr: Vec<u8>,
        success: bool,
        touch: Option<PathBuf>,
    }

    impl FakeStep {
        fn ok(stdout: &str) -> Self {
            Self {
                stdout: stdout.as_bytes().to_vec(),
                stderr: Vec::new(),
                success: true,
                touch: None,
            }
        }

        fn ok_touch(path: PathBuf) -> Self {
            Self {
                stdout: Vec::new(),
                stderr: Vec::new(),
                success: true,
                touch: Some(path),
            }
        }

        fn fail(stderr: &str) -> Self {
            Self {
                stdout: Vec::new(),
                stderr: stderr.as_bytes().to_vec(),
                success: false,
                touch: None,
            }
        }
    }

    #[derive(Default)]
    struct FakeRunner {
        state: Mutex<FakeState>,
    }

    #[derive(Default)]
    struct FakeState {
        steps: VecDeque<FakeStep>,
        calls: Vec<Call>,
    }

    impl FakeRunner {
        fn new(steps: Vec<FakeStep>) -> Self {
            Self {
                state: Mutex::new(FakeState {
                    steps: steps.into(),
                    calls: Vec::new(),
                }),
            }
        }

        fn calls(&self) -> Vec<Call> {
            self.state.lock().unwrap().calls.clone()
        }
    }

    #[async_trait]
    impl CommandRunner for FakeRunner {
        async fn run(
            &self,
            program: &str,
            args: &[String],
            stdin: Option<&[u8]>,
        ) -> Result<CommandOutput, ProvisionError> {
            let mut state = self.state.lock().unwrap();
            state.calls.push(Call {
                program: program.to_owned(),
                args: args.to_vec(),
                stdin: stdin.map(|bytes| bytes.to_vec()),
            });
            let step = state.steps.pop_front().unwrap_or(FakeStep {
                stdout: Vec::new(),
                stderr: Vec::new(),
                success: true,
                touch: None,
            });
            if let Some(path) = step.touch {
                std::fs::write(path, b"").expect("fake touch");
            }
            Ok(CommandOutput {
                stdout: step.stdout,
                stderr: step.stderr,
                success: step.success,
                code: Some(if step.success { 0 } else { 1 }),
            })
        }
    }

    fn provisioner(
        steps: Vec<FakeStep>,
        config: ProvisionConfig,
        proxmox_marker: PathBuf,
    ) -> (LibvirtProvisioner, Arc<FakeRunner>) {
        let runner = Arc::new(FakeRunner::new(steps));
        (
            LibvirtProvisioner::with_proxmox_marker(config, runner.clone(), proxmox_marker),
            runner,
        )
    }

    fn missing_marker() -> PathBuf {
        PathBuf::from("/nonexistent/ozma-provisioning-test-marker")
    }

    // ── Pure XML transformation ───────────────────────────────────────────

    #[test]
    fn inject_adds_namespace_and_block_with_type_kvm() {
        let xml = inject_ozma_commandline(DOMAIN_KVM, &config(), "vm-a").unwrap();
        let start = xml.find("<domain").unwrap();
        let end = find_tag_end(&xml, start).unwrap();

        assert!(xml[start..=end].contains(QEMU_NS));
        assert_eq!(xml.matches("<qemu:commandline>").count(), 1);
        assert_eq!(xml.matches("socket,id=ozma-mon").count(), 1);
        assert_eq!(xml.matches("socket,id=ozma-input").count(), 1);
        assert_eq!(xml.matches("socket,id=ozma-display").count(), 1);
        assert!(xml.contains("value='dbus,p2p=yes'"));
        assert!(xml.contains("/run/ozma/qmp/vm-a.sock"));
        assert!(xml.contains("/run/ozma/qmp/vm-a-input.sock"));
        assert!(xml.contains("/run/ozma/qmp/vm-a-display.sock"));
    }

    #[test]
    fn inject_adds_namespace_without_type_kvm() {
        let xml = inject_ozma_commandline(DOMAIN_PLAIN, &config(), "vm-a").unwrap();
        let start = xml.find("<domain").unwrap();
        let end = find_tag_end(&xml, start).unwrap();

        assert!(xml[start..=end].contains(QEMU_NS));
        assert_eq!(xml.matches("xmlns:qemu=").count(), 1);
        assert_eq!(xml.matches("</domain>").count(), 1);
    }

    #[test]
    fn inject_does_not_duplicate_on_repeat() {
        let once = inject_ozma_commandline(DOMAIN_KVM, &config(), "vm-a").unwrap();
        let twice = inject_ozma_commandline(&once, &config(), "vm-a").unwrap();

        assert_eq!(once, twice);
        assert_eq!(twice.matches("<qemu:commandline>").count(), 1);
        assert_eq!(twice.matches("socket,id=ozma-mon").count(), 1);
        assert_eq!(twice.matches("</domain>").count(), 1);
    }

    #[test]
    fn inject_appends_to_existing_qemu_commandline() {
        let existing = r#"<domain type='kvm' xmlns:qemu='http://libvirt.org/schemas/domain/qemu/1.0'>
  <devices/>
  <qemu:commandline>
    <qemu:arg value='-machine'/>
    <qemu:arg value='foo'/>
  </qemu:commandline>
</domain>"#;

        let xml = inject_ozma_commandline(existing, &config(), "vm-a").unwrap();
        assert_eq!(xml.matches("<qemu:commandline>").count(), 1);
        assert_eq!(xml.matches("</qemu:commandline>").count(), 1);
        assert_eq!(xml.matches("socket,id=ozma-mon").count(), 1);
        assert!(xml.contains("value='-machine'"));
    }

    #[test]
    fn inject_replaces_audio_only_with_pulse_server() {
        let none = inject_ozma_commandline(DOMAIN_KVM, &config(), "vm-a").unwrap();
        assert!(none.contains("<audio id='1' type='none'/>"));

        let mut with_pulse = config();
        with_pulse.pulse_server = Some("unix:/run/user/1000/pulse/native".to_owned());
        let xml = inject_ozma_commandline(DOMAIN_KVM, &with_pulse, "vm-a").unwrap();

        assert!(xml.contains("<audio id='1' type='pulseaudio'/>"));
        assert!(!xml.contains("type='none'"));
        assert!(xml
            .contains("<qemu:env name='PULSE_SERVER' value='unix:/run/user/1000/pulse/native'/>"));
    }

    #[test]
    fn inject_adds_evdev_only_when_both_paths_are_set() {
        let mut one = config();
        one.evdev_keyboard = Some(PathBuf::from("/dev/input/event5"));
        let xml = inject_ozma_commandline(DOMAIN_KVM, &one, "vm-a").unwrap();
        assert!(!xml.contains("input-linux"));

        let mut both = config();
        both.evdev_keyboard = Some(PathBuf::from("/dev/input/event5"));
        both.evdev_mouse = Some(PathBuf::from("/dev/input/event6"));
        let xml = inject_ozma_commandline(DOMAIN_KVM, &both, "vm-a").unwrap();
        assert_eq!(xml.matches("input-linux").count(), 2);
        assert!(xml.contains("id=ozma-kbd,evdev=/dev/input/event5"));
        assert!(xml.contains("id=ozma-mouse,evdev=/dev/input/event6,grab_all=on"));
    }

    #[test]
    fn inject_adds_xdg_runtime_env() {
        let mut config = config();
        config.xdg_runtime_dir = Some("/run/user/1000".to_owned());
        let xml = inject_ozma_commandline(DOMAIN_KVM, &config, "vm-a").unwrap();
        assert!(xml.contains("<qemu:env name='XDG_RUNTIME_DIR' value='/run/user/1000'/>"));
        assert!(!xml.contains("PULSE_SERVER"));
    }

    #[test]
    fn inject_leaves_existing_ozma_block_unchanged() {
        let once = inject_ozma_commandline(DOMAIN_KVM, &config(), "vm-a").unwrap();
        let again = inject_ozma_commandline(&once, &config(), "vm-a").unwrap();
        assert_eq!(once, again);
    }

    #[test]
    fn inject_rejects_empty_or_multiple_domain_documents() {
        assert!(matches!(
            inject_ozma_commandline("", &config(), "vm-a"),
            Err(ProvisionError::InvalidXml { .. })
        ));
        assert!(matches!(
            inject_ozma_commandline("<domain></domain><domain></domain>", &config(), "vm-a"),
            Err(ProvisionError::InvalidXml { .. })
        ));
    }

    #[test]
    fn inject_rejects_unsafe_domains() {
        for domain in ["", ".", "..", "../vm", "vm/../../etc", "-vm", "vm a"] {
            assert!(
                matches!(
                    inject_ozma_commandline(DOMAIN_KVM, &config(), domain),
                    Err(ProvisionError::UnsafeDomain { .. })
                ),
                "domain {domain:?} should be rejected"
            );
        }
    }

    // ── ensure() with the fake runner ─────────────────────────────────────

    #[tokio::test]
    async fn ensure_returns_existing_discovered_socket_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let qmp = dir.path().join("vm-a.sock");
        std::fs::write(&qmp, b"").unwrap();

        let (provisioner, runner) = provisioner(Vec::new(), config(), missing_marker());
        let result = provisioner
            .ensure(&vm("vm-a", &qmp.to_string_lossy()))
            .await
            .unwrap();

        assert_eq!(result.qmp_socket, qmp);
        assert_eq!(result.qmp_input_socket, dir.path().join("vm-a-input.sock"));
        assert_eq!(result.display_socket, dir.path().join("vm-a-display.sock"));
        assert!(!result.changed);
        assert!(!result.needs_restart);
        assert!(runner.calls().is_empty());
    }

    #[tokio::test]
    async fn ensure_refuses_proxmox_without_native_qmp() {
        let marker = tempfile::tempdir().unwrap();
        let (provisioner, runner) = provisioner(Vec::new(), config(), marker.path().to_path_buf());

        let error = provisioner.ensure(&vm("vm-a", "")).await.unwrap_err();
        assert!(matches!(error, ProvisionError::ProxmoxNoQmp { .. }));
        assert!(runner.calls().is_empty());
    }

    #[tokio::test]
    async fn ensure_defines_xml_via_stdin_without_restart() {
        let dir = tempfile::tempdir().unwrap();
        let qmp_dir = dir.path().join("qmp");
        let mut config = config();
        config.qmp_dir = qmp_dir.clone();

        let (provisioner, runner) = provisioner(
            vec![FakeStep::ok(DOMAIN_KVM), FakeStep::ok("")],
            config,
            missing_marker(),
        );

        let result = provisioner.ensure(&vm("vm-a", "")).await.unwrap();
        assert_eq!(result.qmp_socket, qmp_dir.join("vm-a.sock"));
        assert_eq!(result.qmp_input_socket, qmp_dir.join("vm-a-input.sock"));
        assert_eq!(result.display_socket, qmp_dir.join("vm-a-display.sock"));
        assert!(result.changed);
        assert!(result.needs_restart);
        assert!(qmp_dir.is_dir());

        let calls = runner.calls();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].program, "virsh");
        assert_eq!(
            calls[0].args,
            vec![
                "dumpxml".to_owned(),
                "--inactive".to_owned(),
                "vm-a".to_owned()
            ]
        );
        assert_eq!(
            calls[1].args,
            vec!["define".to_owned(), "/dev/stdin".to_owned()]
        );
        let stdin = calls[1].stdin.as_deref().unwrap();
        let stdin = std::str::from_utf8(stdin).unwrap();
        assert!(stdin.contains("socket,id=ozma-mon"));
        assert!(stdin.contains("value='dbus,p2p=yes'"));
        assert!(!calls.iter().any(|call| call.args[0] == "destroy"));
        assert!(!calls.iter().any(|call| call.args[0] == "start"));
    }

    #[tokio::test]
    async fn ensure_existing_ozma_mon_only_reports_restart_when_socket_is_absent() {
        let dir = tempfile::tempdir().unwrap();
        let qmp_dir = dir.path().join("qmp");
        let mut config = config();
        config.qmp_dir = qmp_dir.clone();

        let already = inject_ozma_commandline(DOMAIN_KVM, &config, "vm-a").unwrap();
        let (provisioner, runner) =
            provisioner(vec![FakeStep::ok(&already)], config, missing_marker());

        let result = provisioner.ensure(&vm("vm-a", "")).await.unwrap();
        assert!(!result.changed);
        assert!(result.needs_restart);
        assert_eq!(result.qmp_socket, qmp_dir.join("vm-a.sock"));
        assert_eq!(runner.calls().len(), 1);
    }

    #[tokio::test]
    async fn ensure_existing_ozma_mon_with_socket_needs_no_restart() {
        let dir = tempfile::tempdir().unwrap();
        let qmp_dir = dir.path().join("qmp");
        std::fs::create_dir_all(&qmp_dir).unwrap();
        std::fs::write(qmp_dir.join("vm-a.sock"), b"").unwrap();

        let mut config = config();
        config.qmp_dir = qmp_dir.clone();
        let already = inject_ozma_commandline(DOMAIN_KVM, &config, "vm-a").unwrap();
        let (provisioner, runner) =
            provisioner(vec![FakeStep::ok(&already)], config, missing_marker());

        let result = provisioner.ensure(&vm("vm-a", "")).await.unwrap();
        assert!(!result.changed);
        assert!(!result.needs_restart);
        assert_eq!(runner.calls().len(), 1);
    }

    #[tokio::test]
    async fn ensure_reports_dumpxml_failure() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = config();
        config.qmp_dir = dir.path().join("qmp");
        let (provisioner, _) = provisioner(
            vec![FakeStep::fail("error: failed to get domain 'vm-a'")],
            config,
            missing_marker(),
        );

        let error = provisioner.ensure(&vm("vm-a", "")).await.unwrap_err();
        match error {
            ProvisionError::DumpXml { domain, detail } => {
                assert_eq!(domain, "vm-a");
                assert!(detail.contains("failed to get domain"));
            }
            other => panic!("unexpected error: {other}"),
        }
    }

    #[tokio::test]
    async fn ensure_reports_define_failure_and_does_not_restart() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = config();
        config.qmp_dir = dir.path().join("qmp");
        let (provisioner, runner) = provisioner(
            vec![
                FakeStep::ok(DOMAIN_KVM),
                FakeStep::fail("error: XML error: unknown element"),
            ],
            config,
            missing_marker(),
        );

        let error = provisioner.ensure(&vm("vm-a", "")).await.unwrap_err();
        assert!(matches!(error, ProvisionError::Define { .. }));
        let calls = runner.calls();
        assert_eq!(calls.len(), 2);
        assert!(!calls.iter().any(|call| call.args[0] == "destroy"));
        assert!(!calls.iter().any(|call| call.args[0] == "start"));
    }

    #[tokio::test]
    async fn ensure_restarts_and_waits_for_the_socket_when_enabled() {
        let dir = tempfile::tempdir().unwrap();
        let qmp_dir = dir.path().join("qmp");
        let qmp = qmp_dir.join("vm-a.sock");
        let mut config = config();
        config.qmp_dir = qmp_dir;
        config.restart_vm = true;

        let (provisioner, runner) = provisioner(
            vec![
                FakeStep::ok(DOMAIN_KVM),
                FakeStep::ok(""),
                FakeStep::ok("Domain 'vm-a' destroyed"),
                FakeStep::ok_touch(qmp.clone()),
            ],
            config,
            missing_marker(),
        );

        let result = provisioner.ensure(&vm("vm-a", "")).await.unwrap();
        assert!(result.changed);
        assert!(!result.needs_restart);
        assert_eq!(result.qmp_socket, qmp);

        let calls = runner.calls();
        assert_eq!(calls.len(), 4);
        assert_eq!(calls[2].args, vec!["destroy".to_owned(), "vm-a".to_owned()]);
        assert_eq!(calls[3].args, vec!["start".to_owned(), "vm-a".to_owned()]);
    }

    #[tokio::test]
    async fn ensure_treats_already_stopped_destroy_as_ignorable() {
        let dir = tempfile::tempdir().unwrap();
        let qmp_dir = dir.path().join("qmp");
        let qmp = qmp_dir.join("vm-a.sock");
        let mut config = config();
        config.qmp_dir = qmp_dir;
        config.restart_vm = true;

        let (provisioner, _) = provisioner(
            vec![
                FakeStep::ok(DOMAIN_KVM),
                FakeStep::ok(""),
                FakeStep::fail("error: domain 'vm-a' is not running"),
                FakeStep::ok_touch(qmp),
            ],
            config,
            missing_marker(),
        );

        let result = provisioner.ensure(&vm("vm-a", "")).await.unwrap();
        assert!(!result.needs_restart);
    }

    #[tokio::test]
    async fn ensure_times_out_when_restarted_socket_never_appears() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = config();
        config.qmp_dir = dir.path().join("qmp");
        config.restart_vm = true;
        config.restart_wait = Duration::from_millis(30);

        let (provisioner, _) = provisioner(
            vec![
                FakeStep::ok(DOMAIN_KVM),
                FakeStep::ok(""),
                FakeStep::ok(""),
                FakeStep::ok(""),
            ],
            config,
            missing_marker(),
        );

        let error = provisioner.ensure(&vm("vm-a", "")).await.unwrap_err();
        assert!(matches!(error, ProvisionError::Timeout { .. }));
    }

    #[tokio::test]
    async fn ensure_rejects_unsafe_domain_before_running_virsh() {
        let (provisioner, runner) = provisioner(Vec::new(), config(), missing_marker());
        let error = provisioner.ensure(&vm("../vm-a", "")).await.unwrap_err();

        assert!(matches!(error, ProvisionError::UnsafeDomain { .. }));
        assert!(runner.calls().is_empty());
    }

    #[test]
    fn sanitize_detail_flattens_and_bounds_output() {
        let long = "x".repeat(900);
        let detail = sanitize_detail(format!("line one\nline two {long}").as_bytes());
        assert!(!detail.contains('\n'));
        assert!(detail.len() <= 404);
        assert!(detail.ends_with("..."));
    }
}
