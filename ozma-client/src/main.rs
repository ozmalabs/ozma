// SPDX-License-Identifier: AGPL-3.0-only
//! Headless native Ozma client.
//!
//! `list`, `get`, and `activate` exercise the controller control plane.
//! `connect` discovers a node through the controller, creates and starts a
//! node session, fetches the native QUIC transport, and runs the probe loop
//! against the live session. `probe` remains the lower-level M1 acceptance
//! harness: it takes an existing offer, performs `Hello`/`HelloAck`, waits for
//! one video datagram, sends `ReleaseAll`, and closes.
//! `probe --input-latency` additionally measures client send to the first
//! client-visible encoded frame. It does not measure decode, display scanout,
//! or input-to-photon latency.
//!
//! Neither command decodes live video or needs a GUI.

#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use clap::{Parser, Subcommand, ValueEnum};
use ozma_decode::VideoDecoder as _;
use ozma_client::{
    codes, fetch_transport_info, load_certificate, ClientEvent, ControllerClient, ControllerNode,
    NativeClientConfig, NativeSessionClient, NodeApiClient, VideoAssembler, VideoFrame,
};
use ozma_protocol::{
    ApiError, AudioSpec, ChannelKind, ClientId, ConsentRequirement, DisplayMode, InputEvent,
    InputSpec, LocalDisplayAction, PointerMode, PrivacyPolicy, Session, SessionOffer, SessionSpec,
    SessionState, TrackId, TrackKind, TrackSpec, TransportPreference,
};
use serde_json::json;

#[derive(Debug, Parser)]
#[command(
    name = "ozma-client",
    version,
    about = "Native Ozma controller and session client",
    long_about = None
)]
struct Cli {
    /// Controller base URL, for example http://controller:7380.
    #[arg(long, global = true, env = "OZMA_CONTROLLER_URL", value_name = "URL")]
    controller: Option<String>,

    /// Optional bearer token for the controller or node API.
    #[arg(
        long = "controller-token",
        global = true,
        env = "OZMA_CONTROLLER_TOKEN",
        value_name = "TOKEN"
    )]
    controller_token: Option<String>,

    /// Accept invalid TLS certificates for the controller/node HTTP API.
    ///
    /// QUIC pinning still requires the node certificate (--cert or --node).
    #[arg(long, global = true)]
    insecure: bool,

    /// Emit machine-readable JSON.
    #[arg(long, global = true)]
    json: bool,

    #[command(subcommand)]
    command: Command,
}

/// Consent requirement requested in the session spec.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum ConsentArg {
    /// Never ask the node's local operator (unless node policy overrides it).
    Never,
    /// Ask only when the node is a workstation.
    Workstation,
    /// Always ask the node's local operator.
    Always,
}

impl From<ConsentArg> for ConsentRequirement {
    fn from(value: ConsentArg) -> Self {
        match value {
            ConsentArg::Never => ConsentRequirement::Never,
            ConsentArg::Workstation => ConsentRequirement::Workstation,
            ConsentArg::Always => ConsentRequirement::Always,
        }
    }
}

#[derive(Debug, Subcommand)]
enum Command {
    /// List nodes known to the controller.
    List,

    /// Make a controller node active.
    Activate {
        /// Node id, exactly as returned by `list`.
        node: String,
    },

    /// Fetch one controller node.
    Get {
        /// Node id, exactly as returned by `list`.
        node: String,
    },

    /// Discover a node through the controller, start a session, and probe it.
    Connect {
        /// Node id, exactly as returned by `list`.
        #[arg(long, value_name = "ID")]
        node_id: String,

        /// Capture source id. Omit to let the node choose its default source.
        #[arg(long, value_name = "PATH|ID")]
        source: Option<String>,

        /// Requested capture width (with --height/--fps).
        #[arg(long, default_value_t = 1920, value_parser = clap::value_parser!(u32).range(1..))]
        width: u32,

        /// Requested capture height (with --width/--fps).
        #[arg(long, default_value_t = 1080, value_parser = clap::value_parser!(u32).range(1..))]
        height: u32,

        /// Requested capture refresh rate in Hz.
        #[arg(long, default_value_t = 30, value_parser = clap::value_parser!(u32).range(1..))]
        fps: u32,

        /// Request audio in addition to video.
        #[arg(long)]
        audio: bool,

        /// Write the first video payload to this file.
        #[arg(long, value_name = "FILE")]
        save_video: Option<PathBuf>,

        /// Decode the first complete access unit and write a PPM image here.
        #[arg(long, value_name = "FILE")]
        save_frame: Option<PathBuf>,

        /// Seconds to wait for HelloAck and for the first video datagram.
        #[arg(long, default_value_t = 10, value_name = "SECONDS")]
        timeout: u64,

        /// Consent policy to request from the node.
        #[arg(long, value_enum, default_value_t = ConsentArg::Never)]
        consent: ConsentArg,

        /// TLS server name for certificate validation.
        #[arg(long, default_value = "localhost")]
        server_name: String,
    },

    /// Connect to a node and measure receipt of a complete video frame.
    Probe {
        /// A SessionOffer JSON file, or inline JSON beginning with `{`.
        #[arg(long, value_name = "JSON|PATH")]
        offer: String,

        /// DER or PEM certificate file. Overrides --node certificate fetching.
        #[arg(long, value_name = "FILE")]
        cert: Option<PathBuf>,

        /// Node API base URL; fetch /api/v1/transport to obtain the DER cert.
        #[arg(long, env = "OZMA_NODE_URL", value_name = "URL")]
        node: Option<String>,

        /// Override the session token from the offer.
        #[arg(long, env = "OZMA_SESSION_TOKEN", value_name = "TOKEN")]
        token: Option<String>,

        /// TLS server name for certificate validation.
        #[arg(long, default_value = "localhost")]
        server_name: String,

        /// Seconds to wait for HelloAck and for the first video datagram.
        #[arg(long, default_value_t = 10, value_name = "SECONDS")]
        timeout: u64,

        /// Write the first video payload to this file.
        #[arg(long, value_name = "FILE")]
        save_video: Option<PathBuf>,

        /// Decode the first complete access unit and write a PPM image here.
        #[arg(long, value_name = "FILE")]
        save_frame: Option<PathBuf>,

        /// Negotiate scene support and wait for at least this overlay revision.
        #[arg(long, value_name = "REVISION", value_parser = clap::value_parser!(u64).range(1..))]
        scene_revision: Option<u64>,

        /// Measure input-to-encoded-frame latency with a harmless ReleaseAll
        /// probe instead of only waiting for the first video frame.
        ///
        /// This measures client send to client-visible encoded frame arrival,
        /// not decode, display scanout, or photon latency.
        #[arg(long)]
        input_latency: bool,
    },
}

#[tokio::main]
async fn main() -> ExitCode {
    init_tracing();
    let cli = Cli::parse();
    match run(cli).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error [{}]: {}", error.code, error.message);
            if let Some(remediation) = &error.remediation {
                eprintln!("  remediation: {remediation}");
            }
            if !error.details.is_null() {
                eprintln!("  details: {}", error.details);
            }
            ExitCode::FAILURE
        }
    }
}

fn init_tracing() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn"));
    let _ = tracing_subscriber::fmt().with_env_filter(filter).try_init();
}

#[derive(Clone, Debug)]
struct GlobalOptions {
    controller: Option<String>,
    controller_token: Option<String>,
    insecure: bool,
    json: bool,
}

async fn run(cli: Cli) -> Result<(), ApiError> {
    let options = GlobalOptions {
        controller: cli.controller,
        controller_token: cli.controller_token,
        insecure: cli.insecure,
        json: cli.json,
    };
    match cli.command {
        Command::List => list(&options).await,
        Command::Activate { node } => activate(&options, &node).await,
        Command::Get { node } => get(&options, &node).await,
        Command::Connect {
            node_id,
            source,
            width,
            height,
            fps,
            audio,
            save_video,
            save_frame,
            timeout,
            consent,
            server_name,
        } => {
            let args = ConnectArgs {
                node_id,
                source,
                width,
                height,
                fps,
                audio,
                save_video,
                save_frame,
                timeout,
                consent,
                server_name,
            };
            connect(&options, args).await
        }
        Command::Probe {
            offer,
            cert,
            node,
            token,
            server_name,
            timeout,
            save_video,
            save_frame,
            scene_revision,
            input_latency,
        } => {
            probe(
                &options,
                &offer,
                cert,
                node,
                token,
                server_name,
                timeout,
                save_video,
                save_frame,
                scene_revision,
                input_latency,
            )
            .await
        }
    }
}

fn controller(options: &GlobalOptions) -> Result<ControllerClient, ApiError> {
    let base_url = options.controller.as_deref().ok_or_else(|| {
        ApiError::new(codes::CLIENT_CONFIG, "No controller URL configured")
            .with_remediation("Pass --controller URL or set OZMA_CONTROLLER_URL.")
    })?;
    let mut builder = ControllerClient::builder(base_url).insecure(options.insecure);
    if let Some(token) = options.controller_token.as_deref() {
        builder = builder.token(token);
    }
    builder.build()
}

async fn list(options: &GlobalOptions) -> Result<(), ApiError> {
    let client = controller(options)?;
    let list = client.list_nodes_response().await?;
    if options.json {
        print_json(&list)?;
        return Ok(());
    }

    if list.nodes.is_empty() {
        println!("No nodes are registered with the controller.");
        return Ok(());
    }
    println!(
        "{:<2}  {:<24}  {:<24}  {:<22}  {:<6}  {:<8}  CAPABILITIES",
        "", "ID", "NAME", "HOST", "PORT", "ONLINE"
    );
    for node in &list.nodes {
        let active = list.active_node_id.as_deref() == Some(node.id.as_str()) || node.active;
        println!(
            "{:<2}  {:<24}  {:<24}  {:<22}  {:<6}  {:<8}  {}",
            if active { "*" } else { "" },
            node.id,
            node.display_name(),
            node.host.as_deref().unwrap_or("-"),
            node.api_port
                .map(|port| port.to_string())
                .unwrap_or_else(|| "-".to_string()),
            if node.online { "online" } else { "offline" },
            node.capabilities.join(",")
        );
    }
    if let Some(active) = list.active_node_id {
        println!("\nActive node: {active}");
    }
    Ok(())
}

async fn activate(options: &GlobalOptions, node_id: &str) -> Result<(), ApiError> {
    let client = controller(options)?;
    let response = client.activate_node(node_id).await?;
    if options.json {
        print_json(&response)?;
    } else {
        println!(
            "Activated {}",
            response.active_node_id.as_deref().unwrap_or(node_id)
        );
    }
    Ok(())
}

async fn get(options: &GlobalOptions, node_id: &str) -> Result<(), ApiError> {
    let client = controller(options)?;
    let node = client.get_node(node_id).await?;
    if options.json {
        print_json(&node)?;
    } else {
        println!("id:            {}", node.id);
        println!("name:          {}", node.display_name());
        println!("host:          {}", node.host.as_deref().unwrap_or("-"));
        println!(
            "api_port:      {}",
            node.api_port
                .map(|port| port.to_string())
                .unwrap_or_else(|| "-".to_string())
        );
        println!("online:        {}", node.online);
        println!("active:        {}", node.active);
        println!("capabilities:  {}", node.capabilities.join(","));
        if let Some(role) = &node.role {
            println!("role:          {role}");
        }
        if let Some(machine_class) = &node.machine_class {
            println!("machine_class: {machine_class}");
        }
    }
    Ok(())
}

#[derive(Clone, Debug)]
struct ConnectArgs {
    node_id: String,
    source: Option<String>,
    width: u32,
    height: u32,
    fps: u32,
    audio: bool,
    save_video: Option<PathBuf>,
    save_frame: Option<PathBuf>,
    timeout: u64,
    consent: ConsentArg,
    server_name: String,
}

/// The M1 control-plane loop: controller discovery + activation, node session
/// creation (and consent), transport discovery, then the existing QUIC probe.
async fn connect(options: &GlobalOptions, args: ConnectArgs) -> Result<(), ApiError> {
    let controller = controller(options)?;
    let node = controller
        .get_node(&args.node_id)
        .await
        .map_err(|error| step_error("look up the node in the controller", error))?;
    if !node.online {
        return Err(ApiError::new(
            codes::CLIENT_CONTROLLER,
            format!(
                "The controller reports node '{}' as offline",
                node.display_name()
            ),
        )
        .with_details(json!({ "node_id": node.id, "host": node.host }))
        .with_remediation(
            "Power on the node, confirm its registration, then retry `ozma-client list`.",
        ));
    }

    let activation = controller
        .activate_node(&node.id)
        .await
        .map_err(|error| step_error("activate the node in the controller", error))?;
    if !activation.ok && activation.active_node_id.as_deref() != Some(node.id.as_str()) {
        return Err(ApiError::new(
            codes::CLIENT_CONTROLLER,
            format!("The controller did not activate node '{}'", node.id),
        )
        .with_details(json!({ "response": activation }))
        .with_remediation("Check the controller's node registry and activation policy."));
    }

    let node_url = node_api_url(&node)?;
    let mut builder = NodeApiClient::builder(&node_url).insecure(options.insecure);
    if let Some(token) = options.controller_token.as_deref() {
        builder = builder.token(token);
    }
    let node_api = builder.build().map_err(|error| {
        step_error(
            &format!("build a client for the node API at {node_url}"),
            error,
        )
    })?;

    let spec = build_session_spec(&args);
    let (mut session, mut offer) = node_api
        .create_session(&spec)
        .await
        .map_err(|error| step_error(&format!("create a session on {node_url}"), error))?;

    if session.state == SessionState::PendingConsent {
        session = node_api
            .approve_consent(&session.id, "ozma-client")
            .await
            .map_err(|error| {
                step_error(
                    &format!("approve consent for session '{}'", session.id),
                    error,
                )
            })?;
        if session.state == SessionState::PendingConsent {
            return Err(ApiError::new(
                ozma_protocol::codes::CONSENT_REQUIRED,
                "The node still requires local consent after the approval request",
            )
            .with_details(json!({ "session_id": session.id }))
            .with_remediation(
                "Approve the session on the node's local console, or request --consent never \
                 where node policy allows it.",
            ));
        }
    }

    if session.state.is_terminal() {
        return Err(session_failure(&session, "creating the session"));
    }

    if matches!(
        session.state,
        SessionState::Created | SessionState::Offered | SessionState::Preparing
    ) {
        session = node_api.start_session(&session.id).await.map_err(|error| {
            step_error(
                &format!("start session '{}' on {node_url}", session.id),
                error,
            )
        })?;
    }

    if session.state.is_terminal() {
        return Err(session_failure(&session, "starting the session"));
    }
    if !session.state.is_active() {
        return Err(ApiError::new(
            codes::CLIENT_PROTOCOL,
            format!(
                "The node left session '{}' in state {:?} instead of starting it",
                session.id, session.state
            ),
        )
        .with_details(json!({ "session": session }))
        .with_remediation(
            "Check the node session log and whether another client holds the session.",
        ));
    }

    if offer.is_none() {
        offer = node_api
            .get_session_with_offer(&session.id)
            .await
            .map_err(|error| {
                step_error(
                    &format!("fetch the transport offer for session '{}'", session.id),
                    error,
                )
            })?
            .1;
    }
    let offer = offer.ok_or_else(|| {
        ApiError::new(
            ozma_protocol::codes::TRANSPORT_UNAVAILABLE,
            format!(
                "The node created session '{}' but returned no transport offer",
                session.id
            ),
        )
        .with_details(json!({ "session_id": session.id, "node_url": node_url }))
        .with_remediation(
            "Confirm the node has the native QUIC transport enabled by fetching \
             GET /api/v1/transport from the node.",
        )
    })?;

    let transport = node_api.transport_info().await.map_err(|error| {
        step_error(
            &format!("fetch the native transport from {node_url}"),
            error,
        )
    })?;

    let timeout = Duration::from_secs(args.timeout.max(1));
    let mut config = NativeClientConfig::new().with_server_name(args.server_name.clone());
    config.hello_timeout = timeout;
    config.handshake_timeout = timeout.min(Duration::from_secs(5));
    let started = Instant::now();

    if !options.json {
        println!("node:         {} ({})", node.display_name(), node.id);
        println!("controller:   {}", controller.base_url());
        println!("node api:     {node_url}");
        println!(
            "activated:    {}",
            activation.active_node_id.as_deref().unwrap_or(&node.id)
        );
        println!("session:      {} ({:?})", session.id, session.state);
        println!("transport:    {}", transport.endpoint());
    }

    let mut client =
        NativeSessionClient::connect_with_config(&offer, &transport.certificate_der, config)
            .await
            .map_err(|error| {
                step_error(
                    &format!(
                        "connect to the node QUIC endpoint '{}' for session '{}'",
                        transport.endpoint(),
                        offer.session_id
                    ),
                    error,
                )
            })?;

    let session_id = client
        .session()
        .map(|session| session.id.clone())
        .unwrap_or_else(|| offer.session_id.clone());
    let state = client.state();
    let permissions = client.permissions();
    let endpoint = client
        .transport_descriptor()
        .endpoint
        .clone()
        .unwrap_or_else(|| transport.endpoint().to_string());

    let measurement = wait_for_probe(&mut client, timeout, started, None).await;

    // Release before closing on every path. Input may be view-only, in which
    // case release_all is a no-op.
    let release = client.release_all().await;
    let close = client.close().await;

    let ProbeMeasurement {
        packet,
        frame,
        video_ms,
        ..
    } = measurement.map_err(|error| {
        step_error(
            &format!(
                "wait for video from session '{}' on {}",
                session_id,
                transport.endpoint()
            ),
            error,
        )
    })?;
    release?;
    close?;

    let outputs = write_outputs(
        &frame,
        args.save_video.as_deref(),
        args.save_frame.as_deref(),
    )
    .await?;

    if options.json {
        let report = json!({
            "controller": controller.base_url(),
            "node": {
                "id": node.id,
                "name": node.display_name(),
                "host": node.host,
                "api_port": node.api_port,
            },
            "session_id": session_id,
            "state": state,
            "permissions": permissions,
            "endpoint": endpoint,
            "offer": {
                "session_id": offer.session_id,
                "expires_at": offer.expires_at,
            },
            "video": {
                "ready_ms": video_ms,
                "channel": packet.channel.raw(),
                "kind": packet.kind,
                "track_id": packet.track_id,
                "sequence": frame.sequence,
                "keyframe": frame.keyframe,
                "payload_len": frame.data.len(),
                "saved_to": outputs.video_path,
                "decoded": outputs.decoded.as_ref().map(|decoded| json!({
                    "width": decoded.width,
                    "height": decoded.height,
                    "frame_saved_to": outputs.frame_path,
                })),
            }
        });
        print_json(&report)?;
    } else {
        println!("session id:   {session_id}");
        println!(
            "state:        {}",
            state
                .map(|state| format!("{state:?}"))
                .unwrap_or_else(|| "-".into())
        );
        println!(
            "permissions:  {}",
            permissions
                .iter()
                .map(|permission| permission.to_string())
                .collect::<Vec<_>>()
                .join(",")
        );
        println!("endpoint:     {endpoint}");
        println!(
            "video:        channel {} kind {:?} track {} sequence {} payload {} bytes (keyframe {})",
            packet.channel.raw(),
            packet.kind,
            packet
                .track_id
                .as_ref()
                .map(|track| track.as_str())
                .unwrap_or("-"),
            frame.sequence,
            frame.data.len(),
            frame.keyframe
        );
        println!("video ready:  {video_ms} ms");
        if let Some(path) = &outputs.video_path {
            println!("saved:        {path}");
        }
        if let Some(decoded) = &outputs.decoded {
            println!(
                "decoded:      {}x{} ({} bytes YUV)",
                decoded.width, decoded.height, decoded.yuv_len
            );
        }
        println!("released all held input and closed the session.");
    }
    Ok(())
}

fn node_api_url(node: &ControllerNode) -> Result<String, ApiError> {
    let host = node
        .host
        .as_deref()
        .map(str::trim)
        .filter(|host| !host.is_empty())
        .ok_or_else(|| {
            ApiError::new(
                codes::CLIENT_CONFIG,
                format!(
                    "The controller did not report a host address for node '{}'",
                    node.id
                ),
            )
            .with_remediation("Check the node's registration and refresh `ozma-client list`.")
        })?;

    let host = if host.contains("://") {
        host.to_string()
    } else if host.contains(':') && !host.starts_with('[') {
        // Bare IPv6 address: the URL parser requires brackets.
        format!("http://[{host}]")
    } else {
        format!("http://{host}")
    };
    let mut url = reqwest::Url::parse(&host).map_err(|error| {
        ApiError::new(
            codes::CLIENT_CONFIG,
            format!("The controller returned an invalid node address '{host}': {error}"),
        )
        .with_remediation("Check the node's host and API port in the controller.")
    })?;
    if let Some(port) = node.api_port {
        url.set_port(Some(port)).map_err(|_| {
            ApiError::new(
                codes::CLIENT_CONFIG,
                format!(
                    "The controller returned an invalid API port for node '{}'",
                    node.id
                ),
            )
        })?;
    } else if url.port().is_none() {
        return Err(ApiError::new(
            codes::CLIENT_CONFIG,
            format!(
                "The controller did not report an API port for node '{}'",
                node.id
            ),
        )
        .with_remediation("Confirm the node advertises its HTTP API in mDNS/registration."));
    }
    Ok(url.to_string().trim_end_matches('/').to_string())
}

fn build_session_spec(args: &ConnectArgs) -> SessionSpec {
    let mode = DisplayMode::new(args.width, args.height, args.fps);
    let track = TrackSpec {
        track_id: TrackId::new("video0"),
        kind: TrackKind::Capture,
        source_id: args.source.clone(),
        display: None,
        format: None,
        mode: Some(mode),
        hdr: false,
    };
    SessionSpec {
        client_id: ClientId::new("ozma-client"),
        principal: "ozma-client".to_string(),
        tracks: vec![track],
        audio: AudioSpec {
            enabled: args.audio,
            mic: false,
            ..Default::default()
        },
        input: InputSpec {
            keyboard: true,
            pointer: PointerMode::Absolute,
            owner_only: true,
            ..Default::default()
        },
        privacy: PrivacyPolicy {
            local_display: LocalDisplayAction::Unchanged,
            consent: args.consent.into(),
            indicate: true,
            disconnect_on_local_unlock: false,
        },
        transport: TransportPreference::Quic,
        ttl_seconds: 300,
    }
}

fn session_failure(session: &Session, phase: &str) -> ApiError {
    if let Some(error) = &session.error {
        return error.clone();
    }
    ApiError::new(
        codes::CLIENT_PROTOCOL,
        format!(
            "The node rejected session '{}' while {phase} (state {:?})",
            session.id, session.state
        ),
    )
    .with_details(json!({ "session": session }))
    .with_remediation(
        "Check the node log and session policy; the controller did not grant the request.",
    )
}

fn step_error(context: &str, error: ApiError) -> ApiError {
    let mut error = error;
    if error.remediation.is_none() {
        error.remediation = Some(format!(
            "Check that the controller/node is reachable and that --controller-token is valid, \
             then retry after: {context}."
        ));
    }
    error
}

struct DecodedSummary {
    width: u32,
    height: u32,
    yuv_len: usize,
}

struct SavedOutputs {
    video_path: Option<String>,
    frame_path: Option<String>,
    decoded: Option<DecodedSummary>,
}

/// Write the first video payload and, when requested, one decoded PPM frame.
/// This is the only place the CLI decodes anything; live video is never
/// presented or decoded by `connect`/`probe`.
async fn write_outputs(
    frame: &VideoFrame,
    save_video: Option<&Path>,
    save_frame: Option<&Path>,
) -> Result<SavedOutputs, ApiError> {
    let decoded = if let Some(path) = save_frame {
        let mut decoder = ozma_decode::H264Decoder::new().map_err(|error| {
            ApiError::new(
                codes::CLIENT_PROTOCOL,
                format!("could not start the H.264 decoder: {error}"),
            )
        })?;
        let meta = ozma_decode::DecodeMeta::new(0, frame.sequence, frame.keyframe);
        let mut decoded = ozma_decode::VideoDecoder::decode(&mut decoder, &frame.data, meta)
            .await
            .map_err(|error| {
                ApiError::new(
                    codes::CLIENT_PROTOCOL,
                    format!("could not decode the first video frame: {error}"),
                )
            })?;
        if decoded.is_none() {
            // Some hardware encoders emit a picture the decoder holds until
            // flush (for example a High-profile first frame).
            decoded = decoder
                .flush()
                .await
                .map_err(|error| {
                    ApiError::new(
                        codes::CLIENT_PROTOCOL,
                        format!("could not flush the first video frame: {error}"),
                    )
                })?
                .into_iter()
                .next();
        }
        let decoded = decoded.ok_or_else(|| {
            ApiError::new(
                codes::CLIENT_PROTOCOL,
                "the decoder did not produce a picture for the first access unit",
            )
        })?;
        decoded.write_ppm(path).map_err(|error| {
            ApiError::new(
                codes::CLIENT_CONFIG,
                format!("Could not write '{}': {error}", path.display()),
            )
        })?;
        Some(DecodedSummary {
            width: decoded.width,
            height: decoded.height,
            yuv_len: decoded.yuv_len(),
        })
    } else {
        None
    };

    if let Some(path) = save_video {
        std::fs::write(path, &frame.data).map_err(|error| {
            ApiError::new(
                codes::CLIENT_CONFIG,
                format!("Could not write '{}': {error}", path.display()),
            )
        })?;
    }

    Ok(SavedOutputs {
        video_path: save_video.map(|path| path.display().to_string()),
        frame_path: save_frame.map(|path| path.display().to_string()),
        decoded,
    })
}

#[allow(clippy::too_many_arguments)]
async fn probe(
    options: &GlobalOptions,
    offer_input: &str,
    cert_path: Option<PathBuf>,
    node_url: Option<String>,
    token: Option<String>,
    server_name: String,
    timeout_secs: u64,
    save_video: Option<PathBuf>,
    save_frame: Option<PathBuf>,
    scene_revision: Option<u64>,
    input_latency: bool,
) -> Result<(), ApiError> {
    let mut offer = load_offer(offer_input)?;
    if let Some(token) = token {
        offer.token = token;
    }

    // --cert is the explicit override. Otherwise the node API supplies the
    // base64 DER certificate from GET /api/v1/transport.
    let certificate_der = match cert_path {
        Some(path) => load_certificate(&path)?,
        None => {
            let node_url = node_url.as_deref().ok_or_else(|| {
                ApiError::new(
                    codes::CLIENT_CERTIFICATE,
                    "No node certificate was supplied",
                )
                .with_remediation(
                    "Pass --cert FILE, or pass --node URL so the client can fetch \
                     /api/v1/transport and decode the DER certificate.",
                )
            })?;
            let fetched = fetch_transport_info(
                node_url,
                options.controller_token.as_deref(),
                options.insecure,
            )
            .await?;
            fetched.certificate_der
        }
    };

    let timeout = Duration::from_secs(timeout_secs.max(1));
    let mut config = NativeClientConfig::new().with_server_name(server_name);
    config.hello_timeout = timeout;
    config.handshake_timeout = timeout.min(Duration::from_secs(5));
    config.scene_cache_bytes = scene_revision.map(|_| 4 * 1024 * 1024);
    let started = Instant::now();

    let mut client =
        NativeSessionClient::connect_with_config(&offer, &certificate_der, config).await?;

    let session_id = client
        .session()
        .map(|session| session.id.clone())
        .unwrap_or_else(|| offer.session_id.clone());
    let state = client.state();
    let permissions = client.permissions();
    let endpoint = client
        .transport_descriptor()
        .endpoint
        .clone()
        .unwrap_or_else(|| "-".to_string());

    if !options.json {
        println!("session:      {session_id}");
        println!(
            "state:        {}",
            state
                .map(|state| format!("{state:?}"))
                .unwrap_or_else(|| "-".into())
        );
        println!(
            "permissions:  {}",
            permissions
                .iter()
                .map(|permission| permission.to_string())
                .collect::<Vec<_>>()
                .join(",")
        );
        println!("endpoint:     {endpoint}");
    }

    if input_latency {
        // Wait for one live frame first so the probe starts from a warm media
        // path rather than racing session startup. Then send a harmless
        // ReleaseAll through the latency path. This measures client send to
        // client-visible encoded frame, not decode/scanout/display latency.
        let readiness = wait_for_probe(&mut client, timeout, started, scene_revision).await;
        let measurement = match readiness {
            Ok(_) => {
                client
                    .measure_input_latency(&InputEvent::ReleaseAll, timeout)
                    .await
            }
            Err(error) => Err(error),
        };

        // Release before closing on every path. Input may be view-only, in
        // which case release_all is a no-op.
        let release = client.release_all().await;
        let close = client.close().await;
        let report = measurement?;
        release?;
        close?;

        if options.json {
            print_json(&json!({
                "session_id": session_id,
                "state": state,
                "permissions": permissions,
                "endpoint": endpoint,
                "input_latency": report,
            }))?;
        } else {
            println!("ack RTT:      {:.3} ms", report.ack_rtt_ms);
            println!(
                "input->frame: {:.3} ms (encoded frame, not display scanout)",
                report.input_to_frame_ms
            );
            match report.server_to_frame_ms {
                Some(ms) => println!("server->frame:{ms:.3} ms"),
                None => println!("server->frame: unavailable (client/server clocks are skewed)"),
            }
            println!(
                "frame:        sequence {} (latest at ack {})",
                report.frame_sequence, report.latest_frame_sequence
            );
            println!("accepted:     {}", report.event_accepted);
            println!(
                "note:         send -> client-visible encoded frame; decode, presentation, \
                 scanout, and panel latency are not included."
            );
        }
        return Ok(());
    }

    let measurements = wait_for_probe(&mut client, timeout, started, scene_revision).await;

    // Release before closing on every path. Input may be view-only, in which
    // case release_all is a no-op.
    let release = client.release_all().await;
    let close = client.close().await;

    let ProbeMeasurement {
        packet,
        frame,
        video_ms,
        scene,
    } = measurements?;
    release?;
    close?;

    let decoded = if let Some(path) = save_frame.as_deref() {
        let mut decoder = ozma_decode::H264Decoder::new().map_err(|error| {
            ApiError::new(
                codes::CLIENT_PROTOCOL,
                format!("could not start the H.264 decoder: {error}"),
            )
        })?;
        let meta = ozma_decode::DecodeMeta::new(0, frame.sequence, frame.keyframe);
        let mut decoded = ozma_decode::VideoDecoder::decode(&mut decoder, &frame.data, meta)
            .await
            .map_err(|error| {
                ApiError::new(
                    codes::CLIENT_PROTOCOL,
                    format!("could not decode the first video frame: {error}"),
                )
            })?;
        if decoded.is_none() {
            // Some hardware encoders emit a picture the decoder holds until
            // flush (for example a High-profile first frame).
            decoded = decoder
                .flush()
                .await
                .map_err(|error| {
                    ApiError::new(
                        codes::CLIENT_PROTOCOL,
                        format!("could not flush the first video frame: {error}"),
                    )
                })?
                .into_iter()
                .next();
        }
        let decoded = decoded.ok_or_else(|| {
            ApiError::new(
                codes::CLIENT_PROTOCOL,
                "the decoder did not produce a picture for the first access unit",
            )
        })?;
        decoded.write_ppm(path).map_err(|error| {
            ApiError::new(
                codes::CLIENT_CONFIG,
                format!("Could not write '{}': {error}", path.display()),
            )
        })?;
        Some(decoded)
    } else {
        None
    };

    if let Some(path) = save_video.as_deref() {
        std::fs::write(path, &frame.data).map_err(|error| {
            ApiError::new(
                codes::CLIENT_CONFIG,
                format!("Could not write '{}': {error}", path.display()),
            )
        })?;
    }

    if options.json {
        let report = json!({
            "session_id": session_id,
            "state": state,
            "permissions": permissions,
            "endpoint": endpoint,
            "scene": scene.map(|(revision, ready_ms)| json!({ "revision": revision, "ready_ms": ready_ms })),
            "video": {
                "ready_ms": video_ms,
                "channel": packet.channel.raw(),
                "kind": packet.kind,
                "track_id": packet.track_id,
                "sequence": frame.sequence,
                "keyframe": frame.keyframe,
                "payload_len": frame.data.len(),
                "saved_to": save_video.map(|path| path.display().to_string()),
                "decoded": decoded.as_ref().map(|frame| json!({
                    "width": frame.width,
                    "height": frame.height,
                    "frame_saved_to": save_frame.map(|path| path.display().to_string()),
                })),
            }
        });
        print_json(&report)?;
    } else {
        println!(
            "video:        channel {} kind {:?} track {} sequence {} payload {} bytes (keyframe {})",
            packet.channel.raw(),
            packet.kind,
            packet
                .track_id
                .as_ref()
                .map(|track| track.as_str())
                .unwrap_or("-"),
            frame.sequence,
            frame.data.len(),
            frame.keyframe
        );
        println!("video ready:  {video_ms} ms");
        if let Some((revision, ready_ms)) = scene {
            println!("scene ready:  revision {revision} after {ready_ms} ms");
        }
        if let Some(path) = save_video {
            println!("saved:        {}", path.display());
        }
        if let Some(frame) = &decoded {
            println!(
                "decoded:      {}x{} ({} bytes YUV)",
                frame.width,
                frame.height,
                frame.yuv_len()
            );
        }
        println!("released all held input and closed the session.");
    }
    Ok(())
}

struct ProbeMeasurement {
    packet: ozma_client::MediaPacket,
    frame: VideoFrame,
    video_ms: u128,
    scene: Option<(u64, u128)>,
}

async fn wait_for_probe(
    client: &mut NativeSessionClient,
    timeout: Duration,
    started: Instant,
    target_revision: Option<u64>,
) -> Result<ProbeMeasurement, ApiError> {
    if target_revision.is_some() && client.scene().is_none() {
        return Err(ApiError::new(
            codes::CLIENT_PROTOCOL,
            "The node did not negotiate the requested scene profile",
        ));
    }

    let mut assembler = VideoAssembler::default();
    let mut video = None;
    let mut scene = None;
    tokio::time::timeout(timeout, async {
        loop {
            match client.recv_event().await? {
                ClientEvent::Media(packet) if packet.kind == ChannelKind::Video && video.is_none() => {
                    let frame = assembler.push_or_complete(&packet.payload).map_err(|error| {
                        ApiError::new(codes::CLIENT_PROTOCOL, format!("invalid video fragment: {error}"))
                    })?;
                    if let Some(frame) = frame {
                        video = Some((packet, frame, started.elapsed().as_millis()));
                    }
                }
                ClientEvent::ScenePresented { revision }
                    if scene.is_none() && target_revision.is_some_and(|target| revision >= target) =>
                {
                    scene = Some((revision, started.elapsed().as_millis()));
                }
                ClientEvent::Error { error } => return Err(error),
                ClientEvent::Closed { reason } => {
                    return Err(ApiError::new(
                        codes::CLIENT_DISCONNECTED,
                        format!("The session closed before the requested video and scene arrived ({})", reason.as_str()),
                    ).retryable());
                }
                ClientEvent::Bye { reason } => {
                    return Err(ApiError::new(
                        codes::CLIENT_DISCONNECTED,
                        format!("The node ended the session before the requested video and scene arrived ({reason:?})"),
                    ));
                }
                _ => {}
            }
            if video.is_some() && (target_revision.is_none() || scene.is_some()) {
                let (packet, frame, video_ms) = video.expect("complete video frame was checked");
                return Ok(ProbeMeasurement { packet, frame, video_ms, scene });
            }
        }
    })
    .await
    .map_err(|_| {
        ApiError::new(
            codes::CLIENT_TIMEOUT,
            format!("Timed out waiting for the requested video and scene after {} ms", timeout.as_millis()),
        )
        .with_remediation("Check that the session is streaming and the node can supply the requested scene revision.")
        .retryable()
    })?
}

fn load_offer(input: &str) -> Result<SessionOffer, ApiError> {
    let trimmed = input.trim();
    let text = if trimmed.starts_with('{') {
        trimmed.to_string()
    } else {
        std::fs::read_to_string(input).map_err(|error| {
            ApiError::new(
                codes::CLIENT_CONFIG,
                format!("Could not read the session offer '{input}': {error}"),
            )
        })?
    };
    serde_json::from_str(&text).map_err(|error| {
        ApiError::new(
            codes::CLIENT_CONFIG,
            format!("The session offer is not valid JSON: {error}"),
        )
    })
}

fn print_json(value: &impl serde::Serialize) -> Result<(), ApiError> {
    let text = serde_json::to_string_pretty(value).map_err(|error| {
        ApiError::new(
            codes::CLIENT_CONFIG,
            format!("Could not encode JSON output: {error}"),
        )
    })?;
    println!("{text}");
    Ok(())
}
