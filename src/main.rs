use tokio;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("Starting ozma-agent...");
    
    // TODO: Initialize and spawn API server task
    // TODO: Initialize and spawn capture task
    // TODO: Initialize and spawn metrics task
    // TODO: Initialize and spawn WG mesh task
    
    // Keep the main thread alive
    tokio::signal::ctrl_c().await?;
    println!("Shutting down ozma-agent...");
    
    Ok(())
}
