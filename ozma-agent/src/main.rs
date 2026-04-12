//! ozma-agent binary entry point.
//!
//! Reads configuration from environment variables, registers with Connect,
//! and runs until SIGTERM / Ctrl-C.

use std::path::PathBuf;

use anyhow::Result;
use tracing::info;
use tracing_subscriber::EnvFilter;

use ozma_agent::{AgentConfig, ConnectClient};

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::from_default_env()
                .add_directive("ozma_agent=debug".parse()?),
        )
        .init();

    info!("ozma-agent starting");

    let cfg = AgentConfig {
        api_base: std::env::var("OZMA_CONNECT_API")
            .unwrap_or_else(|_| "https://connect.ozma.dev/api/v1".into()),
        token: std::env::var("OZMA_CONNECT_TOKEN").unwrap_or_default(),
        machine_id: std::env::var("OZMA_MACHINE_ID")
            .unwrap_or_else(|_| uuid::Uuid::new_v4().to_string()),
        capabilities: std::env::var("OZMA_CAPABILITIES")
            .unwrap_or_else(|_| "hid,stream".into())
            .split(',')
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
            .collect(),
        version: env!("CARGO_PKG_VERSION").to_owned(),
        client_cert_pem: PathBuf::from(
            std::env::var("OZMA_CLIENT_CERT")
                .unwrap_or_else(|_| "/etc/ozma/agent.crt".into()),
        ),
        client_key_pem: PathBuf::from(
            std::env::var("OZMA_CLIENT_KEY")
                .unwrap_or_else(|_| "/etc/ozma/agent.key".into()),
        ),
        ca_cert_pem: std::env::var("OZMA_CA_CERT").ok().map(PathBuf::from),
        wg_private_key: std::env::var("OZMA_WG_PRIVATE_KEY").unwrap_or_default(),
        wg_public_key: std::env::var("OZMA_WG_PUBLIC_KEY").unwrap_or_default(),
    };

    let client = ConnectClient::new(cfg)?;
    client.start().await?;

    info!("ozma-agent running — waiting for shutdown signal");
    tokio::signal::ctrl_c().await?;
    info!("Shutdown signal received");

    client.stop().await;
    Ok(())
}
