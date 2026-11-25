// SPDX-License-Identifier: AGPL-3.0-only
//! `ozma-multiseat` — plan and supervise one `ozma-node --profile desktop`
//! per physical seat.
//!
//! Detection is honest and explicit:
//!
//! * X11 outputs come from `xrandr --listactivemonitors` for an explicit
//!   `--display` (or the ambient `DISPLAY`, which the CLI prints).
//! * input devices come from read-only sysfs/evdev enumeration.
//! * DRM connectors are reported for diagnostics only; they do not create
//!   capture sources.
//!
//! A display enumeration failure, an empty display list, or an input
//! enumeration failure aborts before planning. No fake display, input, or
//! process is ever substituted. `--print-plan` and `--dry-run` never spawn.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::thread;
use std::time::{Duration, Instant};

use clap::Parser;
use ozma_multiseat::{
    detect_drm, render_metrics, DrmDetection, EvdevGrabRouter, HelperClient, HelperClientConfig,
    HostTopology, HotplugDebounce, HotplugOptions, HttpHealthProbe, MultiseatPlan,
    MultiseatPlanner, MultiseatStatus, OzmaAudioRouter, PlannerOptions, PortProbe,
    ProcessCommandRunner, ProcessLauncher, ProcessSeatDisplayLauncher, ProcessSeatSupervisor,
    ReadinessProbe, SeatDisplayBackend, SeatDisplayOptions, SeatDisplaySize, SeatDisplayStatus,
    SeatProfile, SeatReconciler, SeatResourceStatus, SeatState, SeatStore, SeatSupervisor,
    SupervisorOptions, SysfsHotplugMonitor, SysfsInputBackend, XInputRouter, XrandrDisplayBackend,
    XrandrOwnershipController, DEFAULT_DEBOUNCE, DEFAULT_HEALTH_PATH, DEFAULT_NODE_BIN,
};

/// Plan and supervise one `ozma-node` per seat on this host.
#[derive(Parser, Debug)]
#[command(
    name = "ozma-multiseat",
    about = "Plan and supervise one ozma-node --profile desktop per physical seat",
    long_about = "Enumerate X11 outputs and read-only input topology, plan one seat per \
                  display, print or validate the plan, and optionally spawn and supervise \
                  one `ozma-node --profile desktop` child per seat.\n\n\
                  Display enumeration is explicit (`--display` or ambient DISPLAY); a \
                  failure or an empty output list aborts the plan. Input enumeration is \
                  read-only; a failure aborts rather than inventing devices. DRM connectors \
                  are reported for diagnostics only. No input is injected and no virtual \
                  display is created; an audio sink is created only with `--audio-router`.\n\n\
                  `--print-plan` prints the plan and exits. `--dry-run` records and prints \
                  the launch plan without spawning. Without either flag the supervisor \
                  starts every seat, waits for `/health` (or a bounded port check), restarts \
                  failed children with bounded exponential backoff, and stops them on \
                  SIGINT/SIGTERM. `--apply-ownership` and `--apply-routing` opt in to applying \
                  each seat's xrandr ownership and xinput/evdev routing before spawn; both \
                  default off, a failed apply stops that spawn, and earlier seats are released. \
                  `--input-isolation helper` instead asks the privileged `ozma-seat-input` \
                  helper to EVIOCGRAB each seat's devices and create per-seat uinput devices; \
                  a missing helper or failed grab stops that spawn. `--seats-file` loads/reuses \
                  and atomically updates persisted seat identities; `--hotplug` reconciles \
                  read-only sysfs display/input events; and `--audio-router` ensures/removes \
                  per-seat null sinks. `--print-plan` and `--dry-run` never run routing, \
                  ownership, audio, input-isolation, or hotplug actions and never write the \
                  seat file."
)]
struct Args {
    /// Node binary or path passed to every planned seat.
    #[arg(long, default_value = DEFAULT_NODE_BIN, value_name = "PATH")]
    node_bin: PathBuf,

    /// First HID UDP port; seat N gets this value plus N.
    #[arg(long, default_value_t = 7331)]
    base_port: u16,

    /// First HTTP API port; seat N gets this value plus N.
    #[arg(long, default_value_t = 7382)]
    base_http_port: u16,

    /// Read-only status/metrics HTTP port on 127.0.0.1 (0 disables it).
    #[arg(long, default_value_t = 0)]
    status_port: u16,

    /// Initial restart delay after a failed child, in milliseconds.
    #[arg(long, default_value_t = 1000)]
    restart_backoff_ms: u64,

    /// Maximum restart delay after repeated failures, in milliseconds.
    #[arg(long, default_value_t = 30000)]
    restart_backoff_max_ms: u64,

    /// How long a newly spawned node has to become ready, in milliseconds.
    #[arg(long, default_value_t = 15000)]
    readiness_timeout_ms: u64,

    /// Interval between readiness probes, in milliseconds.
    #[arg(long, default_value_t = 200)]
    readiness_poll_ms: u64,

    /// Child poll/restart tick, in milliseconds.
    #[arg(long, default_value_t = 1000)]
    poll_interval_ms: u64,

    /// Uptime after which a child's failure count resets, in seconds.
    #[arg(long, default_value_t = 30)]
    healthy_uptime_secs: u64,

    /// Print the plan and exit without spawning.
    #[arg(long)]
    print_plan: bool,

    /// Record and print the launch plan without spawning any child.
    #[arg(long)]
    dry_run: bool,

    /// Apply each seat's xrandr output ownership before spawning (default off).
    #[arg(long)]
    apply_ownership: bool,

    /// Apply each seat's xinput/evdev input routing before spawning (default off).
    #[arg(long)]
    apply_routing: bool,

    /// Input path: `xinput` (default, existing map-to-output strategy) or
    /// `helper` (privileged `ozma-seat-input` EVIOCGRAB/uinput isolation).
    #[arg(long, default_value = "xinput", value_name = "xinput|helper")]
    input_isolation: String,

    /// Per-seat display: `shared` (default, one X root), `xvfb` (private Xvfb
    /// server per seat), or `xephyr` (private Xephyr nested in the shared
    /// display). Nested backends are opt-in and stopped with the seat.
    #[arg(long, default_value = "shared", value_name = "shared|xvfb|xephyr")]
    seat_display: String,

    /// Private X screen size for `--seat-display xvfb|xephyr`, for example
    /// `1280x800`.
    #[arg(long, default_value = "1280x800", value_name = "WxH")]
    seat_display_size: String,

    /// Helper binary used by `--input-isolation helper`.
    #[arg(long, default_value = ozma_multiseat::HELPER_PROGRAM, value_name = "PATH")]
    input_helper: PathBuf,

    /// Machine name used to build seat ids (`<machine>-seat-N`).
    #[arg(long, default_value = "ozma")]
    machine_name: String,

    /// Seat profile: desktop, gaming, workstation, media, kiosk, signage, custom.
    #[arg(long, default_value = "desktop")]
    profile: String,

    /// Requested seat count (default: one per enumerated display).
    #[arg(long)]
    seats: Option<usize>,

    /// Persisted seat store. Existing seat ids/ports/audio sinks are reused and
    /// the file is updated atomically after a successful replan. Reads/writes
    /// are skipped by `--print-plan` and `--dry-run`.
    #[arg(long, value_name = "PATH")]
    seats_file: Option<PathBuf>,

    /// Reconcile display/input hotplug events into seat start/stop.
    #[arg(long)]
    hotplug: bool,

    /// Create per-seat null sinks through ozma-audio before start and remove
    /// them after stop. Optional value: pipewire (default) or pulseaudio.
    #[arg(
        long,
        value_name = "pipewire|pulseaudio",
        num_args = 0..=1,
        default_missing_value = "pipewire"
    )]
    audio_router: Option<String>,

    /// Explicit X display for `xrandr` (default: ambient DISPLAY).
    #[arg(long, value_name = "DISPLAY")]
    display: Option<String>,

    /// Readiness probe: `health` (GET /health) or `port` (TCP connect only).
    #[arg(long, default_value = "health", value_name = "health|port")]
    readiness: String,

    /// Health endpoint path used by `--readiness health`.
    #[arg(long, default_value = DEFAULT_HEALTH_PATH)]
    health_path: String,

    /// Emit plan/status JSON instead of the human summary.
    #[arg(long)]
    json: bool,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), ozma_multiseat::MultiseatError> {
    let args = Args::parse();
    validate(&args)?;

    let display = resolve_display(&args)?;
    let display_backend = XrandrDisplayBackend::new(&display);
    let detection = XrandrDisplayBackend::detect(&display);
    if !detection.available {
        return Err(ozma_multiseat::MultiseatError::DisplayBackend(
            detection
                .reason
                .unwrap_or_else(|| format!("xrandr is unavailable for display {display}")),
        ));
    }

    let input_backend = SysfsInputBackend::new();
    let topology = HostTopology::enumerate(&display_backend, &input_backend)?;
    let drm = detect_drm();
    if !args.json {
        print_topology(&display, &topology, &drm);
    }

    let profile = args.profile.parse::<SeatProfile>()?;
    let planner = MultiseatPlanner::new(PlannerOptions {
        base_udp_port: args.base_port,
        base_http_port: args.base_http_port,
        machine_name: args.machine_name.clone(),
        node_program: args.node_bin.to_string_lossy().into_owned(),
        seat_count: args.seats,
        profile,
        audio_sink_prefix: "ozma-seat".to_owned(),
        x11_display: Some(display.clone()),
        ..PlannerOptions::default()
    });
    let (mut plan, mut store) = match args.seats_file.as_ref() {
        Some(path) => {
            let store = SeatStore::load(path)?;
            let persisted = topology.plan_with_store(&planner, &store)?;
            (persisted.plan, persisted.store)
        }
        None => {
            let plan = topology.plan(&planner)?;
            let store = SeatStore::from_plan(&plan);
            (plan, store)
        }
    };
    plan.display_backend = topology.display_backend.clone();
    plan.input_backend = topology.input_backend.clone();

    // Selecting `helper` changes routing intent before either the plan report
    // or the supervisor sees it. It never grabs anything by itself.
    if args.input_isolation == "helper" {
        for seat in &mut plan.seats {
            if let Some(route) = seat.input_route.as_mut() {
                route.enable_input_isolation();
                for (key, value) in route.environment() {
                    seat.command.env.insert(key, value);
                }
            }
        }
    }

    if args.print_plan {
        print_plan_report(&args, &display, &topology, &drm, &plan)?;
        return Ok(());
    }

    let seat_display_backend: SeatDisplayBackend = args.seat_display.parse()?;
    let seat_display_size: SeatDisplaySize = args.seat_display_size.parse()?;
    let seat_display_options = SeatDisplayOptions {
        backend: seat_display_backend,
        size: seat_display_size,
        ..SeatDisplayOptions::default()
    };

    let options = SupervisorOptions {
        node_bin: Some(args.node_bin.to_string_lossy().into_owned()),
        readiness_timeout: Duration::from_millis(args.readiness_timeout_ms),
        readiness_poll: Duration::from_millis(args.readiness_poll_ms),
        restart_backoff: Duration::from_millis(args.restart_backoff_ms),
        restart_backoff_max: Duration::from_millis(args.restart_backoff_max_ms),
        healthy_uptime: Duration::from_secs(args.healthy_uptime_secs),
        dry_run: args.dry_run,
        apply_ownership: args.apply_ownership,
        apply_routing: args.apply_routing,
        input_isolation: args.input_isolation == "helper",
        seat_display: seat_display_options.clone(),
    };

    if args.dry_run {
        print_plan_report(&args, &display, &topology, &drm, &plan)?;
        let mut supervisor = ProcessSeatSupervisor::new(options);
        for seat in &plan.seats {
            let status = supervisor.start(seat)?;
            print_status(&args, &status);
        }
        return Ok(());
    }

    let probe: Arc<dyn ReadinessProbe> = match args.readiness.as_str() {
        "health" => Arc::new(HttpHealthProbe::new(args.health_path.clone())),
        "port" => Arc::new(PortProbe::default()),
        other => {
            return Err(ozma_multiseat::MultiseatError::Supervisor(format!(
                "unknown readiness probe {other:?}; expected health or port"
            )))
        }
    };
    let mut supervisor =
        ProcessSeatSupervisor::with_parts(options, Arc::new(ProcessLauncher), probe);
    if seat_display_options.backend.is_nested() {
        supervisor = supervisor.with_display_launcher(Box::new(ProcessSeatDisplayLauncher::new(
            seat_display_options,
        )));
    }
    if args.apply_ownership {
        supervisor = supervisor.with_ownership_controller(Box::new(
            XrandrOwnershipController::new(Box::new(ProcessCommandRunner)),
        ));
    }
    if args.apply_routing {
        supervisor = supervisor
            .with_routing_controller(Box::new(XInputRouter::new(Box::new(ProcessCommandRunner))));
        // The planner emits xinput mapping today. Keeping the default evdev
        // router wired means an explicit evdev strategy fails with the typed
        // unsupported error rather than pretending a grab happened.
        supervisor = supervisor.with_evdev_routing_controller(Box::new(EvdevGrabRouter::new()));
    }
    if args.input_isolation == "helper" {
        supervisor = supervisor.with_input_isolation_helper(Box::new(HelperClient::new(
            HelperClientConfig {
                program: args.input_helper.to_string_lossy().into_owned(),
                ..HelperClientConfig::default()
            },
        )));
    }
    if let Some(kind) = args.audio_router.as_deref() {
        let router: Box<dyn ozma_multiseat::SeatAudioRouter> = match kind {
            "pipewire" => Box::new(OzmaAudioRouter::pipewire()),
            "pulseaudio" => Box::new(OzmaAudioRouter::pulseaudio()),
            other => {
                return Err(ozma_multiseat::MultiseatError::Audio(format!(
                    "unknown audio router {other:?}; expected pipewire or pulseaudio"
                )))
            }
        };
        supervisor = supervisor.with_audio_router(router);
    }

    // Persist the reconciled plan only in the real supervision path. Both
    // `--print-plan` and `--dry-run` returned above without touching the file.
    if let Some(path) = args.seats_file.as_ref() {
        store.save_atomic(path)?;
    }

    let mut started = 0usize;
    for seat in &plan.seats {
        match supervisor.start(seat) {
            Ok(status) => {
                if status.state == SeatState::Running {
                    started += 1;
                }
                print_status(&args, &status);
            }
            Err(error) => {
                eprintln!("seat {} failed to start: {error}", seat.config.id);
                if args.apply_ownership
                    || args.apply_routing
                    || args.input_isolation == "helper"
                    || args.seat_display != "shared"
                {
                    // The supervisor already released every applied resource
                    // when the later seat could not be reserved. Stop any
                    // children started by this plan so the CLI does not leave
                    // a partial seat set running, then print the final
                    // released/error state.
                    supervisor.stop_all();
                    for status in supervisor.status() {
                        print_status(&args, &status);
                    }
                    return Err(error);
                }
            }
        }
    }

    if started == 0 {
        supervisor.stop_all();
        return Err(ozma_multiseat::MultiseatError::Supervisor(
            "no seat became ready; not supervising a dead plan".to_owned(),
        ));
    }

    let snapshot = Arc::new(RwLock::new(MultiseatStatus::from_plan_with_runtime(
        &plan,
        &supervisor.status(),
    )));
    if args.status_port != 0 {
        spawn_status_server(args.status_port, Arc::clone(&snapshot)).map_err(|error| {
            ozma_multiseat::MultiseatError::Supervisor(format!(
                "binding status port {}: {error}",
                args.status_port
            ))
        })?;
        eprintln!(
            "status: http://127.0.0.1:{}/ (health, status, metrics)",
            args.status_port
        );
    }

    let mut reconciler = if args.hotplug {
        Some(
            SeatReconciler::new(
                planner.clone(),
                store.clone(),
                topology.displays.clone(),
                topology.inputs.clone(),
            )
            .with_options(HotplugOptions {
                debounce: DEFAULT_DEBOUNCE,
                ..HotplugOptions::default()
            })
            .with_backend_names(
                topology.display_backend.clone(),
                topology.input_backend.clone(),
            ),
        )
    } else {
        None
    };
    // Hotplug display events use the same Xrandr surface (names, geometry, and
    // X screen) as the initial plan, rather than the DRM-only model.
    let mut hotplug_monitor = SysfsHotplugMonitor::new()
        .with_display_backend(Box::new(XrandrDisplayBackend::new(display.clone())));
    let mut hotplug_debounce = HotplugDebounce::new(DEFAULT_DEBOUNCE);

    let poll = Duration::from_millis(args.poll_interval_ms);
    let shutdown = wait_for_shutdown();
    tokio::pin!(shutdown);

    loop {
        tokio::select! {
            _ = &mut shutdown => {
                eprintln!("shutdown signal received; stopping seats");
                break;
            }
            _ = tokio::time::sleep(poll) => {
                supervisor.tick();
                if let Some(reconciler) = reconciler.as_mut() {
                    match reconciler.poll_and_reconcile(
                        &mut hotplug_monitor,
                        &mut hotplug_debounce,
                        Instant::now(),
                        &mut supervisor,
                    ) {
                        Ok(report) => {
                            if let Some(updated) = report.plan {
                                plan = updated;
                            }
                            for diagnostic in &report.diagnostics {
                                eprintln!(
                                    "hotplug diagnostic {:?}{}: {}",
                                    diagnostic.code,
                                    diagnostic
                                        .seat_id
                                        .as_deref()
                                        .map(|seat| format!(" [{seat}]"))
                                        .unwrap_or_default(),
                                    diagnostic.message
                                );
                            }
                            if let Some(path) = args.seats_file.as_ref() {
                                if report.store != store {
                                    match report.store.save_atomic(path) {
                                        Ok(()) => store = report.store,
                                        Err(error) => eprintln!(
                                            "persisting seat store {} failed: {error}",
                                            path.display()
                                        ),
                                    }
                                }
                            }
                        }
                        Err(error) => eprintln!("hotplug poll failed: {error}"),
                    }
                }
                let status = MultiseatStatus::from_plan_with_runtime(&plan, &supervisor.status());
                let changed = {
                    let previous = snapshot.read().expect("status snapshot lock");
                    *previous != status
                };
                if changed {
                    if let Ok(mut current) = snapshot.write() {
                        *current = status.clone();
                    }
                    print_status_document(&args, &status);
                }
            }
        }
    }

    supervisor.stop_all();
    let final_status = MultiseatStatus::from_plan_with_runtime(&plan, &supervisor.status());
    if let Ok(mut current) = snapshot.write() {
        *current = final_status.clone();
    }
    print_status_document(&args, &final_status);

    Ok(())
}

fn validate(args: &Args) -> Result<(), ozma_multiseat::MultiseatError> {
    if args.base_port == 0 || args.base_http_port == 0 {
        return Err(ozma_multiseat::MultiseatError::Supervisor(
            "--base-port and --base-http-port must be non-zero".to_owned(),
        ));
    }
    if args.restart_backoff_ms == 0 || args.restart_backoff_max_ms == 0 {
        return Err(ozma_multiseat::MultiseatError::Supervisor(
            "--restart-backoff-ms and --restart-backoff-max-ms must be greater than zero"
                .to_owned(),
        ));
    }
    if args.restart_backoff_max_ms < args.restart_backoff_ms {
        return Err(ozma_multiseat::MultiseatError::Supervisor(
            "--restart-backoff-max-ms must be greater than or equal to --restart-backoff-ms"
                .to_owned(),
        ));
    }
    if args.readiness_timeout_ms == 0 || args.readiness_poll_ms == 0 {
        return Err(ozma_multiseat::MultiseatError::Supervisor(
            "--readiness-timeout-ms and --readiness-poll-ms must be greater than zero".to_owned(),
        ));
    }
    if args.poll_interval_ms == 0 {
        return Err(ozma_multiseat::MultiseatError::Supervisor(
            "--poll-interval-ms must be greater than zero".to_owned(),
        ));
    }
    if !matches!(args.readiness.as_str(), "health" | "port") {
        return Err(ozma_multiseat::MultiseatError::Supervisor(format!(
            "unknown readiness probe {:?}; expected health or port",
            args.readiness
        )));
    }
    if !matches!(args.input_isolation.as_str(), "xinput" | "helper") {
        return Err(ozma_multiseat::MultiseatError::Supervisor(format!(
            "unknown input isolation {:?}; expected xinput or helper",
            args.input_isolation
        )));
    }
    if !matches!(args.seat_display.as_str(), "shared" | "xvfb" | "xephyr") {
        return Err(ozma_multiseat::MultiseatError::SeatDisplay(format!(
            "unknown seat display {:?}; expected shared, xvfb, or xephyr",
            args.seat_display
        )));
    }
    let _: SeatDisplaySize = args.seat_display_size.parse()?;
    if args.input_isolation == "helper" && args.apply_routing {
        return Err(ozma_multiseat::MultiseatError::Supervisor(
            "--input-isolation helper and --apply-routing are mutually exclusive; the helper \
             grabs the seat's devices, while xinput mapping expects to share them"
                .to_owned(),
        ));
    }
    if let Some(kind) = args.audio_router.as_deref() {
        if !matches!(kind, "pipewire" | "pulseaudio") {
            return Err(ozma_multiseat::MultiseatError::Audio(format!(
                "unknown audio router {kind:?}; expected pipewire or pulseaudio"
            )));
        }
    }
    Ok(())
}

fn resolve_display(args: &Args) -> Result<String, ozma_multiseat::MultiseatError> {
    if let Some(display) = args
        .display
        .as_deref()
        .map(str::trim)
        .filter(|display| !display.is_empty())
    {
        return Ok(display.to_owned());
    }
    match std::env::var("DISPLAY") {
        Ok(display) if !display.trim().is_empty() => Ok(display.trim().to_owned()),
        _ => Err(ozma_multiseat::MultiseatError::Supervisor(
            "no X display configured; pass --display or set DISPLAY (ambient DISPLAY is never \
             guessed)"
                .to_owned(),
        )),
    }
}

fn print_topology(display: &str, topology: &HostTopology, drm: &DrmDetection) {
    println!(
        "topology: display={display} backend={} ({} output(s)), input={} ({} device(s))",
        topology.display_backend,
        topology.displays.len(),
        topology.input_backend,
        topology.inputs.len()
    );
    for output in &topology.displays {
        println!(
            "  display [{}] {} {}{}",
            output.index,
            output.name,
            output.geometry(),
            if output.primary { " primary" } else { "" }
        );
    }
    for device in &topology.inputs {
        println!(
            "  input {} {:?} hub={:?}",
            device.path, device.kind, device.usb_hub
        );
    }
    println!(
        "  drm: available={} connectors={}{}",
        drm.available,
        drm.connectors.len(),
        drm.reason
            .as_deref()
            .map(|reason| format!(" ({reason})"))
            .unwrap_or_default()
    );
}

fn print_plan_report(
    args: &Args,
    display: &str,
    topology: &HostTopology,
    drm: &DrmDetection,
    plan: &MultiseatPlan,
) -> Result<(), ozma_multiseat::MultiseatError> {
    if args.json {
        let report = serde_json::json!({
            "ok": true,
            "topology": {
                "x11_display": display,
                "display_backend": topology.display_backend,
                "input_backend": topology.input_backend,
                "displays": &topology.displays,
                "inputs": &topology.inputs,
                "drm": drm,
            },
            "plan": plan,
            "status": MultiseatStatus::from_plan(plan),
        });
        let encoded = serde_json::to_string_pretty(&report).map_err(|error| {
            ozma_multiseat::MultiseatError::Supervisor(format!("serializing plan: {error}"))
        })?;
        println!("{encoded}");
        return Ok(());
    }

    println!(
        "plan: {} seat(s), {} node(s) [{}]",
        plan.seat_count(),
        plan.node_count(),
        plan.display_backend
    );
    for seat in &plan.seats {
        println!(
            "  {}: display [{}] {} {}{}",
            seat.config.id,
            seat.display.index,
            seat.config.display_output,
            seat.display.geometry(),
            if seat.display.primary { " primary" } else { "" }
        );
        println!(
            "    ports: udp={} http={} audio_sink={} profile={}",
            seat.config.udp_port,
            seat.config.http_port,
            seat.config.audio_sink,
            seat.config.profile
        );
        println!(
            "    inputs: {}",
            if seat.config.input_devices.is_empty() {
                "(none assigned)".to_owned()
            } else {
                seat.config.input_devices.join(", ")
            }
        );
        if let Some(ownership) = &seat.ownership {
            println!(
                "    ownership: {} {} mode={} pos={} [{}]",
                ownership.kind.as_str(),
                ownership.output,
                ownership.mode,
                ownership.position(),
                if ownership.x_screen.is_empty() {
                    "(no X screen)"
                } else {
                    &ownership.x_screen
                }
            );
        }
        if let Some(route) = &seat.input_route {
            println!(
                "    input route: {} hub={} pointers={} grab={}",
                route.strategy.as_str(),
                route.hub_path,
                if route.pointer_devices.is_empty() {
                    "(none)".to_owned()
                } else {
                    route.pointer_devices.join(", ")
                },
                if route.grab_exclusive {
                    if route.strategy == ozma_multiseat::InputRoutingStrategy::InputIsolation {
                        "helper"
                    } else {
                        "modelled"
                    }
                } else {
                    "no"
                }
            );
        }
        println!("    argv: {}", seat.node_argv().join(" "));
        if !seat.command.env.is_empty() {
            let env = seat
                .command
                .env
                .iter()
                .map(|(key, value)| format!("{key}={value}"))
                .collect::<Vec<_>>()
                .join(" ");
            println!("    env: {env}");
        }
    }
    for diagnostic in &plan.diagnostics {
        match &diagnostic.seat_id {
            Some(seat) => println!(
                "  diagnostic {:?} [{}]: {}",
                diagnostic.code, seat, diagnostic.message
            ),
            None => println!("  diagnostic {:?}: {}", diagnostic.code, diagnostic.message),
        }
    }
    Ok(())
}

fn print_status(args: &Args, status: &ozma_multiseat::SeatRuntimeStatus) {
    if args.json {
        match serde_json::to_string(status) {
            Ok(encoded) => println!("{encoded}"),
            Err(error) => eprintln!("serializing seat status: {error}"),
        }
        return;
    }
    let mut line = format!(
        "  {}: {} pid={} udp={} http={} restarts={}",
        status.seat_id,
        status.state.as_str(),
        status
            .pid
            .map(|pid| pid.to_string())
            .unwrap_or_else(|| "-".to_owned()),
        status
            .udp_port
            .map(|port| port.to_string())
            .unwrap_or_else(|| "-".to_owned()),
        status
            .http_port
            .map(|port| port.to_string())
            .unwrap_or_else(|| "-".to_owned()),
        status.restarts
    );
    if let Some(delay) = status.next_restart_in_ms {
        line.push_str(&format!(" restart_in={delay}ms"));
    }
    if let Some(error) = &status.last_error {
        line.push_str(&format!(" error={error}"));
    }
    if !status.resources.is_empty() {
        line.push_str(&resource_summary(&status.resources));
    }
    if let Some(display) = display_summary(&status.display) {
        line.push_str(&display);
    }
    if let Some(message) = &status.message {
        line.push_str(&format!(" ({message})"));
    }
    println!("{line}");
}

fn resource_state(applied: bool, released: bool) -> &'static str {
    if applied {
        "applied"
    } else if released {
        "released"
    } else {
        "none"
    }
}

fn display_summary(display: &SeatDisplayStatus) -> Option<String> {
    if display.display_backend == SeatDisplayBackend::Shared {
        return None;
    }
    let mut summary = format!(" display({}", display.display_backend.as_str());
    if let Some(name) = &display.display {
        summary.push_str(&format!(" {name}"));
    }
    if let Some(pid) = display.display_pid {
        summary.push_str(&format!(" pid={pid}"));
    }
    summary.push_str(&format!(
        " ready={}",
        if display.display_ready {
            "true"
        } else {
            "false"
        }
    ));
    if let Some(error) = &display.display_error {
        summary.push_str(&format!(" error[{}]={}", error.kind, error.message));
    }
    for note in &display.display_notes {
        summary.push_str(&format!(" note[{}]={}", note.kind.as_str(), note.message));
    }
    summary.push(')');
    Some(summary)
}

fn resource_summary(resources: &SeatResourceStatus) -> String {
    let mut summary = format!(
        " resources(ownership={} routing={} isolation={} audio={})",
        resource_state(resources.ownership_applied, resources.ownership_released),
        resource_state(resources.routing_applied, resources.routing_released),
        resource_state(
            resources.input_isolation_applied,
            resources.input_isolation_released
        ),
        resource_state(resources.audio_applied, resources.audio_released),
    );
    if resources.grabbed_device_count != 0 || !resources.uinput_devices.is_empty() {
        summary.push_str(&format!(
            " grabbed={} uinput={}",
            resources.grabbed_device_count,
            if resources.uinput_devices.is_empty() {
                "(none)".to_owned()
            } else {
                resources.uinput_devices.join(",")
            }
        ));
    }
    for error in &resources.errors {
        summary.push_str(&format!(
            " resource_error[{} {}]={}",
            error.kind.as_str(),
            error.action.as_str(),
            error.message
        ));
    }
    summary
}

fn print_status_document(args: &Args, status: &MultiseatStatus) {
    if args.json {
        match serde_json::to_string(status) {
            Ok(encoded) => println!("{encoded}"),
            Err(error) => eprintln!("serializing status: {error}"),
        }
        return;
    }
    println!(
        "status: {} seat(s) / {} node(s), running={} failed={} stopped={} planned={}",
        status.capability.seat_count,
        status.capability.node_count,
        count_state(status, SeatState::Running),
        count_state(status, SeatState::Failed),
        count_state(status, SeatState::Stopped),
        count_state(status, SeatState::Planned)
    );
    for seat in &status.seats {
        let mut line = format!(
            "  {}: {} pid={} udp={} http={} restarts={}",
            seat.seat_id,
            seat.state.as_str(),
            seat.pid
                .map(|pid| pid.to_string())
                .unwrap_or_else(|| "-".to_owned()),
            seat.udp_port,
            seat.http_port,
            seat.restarts
        );
        if let Some(delay) = seat.next_restart_in_ms {
            line.push_str(&format!(" restart_in={delay}ms"));
        }
        if let Some(error) = &seat.last_error {
            line.push_str(&format!(" error={error}"));
        }
        if !seat.resources.is_empty() {
            line.push_str(&resource_summary(&seat.resources));
        }
        if let Some(display) = display_summary(&seat.display) {
            line.push_str(&display);
        }
        if let Some(message) = &seat.message {
            line.push_str(&format!(" ({message})"));
        }
        println!("{line}");
    }
    for diagnostic in &status.diagnostics {
        println!("  diagnostic {:?}: {}", diagnostic.code, diagnostic.message);
    }
}

fn count_state(status: &MultiseatStatus, state: SeatState) -> usize {
    status
        .seats
        .iter()
        .filter(|seat| seat.state == state)
        .count()
}

async fn wait_for_shutdown() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut interrupt = signal(SignalKind::interrupt()).expect("SIGINT handler");
        let mut terminate = signal(SignalKind::terminate()).expect("SIGTERM handler");
        tokio::select! {
            _ = interrupt.recv() => {}
            _ = terminate.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

fn spawn_status_server(
    port: u16,
    snapshot: Arc<RwLock<MultiseatStatus>>,
) -> std::io::Result<thread::JoinHandle<()>> {
    let listener = TcpListener::bind(("127.0.0.1", port))?;
    Ok(thread::spawn(move || {
        for stream in listener.incoming() {
            match stream {
                Ok(stream) => {
                    let snapshot = Arc::clone(&snapshot);
                    thread::spawn(move || handle_status_request(stream, snapshot, port));
                }
                Err(_) => continue,
            }
        }
    }))
}

fn handle_status_request(
    mut stream: TcpStream,
    snapshot: Arc<RwLock<MultiseatStatus>>,
    status_port: u16,
) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
    let mut request = [0u8; 4096];
    let read = match stream.read(&mut request) {
        Ok(read) => read,
        Err(_) => return,
    };
    let request = String::from_utf8_lossy(&request[..read]);
    let path = request
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .unwrap_or("/");

    let status = match snapshot.read() {
        Ok(status) => status.clone(),
        Err(_) => {
            write_response(
                &mut stream,
                "500 Internal Server Error",
                "application/json",
                "{\"ok\":false,\"error\":\"status unavailable\"}",
            );
            return;
        }
    };

    let (code, content_type, body) = match path {
        "/health" => {
            let body = serde_json::json!({
                "ok": true,
                "profile": "multiseat",
                "seat_count": status.capability.seat_count,
                "node_count": status.capability.node_count,
                "running": count_state(&status, SeatState::Running),
                "failed": count_state(&status, SeatState::Failed),
            });
            (
                "200 OK",
                "application/json",
                serde_json::to_string(&body).unwrap_or_else(|_| "{}".to_owned()),
            )
        }
        "/status" => (
            "200 OK",
            "application/json",
            serde_json::to_string(&status).unwrap_or_else(|_| "{}".to_owned()),
        ),
        "/metrics" => (
            "200 OK",
            "text/plain; version=0.0.4",
            render_metrics(&status),
        ),
        "/api/v1/connection" => {
            let body = serde_json::json!({
                "ok": true,
                "node_id": "ozma-multiseat",
                "profile": "multiseat",
                "version": env!("CARGO_PKG_VERSION"),
                "api_port": status_port,
                "quic_port": 0,
                "hid_udp_port": 0,
                "sink_kind": "supervisor",
                "seat_count": status.capability.seat_count,
                "node_count": status.capability.node_count,
            });
            (
                "200 OK",
                "application/json",
                serde_json::to_string(&body).unwrap_or_else(|_| "{}".to_owned()),
            )
        }
        _ => (
            "404 Not Found",
            "application/json",
            "{\"ok\":false,\"error\":\"not found\"}".to_owned(),
        ),
    };

    write_response(&mut stream, code, content_type, &body);
}

fn write_response(stream: &mut TcpStream, code: &str, content_type: &str, body: &str) {
    let response = format!(
        "HTTP/1.1 {code}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes());
}
