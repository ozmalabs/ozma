// SPDX-License-Identifier: AGPL-3.0-only
//! Typed client for the controller's node API.
//!
//! These are the endpoints the controller actually exposes, not the older
//! imagined `/api/machines` contract:
//!
//! - `GET  /api/v1/nodes`
//! - `GET  /api/v1/nodes/{id}`
//! - `POST /api/v1/nodes/{id}/activate`

use std::time::Duration;

use ozma_protocol::ApiError;
use reqwest::{Client, Method};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{json, Value};

use crate::error;
use crate::http::{self, DEFAULT_HTTP_TIMEOUT};

/// One node as the controller describes it.
///
/// The controller evolves this object frequently, so deserialization is
/// deliberately lenient: unknown fields are ignored, `name` falls back to the
/// id, `api_port` accepts a number or numeric string, and `online` defaults to
/// true for a node that appears in the registry. `active` reflects the
/// controller's `hid_active` flag.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "RawControllerNode")]
pub struct ControllerNode {
    /// Stable node identity (normally the mDNS instance name).
    pub id: String,
    /// Human-readable name; falls back to `id` when the controller has none.
    pub name: String,
    /// Resolved host/IP, when the controller knows it.
    pub host: Option<String>,
    /// HTTP API port, when the node advertises one.
    pub api_port: Option<u16>,
    /// Capability names such as `hid`, `video`, or `audio`.
    pub capabilities: Vec<String>,
    /// Whether the controller considers the node online.
    pub online: bool,
    /// Whether the controller currently has this node active.
    pub active: bool,
    /// Node role, for example `compute` or `display`.
    pub role: Option<String>,
    /// Hardware identifier, for example `milkv-duos`.
    pub hw: Option<String>,
    /// Firmware version string.
    pub fw_version: Option<String>,
    /// `workstation`, `server`, `kiosk`, or `camera`.
    pub machine_class: Option<String>,
    /// The node's UDP/HID port.
    pub port: Option<u16>,
}

impl ControllerNode {
    /// Best available display name.
    pub fn display_name(&self) -> &str {
        if self.name.is_empty() {
            &self.id
        } else {
            &self.name
        }
    }
}

#[derive(Deserialize)]
struct RawControllerNode {
    #[serde(deserialize_with = "de_string")]
    id: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    display_name: Option<String>,
    #[serde(default)]
    host: Option<String>,
    #[serde(default)]
    address: Option<String>,
    #[serde(default)]
    ip: Option<String>,
    #[serde(default, deserialize_with = "de_opt_u16")]
    api_port: Option<u16>,
    #[serde(default, deserialize_with = "de_string_vec")]
    capabilities: Vec<String>,
    #[serde(default, deserialize_with = "de_opt_bool")]
    online: Option<bool>,
    #[serde(default)]
    hid_active: bool,
    #[serde(default)]
    active: bool,
    #[serde(default)]
    role: Option<String>,
    #[serde(default)]
    hw: Option<String>,
    #[serde(default)]
    fw_version: Option<String>,
    #[serde(default)]
    machine_class: Option<String>,
    #[serde(default, deserialize_with = "de_opt_u16")]
    port: Option<u16>,
}

impl From<RawControllerNode> for ControllerNode {
    fn from(raw: RawControllerNode) -> Self {
        let name = raw
            .name
            .or(raw.display_name)
            .map(|name| name.trim().to_string())
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| raw.id.clone());
        Self {
            id: raw.id,
            name,
            host: raw.host.or(raw.address).or(raw.ip),
            api_port: raw.api_port,
            capabilities: raw.capabilities,
            online: raw.online.unwrap_or(true),
            active: raw.hid_active || raw.active,
            role: raw.role,
            hw: raw.hw,
            fw_version: raw.fw_version,
            machine_class: raw.machine_class,
            port: raw.port,
        }
    }
}

/// The controller's list response, including the active node id.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct NodeList {
    /// Nodes known to the controller.
    pub nodes: Vec<ControllerNode>,
    /// The controller's currently active node, if any.
    pub active_node_id: Option<String>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum NodeListPayload {
    Wrapped {
        #[serde(default)]
        nodes: Vec<ControllerNode>,
        #[serde(default)]
        active_node_id: Option<String>,
    },
    Bare(Vec<ControllerNode>),
}

impl From<NodeListPayload> for NodeList {
    fn from(payload: NodeListPayload) -> Self {
        match payload {
            NodeListPayload::Wrapped {
                nodes,
                active_node_id,
            } => Self {
                nodes,
                active_node_id,
            },
            NodeListPayload::Bare(nodes) => Self {
                nodes,
                active_node_id: None,
            },
        }
    }
}

/// Result of activating a node.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActivateResponse {
    #[serde(default)]
    pub ok: bool,
    #[serde(default)]
    pub active_node_id: Option<String>,
}

/// Builder for [`ControllerClient`].
#[derive(Clone, Debug)]
pub struct ControllerClientBuilder {
    base_url: String,
    token: Option<String>,
    insecure: bool,
    timeout: Duration,
    no_proxy: bool,
}

impl ControllerClientBuilder {
    /// Start a builder for a controller base URL.
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            token: None,
            insecure: false,
            timeout: DEFAULT_HTTP_TIMEOUT,
            no_proxy: false,
        }
    }

    /// Send `Authorization: Bearer <token>` on every request.
    pub fn token(mut self, token: impl Into<String>) -> Self {
        self.token = Some(token.into());
        self
    }

    /// Accept invalid TLS certificates. Explicitly opt-in for local bring-up.
    pub fn insecure(mut self, insecure: bool) -> Self {
        self.insecure = insecure;
        self
    }

    /// Per-request timeout.
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Bypass environment proxies. Useful for a controller on the LAN and for
    /// deterministic tests.
    pub fn no_proxy(mut self, no_proxy: bool) -> Self {
        self.no_proxy = no_proxy;
        self
    }

    /// Validate the base URL and build the client.
    pub fn build(self) -> Result<ControllerClient, ApiError> {
        let base_url = http::normalize_base_url(&self.base_url)?;
        let http = http::build_client(
            self.token.as_deref(),
            self.insecure,
            self.timeout,
            self.no_proxy,
        )?;
        Ok(ControllerClient {
            base_url,
            http,
            timeout: self.timeout,
        })
    }
}

/// Native client for the controller control plane.
#[derive(Clone, Debug)]
pub struct ControllerClient {
    base_url: String,
    http: Client,
    timeout: Duration,
}

impl ControllerClient {
    /// Create a client for a controller URL.
    pub fn new(base_url: impl Into<String>) -> Result<Self, ApiError> {
        Self::builder(base_url).build()
    }

    /// Start a builder, for tokens and TLS options.
    pub fn builder(base_url: impl Into<String>) -> ControllerClientBuilder {
        ControllerClientBuilder::new(base_url)
    }

    /// The normalised base URL.
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// The request timeout.
    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    /// List known nodes (the node array only).
    pub async fn list_nodes(&self) -> Result<Vec<ControllerNode>, ApiError> {
        Ok(self.list_nodes_response().await?.nodes)
    }

    /// List known nodes plus the active node id.
    pub async fn list_nodes_response(&self) -> Result<NodeList, ApiError> {
        let url = http::join_url(&self.base_url, "/api/v1/nodes")?;
        let payload: NodeListPayload =
            http::send_json(self.http.request(Method::GET, url), "controller node list").await?;
        Ok(payload.into())
    }

    /// Fetch one node by id.
    pub async fn get_node(&self, node_id: &str) -> Result<ControllerNode, ApiError> {
        let url = http::join_url(&self.base_url, &self.node_path(node_id))?;
        let value: Value =
            http::send_json(self.http.request(Method::GET, url), "controller node").await?;
        parse_single_node(value)
    }

    /// Make a node active.
    pub async fn activate_node(&self, node_id: &str) -> Result<ActivateResponse, ApiError> {
        let url = http::join_url(
            &self.base_url,
            &format!("{}/activate", self.node_path(node_id)),
        )?;
        http::send_json(
            self.http.request(Method::POST, url),
            "controller activation",
        )
        .await
    }

    /// Path for one node, with the id percent-encoded as a single segment.
    pub(crate) fn node_path(&self, node_id: &str) -> String {
        format!("/api/v1/nodes/{}", urlencoding::encode(node_id))
    }
}

fn parse_single_node(value: Value) -> Result<ControllerNode, ApiError> {
    let node = value
        .get("node")
        .filter(|node| node.is_object())
        .cloned()
        .unwrap_or_else(|| value.clone());
    serde_json::from_value(node).map_err(|error| {
        error::controller(format!(
            "The controller returned an invalid node object: {error}"
        ))
        .with_details(json!({ "body": value }))
    })
}

fn de_string<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Value::deserialize(deserializer)?;
    match value {
        Value::String(text) => Ok(text),
        Value::Number(number) => Ok(number.to_string()),
        Value::Bool(boolean) => Ok(boolean.to_string()),
        other => Err(serde::de::Error::custom(format!(
            "expected a string id, got {other}"
        ))),
    }
}

fn de_opt_u16<'de, D>(deserializer: D) -> Result<Option<u16>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Option::<Value>::deserialize(deserializer)?;
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(number)) => number
            .as_u64()
            .and_then(|number| u16::try_from(number).ok())
            .map(Some)
            .ok_or_else(|| serde::de::Error::custom("port is out of range")),
        Some(Value::String(text)) => {
            let text = text.trim();
            if text.is_empty() {
                return Ok(None);
            }
            text.parse::<u16>()
                .map(Some)
                .map_err(|error| serde::de::Error::custom(format!("invalid port: {error}")))
        }
        Some(other) => Err(serde::de::Error::custom(format!(
            "expected a numeric port, got {other}"
        ))),
    }
}

fn de_opt_bool<'de, D>(deserializer: D) -> Result<Option<bool>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Option::<Value>::deserialize(deserializer)?;
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(boolean)) => Ok(Some(boolean)),
        Some(Value::Number(number)) => Ok(Some(number.as_u64().unwrap_or(0) != 0)),
        Some(Value::String(text)) => match text.trim().to_ascii_lowercase().as_str() {
            "true" | "yes" | "online" | "up" | "1" => Ok(Some(true)),
            "false" | "no" | "offline" | "down" | "0" => Ok(Some(false)),
            other => Err(serde::de::Error::custom(format!(
                "invalid online value '{other}'"
            ))),
        },
        Some(other) => Err(serde::de::Error::custom(format!(
            "expected an online flag, got {other}"
        ))),
    }
}

fn de_string_vec<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: Deserializer<'de>,
{
    let values = Option::<Vec<Value>>::deserialize(deserializer)?;
    Ok(values
        .unwrap_or_default()
        .into_iter()
        .filter_map(|value| match value {
            Value::String(text) => Some(text),
            Value::Number(number) => Some(number.to_string()),
            _ => None,
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_payload_is_lenient() {
        let payload: NodeListPayload = serde_json::from_str(
            r#"{
                "nodes": [
                    {
                        "id": "node-a",
                        "host": "10.0.0.5",
                        "api_port": 7390,
                        "capabilities": ["hid", "video"],
                        "last_seen": 12.5,
                        "future_field": {"ignored": true}
                    },
                    {
                        "id": "node-b",
                        "name": "Desk node",
                        "online": "offline",
                        "api_port": "7391",
                        "capabilities": null,
                        "hid_active": true
                    }
                ],
                "active_node_id": "node-b",
                "extra": 1
            }"#,
        )
        .unwrap();
        let list: NodeList = payload.into();
        assert_eq!(list.active_node_id.as_deref(), Some("node-b"));
        assert_eq!(list.nodes.len(), 2);

        let first = &list.nodes[0];
        assert_eq!(first.id, "node-a");
        assert_eq!(first.name, "node-a");
        assert_eq!(first.host.as_deref(), Some("10.0.0.5"));
        assert_eq!(first.api_port, Some(7390));
        assert_eq!(first.capabilities, vec!["hid", "video"]);
        assert!(first.online);
        assert!(!first.active);

        let second = &list.nodes[1];
        assert_eq!(second.name, "Desk node");
        assert_eq!(second.api_port, Some(7391));
        assert!(!second.online);
        assert!(second.active);
        assert!(second.capabilities.is_empty());
    }

    #[test]
    fn bare_array_and_single_wrapper_are_accepted() {
        let payload: NodeListPayload = serde_json::from_str(r#"[{"id":"only"}]"#).unwrap();
        let list: NodeList = payload.into();
        assert_eq!(list.nodes.len(), 1);
        assert_eq!(list.nodes[0].name, "only");

        let node = parse_single_node(serde_json::json!({
            "ok": true,
            "node": {"id": "wrapped", "display_name": "Wrapped"}
        }))
        .unwrap();
        assert_eq!(node.id, "wrapped");
        assert_eq!(node.name, "Wrapped");
    }

    #[test]
    fn node_path_encodes_the_id_as_one_segment() {
        let client = ControllerClient::new("http://127.0.0.1:7380").unwrap();
        assert_eq!(client.node_path("node a/b"), "/api/v1/nodes/node%20a%2Fb");
        assert_eq!(client.base_url(), "http://127.0.0.1:7380");
    }
}
