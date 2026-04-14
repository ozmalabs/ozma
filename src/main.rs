use anyhow::Result;
use std::sync::Arc;

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize tracing/logging for the controller
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("ozma=debug".parse()?),
        )
        .init();

    tracing::info!("Starting ozma-agent controller...");

    // Create the approval queue - shared by IPC server and future capture/RPA code
    let queue: Arc<ozma_agent::approvals::ApprovalQueue> =
        ozma_agent::approvals::ApprovalQueue::new();

    // Create shared state for the API server
    let api_state = ozma_agent::api::AppState {
        queue: queue.clone(),
    };

    // Spawn the API server
    let api_handle = tokio::spawn(async move {
        ozma_agent::api::serve_with_state("0.0.0.0:7381", api_state).await
    });

    // Spawn the IPC server (privileged socket/pipe)
    let ipc_handle = tokio::spawn(async move {
        ozma_agent::ipc_server::serve(queue.clone()).await
    });

    // Spawn placeholder tasks for other services
    let capture_handle = tokio::spawn(async move {
        tracing::info!("Capture task placeholder - not yet implemented");
        loop {
            tokio::time::sleep(tokio::time::Duration::from_secs(60)).await;
        }
    });

    let metrics_handle = tokio::spawn(async move {
        tracing::info!("Metrics task placeholder - not yet implemented");
        loop {
            tokio::time::sleep(tokio::time::Duration::from_secs(60)).await;
        }
    });

    let mesh_handle = tokio::spawn(async move {
        tracing::info!("Mesh task placeholder - not yet implemented");
        loop {
            tokio::time::sleep(tokio::time::Duration::from_secs(60)).await;
        }
    });

    let register_handle = tokio::spawn(async move {
        tracing::info!("Registration task placeholder - not yet implemented");
        loop {
            tokio::time::sleep(tokio::time::Duration::from_secs(60)).await;
        }
    });

    tracing::info!("ozma-agent started, listening on:");
    tracing::info!("  HTTP API: http://0.0.0.0:7381");
    tracing::info!(
        "  IPC socket: {}",
        ozma_agent::ipc_server::socket_path().display()
    );
    tracing::info!("Press Ctrl+C to shut down");

    // Wait for shutdown signal
    tokio::signal::ctrl_c().await?;
    tracing::info!("Shutdown signal received");

    // Abort all tasks gracefully
    api_handle.abort();
    ipc_handle.abort();
    capture_handle.abort();
    metrics_handle.abort();
    mesh_handle.abort();
    register_handle.abort();

    tracing::info!("ozma-agent stopped");
    Ok(())
}
