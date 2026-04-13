mod agent;

use axum::Server;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::RwLock;

use agent::api::create_router;
use agent::state::AgentState;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Initialize tracing
    tracing_subscriber::fmt::init();

    // Create shared state
    let state = Arc::new(RwLock::new(AgentState::new()));

    // Create the router
    let app = create_router(state);

    // Run the server
    let addr = SocketAddr::from(([0, 0, 0, 0], 7380));
    println!("Agent API server running on http://{}", addr);

    Server::bind(&addr)
        .serve(app.into_make_service())
        .await?;

    Ok(())
}
