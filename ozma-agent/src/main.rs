//! ozma-agent — desktop seat agent daemon.
//!
//! Spawns six long-running tasks:
//!   • API server        — axum HTTP on `--api-port` (default 7381)
//!   • IPC server        — privileged socket/pipe
//!   • Capture task      — screen / audio capture loop
//!   • Metrics task      — Prometheus scrape endpoint on `--metrics-port` (default 9101)
//!   • Mesh task         — WireGuard peer management
//!   • Registration task — controller registration + heartbeat

mod api;
mod approvals;
mod capture;
mod ipc_server;
mod mesh;
mod metrics;
mod register;

use anyhow::Result;
use clap::Parser;
use tracing::{error, info};

/// ozma-agent: desktop seat agent daemon.
#[derive(Parser, Debug)]
#[command(name = "ozma-agent", version, about)]
struct Cli {
    /// Host/IP to bind the API server on.
    #[arg(long, env = "OZMA_API_HOST", default_value = "0.0.0.0")]
    api_host: String,

    /// TCP port for the HTTP API server.
    #[arg(long, env = "OZMA_API_PORT", default_value_t = 7381)]
    api_port: u16,

    /// TCP port for the Prometheus metrics scrape endpoint.
    #[arg(long, env = "OZMA_METRICS_PORT", default_value_t = 9101)]
    metrics_port: u16,

    /// WireGuard UDP listen port.
    #[arg(long, env = "OZMA_WG_PORT", default_value_t = 51820)]
    wg_port: u16,

    /// Controller URL (used by mesh task to fetch peer list).
    #[arg(long, env = "OZMA_CONTROLLER_URL", default_value = "http://localhost:7380")]
    controller_url: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("ozma_agent=debug".parse()?),
        )
        .init();

    let cli = Cli::parse();

    info!(
        version = env!("CARGO_PKG_VERSION"),
        api_port = cli.api_port,
        metrics_port = cli.metrics_port,
        wg_port = cli.wg_port,
        node_id = register::node_id().as_str(),
        "ozma-agent starting",
    );

    // Shared Prometheus registry.
    let registry = metrics::build_registry();

    // Approval queue.
    let queue = approvals::ApprovalQueue::new();

    let api_addr = format!("{}:{}", cli.api_host, cli.api_port);
    let metrics_addr = format!("{}:{}", cli.api_host, cli.metrics_port);
    let controller_url = cli.controller_url.clone();
    let wg_port = cli.wg_port;
    let api_port = cli.api_port;

    // Spawn all tasks concurrently; if any exits with an error, propagate it.
    let (r1, r2, r3, r4, r5, r6) = tokio::join!(
        tokio::spawn(api::serve(api_addr, queue.clone())),
        tokio::spawn(ipc_server::serve(queue.clone())),
        tokio::spawn(capture::run()),
        tokio::spawn(metrics::serve(metrics_addr, registry)),
        tokio::spawn(mesh::run(controller_url.clone(), wg_port)),
        tokio::spawn(register::run(controller_url, api_port, wg_port)),
    );

    for result in [r1, r2, r3, r4, r5, r6] {
        match result {
            Ok(Ok(())) => {}
            Ok(Err(e)) => error!("task error: {e:#}"),
            Err(e)     => error!("task panicked: {e}"),
        }
    }

    Ok(())
}
