use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, sync::Arc};
use tokio::sync::{broadcast, RwLock};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum ActionType {
    ScreenCapture,
    KeyboardInput,
    MouseClick,
    FileAccess,
    NetworkRequest,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalState {
    Pending,
    Approved,
    Denied,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalRequest {
    pub id: Uuid,
    pub action_type: ActionType,
    pub description: String,
    pub target: String,
    pub requested_at: DateTime<Utc>,
    pub screenshot_b64: Option<String>,
    pub state: ApprovalState,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalConfig {
    pub modes: HashMap<ActionType, String>,
}

pub struct ApprovalQueue {
    pending: RwLock<HashMap<Uuid, ApprovalRequest>>,
    pub tx: broadcast::Sender<AgentEvent>,
    config: RwLock<ApprovalConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentEvent {
    ApprovalRequested {
        request: ApprovalRequest,
    },
    ApprovalResolved {
        id: Uuid,
        state: ApprovalState,
    },
    StatusUpdate {
        connected: bool,
        controller: String,
        uptime_s: u64,
    },
}

impl ApprovalQueue {
    pub fn new() -> Arc<Self> {
        let (tx, _) = broadcast::channel(100);
        Arc::new(Self {
            pending: RwLock::new(HashMap::new()),
            tx,
            config: RwLock::new(ApprovalConfig {
                modes: HashMap::new(),
            }),
        })
    }

    pub async fn push(&self, req: ApprovalRequest) {
        let mut pending = self.pending.write().await;
        pending.insert(req.id, req.clone());
        let _ = self.tx.send(AgentEvent::ApprovalRequested { request: req });
    }

    pub async fn resolve(&self, id: Uuid, state: ApprovalState) -> Option<ApprovalRequest> {
        let mut pending = self.pending.write().await;
        if let Some(req) = pending.remove(&id) {
            let mut resolved_req = req.clone();
            resolved_req.state = state.clone();
            let _ = self.tx.send(AgentEvent::ApprovalResolved { id, state });
            Some(resolved_req)
        } else {
            None
        }
    }

    pub async fn list_pending(&self) -> Vec<ApprovalRequest> {
        let pending = self.pending.read().await;
        pending.values().cloned().collect()
    }

    pub async fn get_config(&self) -> ApprovalConfig {
        self.config.read().await.clone()
    }

    pub async fn set_config(&self, config: ApprovalConfig) {
        let mut cfg = self.config.write().await;
        *cfg = config;
    }
}

impl Default for ApprovalQueue {
    fn default() -> Self {
        Self {
            pending: RwLock::new(HashMap::new()),
            tx: broadcast::channel(100).0,
            config: RwLock::new(ApprovalConfig {
                modes: HashMap::new(),
            }),
        }
    }
}
