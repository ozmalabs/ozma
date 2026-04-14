use anyhow::Result;
use serde::{Deserialize, Serialize};
use tracing::info;

#[derive(Debug, Serialize, Deserialize)]
pub struct AgentStatus {
    pub is_running: bool,
    pub version: String,
}

pub struct OzmaClient {
    base_url: String,
}

impl OzmaClient {
    pub fn new(base_url: String) -> Self {
        Self { base_url }
    }

    pub async fn get_status(&self) -> Result<AgentStatus> {
        info!("Fetching agent status from {}", self.base_url);
        // Stub implementation
        // In a real implementation, we would use reqwest here.
        Ok(AgentStatus {
            is_running: true,
            version: "0.1.0".to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_client_new() {
        let client = OzmaClient::new("http://localhost:7381".to_string());
        assert_eq!(client.base_url, "http://localhost:7381");
    }

    #[tokio::test]
    async fn test_get_status_stub() {
        let client = OzmaClient::new("http://localhost:7381".to_string());
        let status = client.get_status().await.unwrap();
        assert!(status.is_running);
        assert_eq!(status.version, "0.1.0");
    }
}
