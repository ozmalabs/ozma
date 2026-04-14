use anyhow::Result;
use serde::{Deserialize, Serialize};

/// Status response from the Ozma Agent
#[derive(Debug, Serialize, Deserialize)]
pub struct AgentStatus {
    /// Whether the agent is currently running
    pub is_running: bool,
    /// Agent version string
    pub version: String,
}

/// HTTP client for communicating with the Ozma Agent
pub struct OzmaClient {
    /// Base URL of the agent API
    base_url: String,
}

impl OzmaClient {
    /// Create a new client for the given agent URL
    pub fn new(base_url: String) -> Self {
        Self { base_url }
    }

    /// Get the agent's current status
    ///
    /// Currently returns a stub response. In a real implementation,
    /// this would make an HTTP request to the agent.
    pub async fn get_status(&self) -> Result<AgentStatus> {
        tracing::info!("Fetching agent status from {}", self.base_url);
        
        // Stub implementation
        // TODO: Use reqwest to make actual HTTP request
        Ok(AgentStatus {
            is_running: true,
            version: "0.1.0".to_string(),
        })
    }

    /// Get the base URL of this client
    pub fn base_url(&self) -> &str {
        &self.base_url
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_client_new() {
        let client = OzmaClient::new("http://localhost:7381".to_string());
        assert_eq!(client.base_url(), "http://localhost:7381");
    }

    #[test]
    fn test_client_different_url() {
        let client = OzmaClient::new("http://192.168.1.100:8080".to_string());
        assert_eq!(client.base_url(), "http://192.168.1.100:8080");
    }
}
