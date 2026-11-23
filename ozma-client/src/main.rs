// SPDX-License-Identifier: AGPL-3.0-only
//! Headless native Ozma client.
//!
//! `list` and `activate` exercise the controller control plane. `probe` is the
//! M1 acceptance harness: it connects to a node over QUIC, performs
//! `Hello`/`HelloAck`, waits for one video datagram, sends `ReleaseAll`, and
//! closes. No video decoding and no controller are required for `probe`.

#![forbid(unsafe_code)]

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use clap::{Parser, Subcommand};
use ozma_client::{
    codes, fetch_transport_info, load_certificate, ClientEvent, ControllerClient,
    NativeClientConfig, NativeSessionClient,
};
use ozma_protocol::{ApiError, ChannelKind, SessionOffer};
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

    /// Connect to a node, hello, receive one video datagram, release, close.
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
        Command::Probe {
            offer,
            cert,
            node,
            token,
            server_name,
            timeout,
            save_video,
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

    let first_video = wait_for_video(&mut client, timeout).await;

    // Release before closing on every path. Input may be view-only, in which
    // case release_all is a no-op.
    let release = client.release_all().await;
    let close = client.close().await;

    let packet = first_video?;
    release?;
    close?;

    if let Some(path) = save_video.as_deref() {
        std::fs::write(path, &packet.payload).map_err(|error| {
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
            "video": {
                "channel": packet.channel.raw(),
                "kind": packet.kind,
                "track_id": packet.track_id,
                "sequence": packet.sequence,
                "payload_len": packet.len(),
                "saved_to": save_video.map(|path| path.display().to_string()),
            }
        });
        print_json(&report)?;
    } else {
        println!(
            "video:        channel {} kind {:?} track {} sequence {} payload {} bytes",
            packet.channel.raw(),
            packet.kind,
            packet
                .track_id
                .as_ref()
                .map(|track| track.as_str())
                .unwrap_or("-"),
            packet.sequence,
            packet.len()
        );
        if let Some(path) = save_video {
            println!("saved:        {}", path.display());
        }
        println!("released all held input and closed the session.");
    }
    Ok(())
}

async fn wait_for_video(
    client: &mut NativeSessionClient,
    timeout: Duration,
) -> Result<ozma_client::MediaPacket, ApiError> {
    tokio::time::timeout(timeout, async {
        loop {
            match client.recv_event().await? {
                ClientEvent::Media(packet) if packet.kind == ChannelKind::Video => {
                    return Ok(packet);
                }
                ClientEvent::Media(_) => continue,
                ClientEvent::Error { error } => return Err(error),
                ClientEvent::Closed { reason } => {
                    return Err(ApiError::new(
                        codes::CLIENT_DISCONNECTED,
                        format!(
                            "The session transport closed before video arrived ({})",
                            reason.as_str()
                        ),
                    )
                    .retryable());
                }
                ClientEvent::Bye { reason } => {
                    return Err(ApiError::new(
                        codes::CLIENT_DISCONNECTED,
                        format!("The node ended the session before video arrived ({reason:?})"),
                    ));
                }
                _ => continue,
            }
        }
    })
    .await
    .map_err(|_| {
        ApiError::new(
            codes::CLIENT_TIMEOUT,
            format!(
                "Timed out waiting for the first video datagram after {} ms",
                timeout.as_millis()
            ),
        )
        .with_remediation(
            "Check that the node has a capture source and that the session is streaming.",
        )
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
